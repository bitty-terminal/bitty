#!/usr/bin/env python3
"""CTX-2413 expanded runtime acceptance matrix (0.0.25 smoke / 0.0.26 campaign).

Extends the CTX-1344 release matrix so the owner-requested dimensions
(bitty-terminal/bitty#2413) are automated rows instead of manual notes. One
``verification.json`` row is emitted per (configuration, check); every row
carries its axes, status, reason, screenshots, and evidence.

Design constraints (owner directive, see EXPANDED-PLAN.md):

* every run uses isolated XDG roots whose full path stays inside the 100-byte
  AF_UNIX budget (``$XDG_RUNTIME_DIR/ctx-2413/m26``, or ``/tmp/ctx-2413/m26``
  when the runtime dir is unavailable); the owner's config, plugin store, and
  session state are never read or written;
* every headed run happens on Hyprland workspace 5 and is captured with
  ``grim``; the desktop is recorded before and restored afterwards;
* no clipboard read or write, no pattern kills (only PIDs recorded by this
  process group); extension downloads are opt-in and recorded, never silent;
* the candidate binary is built once outside the worktree and reused.

Status vocabulary (normative for this harness):

``PASS`` / ``FAIL``
    The check ran and the product met / missed its acceptance criterion.
``NOT_EXERCISED``
    The check was skipped for an environmental reason that is recorded.
``NOT_APPLICABLE``
    The dimension cannot exist in this build (library-only extension,
    retired plugin, compiled-in extension with no installable artifact).
``BLOCKED``
    The acceptance criterion depends on an open issue or pull request. The
    row names the dependency and is never reported as ``PASS`` until the
    dependency lands and the probe actually runs.

Plans:

``smoke``
    CTX-1344 harness-validation plan (one profile, one background, one plugin).
``smoke2413``
    Short CTX-2413 validation plan: one row per new dimension (settings and
    bar-off profiles, bar top/bottom, light preset + custom palette, graphics
    and net absent, plugin-alone with no extension, restored session).
``full``
    The 0.0.24 campaign: profile/background/theme/plugin one-factor axes.
``expanded``
    The 0.0.26 acceptance campaign: every owner dimension.

Commands::

    python3 harness.py matrix --plan expanded   # print the planned matrix
    python3 harness.py prepare                  # XDG roots, profiles, assets
    python3 harness.py run --plan smoke2413     # CTX-2413 smoke
    python3 harness.py run --plan expanded      # 0.0.26 campaign
    python3 harness.py report --out <dir>       # render RESULTS.md
    python3 harness.py restore-desktop          # restore a recorded state

The harness never edits product source. Product failures are recorded as
rows; the harness only fails (exit 3) when its own assumptions break.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import tomllib

SCHEMA_VERSION = 4
HARNESS_VERSION = "3.0.0"

SCRIPT_DIR = Path(__file__).resolve().parent
FIXTURE_DIR = SCRIPT_DIR / "fixtures"
DEFAULT_OUT = SCRIPT_DIR / "rerun"
# Isolated root (owner-specified). Derived from the runtime dir so no host
# path is hardcoded, and kept short: AF_UNIX socket paths must stay under
# 100 bytes and every store socket lives below this root.
XDG_ROOT_TOKEN = "ctx-2413"
AF_UNIX_PATH_BUDGET = 100


def default_xdg_root() -> Path:
    """Return the isolated XDG root, short enough for AF_UNIX sockets."""
    # Budget: the root plus /stores/<key>/run/<instance>.sock must stay under
    # the AF_UNIX limit; the longest planned store key and instance are
    # covered by the 70-byte reserve.
    reserve = 70
    for base in (os.environ.get("XDG_RUNTIME_DIR"), "/tmp"):
        if not base:
            continue
        candidate = Path(base) / XDG_ROOT_TOKEN / "m26"
        if len(str(candidate)) + reserve <= AF_UNIX_PATH_BUDGET:
            return candidate
    raise SystemExit(
        "no base directory yields an AF_UNIX-safe XDG root under "
        f"{AF_UNIX_PATH_BUDGET} bytes; pass --xdg-root explicitly"
    )


DEFAULT_XDG_ROOT = default_xdg_root()
CAMPAIGN_WORKSPACE = 5
RESTORE_WORKSPACE = 1
INSTANCE_PREFIX = "ctx2413"
EXPECTED_WINDOW_CLASS = "run.bitty.Bitty"

# Theme used for the "file theme" axis; the CLI axis overrides it with a
# different preset so the two layers are distinguishable.
FILE_THEME = "dracula"
CLI_THEME = "nord"

# Documented background fit enum (reference/configuration-options.md).
DOCUMENTED_BG_FITS = ("fill", "fit", "center", "tile", "stretch")
# The owner asked for CSS-style values that the shipped surface does not
# document; they are recorded as NOT_APPLICABLE with a citation and exercised
# through their documented nearest equivalents.
REQUESTED_BG_FITS = ("cover", "contain")

# Published native components (manual/en/extensions/*): the only two with a
# live CDN artifact on 2026-10-21. Exact pins only; latest needs a catalog.
PUBLISHED_EXTENSIONS = {
    "graphics": "0.0.24",
    "net": "0.0.1",
}
# Extension repository slugs vs stored component names.
COMPONENT_NAMES = {"network": "net"}
# Component fixtures used for the net request/denial rows. They are harness
# fixtures under recording/, never product source.
COMPONENT_FIXTURE_NAMES = ("allowed", "denied")
COMPONENT_REQUEST_TIMEOUT_S = 8.0

# Bar/plugin ids used by named surface rows.
BAR_PLUGIN_ID = "bitty-terminal.bar"
PALETTE_PLUGIN_ID = "bitty-terminal.palette"
COMPOSER_PLUGIN_ID = "bitty-terminal.composer"
FILE_MANAGER_PLUGIN_ID = "bitty-terminal.file-manager"
STATUSLINE_PLUGIN_ID = "bitty-terminal.statusline"
NAMED_PLUGIN_SURFACES = (
    PALETTE_PLUGIN_ID,
    COMPOSER_PLUGIN_ID,
    FILE_MANAGER_PLUGIN_ID,
    STATUSLINE_PLUGIN_ID,
)

# Dependencies for rows that cannot pass yet (#2413: never PASS while blocked).
CORE_BAR_ANCHORS_ISSUE = 2411
CORE_BAR_ANCHORS_PR = 2423
BAR_ANCHORS_BLOCKER = (
    "bitty-terminal/bitty#2411 (band group anchors + pointer hover) is open; "
    "PR bitty-terminal/bitty#2423 carries the implementation"
)
BAR_ALIGNMENT_BLOCKER = (
    "bitty-terminal/bitty#2411 (band group anchors) is open; "
    "PR bitty-terminal/bitty#2423 is the implementation"
)
BAR_HOVER_BLOCKER = (
    "bitty-terminal/bitty#2411 (pointer enter/leave hover) is open; "
    "PR bitty-terminal/bitty#2423 is the implementation"
)
# bar#31 (icon maps + per-button pointer actions, including on_scroll) merged;
# the wheel row now asserts routing instead of excusing it.
BAR_SCROLL_DEPENDENCY = "bar#31 merged; per-button/wheel routing is expected"
STATUSLINE_BLOCKER = (
    "statusline is not an official registry entry and its manifest requires "
    "bitty >=0.1,<1.0; the shipped 0.0.x candidate cannot activate it"
)
URL_DETECTOR_CITATION = (
    "manual/en/extensions/url-detector.md: runtime dependency "
    "bitty-url-detector; hover underline is the visible surface"
)
GRAPHICS_CITATION = (
    "manual/en/extensions/graphics.md: published 0.0.24 decode component; "
    "Core owns intake/placement/present"
)
NET_CITATION = (
    "manual/en/extensions/network.md: published net 0.0.1 component; "
    "component broker owns request/denial"
)
STORAGE_CITATION = (
    "manual/en/extensions/storage.md: bounded KV mechanics; no published "
    "component artifact, session snapshot is the visible runtime path"
)

# Extensions that are compiled into the 0.0.x binary through an exact-rev git
# dependency. There is no installable artifact and no feature flag, so
# "absent" is not a shippable configuration; the present behaviour is
# asserted instead and the absent column is an explicit NOT_APPLICABLE row.
COMPILED_EXTENSIONS = {
    "ipc": "bitty-ipc (workspace dependency, exact rev)",
    "storage": "bitty-storage (workspace dependency, exact rev)",
    "url-detector": "bitty-url-detector (workspace dependency, exact rev)",
    "platform-services": "bitty-platform-services (workspace dependency, exact rev)",
}
# Published native components with a live CDN artifact: presence is a real
# axis (installed / absent / removed).
COMPONENT_EXTENSIONS = ("graphics", "net")


# Bounded waits. Every loop below is a bounded local wait, never a poll for
# remote state.
WINDOW_TIMEOUT_S = 30.0
WINDOW_POLL_S = 0.25
PROCESS_EXIT_TIMEOUT_S = 15.0
PROCESS_POLL_S = 0.2
SHELL_READY_TIMEOUT_S = 20.0
LOG_LINE_TIMEOUT_S = 4.0
RENDER_SETTLE_S = 1.2

# Screenshot / rendering thresholds.
MIN_DISTINCT_COLORS = 6
BG_COVERAGE_MIN = 0.45
BG_CENTER_COVERAGE_MIN = 0.5
THEME_BG_TOLERANCE = 3
KITTY_DIFF_MIN_PX = 2000
OVERLAY_DIFF_MIN_PX = 1200
BAR_EDGE_MIN_PX = 40
BAR_EDGE_CONTRAST_PX = 200
URL_HOVER_MIN_PX = 120
BAR_OCR_CONFIDENCE_MIN = 30
# Expanded-plan thresholds (#2413).
PROFILE_DELTA_MIN_PX = 20000  # settings-heavy vs minimal must be visibly different
# Distinct non-background colors per ANSI consumer. `ls --color` only paints a
# few GNU coreutils slots (di/ln/ex), so its bar is lower than Vim's syntax.
ANSI_SPOT_MIN_COLORS = {"vim": 6, "ls": 3}
ANSI_SPOT_MIN_PIXELS = 25  # ignore anti-aliasing fringes and single glyphs
THEME_TOTAL_MIN = 30  # shipped presets the campaign must cover
THEME_FAMILY_MIN = 1  # at least one light and one dark preset
BAR_ABSENT_MAX_WORKSPACE_CHANGES = 0  # clicks on a bar-less window never switch
GRAPHICS_RESIDUE_MAX = 0  # component artifacts left after `extension remove`

# Chord pool for plugin-command probes. ctrl+alt+letters are unbound in the
# shipped keymap (only arrows and ctrl+alt+shift+* are taken), and they avoid
# the kernel VT-switch chords ctrl+alt+F1..F12. Empirically the host consumes
# ctrl+alt+h, ctrl+alt+j, and ctrl+alt+p as legacy control input before keymap
# dispatch (measured on candidate 00b20089), so the pool excludes them.
CHORD_POOL = [f"ctrl+alt+{c}" for c in "abcdefgiklmno"]
FAILURE_PROBE_CHORD = "ctrl+alt+q"

# First-command probes can only run through a keymap, which cannot pass
# arguments. Prefer a documented no-argument command where the manifest's
# first lazy command requires args (bar's `focus` needs a workspace id).
PROBE_COMMAND_OVERRIDES = {
    "bitty-terminal.bar": "bitty-terminal.bar:audio-toggle",
}

# Named error class expected from a failed keymap command dispatch.
FAILURE_PATH_PATTERN = re.compile(
    r"keymap command:(?P<target>\S+) refused \((?P<detail>[^)]*)\)"
)

# Extension check specifications. Ids are derived from workspace.toml
# (kind = "extension"), never hardcoded as a roster.
EXTENSION_CHECKS = {
    "ipc": {
        "check": "ctl instance/window/view/terminal list + view split + terminal send",
        "surface": "runtime",
    },
    "storage": {
        "check": "session snapshot written on window close",
        "surface": "runtime",
        "reason": "no published component artifact; session save is the visible path",
    },
    "url-detector": {
        "check": "Ctrl+hover URL underline over a printed plaintext link",
        "surface": "runtime",
    },
    "graphics": {
        "check": "published decode component installed/removed; kitty image + background image",
        "surface": "runtime",
    },
    "network": {
        "check": "net component request/response and denied request through the broker",
        "surface": "runtime",
    },
    "platform-services": {
        "check": "desktop + in-app notification through a plugin command",
        "surface": "runtime",
        "reason": "notification delivery needs a plugin-specific probe chord",
    },
    "observability": {
        "check": "tracing seam",
        "surface": "library-only",
        "reason": "API-only seam; not consumed by this build",
    },
    "a11y": {
        "check": "accessibility adapter",
        "surface": "library-only",
        "reason": "extracted adapter is not a Core dependency of this build",
    },
    "agent": {
        "check": "agent protocol layer",
        "surface": "library-only",
        "reason": "no runtime or component surface in this build",
    },
    "execution": {
        "check": "execution supervisor",
        "surface": "library-only",
        "reason": "no runtime or component surface in this build",
    },
}

DOC_CITATIONS = {
    "background_fit": (
        "reference/configuration-options.md:218 documents "
        "decoration.background_fit as fill|fit|center|tile|stretch "
        "(window.background_fit mirrors it); cover/contain are not shipped"
    ),
    "background_contract": (
        "configuration/lua-and-xdg.md:831-838: the per-panel "
        "background-image contract is accepted but not supported yet"
    ),
    "profile_selection": (
        "configuration/lua-and-xdg.md:370-380: --profile over BITTY_PROFILE; "
        "a requested-but-missing profile fails closed (exit 2)"
    ),
    "theme": (
        "reference/configuration-options.md:266: appearance.theme accepts a "
        "known preset; there is no theme-file path (line 290)"
    ),
    "animations": (
        "reference/configuration-options.md:272: appearance.animations.enabled"
    ),
    "session": "specifications/verify-ctx-0940-w147-evidence.md:153: opt-in session.restore_on_startup",
    "kitty": "reference/terminal-parity-matrix.md: kitty graphics subset",
    "plugin_registry": (
        "bitty-plugins/registry/official/*.toml is the official plugin roster; "
        "bitty-plugins/plugins/* directory listings are not authoritative"
    ),
}

PROFILE_DEV = """\
-- CTX-2413 matrix profile: settings-heavy keys present.
-- Core tables require their mandatory members: `terminal` needs scrollback,
-- `font` needs family and size, `window` needs opacity and padding.
return {
    terminal = { scrollback = 50000 },
    font = { family = "JetBrainsMono Nerd Font", size = 13.0 },
    layout = { gaps_out = 10 },
}
"""

PROFILE_MINIMAL = """\
-- CTX-2413 matrix profile: no settings; defaults must win.
return {}
"""

PROFILE_SETTINGS = """\
-- CTX-2413 expanded matrix profile: settings-heavy but schema-valid.
-- Every table carries its mandatory members; values are inside documented
-- bounds (reference/configuration-options.md).
return {
    terminal = { scrollback = 100000, cursor_style = "steady_bar" },
    font = { family = "JetBrainsMono Nerd Font", size = 14.0, line_height = 1.2 },
    layout = { gaps_in = 4, gaps_out = 12 },
    window = { opacity = 0.96, padding = 12 },
    appearance = { animations = { enabled = true } },
}
"""

PROFILE_BAR_OFF = """\
-- CTX-2413 expanded matrix profile: the bar is declared disabled.
-- `workspace.show_bar` is the documented reserved Core input consumed by
-- extensions; the bar plugin's own `show = false` setting is written next to
-- this profile because plugin visibility is plugin-owned in 0.0.x.
return {
    workspace = { show_bar = false },
}
"""

PROFILE_PLUGINS_TEMPLATE = """\
-- CTX-2413 matrix profile: every official plugin declared enabled.
return {
    plugins = {
%(entries)s
    },
}
"""


# ---------------------------------------------------------------------------
# Small utilities
# ---------------------------------------------------------------------------


def now() -> str:
    return dt.datetime.now().astimezone().isoformat(timespec="seconds")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def sanitize(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-")


def read_json(path: Path):
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path: Path, payload) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    tmp.replace(path)


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def probe_bar_anchors(repo: str = "bitty-terminal/bitty") -> dict:
    """Return the merge state of the band-anchor dependency (#2411/#2423).

    One bounded ``gh`` call; any failure degrades to ``open`` so a row can
    only ever be BLOCKED, never a fabricated PASS. The probe result is
    recorded in the row evidence.
    """
    result = {
        "issue": CORE_BAR_ANCHORS_ISSUE,
        "pull_request": CORE_BAR_ANCHORS_PR,
        "state": "open",
        "source": "default (gh probe skipped or failed)",
        "checked_at": now(),
    }
    try:
        proc = run_cmd(
            [
                "gh",
                "pr",
                "view",
                str(CORE_BAR_ANCHORS_PR),
                "--repo",
                repo,
                "--json",
                "state,mergedAt",
            ],
            timeout=20.0,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        result["source"] = f"gh probe error: {exc!r}"
        return result
    if proc.returncode != 0:
        result["source"] = (
            f"gh probe exit {proc.returncode}: {proc.stderr.strip()[:200]}"
        )
        return result
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError:
        result["source"] = "gh probe returned non-JSON"
        return result
    result["pull_request_state"] = payload.get("state")
    result["merged_at"] = payload.get("mergedAt")
    if payload.get("state") == "MERGED":
        result["state"] = "merged"
    result["source"] = "gh pr view"
    return result


def read_text_tail(path: Path, lines: int) -> list[str]:
    if not path.is_file():
        return []
    text = path.read_text(encoding="utf-8", errors="replace").splitlines()
    return text[-lines:]


def indent(text: str, spaces: int) -> str:
    prefix = " " * spaces
    return "\n".join(prefix + line if line else line for line in text.splitlines())


def lua_value(value, depth: int = 0) -> str:
    """Render a Python scalar/list/dict as a bounded Lua table literal."""
    pad = " " * (depth * 4)
    child = " " * ((depth + 1) * 4)
    if isinstance(value, dict):
        items = [
            f"{child}{key} = {lua_value(item, depth + 1)},"
            for key, item in value.items()
        ]
        return "{\n" + "\n".join(items) + f"\n{pad}}}"
    if isinstance(value, (list, tuple)):
        items = [f"{child}{lua_value(item, depth + 1)}," for item in value]
        return "{\n" + "\n".join(items) + f"\n{pad}}}"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    return json.dumps(str(value), ensure_ascii=False)


def lua_table(value: dict, base_depth: int = 0) -> str:
    return lua_value(value, base_depth)


def is_plugin_id(value: str) -> bool:
    return bool(re.fullmatch(r"[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*", value))


def free_tcp_port() -> int:
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def probe_tcp_port(port: int) -> bool:
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.2)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def find_workspace(start: Path) -> Path:
    override = os.environ.get("BITTY_WORKSPACE")
    if override:
        return Path(override).expanduser().resolve()
    for parent in [start, *start.parents]:
        if (parent / "workspace.toml").is_file() and (parent / "bitty").is_dir():
            return parent
    raise SystemExit(
        "cannot locate the Bitty workspace; set BITTY_WORKSPACE or pass --workspace"
    )


def run_cmd(
    argv: list[str],
    *,
    env: dict[str, str] | None = None,
    timeout: float = 30.0,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess:
    return subprocess.run(
        argv,
        env=env,
        cwd=str(cwd) if cwd else None,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def hypr_eval(expr: str) -> subprocess.CompletedProcess:
    return run_cmd(["hyprctl", "eval", expr], timeout=10.0)


def hypr_json(what: str):
    proc = run_cmd(["hyprctl", what, "-j"], timeout=10.0)
    if proc.returncode != 0:
        raise RuntimeError(f"hyprctl {what} failed: {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def hypr_dispatch(expr: str) -> None:
    proc = hypr_eval(f"hl.dispatch({expr})")
    if proc.returncode != 0:
        raise RuntimeError(f"hyprctl dispatch failed: {proc.stderr.strip()}")


def focus_workspace(ws: int) -> None:
    hypr_dispatch(f"hl.dsp.focus({{workspace={ws}}})")


def focus_window(address: str) -> None:
    hypr_dispatch(f'hl.dsp.focus({{window="address:{address}"}})')


def close_window(address: str) -> None:
    hypr_dispatch(f'hl.dsp.window.close({{window="address:{address}"}})')


def desktop_state() -> dict:
    workspace = hypr_json("activeworkspace")
    active = hypr_json("activewindow") or {}
    clients = hypr_json("clients")
    return {
        "captured_at": now(),
        "workspace": workspace.get("id"),
        "workspace_name": workspace.get("name"),
        "active_window": active.get("address"),
        "active_window_class": active.get("class"),
        "clients": [
            {
                "address": c.get("address"),
                "class": c.get("class"),
                "pid": c.get("pid"),
                "workspace": (c.get("workspace") or {}).get("id"),
            }
            for c in clients
        ],
    }


def restore_desktop(state: dict) -> dict:
    """Restore a recorded desktop state without touching foreign processes."""
    target_ws = state.get("workspace") or RESTORE_WORKSPACE
    focus_workspace(int(target_ws))
    restored_window = None
    active = state.get("active_window")
    if active:
        alive = {c.get("address") for c in hypr_json("clients")}
        if active in alive:
            focus_window(active)
            restored_window = active
    after = desktop_state()
    return {
        "requested_workspace": target_ws,
        "restored_window": restored_window,
        "after": after,
    }


# ---------------------------------------------------------------------------
# Matrix definition
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class MatrixConfig:
    """One configuration row-set in the matrix.

    ``cid`` is the stable identifier used in screenshot names and the
    ``config`` field of every row.
    """

    cid: str
    axes: dict
    store: str = "full"
    profile: str | None = None
    profile_env: str | None = None
    theme_file: str | None = None
    theme_cli: str | None = None
    bg: str | None = None  # documented fit value
    bg_requested: str | None = None  # cover/contain probe
    animations: bool | None = None
    plugin_id: str | None = None
    session_restore: bool = False
    gui: bool = True
    interactions: bool = True
    kitty_image: bool = False
    fail_closed: str | None = None  # profile | bg-fit | theme | palette
    plugins_enabled: bool = True
    # Expanded-plan axes (0.0.26 acceptance). Never used by the 0.0.24 plans.
    plugin_mode: str = ""  # "all" | "none" | "" (derive from store)
    plugin_settings: dict = dataclasses.field(default_factory=dict)
    extensions: tuple = ()
    extra_keymaps: tuple = ()
    local_plugins: tuple = ()
    custom_palette: dict = dataclasses.field(default_factory=dict)
    theme_category: str | None = None
    quick: bool = False
    bar_check: bool = False
    bar_edge: str | None = None
    bar_groups: tuple = ()
    hover_url: bool = False
    graphics_cycle: bool = False
    net_checks: bool = False
    graphics_expected: bool | None = None
    net_expected: bool | None = None
    # CTX-2413 additions.
    spot_check: str = ""  # "" | "vim" | "ls" (ANSI spot check in the shell)
    compare_group: str = ""  # cross-config visible-delta family
    bar_absent_probe: bool = False  # exercise hit regions without the bar

    def axes_dict(self) -> dict:
        return {
            "profile": self.profile or "(none)",
            "profile_env": self.profile_env,
            "theme_file": self.theme_file,
            "theme_cli": self.theme_cli,
            "theme_category": self.theme_category,
            "custom_palette": bool(self.custom_palette),
            "background": self.bg,
            "background_requested": self.bg_requested,
            "animations": self.animations,
            "plugin_set": self.store,
            "plugin_mode": self.plugin_mode or "(store)",
            "extensions": list(self.extensions),
            "bar_edge": self.bar_edge,
            "bar_groups": list(self.bar_groups),
            "session": "restored" if self.session_restore else "fresh",
            "spot_check": self.spot_check or None,
            "compare_group": self.compare_group or None,
        }


def build_matrix(catalog: PluginCatalog) -> list[MatrixConfig]:
    """Return the planned configurations (one-factor-at-a-time baseline)."""
    cfgs: list[MatrixConfig] = [
        MatrixConfig(
            cid="baseline",
            axes={"note": "defaults, full plugin set, fresh session"},
        ),
        # Profile axis.
        MatrixConfig(
            cid="profile-dev",
            axes={
                "note": "settings-heavy profile: scrollback 50000, font 13, gaps_out 10"
            },
            profile="dev",
        ),
        MatrixConfig(
            cid="profile-minimal",
            axes={"note": "empty profile: every key must resolve to defaults"},
            profile="minimal",
        ),
        MatrixConfig(
            cid="profile-plugins",
            axes={"note": "all official plugins declared enabled"},
            profile="plugins",
        ),
        # Background axis: documented substitutes. The requested cover/contain
        # values live in PROBE_CONFIGS because the shipped enum rejects them.
        MatrixConfig(
            cid="bg-fill",
            axes={"note": "documented substitute for cover (fill)"},
            bg="fill",
        ),
        MatrixConfig(
            cid="bg-fit",
            axes={"note": "documented substitute for contain (fit)"},
            bg="fit",
        ),
        # Appearance axis.
        MatrixConfig(
            cid="theme-file",
            axes={"note": "file-level theme override"},
            theme_file=FILE_THEME,
        ),
        MatrixConfig(
            cid="theme-cli",
            axes={"note": "file theme overridden by --theme"},
            theme_file=FILE_THEME,
            theme_cli=CLI_THEME,
        ),
        # Animations axis.
        MatrixConfig(
            cid="animations-off",
            axes={"note": "appearance.animations.enabled = false"},
            animations=False,
        ),
        # Session axis.
        MatrixConfig(
            cid="session-restored",
            axes={"note": "split, close, relaunch with restore_on_startup"},
            session_restore=True,
        ),
        # Image rendering row.
        MatrixConfig(
            cid="image-kitty",
            axes={"note": "kitty graphics PNG crop from the selected wallpaper"},
            interactions=False,
            kitty_image=True,
        ),
    ]
    # Each official plugin alone.
    for plugin in catalog.plugins:
        cfgs.append(
            MatrixConfig(
                cid=f"plugin-{sanitize(plugin.slug)}",
                axes={"note": f"only {plugin.id} installed and enabled"},
                store=f"plugin-{sanitize(plugin.slug)}",
                plugin_id=plugin.id,
            )
        )
    return cfgs


def smoke_configs() -> list[MatrixConfig]:
    """Short harness-validation plan: one profile, one background, one plugin."""
    return [
        MatrixConfig(
            cid="profile-dev",
            axes={"note": "smoke: settings-heavy profile"},
            profile="dev",
        ),
        MatrixConfig(
            cid="bg-fill",
            axes={"note": "smoke: documented background fill"},
            bg="fill",
            interactions=False,
        ),
        MatrixConfig(
            cid="plugin-activity",
            axes={"note": "smoke: activity plugin alone"},
            store="plugin-activity",
            plugin_id="bitty-featured.activity",
        ),
    ]


def smoke2413_configs() -> list[MatrixConfig]:
    """CTX-2413 validation plan: exactly one row per new dimension.

    Every configuration below exercises an owner-requested dimension from
    bitty#2413; the plan is the fast proof that the new rows execute before
    the full ``expanded`` campaign runs on the release candidate.
    """
    return [
        MatrixConfig(
            cid="profile-minimal",
            axes={"note": "smoke2413: empty profile, baseline for the visible delta"},
            profile="minimal",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-settings",
            axes={
                "note": (
                    "smoke2413: settings-heavy profile "
                    "(scrollback 100000, font 14/1.2, opacity .96, padding 12)"
                )
            },
            profile="settings",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-bar-off",
            axes={"note": "smoke2413: bar declared disabled via profile + settings"},
            profile="bar-off",
            store="full",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(show=False)},
            bar_check=True,
            bar_edge="none",
            bar_absent_probe=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-top",
            axes={"note": "smoke2413: bar plugin alone, top edge, dark preset"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="top")},
            bar_check=True,
            bar_edge="top",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-bottom-light",
            axes={"note": "smoke2413: bottom edge on a light preset (github-light)"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            theme_file="github-light",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="bottom")},
            bar_check=True,
            bar_edge="bottom",
            interactions=False,
        ),
        MatrixConfig(
            cid="theme-github-light",
            axes={"note": "smoke2413: shipped light preset"},
            store="none",
            plugin_mode="none",
            theme_file="github-light",
            theme_category="light",
            quick=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="theme-custom-palette",
            axes={"note": "smoke2413: complete inline custom palette"},
            store="none",
            plugin_mode="none",
            theme_file="bitty-dark",
            theme_category="custom",
            custom_palette=CUSTOM_PALETTE,
            quick=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="theme-vim-dark",
            axes={"note": "smoke2413: Vim syntax colors under a dark preset"},
            store="none",
            plugin_mode="none",
            theme_file="tokyo-night",
            theme_category="dark",
            spot_check="vim",
            interactions=False,
        ),
        MatrixConfig(
            cid="theme-ls-light",
            axes={"note": "smoke2413: ls --color ANSI slots under a light preset"},
            store="none",
            plugin_mode="none",
            theme_file="catppuccin-latte",
            theme_category="light",
            spot_check="ls",
            interactions=False,
        ),
        MatrixConfig(
            cid="graphics-on",
            axes={"note": "smoke2413: published graphics 0.0.24 installed"},
            store="ext-graphics-on",
            plugin_mode="none",
            extensions=("graphics",),
            graphics_expected=True,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="graphics-off",
            axes={"note": "smoke2413: no graphics component; kitty + background image"},
            store="ext-graphics-off",
            plugin_mode="none",
            extensions=(),
            graphics_expected=False,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="graphics-removed",
            axes={"note": "smoke2413: install, remove, render; no stale kitty cache"},
            store="ext-graphics-removed",
            plugin_mode="none",
            extensions=(),
            graphics_expected=False,
            graphics_cycle=True,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="net-off",
            axes={"note": "smoke2413: net component absent; broker denial is typed"},
            store="ext-net-off",
            plugin_mode="none",
            extensions=(),
            net_expected=False,
            net_checks=True,
            local_plugins=COMPONENT_FIXTURE_NAMES,
            extra_keymaps=(
                ("ctrl+alt+o", "command:acme.acceptance-allowed:go"),
                ("ctrl+alt+k", "command:acme.acceptance-allowed:poll"),
                ("ctrl+alt+n", "command:acme.acceptance-denied:deny"),
            ),
            interactions=False,
        ),
        MatrixConfig(
            cid="plugin-palette",
            axes={"note": "smoke2413: palette alone with no native extension"},
            store="plugin-palette",
            plugin_id=PALETTE_PLUGIN_ID,
            interactions=True,
        ),
        MatrixConfig(
            cid="all-on-restored",
            axes={"note": "smoke2413: session restore across relaunch"},
            store="full",
            session_restore=True,
        ),
        MatrixConfig(
            cid="url-hover",
            axes={"note": "smoke2413: Ctrl+hover URL underline"},
            store="ext-url",
            plugin_mode="none",
            hover_url=True,
            interactions=False,
        ),
    ]


def smoke2413_probes() -> list[MatrixConfig]:
    """Headless probes used by the CTX-2413 smoke plan."""
    return [
        MatrixConfig(
            cid="probe-plugin-inventory",
            axes={"note": "official plugin roster and retired-checkout exclusion"},
            gui=False,
            interactions=False,
            fail_closed="inventory",
        ),
        MatrixConfig(
            cid="probe-extension-inventory",
            axes={"note": "workspace extension roster and published artifacts"},
            gui=False,
            interactions=False,
            fail_closed="extensions",
        ),
        MatrixConfig(
            cid="probe-extension-matrix",
            axes={"note": "present/absent row pair for every official extension"},
            gui=False,
            interactions=False,
            fail_closed="extension-matrix",
        ),
        MatrixConfig(
            cid="probe-statusline",
            axes={"note": "statusline surface: retired registry + 0.1 floor"},
            gui=False,
            interactions=False,
            fail_closed="statusline",
        ),
    ]


def bar_lua_settings(
    *,
    edge: str | None = None,
    groups: tuple = (),
    show: bool = True,
    show_single: bool = True,
) -> str:
    """Return the canonical Lua body for the bar plugin settings file."""
    lines = [
        f"show = {str(show).lower()},",
        f"show_single = {str(show_single).lower()},",
    ]
    if edge:
        lines.append(f'edge = "{edge}",')
    names = ("modules_left", "modules_center", "modules_right")
    for index, name in enumerate(names):
        if index < len(groups) and groups[index]:
            body = ", ".join(f'"{item}"' for item in groups[index])
            lines.append(f"{name} = {{ {body} }},")
    return "\n".join(lines)


CUSTOM_PALETTE = {
    "background": "#0b1020",
    "foreground": "#e6e6fa",
    "cursor": "#ff2e88",
    "selection": "#24304a",
    "ansi": [
        "#101020",
        "#ff5555",
        "#50fa7b",
        "#f1fa8c",
        "#7aa2f7",
        "#ff79c6",
        "#8be9fd",
        "#c0caf5",
        "#414868",
        "#ff6e6e",
        "#69ff94",
        "#ffffa5",
        "#d6acff",
        "#ff92df",
        "#a4ffff",
        "#ffffff",
    ],
}


def expanded_theme_configs(themes: list[dict]) -> list[MatrixConfig]:
    """One lightweight launch per shipped preset, plus the custom palette."""
    configs: list[MatrixConfig] = []
    for theme in themes:
        name = theme["name"]
        configs.append(
            MatrixConfig(
                cid=f"theme-{sanitize(name)}",
                axes={"note": f"shipped preset {name} ({theme.get('category')})"},
                store="none",
                plugin_mode="none",
                theme_file=name,
                theme_category=theme.get("category"),
                quick=True,
                interactions=False,
                gui=True,
            )
        )
    configs.append(
        MatrixConfig(
            cid="theme-custom-palette",
            axes={"note": "complete inline appearance.colors palette"},
            store="none",
            plugin_mode="none",
            theme_file="bitty-dark",
            theme_category="custom",
            custom_palette=CUSTOM_PALETTE,
            quick=True,
            interactions=False,
            gui=True,
        )
    )
    return configs


def build_expanded_matrix(
    catalog: PluginCatalog, themes: list[dict]
) -> list[MatrixConfig]:
    """0.0.26 acceptance expansion: profiles, themes, bar, extensions, plugins."""
    configs: list[MatrixConfig] = [
        MatrixConfig(
            cid="all-on",
            axes={"note": "defaults, every official plugin, no native component"},
        ),
        MatrixConfig(
            cid="profile-default",
            axes={"note": "no profile: built-in defaults"},
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-dev",
            axes={"note": "settings-heavy dev profile (scrollback/font/gaps)"},
            profile="dev",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-minimal",
            axes={"note": "empty minimal profile: every key resolves to defaults"},
            profile="minimal",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-plugins",
            axes={"note": "all official plugins declared enabled"},
            profile="plugins",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-settings",
            axes={
                "note": (
                    "settings-heavy: scrollback 100000, font 14/1.2, gaps 4/12, "
                    "opacity .96, padding 12, steady bar cursor"
                )
            },
            profile="settings",
            store="full",
            compare_group="profile",
        ),
        MatrixConfig(
            cid="profile-settings-restored",
            axes={"note": "settings-heavy profile with a restored session"},
            profile="settings",
            store="full",
            session_restore=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="profile-bar-off",
            axes={
                "note": (
                    "bar-off profile sets workspace.show_bar=false; plugin "
                    "settings show=false (visibility is plugin-owned)"
                )
            },
            profile="bar-off",
            store="full",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(show=False)},
            bar_check=True,
            bar_edge="none",
            bar_absent_probe=True,
            interactions=False,
        ),
        # Bar edge/group axis. The bar plugin alone keeps the surface isolated.
        MatrixConfig(
            cid="bar-bottom",
            axes={"note": "bar plugin alone, default bottom edge, clock center"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="bottom")},
            bar_check=True,
            bar_edge="bottom",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-top",
            axes={"note": "bar plugin alone, top edge"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="top")},
            bar_check=True,
            bar_edge="top",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-top-light",
            axes={"note": "top edge on a light preset (github-light)"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            theme_file="github-light",
            theme_category="light",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="top")},
            bar_check=True,
            bar_edge="top",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-bottom-light",
            axes={"note": "bottom edge on a light preset (github-light)"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            theme_file="github-light",
            theme_category="light",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="bottom")},
            bar_check=True,
            bar_edge="bottom",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-top-dark",
            axes={"note": "top edge on the default dark preset (bitty-dark)"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            theme_file="bitty-dark",
            theme_category="dark",
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="top")},
            bar_check=True,
            bar_edge="top",
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-groups",
            axes={
                "note": (
                    "left/center/right groups: workspaces | clock | cpu; "
                    "true alignment blocked by bitty#2411"
                )
            },
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={
                BAR_PLUGIN_ID: bar_lua_settings(
                    edge="bottom",
                    groups=(
                        ("workspaces",),
                        ("clock",),
                        ("cpu",),
                    ),
                )
            },
            bar_check=True,
            bar_edge="bottom",
            bar_groups=("workspaces", "clock", "cpu"),
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-none",
            axes={"note": "no bar plugin installed: no band, no crash, no hit region"},
            store="none",
            plugin_mode="none",
            bar_check=True,
            bar_edge="none",
            bar_absent_probe=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-heavy",
            axes={
                "note": (
                    "right group with pulseaudio + memory exercises the module "
                    "activation budget; may fail closed"
                )
            },
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={
                BAR_PLUGIN_ID: bar_lua_settings(
                    edge="bottom",
                    groups=(
                        ("workspaces",),
                        ("clock",),
                        ("memory", "pulseaudio"),
                    ),
                )
            },
            bar_check=True,
            bar_edge="bottom",
            bar_groups=("workspaces", "clock", "memory,pulseaudio"),
            interactions=False,
        ),
        MatrixConfig(
            cid="bar-restored",
            axes={"note": "top bar and split layout restored across relaunch"},
            store="plugin-bar",
            plugin_id=BAR_PLUGIN_ID,
            plugin_settings={BAR_PLUGIN_ID: bar_lua_settings(edge="top")},
            bar_check=True,
            bar_edge="top",
            session_restore=True,
            interactions=False,
        ),
        # Extension axis: graphics present/absent/removed.
        MatrixConfig(
            cid="graphics-on",
            axes={"note": "published graphics 0.0.24 installed"},
            store="ext-graphics-on",
            plugin_mode="none",
            extensions=("graphics",),
            graphics_expected=True,
            graphics_cycle=False,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="graphics-off",
            axes={"note": "graphics component absent; Core mechanics only"},
            store="ext-graphics-off",
            plugin_mode="none",
            extensions=(),
            graphics_expected=False,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        MatrixConfig(
            cid="graphics-removed",
            axes={"note": "install graphics, remove it, then render"},
            store="ext-graphics-removed",
            plugin_mode="none",
            extensions=(),
            graphics_expected=False,
            graphics_cycle=True,
            bg="fill",
            kitty_image=True,
            interactions=False,
        ),
        # Extension axis: net present/absent via broker fixtures.
        MatrixConfig(
            cid="net-on",
            axes={"note": "published net 0.0.1 + allowed/denied broker fixtures"},
            store="ext-net-on",
            plugin_mode="none",
            extensions=("net",),
            net_expected=True,
            net_checks=True,
            local_plugins=COMPONENT_FIXTURE_NAMES,
            extra_keymaps=(
                ("ctrl+alt+o", "command:acme.acceptance-allowed:go"),
                ("ctrl+alt+k", "command:acme.acceptance-allowed:poll"),
                ("ctrl+alt+n", "command:acme.acceptance-denied:deny"),
            ),
            interactions=False,
        ),
        MatrixConfig(
            cid="net-off",
            axes={"note": "net component absent: typed degradation, no panic"},
            store="ext-net-off",
            plugin_mode="none",
            extensions=(),
            net_expected=False,
            net_checks=True,
            local_plugins=COMPONENT_FIXTURE_NAMES,
            extra_keymaps=(
                ("ctrl+alt+o", "command:acme.acceptance-allowed:go"),
                ("ctrl+alt+k", "command:acme.acceptance-allowed:poll"),
                ("ctrl+alt+n", "command:acme.acceptance-denied:deny"),
            ),
            interactions=False,
        ),
        MatrixConfig(
            cid="baseline-extensions",
            axes={"note": "all plugins with graphics + net installed"},
            store="full-ext",
            plugin_mode="all",
            extensions=("graphics", "net"),
        ),
        # URL-detector visible delta (compiled runtime dependency).
        MatrixConfig(
            cid="url-hover",
            axes={"note": "Ctrl+hover over a printed plaintext URL"},
            store="ext-url",
            plugin_mode="none",
            hover_url=True,
            interactions=False,
        ),
        # Session persistence for the full plugin set.
        MatrixConfig(
            cid="all-on-restored",
            axes={"note": "split layout restored across relaunch"},
            store="full",
            session_restore=True,
        ),
    ]
    configs.extend(expanded_theme_configs(themes))
    # ANSI spot checks: the palette must survive real color consumers under
    # both shipped families (Vim syntax and `ls --color`).
    configs.extend(
        [
            MatrixConfig(
                cid="theme-vim-dark",
                axes={"note": "Vim syntax colors under a dark preset"},
                store="none",
                plugin_mode="none",
                theme_file="tokyo-night",
                theme_category="dark",
                spot_check="vim",
                interactions=False,
            ),
            MatrixConfig(
                cid="theme-vim-light",
                axes={"note": "Vim syntax colors under a light preset"},
                store="none",
                plugin_mode="none",
                theme_file="github-light",
                theme_category="light",
                spot_check="vim",
                interactions=False,
            ),
            MatrixConfig(
                cid="theme-ls-dark",
                axes={"note": "ls --color ANSI slots under a dark preset"},
                store="none",
                plugin_mode="none",
                theme_file="catppuccin-mocha",
                theme_category="dark",
                spot_check="ls",
                interactions=False,
            ),
            MatrixConfig(
                cid="theme-ls-light",
                axes={"note": "ls --color ANSI slots under a light preset"},
                store="none",
                plugin_mode="none",
                theme_file="catppuccin-latte",
                theme_category="light",
                spot_check="ls",
                interactions=False,
            ),
        ]
    )
    # Each official plugin alone (same ids as the full plan).
    for plugin in catalog.plugins:
        configs.append(
            MatrixConfig(
                cid=f"plugin-{sanitize(plugin.slug)}",
                axes={"note": f"only {plugin.id} installed and enabled"},
                store=f"plugin-{sanitize(plugin.slug)}",
                plugin_id=plugin.id,
            )
        )
    return configs


def expanded_probes() -> list[MatrixConfig]:
    """Headless expanded-plan probes."""
    return [
        *PROBE_CONFIGS,
        MatrixConfig(
            cid="probe-statusline",
            axes={
                "note": "statusline surface: retired registry + 0.1 compatibility floor"
            },
            gui=False,
            interactions=False,
            fail_closed="statusline",
        ),
        MatrixConfig(
            cid="probe-extension-inventory",
            axes={"note": "workspace extension roster and published artifacts"},
            gui=False,
            interactions=False,
            fail_closed="extensions",
        ),
        MatrixConfig(
            cid="probe-extension-matrix",
            axes={"note": "present/absent row pair for every official extension"},
            gui=False,
            interactions=False,
            fail_closed="extension-matrix",
        ),
        MatrixConfig(
            cid="probe-palette-invalid",
            axes={"note": "malformed custom palette fails closed"},
            gui=False,
            interactions=False,
            fail_closed="palette",
        ),
    ]


PROBE_CONFIGS = [
    MatrixConfig(
        cid="probe-plugin-inventory",
        axes={"note": "official plugin roster and retired-checkout exclusion"},
        gui=False,
        interactions=False,
        fail_closed="inventory",
    ),
    MatrixConfig(
        cid="bg-cover",
        axes={"note": "requested CSS-style cover value (not shipped)"},
        bg_requested="cover",
        gui=False,
        interactions=False,
        fail_closed="bg-fit",
    ),
    MatrixConfig(
        cid="bg-contain",
        axes={"note": "requested CSS-style contain value (not shipped)"},
        bg_requested="contain",
        gui=False,
        interactions=False,
        fail_closed="bg-fit",
    ),
    MatrixConfig(
        cid="probe-profile-missing",
        axes={"note": "requested-but-missing profile fails closed"},
        gui=False,
        interactions=False,
        fail_closed="profile",
    ),
    MatrixConfig(
        cid="probe-theme-invalid",
        axes={"note": "unknown --theme is a CLI usage error"},
        gui=False,
        interactions=False,
        fail_closed="theme",
    ),
    MatrixConfig(
        cid="probe-profile-env",
        axes={"note": "BITTY_PROFILE selects the profile"},
        gui=False,
        interactions=False,
        profile_env="dev",
    ),
    MatrixConfig(
        cid="probe-profile-precedence",
        axes={"note": "--profile wins over BITTY_PROFILE"},
        gui=False,
        interactions=False,
        profile="dev",
        profile_env="minimal",
    ),
]


# ---------------------------------------------------------------------------
# Workspace catalogs
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Plugin:
    """One official plugin, resolved from the registry and pinned to origin/main."""

    slug: str
    id: str
    repository: str
    manifest_hash: str
    registry_entry: Path
    shared_checkout: Path
    source_dir: Path | None = None
    head: str = ""
    remote_main: str = ""
    clean: bool = False
    commands: tuple[str, ...] = ()
    version: str = ""
    manifest_hash_actual: str = ""
    error: str = ""
    fetched_at: str = ""

    @property
    def source_ok(self) -> bool:
        return (
            self.source_dir is not None
            and not self.error
            and self.clean
            and bool(self.head)
            and self.head == self.remote_main
        )

    @property
    def manifest_hash_ok(self) -> bool:
        expected = self.manifest_hash.removeprefix("sha256:")
        return bool(
            expected
            and self.manifest_hash_actual
            and expected == self.manifest_hash_actual
        )

    @property
    def source_label(self) -> str:
        return f"sources/{self.slug}" if self.source_dir else "(unresolved)"


# Retired checkouts that still exist under bitty-plugins/plugins/ but are not
# official registry entries. The matrix never installs them.
RETIRED_PLUGINS = {
    "statusline": {
        "archived_at": "2026-10-04",
        "reason": (
            "retired checkout: repository archived 2026-10-04 and no "
            "bitty-plugins/registry/official/statusline.toml entry"
        ),
    },
}


@dataclasses.dataclass(frozen=True)
class Extension:
    slug: str
    repo: str
    role: str


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    proxy = env.get("NETWORK_PROXY")
    if proxy:
        env.setdefault("HTTPS_PROXY", proxy)
        env.setdefault("https_proxy", proxy)
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    return env


class PluginCatalog:
    """Official plugin roster and its origin/main-pinned sources.

    The roster is ``bitty-plugins/registry/official/*.toml``; directory
    listings under ``bitty-plugins/plugins/`` are never authoritative.
    """

    def __init__(self, workspace: Path) -> None:
        self.workspace = workspace
        self.registry_repo = workspace / "bitty-plugins"
        self.registry_dir = self.registry_repo / "registry" / "official"
        self.plugins_dir = self.registry_repo / "plugins"
        if not self.registry_dir.is_dir():
            raise SystemExit(
                f"official plugin registry not found at {self.registry_dir}"
            )
        registry_head = run_cmd(
            ["git", "-C", str(self.registry_repo), "rev-parse", "HEAD"], timeout=15.0
        )
        self.registry_revision = (
            registry_head.stdout.strip() if registry_head.returncode == 0 else "unknown"
        )
        self.registry_dirty = bool(
            run_cmd(
                [
                    "git",
                    "-C",
                    str(self.registry_repo),
                    "status",
                    "--porcelain",
                    "--",
                    "registry/official",
                ],
                timeout=15.0,
            ).stdout.strip()
        )
        self.registry_index_dirty = bool(
            run_cmd(
                [
                    "git",
                    "-C",
                    str(self.registry_repo),
                    "status",
                    "--porcelain",
                    "--",
                    "generated/registry.json",
                ],
                timeout=15.0,
            ).stdout.strip()
        )
        entries: list[Plugin] = []
        for entry in sorted(self.registry_dir.glob("*.toml")):
            with entry.open("rb") as fh:
                data = tomllib.load(fh)
            entries.append(
                Plugin(
                    slug=entry.stem,
                    id=data.get("id", ""),
                    repository=data.get("repository", ""),
                    manifest_hash=data.get("manifest_hash", ""),
                    registry_entry=entry,
                    shared_checkout=self.plugins_dir / entry.stem,
                )
            )
        if not entries:
            raise SystemExit(f"no official plugin entries in {self.registry_dir}")
        self.plugins = entries
        known = {p.slug for p in entries}
        self.excluded_checkouts = sorted(
            child.name
            for child in self.plugins_dir.iterdir()
            if child.is_dir()
            and (child / "bitty-plugin.toml").is_file()
            and child.name not in known
        )
        self.retired = {
            slug: RETIRED_PLUGINS[slug]
            for slug in self.excluded_checkouts
            if slug in RETIRED_PLUGINS
        }

    def by_id(self, plugin_id: str) -> Plugin | None:
        return next((p for p in self.plugins if p.id == plugin_id), None)

    def first_command(self, plugin: Plugin) -> str | None:
        return plugin.commands[0] if plugin.commands else None

    def probe_command(self, plugin: Plugin) -> str | None:
        override = PROBE_COMMAND_OVERRIDES.get(plugin.id)
        if override and override in plugin.commands:
            return override
        return self.first_command(plugin)

    def resolve_sources(
        self, sources_root: Path, log_dir: Path, *, reuse: bool = False
    ) -> None:
        """Clone every official repository at origin/main and pin the SHA.

        The shared workspace checkout is never used as install input, so an
        abandoned branch cannot leak into the matrix. ``reuse`` skips the
        clone when the destination already exists (smoke iteration only; the
        accepted campaign must clone fresh).
        """
        sources_root.mkdir(parents=True, exist_ok=True)
        env = git_env()
        for plugin in self.plugins:
            dest = sources_root / plugin.slug
            if dest.exists() and not reuse:
                shutil.rmtree(dest)
            if dest.exists() and reuse:
                clone = subprocess.CompletedProcess(
                    ["git", "clone", "(reused)", str(dest)], 0, "", ""
                )
            else:
                clone = run_cmd(
                    [
                        "git",
                        "clone",
                        "--depth=1",
                        "--single-branch",
                        "--branch",
                        "main",
                        plugin.repository,
                        str(dest),
                    ],
                    env=env,
                    timeout=180.0,
                )
            write_text(
                log_dir / f"source-{plugin.slug}.txt",
                f"$ git clone --depth=1 --single-branch --branch main "
                f"{plugin.repository} sources/{plugin.slug}\n"
                f"reuse={str(reuse).lower()}\n"
                f"exit={clone.returncode}\n--- stdout ---\n{clone.stdout}\n"
                f"--- stderr ---\n{clone.stderr}\n",
            )
            plugin.fetched_at = now()
            if clone.returncode != 0:
                plugin.error = f"git clone failed: {tail(clone.stderr, 2)}"
                continue
            plugin.source_dir = dest
            head = run_cmd(["git", "-C", str(dest), "rev-parse", "HEAD"], env=env)
            plugin.head = head.stdout.strip()
            if reuse:
                # Explicit smoke-only mode: the reused checkout is the pin.
                plugin.remote_main = plugin.head
            else:
                remote = run_cmd(
                    ["git", "ls-remote", plugin.repository, "refs/heads/main"],
                    env=env,
                    timeout=60.0,
                )
                plugin.remote_main = (
                    remote.stdout.split()[0] if remote.stdout.split() else ""
                )
            status = run_cmd(["git", "-C", str(dest), "status", "--porcelain"], env=env)
            plugin.clean = not bool(status.stdout.strip())
            manifest = dest / "bitty-plugin.toml"
            if not manifest.is_file():
                plugin.error = "origin/main has no bitty-plugin.toml"
                continue
            plugin.manifest_hash_actual = sha256_file(manifest)
            with manifest.open("rb") as fh:
                data = tomllib.load(fh)
            plugin.version = data.get("plugin", {}).get("version", "")
            plugin.commands = tuple(data.get("lazy", {}).get("commands", []))
            if plugin.head != plugin.remote_main:
                plugin.error = (
                    f"resolved HEAD {plugin.head or '?'} != origin/main "
                    f"{plugin.remote_main or '?'} at fetch time"
                )
            elif not plugin.clean:
                plugin.error = "origin/main checkout is not clean"
            elif not plugin.id:
                plugin.error = "manifest has no plugin id"
            elif data.get("plugin", {}).get("id") != plugin.id:
                plugin.error = (
                    f"manifest id {data.get('plugin', {}).get('id')!r} != "
                    f"registry id {plugin.id!r}"
                )


def load_extensions(workspace: Path) -> list[Extension]:
    map_path = workspace / "workspace.toml"
    with map_path.open("rb") as fh:
        data = tomllib.load(fh)
    out = []
    for repo, meta in data.get("repo", {}).items():
        if meta.get("kind") != "extension":
            continue
        out.append(
            Extension(
                slug=repo.removeprefix("bitty-"),
                repo=repo,
                role=meta.get("role", ""),
            )
        )
    return sorted(out, key=lambda e: e.slug)


# ---------------------------------------------------------------------------
# Store / XDG management
# ---------------------------------------------------------------------------


class Store:
    """One isolated XDG root plus a plugin store for a plugin set."""

    def __init__(self, root: Path, key: str) -> None:
        self.key = key
        self.root = root / "stores" / sanitize(key)
        self.config = self.root / "config"
        self.data = self.root / "data"
        self.cache = self.root / "cache"
        self.state = self.root / "state"
        self.run = self.root / "run"
        self.home = self.root / "home"
        for path in (self.config, self.data, self.cache, self.state, self.home):
            path.mkdir(parents=True, exist_ok=True)
        self.run.mkdir(parents=True, exist_ok=True)
        self.run.chmod(0o700)
        self.bitty_config = self.config / "bitty"
        self.profiles = self.bitty_config / "profiles"
        self.profiles.mkdir(parents=True, exist_ok=True)
        self.session_file = self.state / "bitty" / "sessions" / "session"

    def env(self, instance: str) -> dict[str, str]:
        env = os.environ.copy()
        socket = self.run / f"{sanitize(instance)}.sock"
        if len(str(socket)) > AF_UNIX_PATH_BUDGET:
            raise SystemExit(
                f"AF_UNIX socket path exceeds {AF_UNIX_PATH_BUDGET} bytes "
                f"({len(str(socket))}): {socket}; use a shorter --xdg-root"
            )
        env.update(
            {
                "XDG_CONFIG_HOME": str(self.config),
                "XDG_DATA_HOME": str(self.data),
                "XDG_CACHE_HOME": str(self.cache),
                "XDG_STATE_HOME": str(self.state),
                "HOME": str(self.home),
                "HISTFILE": str(self.home / ".bash_history"),
                "BITTY_SOCKET": str(socket),
                "BITTY_INSTANCE_ID": instance,
            }
        )
        # A leftover probe variable must never leak into unrelated runs.
        env.pop("BITTY_PROFILE", None)
        env.pop("BITTY_CONFIG", None)
        env.pop("BITTY_CTL_ELEVATE", None)
        return env


# ---------------------------------------------------------------------------
# Main harness
# ---------------------------------------------------------------------------


class Harness:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.workspace = (
            args.workspace.resolve() if args.workspace else find_workspace(SCRIPT_DIR)
        )
        self.binary = args.binary.resolve()
        self.out = args.out.resolve()
        self.shots = self.out / "shots"
        self.logs = self.out / "logs"
        self.assets = self.out / "assets"
        self.xdg_root = args.xdg_root.resolve()
        # Ownership guard: only the CTX-2413 scratch root may ever be cleared
        # or written. The owner's config directories never contain this token.
        if XDG_ROOT_TOKEN not in str(self.xdg_root):
            raise SystemExit(
                f"refusing XDG root outside the {XDG_ROOT_TOKEN} scratch area: "
                f"{self.xdg_root}"
            )
        if len(str(self.xdg_root)) > AF_UNIX_PATH_BUDGET:
            raise SystemExit(
                f"XDG root exceeds the AF_UNIX budget "
                f"({len(str(self.xdg_root))} > {AF_UNIX_PATH_BUDGET} bytes): "
                f"{self.xdg_root}"
            )
        self.catalog = PluginCatalog(self.workspace)
        self.extensions = load_extensions(self.workspace)
        self.wallpapers = self.select_wallpapers()
        self.rows: list[dict] = []
        self.config_results: dict[str, dict] = {}
        self.pids: list[dict] = []
        self.desktop_before: dict = {}
        self.desktop_after: dict = {}
        self.errors: list[str] = []
        self.started_at = now()
        self.stores: dict[str, Store] = {}
        self.selected_wallpaper: Path | None = None
        self.crop_png: Path | None = None
        self.send_image_script: Path | None = None
        self.theme_backgrounds: dict[str, str] = {}
        self.theme_catalog: list[dict] = []
        self.binary_sha: str | None = None
        self.install_cache: dict[str, list[dict]] = {}
        self.local_install_cache: dict[str, list[dict]] = {}
        self.extension_install_cache: dict[str, list[dict]] = {}
        self.procs: dict[int, subprocess.Popen] = {}
        self.log_files: dict[int, object] = {}
        self.component_port: int | None = None
        self.component_server: subprocess.Popen | None = None
        self.component_server_log: Path | None = None
        self.component_fixtures: Path | None = None
        self.network_installs = 0
        self.fixture_assets: dict[str, Path] = {}
        self.bar_anchors = (
            {"state": "merged", "source": "--bar-anchors merged", "checked_at": now()}
            if args.bar_anchors == "merged"
            else {"state": "open", "source": "--bar-anchors open", "checked_at": now()}
            if args.bar_anchors == "open"
            else probe_bar_anchors()
        )
        self.available_profiles = [
            "(none)",
            "dev",
            "minimal",
            "plugins",
            "settings",
            "bar-off",
        ]

    # -- setup -------------------------------------------------------------

    def ensure_layout(self) -> None:
        for path in (self.out, self.shots, self.logs, self.assets, self.xdg_root):
            path.mkdir(parents=True, exist_ok=True)

    def select_wallpapers(self) -> list[Path]:
        directory = self.args.wallpapers.expanduser()
        if not directory.is_dir():
            raise SystemExit(f"wallpaper directory not found: {directory}")
        files = sorted(
            p
            for p in directory.iterdir()
            if p.suffix.lower() in {".jpg", ".jpeg", ".png", ".webp"}
        )
        if not files:
            raise SystemExit(f"no wallpapers in {directory}")
        return files

    def store_for(self, cfg: MatrixConfig) -> Store:
        key = cfg.store
        if key not in self.stores:
            self.stores[key] = Store(self.xdg_root, key)
        return self.stores[key]

    def plugin_set(self, cfg: MatrixConfig) -> list[Plugin]:
        if cfg.plugin_mode == "none" or cfg.store == "none":
            return []
        if cfg.plugin_mode == "all" or cfg.store == "full":
            return list(self.catalog.plugins)
        if cfg.plugin_id:
            plugin = self.catalog.by_id(cfg.plugin_id)
            return [plugin] if plugin else []
        return []

    def prepare(self) -> None:
        self.ensure_layout()
        self.write_profiles()
        self.pick_wallpaper()
        self.prepare_kitty_assets()
        self.catalog.resolve_sources(
            self.xdg_root / "sources", self.logs, reuse=self.args.reuse_sources
        )
        self.load_theme_backgrounds()
        self.prepare_component_fixtures()
        self.write_provenance()

    def write_profiles(self) -> None:
        # Profiles live in every store so any config can select any profile.
        plugins_body = "\n".join(
            f'        {{ id = "{p.id}" }},' for p in self.catalog.plugins
        )
        for store in self.all_stores():
            write_text(store.profiles / "dev.lua", PROFILE_DEV)
            write_text(store.profiles / "minimal.lua", PROFILE_MINIMAL)
            write_text(store.profiles / "settings.lua", PROFILE_SETTINGS)
            write_text(store.profiles / "bar-off.lua", PROFILE_BAR_OFF)
            write_text(
                store.profiles / "plugins.lua",
                PROFILE_PLUGINS_TEMPLATE % {"entries": plugins_body},
            )

    def all_stores(self) -> list[Store]:
        keys = {
            "full",
            "none",
            "plugin-bar",
            "full-ext",
            "ext-graphics-on",
            "ext-graphics-off",
            "ext-graphics-removed",
            "ext-net-on",
            "ext-net-off",
            "ext-url",
            *self.stores,
        }
        keys.update(f"plugin-{sanitize(p.slug)}" for p in self.catalog.plugins)
        out = []
        for key in sorted(keys):
            if key not in self.stores:
                self.stores[key] = Store(self.xdg_root, key)
            out.append(self.stores[key])
        return out

    def pick_wallpaper(self) -> None:
        chosen = self.args.wallpaper
        if chosen:
            path = chosen.expanduser()
            if not path.is_file():
                raise SystemExit(f"wallpaper not found: {path}")
            self.selected_wallpaper = path
        else:
            self.selected_wallpaper = self.wallpapers[0]
        if self.args.wallpaper_index is not None:
            idx = self.args.wallpaper_index
            if not 0 <= idx < len(self.wallpapers):
                raise SystemExit(f"wallpaper index {idx} out of range")
            self.selected_wallpaper = self.wallpapers[idx]

    def prepare_kitty_assets(self) -> None:
        assert self.selected_wallpaper is not None
        from PIL import Image

        crop_w, crop_h = 320, 160
        image = Image.open(self.selected_wallpaper)
        width, height = image.size
        box = (
            max(0, (width - crop_w) // 2),
            max(0, (height - crop_h) // 2),
            min(width, (width + crop_w) // 2),
            min(height, (height + crop_h) // 2),
        )
        self.crop_png = self.assets / "kitty-crop.png"
        image.crop(box).save(self.crop_png)
        self.crop_box = box
        self.send_image_script = self.assets / "send-image.py"
        write_text(self.send_image_script, SEND_IMAGE_SCRIPT)

    def load_theme_backgrounds(self) -> None:
        env = self.all_stores()[0].env(f"{INSTANCE_PREFIX}-theme-probe")
        proc = run_cmd(
            [str(self.binary), "list", "themes", "--format", "json"],
            env=env,
            timeout=20.0,
        )
        if proc.returncode != 0:
            raise SystemExit(f"list themes failed: {proc.stderr.strip()}")
        payload = json.loads(proc.stdout)
        self.theme_catalog = payload["result"]["themes"]
        for theme in self.theme_catalog:
            self.theme_backgrounds[theme["name"]] = theme["background"]
            for alias in theme.get("aliases", []):
                self.theme_backgrounds[alias] = theme["background"]
        self.theme_backgrounds["(default)"] = self.theme_backgrounds["bitty-dark"]

    def prepare_component_fixtures(self) -> None:
        """Stage the two bounded broker fixture plugins with one local port."""
        self.component_port = self.component_port or free_tcp_port()
        source = FIXTURE_DIR / "component"
        if not source.is_dir():
            return
        target = self.assets / "component-fixtures"
        if target.exists():
            shutil.rmtree(target)
        target.mkdir(parents=True, exist_ok=True)
        for name in COMPONENT_FIXTURE_NAMES:
            src = source / name
            if not src.is_dir():
                continue
            dst = target / name
            shutil.copytree(src, dst)
            for path in dst.rglob("*"):
                if path.is_file() and path.suffix in {".lua", ".toml"}:
                    text = path.read_text(encoding="utf-8")
                    path.write_text(
                        text.replace("__PORT__", str(self.component_port)),
                        encoding="utf-8",
                    )
            self.fixture_assets[name] = dst
        server = source / "server.py"
        if server.is_file():
            shutil.copy2(server, target / "server.py")
        self.component_fixtures = target

    def ensure_component_server(self) -> None:
        if self.component_server is not None or self.component_fixtures is None:
            return
        if self.component_port is None:
            return
        script = self.component_fixtures / "server.py"
        if not script.is_file():
            return
        self.component_server_log = self.logs / "component-http.log"
        log_file = self.component_server_log.open("w", encoding="utf-8")
        self.component_server = subprocess.Popen(
            [sys.executable, str(script), str(self.component_port)],
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        self.pids.append(
            {
                "pid": self.component_server.pid,
                "config": "(component fixture)",
                "role": "local HTTP fixture for net component request",
                "started_at": now(),
                "argv": [sys.executable, str(script), str(self.component_port)],
                "socket": None,
                "exit": None,
            }
        )
        # Bounded readiness wait on the loopback socket, never a sleep loop.
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if probe_tcp_port(self.component_port):
                return
            time.sleep(0.1)

    def stop_component_server(self) -> None:
        server = self.component_server
        if server is None:
            return
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5.0)
        for entry in self.pids:
            if entry["pid"] == server.pid and entry["exit"] is None:
                entry["exit"] = f"exited at {now()}"
        self.component_server = None

    def binary_sha256(self) -> str:
        if self.binary_sha is None:
            self.binary_sha = sha256_file(self.binary)
        return self.binary_sha

    def write_provenance(self) -> None:
        assert self.selected_wallpaper is not None
        version = run_cmd(
            [str(self.binary), "version", "--format", "json"], timeout=20.0
        )
        version_payload = json.loads(version.stdout) if version.returncode == 0 else {}
        rustc = run_cmd(["rustc", "-V"], timeout=10.0)
        cargo = run_cmd(["cargo", "-V"], timeout=10.0)
        mon = hypr_json("monitors")[0]
        source = self.args.source.resolve() if self.args.source else None
        if source is None:
            # The harness lives inside the candidate checkout; record its head
            # so provenance names the source revision without an explicit flag.
            toplevel = run_cmd(
                ["git", "-C", str(SCRIPT_DIR), "rev-parse", "--show-toplevel"],
                timeout=10.0,
            )
            if toplevel.returncode == 0 and toplevel.stdout.strip():
                source = Path(toplevel.stdout.strip())
        source_head = ""
        if source:
            head = run_cmd(["git", "-C", str(source), "rev-parse", "HEAD"])
            source_head = head.stdout.strip()
        payload = {
            "harness": HARNESS_VERSION,
            "schema_version": SCHEMA_VERSION,
            "generated_at": self.started_at,
            "candidate": {
                "binary": str(self.binary),
                "binary_sha256": self.binary_sha256(),
                "version": version_payload.get("result", {}).get("version"),
                "channel": version_payload.get("result", {}).get("channel"),
                "commit_short": version_payload.get("result", {}).get("commit"),
                "source_checkout": str(source) if source else None,
                "source_head": source_head,
                "build_profile": self.args.build_profile,
                "target": self.args.target,
            },
            "toolchain": {
                "rustc": rustc.stdout.strip() or None,
                "cargo": cargo.stdout.strip() or None,
                "python": sys.version.split()[0],
            },
            "host": {
                "os": run_cmd(["uname", "-srm"]).stdout.strip(),
                "compositor": run_cmd(["hyprctl", "version"]).stdout.splitlines()[0],
                "monitor": f"{mon.get('name')} {mon.get('width')}x{mon.get('height')}"
                f"@{mon.get('refreshRate')}",
                "workspace": CAMPAIGN_WORKSPACE,
            },
            "wallpaper": {
                "path": str(self.selected_wallpaper),
                "filename": self.selected_wallpaper.name,
                "sha256": sha256_file(self.selected_wallpaper),
                "crop": {"box": list(self.crop_box), "png": str(self.crop_png)},
            },
            "profiles": self.available_profiles,
            "xdg_roots": str(self.xdg_root),
            "clipboard_used": False,
            "network_installs": self.network_installs,
            "source_edits": 0,
            "registry": {
                "revision": self.catalog.registry_revision,
                "official_dirty": self.catalog.registry_dirty,
                "generated_index_dirty": self.catalog.registry_index_dirty,
                "official_entries": len(self.catalog.plugins),
                "excluded_checkouts": self.catalog.excluded_checkouts,
                "retired": self.catalog.retired,
            },
            "plugins": [
                {
                    "slug": p.slug,
                    "id": p.id,
                    "repository": p.repository,
                    "registry_entry": f"registry/official/{p.slug}.toml",
                    "resolved_sha": p.head,
                    "origin_main_sha": p.remote_main,
                    "source_clean": p.clean,
                    "source_ok": p.source_ok,
                    "source_clone": p.source_label,
                    "fetched_at": p.fetched_at,
                    "version": p.version,
                    "manifest_hash": p.manifest_hash,
                    "manifest_hash_actual": p.manifest_hash_actual,
                    "manifest_hash_ok": p.manifest_hash_ok,
                    "error": p.error,
                    "commands": list(p.commands),
                }
                for p in self.catalog.plugins
            ],
            "extensions": [
                {"slug": e.slug, "repo": e.repo, "role": e.role}
                for e in self.extensions
            ],
            "citations": DOC_CITATIONS,
        }
        write_json(self.out / "provenance.json", payload)
        lines = [
            f"harness: {HARNESS_VERSION}",
            f"generated_at: {payload['generated_at']}",
            f"candidate_binary: {payload['candidate']['binary']}",
            f"candidate_sha256: {payload['candidate']['binary_sha256']}",
            (
                f"candidate_version: {payload['candidate']['version']} "
                f"({payload['candidate']['channel']} {payload['candidate']['commit_short']})"
            ),
            f"source_head: {source_head}",
            f"build_profile: {payload['candidate']['build_profile']}",
            f"target: {payload['candidate']['target']}",
            f"rustc: {payload['toolchain']['rustc']}",
            f"cargo: {payload['toolchain']['cargo']}",
            f"host: {payload['host']['os']}",
            f"compositor: {payload['host']['compositor']}",
            f"monitor: {payload['host']['monitor']}",
            (
                f"wallpaper: {payload['wallpaper']['filename']} "
                f"({payload['wallpaper']['sha256']})"
            ),
            f"wallpaper_path: {payload['wallpaper']['path']}",
            f"kitty_crop: {self.crop_png} box={list(self.crop_box)}",
            f"profiles: {', '.join(payload['profiles'])}",
            f"registry_revision: {self.catalog.registry_revision}",
            f"official_plugin_entries: {len(self.catalog.plugins)}",
            f"excluded_checkouts: {', '.join(self.catalog.excluded_checkouts) or '-'}",
            f"xdg_roots: {payload['xdg_roots']}",
            "clipboard_used: false",
            f"network_installs: {self.network_installs}",
            "source_edits: 0",
        ]
        write_text(self.out / "provenance.txt", "\n".join(lines) + "\n")
        source_lines = [
            f"registry_repo: {self.catalog.registry_repo}",
            f"registry_revision: {self.catalog.registry_revision}",
            f"registry_official_dirty: {str(self.catalog.registry_dirty).lower()}",
            f"registry_index_dirty: {str(self.catalog.registry_index_dirty).lower()}",
            f"official_entries: {len(self.catalog.plugins)}",
            f"excluded_checkouts: {', '.join(self.catalog.excluded_checkouts) or '-'}",
            "retired: "
            + (
                ", ".join(
                    f"{slug} (archived {meta['archived_at']})"
                    for slug, meta in self.catalog.retired.items()
                )
                or "-"
            ),
            (
                "source_policy: git clone --depth=1 --branch main "
                "<registry repository>; assert HEAD == origin/main at fetch "
                "time; never the shared checkout"
            ),
            (
                "slug\tid\tresolved_sha\torigin_main_sha\tmatch\tclean"
                "\tmanifest_hash_match\tregistry_entry\terror"
            ),
        ]
        for p in self.catalog.plugins:
            source_lines.append(
                f"{p.slug}\t{p.id}\t{p.head or '-'}\t{p.remote_main or '-'}"
                f"\t{str(bool(p.head and p.head == p.remote_main)).lower()}"
                f"\t{str(p.clean).lower()}"
                f"\t{str(p.manifest_hash_ok).lower()}"
                f"\tregistry/official/{p.slug}.toml"
                f"\t{p.error or '-'}"
            )
        write_text(self.out / "plugin-sources.txt", "\n".join(source_lines) + "\n")

    # -- plugin stores -----------------------------------------------------

    def install_plugins(self, cfg: MatrixConfig) -> list[dict]:
        store = self.store_for(cfg)
        if cfg.store in self.install_cache:
            return self.install_cache[cfg.store]
        env = store.env(f"{INSTANCE_PREFIX}-install")
        wanted = self.plugin_set(cfg)
        results = []
        for plugin in wanted:
            log = self.logs / f"install-{plugin.slug}.txt"
            if not plugin.source_ok or plugin.source_dir is None:
                reason = plugin.error or "origin/main source not resolved"
                write_text(
                    log,
                    f"$ bitty plugin install (skipped: {plugin.id})\nreason={reason}\n",
                )
                results.append(
                    {
                        "plugin": plugin.id,
                        "install_exit": None,
                        "enable_exit": None,
                        "error": reason,
                    }
                )
                continue
            proc = run_cmd(
                [
                    str(self.binary),
                    "plugin",
                    "install",
                    str(plugin.source_dir),
                    "--yes",
                ],
                env=env,
                timeout=120.0,
            )
            enable = run_cmd(
                [str(self.binary), "plugin", "enable", plugin.id],
                env=env,
                timeout=60.0,
            )
            write_text(
                log,
                f"$ bitty plugin install sources/{plugin.slug} --yes\n"
                f"source_sha={plugin.head} origin_main={plugin.remote_main}\n"
                f"exit={proc.returncode}\n"
                f"--- stdout ---\n{proc.stdout}\n"
                f"--- stderr ---\n{proc.stderr}\n"
                f"\n$ bitty plugin enable {plugin.id}\n"
                f"exit={enable.returncode}\n"
                f"--- stdout ---\n{enable.stdout}\n"
                f"--- stderr ---\n{enable.stderr}\n",
            )
            results.append(
                {
                    "plugin": plugin.id,
                    "source_sha": plugin.head,
                    "origin_main": plugin.remote_main,
                    "install_exit": proc.returncode,
                    "enable_exit": enable.returncode,
                    "stdout_tail": tail(proc.stdout, 8),
                    "stderr_tail": tail(proc.stderr, 8),
                }
            )
        listing = run_cmd(
            [str(self.binary), "plugin", "list", "--format", "json"],
            env=env,
            timeout=30.0,
        )
        if listing.returncode == 0:
            write_text(
                self.logs / f"plugin-list-{sanitize(cfg.store)}.json", listing.stdout
            )
        self.install_cache[cfg.store] = results
        return results

    def install_local_plugins(self, cfg: MatrixConfig) -> list[dict]:
        """Install the harness broker fixtures into the isolated store."""
        if not cfg.local_plugins:
            return []
        store = self.store_for(cfg)
        cache_key = f"{cfg.store}:{'/'.join(cfg.local_plugins)}"
        if cache_key in self.local_install_cache:
            return self.local_install_cache[cache_key]
        env = store.env(f"{INSTANCE_PREFIX}-fixture-install")
        results: list[dict] = []
        for name in cfg.local_plugins:
            source = self.fixture_assets.get(name)
            if source is None:
                results.append(
                    {
                        "fixture": name,
                        "install_exit": None,
                        "error": "fixture not staged (missing recording/fixtures/component)",
                    }
                )
                continue
            manifest = source / "bitty-plugin.toml"
            plugin_id = ""
            if manifest.is_file():
                with manifest.open("rb") as handle:
                    plugin_id = tomllib.load(handle).get("plugin", {}).get("id", "")
            # The fixture embeds a per-run loopback port, so a stale package
            # with the same id/version must be dropped before reinstall.
            if plugin_id:
                run_cmd(
                    [str(self.binary), "plugin", "remove", plugin_id, "--force"],
                    env=env,
                    timeout=60.0,
                )
            install = run_cmd(
                [str(self.binary), "plugin", "install", str(source), "--yes"],
                env=env,
                timeout=120.0,
            )
            enable = run_cmd(
                [str(self.binary), "plugin", "enable", plugin_id],
                env=env,
                timeout=60.0,
            )
            write_text(
                self.logs / f"fixture-install-{name}.txt",
                f"$ bitty plugin install {source} --yes\n"
                f"exit={install.returncode}\n--- stdout ---\n{install.stdout}\n"
                f"--- stderr ---\n{install.stderr}\n"
                f"\n$ bitty plugin enable {plugin_id}\nexit={enable.returncode}\n"
                f"--- stdout ---\n{enable.stdout}\n"
                f"--- stderr ---\n{enable.stderr}\n",
            )
            results.append(
                {
                    "fixture": name,
                    "plugin_id": plugin_id,
                    "source": str(source),
                    "install_exit": install.returncode,
                    "enable_exit": enable.returncode,
                    "stderr_tail": tail(install.stderr, 6),
                }
            )
        self.local_install_cache[cache_key] = results
        return results

    def extension_list(self, cfg: MatrixConfig) -> tuple[int, list[dict], str]:
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        proc = run_cmd(
            [str(self.binary), "extension", "list", "--format", "json"],
            env=env,
            timeout=20.0,
        )
        components: list[dict] = []
        if proc.returncode == 0:
            try:
                components = json.loads(proc.stdout)["result"].get("components", [])
            except (json.JSONDecodeError, KeyError):
                components = []
        write_text(
            self.logs / f"{sanitize(cfg.cid)}.extension-list.json",
            proc.stdout or proc.stderr,
        )
        return proc.returncode, components, (proc.stderr or "").strip()

    def install_extensions(self, cfg: MatrixConfig, *, log_prefix: str = "") -> dict:
        """Install the config's published components, pinned and isolated."""
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}-ext")
        results: dict[str, dict] = {}
        for name in cfg.extensions:
            version = PUBLISHED_EXTENSIONS.get(name)
            if version is None:
                results[name] = {
                    "status": "NOT_APPLICABLE",
                    "reason": f"no published component artifact for {name}",
                }
                continue
            if not self.args.network_extensions:
                results[name] = {
                    "status": "NOT_EXERCISED",
                    "name": name,
                    "version": version,
                    "reason": "pass --network-extensions to install the pinned artifact",
                }
                continue
            install = self.headless(
                [
                    "extension",
                    "install",
                    name,
                    "--version",
                    version,
                    "--yes",
                ],
                env,
                f"{log_prefix}{sanitize(cfg.cid)}.extension-install-{name}.txt",
            )
            self.network_installs += 1
            results[name] = {
                "name": name,
                "version": version,
                "exit": install.returncode,
                "stdout_tail": tail(install.stdout, 8),
                "stderr_tail": tail(install.stderr, 8),
            }
        return results

    def remove_extension(self, cfg: MatrixConfig, name: str) -> dict:
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}-ext")
        proc = self.headless(
            ["extension", "remove", name],
            env,
            f"{sanitize(cfg.cid)}.extension-remove-{name}.txt",
        )
        return {
            "name": name,
            "exit": proc.returncode,
            "stdout_tail": tail(proc.stdout, 8),
            "stderr_tail": tail(proc.stderr, 8),
        }

    # -- config file -------------------------------------------------------

    def write_config(self, cfg: MatrixConfig) -> Path:
        store = self.store_for(cfg)
        appearance: list[str] = []
        if cfg.theme_file:
            appearance.append(f'theme = "{cfg.theme_file}"')
        if cfg.custom_palette:
            appearance.append(f"colors = {lua_table(cfg.custom_palette, 8)}")
        if cfg.animations is False:
            appearance.append("animations = { enabled = false }")
        lines = ["return {"]
        if appearance:
            lines.append("    appearance = { " + ", ".join(appearance) + " },")
        if cfg.bg:
            assert self.selected_wallpaper is not None
            lines.append(
                "    window = { opacity = 1.0, padding = 8, "
                f'background_image = "{self.selected_wallpaper}", '
                f'background_fit = "{cfg.bg}" }},'
            )
            lines.append(
                "    decoration = { background_image_roots = "
                f'{{ "{self.selected_wallpaper.parent}" }} }},'
            )
        if cfg.bg_requested:
            assert self.selected_wallpaper is not None
            lines.append(
                "    window = { opacity = 1.0, padding = 8, "
                f'background_image = "{self.selected_wallpaper}", '
                f'background_fit = "{cfg.bg_requested}" }},'
            )
            lines.append(
                "    decoration = { background_image_roots = "
                f'{{ "{self.selected_wallpaper.parent}" }} }},'
            )
        if cfg.session_restore:
            lines.append("    session = { restore_on_startup = true },")
        keymaps = self.probe_keymaps(cfg)
        if keymaps:
            lines.append("    keymaps = {")
            for chord, action in keymaps:
                lines.append(
                    f'        {{ chord = "{chord}", action = "{action}", '
                    'context = "global" },'
                )
            lines.append("    },")
        lines.append("}")
        path = store.bitty_config / "init.lua"
        write_text(path, "\n".join(lines) + "\n")
        self.write_plugin_settings(cfg)
        return path

    def write_plugin_settings(self, cfg: MatrixConfig) -> None:
        """Write canonical per-plugin Lua files and clear stale ones."""
        store = self.store_for(cfg)
        directory = store.bitty_config / "plugins"
        directory.mkdir(parents=True, exist_ok=True)
        for old in directory.glob("*.lua"):
            old.unlink()
        for plugin_id, body in cfg.plugin_settings.items():
            if not is_plugin_id(plugin_id):
                self.errors.append(f"invalid plugin settings id {plugin_id!r}")
                continue
            write_text(
                directory / f"{plugin_id}.lua",
                "return {\n" + indent(body, 4) + "\n}\n",
            )

    def probe_keymaps(self, cfg: MatrixConfig) -> list[tuple[str, str]]:
        out: list[tuple[str, str]] = list(cfg.extra_keymaps)
        if not cfg.interactions:
            return out
        used = {chord for chord, _ in out}
        pool = [chord for chord in CHORD_POOL if chord not in used]
        for plugin in self.plugin_set(cfg):
            command = self.catalog.probe_command(plugin)
            if not command or not pool:
                continue
            chord = pool.pop(0)
            out.append((chord, f"command:{command}"))
        if pool:
            out.append((pool.pop(0), "command:ctx1344.absent:probe"))
        return out

    def chord_for(self, cfg: MatrixConfig, command: str) -> str | None:
        for chord, action in self.probe_keymaps(cfg):
            if action == f"command:{command}":
                return chord
        return None

    # -- bitty invocations -------------------------------------------------

    def config_check(self, cfg: MatrixConfig, env: dict[str, str] | None = None):
        store = self.store_for(cfg)
        use_env = env or store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}-check")
        argv = [str(self.binary), "config", "check"]
        if cfg.profile:
            argv[1:1] = ["--profile", cfg.profile]
        if cfg.theme_cli:
            argv[1:1] = ["--theme", cfg.theme_cli]
        if cfg.profile_env and not env:
            use_env["BITTY_PROFILE"] = cfg.profile_env
        proc = run_cmd(argv, env=use_env, timeout=30.0)
        write_text(
            self.logs / f"{sanitize(cfg.cid)}.config-check.txt",
            f"$ {' '.join(argv)}\nexit={proc.returncode}\n"
            f"--- stdout ---\n{proc.stdout}\n--- stderr ---\n{proc.stderr}\n",
        )
        return proc

    def headless(self, argv: list[str], env: dict[str, str], log_name: str):
        proc = run_cmd([str(self.binary), *argv], env=env, timeout=60.0)
        write_text(
            self.logs / log_name,
            f"$ {' '.join(argv)}\nexit={proc.returncode}\n"
            f"--- stdout ---\n{proc.stdout}\n--- stderr ---\n{proc.stderr}\n",
        )
        return proc

    def ctl(
        self,
        cfg: MatrixConfig,
        args: list[str],
        *,
        log: str | None = None,
        timeout: float = 15.0,
    ) -> tuple[int, dict | None, str]:
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        argv = [str(self.binary), "ctl", *args]
        proc = run_cmd(argv, env=env, timeout=timeout)
        payload = None
        if "--format" in args and "json" in args:
            try:
                payload = json.loads(proc.stdout)
            except json.JSONDecodeError:
                payload = None
        if log:
            write_text(
                self.logs / log,
                f"$ {' '.join(argv)}\nexit={proc.returncode}\n"
                f"--- stdout ---\n{proc.stdout}\n--- stderr ---\n{proc.stderr}\n",
            )
        return proc.returncode, payload, (proc.stderr or "").strip()

    # -- row helpers -------------------------------------------------------

    def add_row(
        self,
        cfg: MatrixConfig,
        *,
        kind: str,
        row_id: str,
        check: str,
        status: str,
        reason: str = "",
        screenshots: list[str] | None = None,
        evidence: dict | None = None,
    ) -> dict:
        row = {
            "config": cfg.cid,
            "config_axes": cfg.axes_dict(),
            "kind": kind,
            "id": row_id,
            "check": check,
            "status": status,
            "reason": reason,
            "screenshots": screenshots or [],
            "evidence": evidence or {},
            "recorded_at": now(),
        }
        self.rows.append(row)
        return row

    def screenshot(self, cfg: MatrixConfig, row_id: str, *, window: bool) -> str | None:
        assert self.current_client is not None
        name = f"{sanitize(cfg.cid)}-{sanitize(row_id)}.png"
        path = self.shots / name
        proc = None
        for attempt in range(2):
            try:
                if window:
                    client = self.current_client
                    at = client.get("at") or [0, 0]
                    size = client.get("size") or [0, 0]
                    geometry = f"{at[0]},{at[1]} {size[0]}x{size[1]}"
                    proc = run_cmd(["grim", "-g", geometry, str(path)], timeout=20.0)
                else:
                    monitor = hypr_json("monitors")[
                        self.current_client.get("monitor", 0)
                    ]
                    proc = run_cmd(
                        ["grim", "-o", monitor["name"], str(path)], timeout=20.0
                    )
            except subprocess.TimeoutExpired:
                proc = None
            if proc is not None and proc.returncode == 0:
                return str(path)
            if attempt == 0:
                time.sleep(0.5)
        detail = proc.stderr.strip() if proc is not None else "timeout"
        self.errors.append(f"grim failed for {name}: {detail}")
        return None

    # -- launched instance -------------------------------------------------

    current_client: dict | None = None

    def wait_for_window(self, pid: int) -> dict | None:
        deadline = time.monotonic() + WINDOW_TIMEOUT_S
        while time.monotonic() < deadline:
            try:
                clients = hypr_json("clients")
            except RuntimeError:
                clients = []
            for client in clients:
                if client.get("pid") == pid:
                    return client
            if not running(pid, self.procs):
                return None
            time.sleep(WINDOW_POLL_S)
        return None

    def wait_process_exit(
        self, pid: int, timeout: float = PROCESS_EXIT_TIMEOUT_S
    ) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not running(pid, self.procs):
                return True
            time.sleep(PROCESS_POLL_S)
        return not running(pid, self.procs)

    def launch(
        self, cfg: MatrixConfig
    ) -> tuple[subprocess.Popen | None, dict | None, dict]:
        store = self.store_for(cfg)
        instance = f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}"
        argv = [
            str(self.binary),
            "--instance",
            instance,
            "--log-level",
            "info",
        ]
        if cfg.profile:
            argv[1:1] = ["--profile", cfg.profile]
        if cfg.theme_cli:
            argv[1:1] = ["--theme", cfg.theme_cli]
        env = store.env(instance)
        if cfg.profile_env and not cfg.profile:
            env["BITTY_PROFILE"] = cfg.profile_env
        log_path = self.logs / f"{sanitize(cfg.cid)}.app.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_file = log_path.open("w", encoding="utf-8")
        proc = subprocess.Popen(
            argv,
            env=env,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        self.procs[proc.pid] = proc
        self.log_files[proc.pid] = log_file
        self.pids.append(
            {
                "pid": proc.pid,
                "config": cfg.cid,
                "role": "headed matrix run",
                "started_at": now(),
                "argv": argv,
                "socket": env["BITTY_SOCKET"],
                "exit": None,
            }
        )
        focus_workspace(CAMPAIGN_WORKSPACE)
        client = self.wait_for_window(proc.pid)
        info = {
            "argv": argv,
            "pid": proc.pid,
            "socket": env["BITTY_SOCKET"],
            "log": str(log_path),
            "client": client,
        }
        if client is None:
            return proc, None, info
        client_ws = (client.get("workspace") or {}).get("id")
        if client_ws != CAMPAIGN_WORKSPACE:
            hypr_dispatch(
                f'hl.dsp.window.move({{window="address:{client["address"]}", '
                f"workspace={CAMPAIGN_WORKSPACE}}})"
            )
            time.sleep(0.3)
            clients = hypr_json("clients")
            client = next(
                (c for c in clients if c.get("address") == client.get("address")),
                client,
            )
        focus_window(client["address"])
        self.current_client = client
        time.sleep(RENDER_SETTLE_S)
        return proc, client, info

    def stop_instance(self, pid: int, client: dict | None) -> dict:
        address = (client or {}).get("address")
        closed_via = None
        if address:
            try:
                close_window(address)
                closed_via = "window close"
            except RuntimeError as err:
                self.errors.append(f"close_window failed: {err}")
        if self.wait_process_exit(pid):
            self.record_exit(pid, closed_via or "exited")
            return {"pid": pid, "exit": "exited", "closed_via": closed_via}
        # Only a PID recorded by this harness is signalled; never a pattern.
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            self.record_exit(pid, "exited before SIGTERM")
            return {"pid": pid, "exit": "exited", "closed_via": "already gone"}
        if self.wait_process_exit(pid, 5.0):
            self.record_exit(pid, "SIGTERM")
            return {"pid": pid, "exit": "SIGTERM", "closed_via": closed_via}
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.wait_process_exit(pid, 5.0)
        self.record_exit(pid, "SIGKILL")
        return {"pid": pid, "exit": "SIGKILL", "closed_via": closed_via}

    def record_exit(self, pid: int, how: str) -> None:
        for entry in self.pids:
            if entry["pid"] == pid and entry["exit"] is None:
                entry["exit"] = f"{how} at {now()}"
        handle = self.log_files.pop(pid, None)
        if handle is not None:
            with contextlib.suppress(Exception):  # closing twice is harmless
                handle.close()

    def wait_log(
        self, path: Path, pattern: str, timeout: float = LOG_LINE_TIMEOUT_S
    ) -> str | None:
        deadline = time.monotonic() + timeout
        regex = re.compile(pattern)
        while time.monotonic() < deadline:
            line = read_last_match(path, regex)
            if line:
                return line
            time.sleep(0.15)
        return None

    # -- checks ------------------------------------------------------------

    def parse_config_rows(self, text: str) -> dict[str, tuple[str, str]]:
        rows: dict[str, tuple[str, str]] = {}
        for line in text.splitlines():
            match = re.match(r"^(?P<key>.+?) = (?P<value>.*) \((?P<src>.*)\)$", line)
            if match:
                rows[match.group("key")] = (
                    match.group("value"),
                    match.group("src"),
                )
        return rows

    def expect(
        self, rows: dict, key: str, value: str, source: str | None = None
    ) -> dict:
        actual = rows.get(key)
        result = {
            "key": key,
            "expected_value": value,
            "expected_source": source,
            "actual": list(actual) if actual else None,
            "ok": bool(actual and actual[0] == value),
        }
        if source is not None:
            result["ok"] = bool(
                result["ok"] and source in (actual[1] if actual else "")
            )
        return result

    def check_config(self, cfg: MatrixConfig) -> dict:
        proc = self.config_check(cfg)
        rows = self.parse_config_rows(proc.stdout)
        expectations: list[dict] = []
        if proc.returncode != 0:
            return {
                "status": "FAIL",
                "reason": f"config check exited {proc.returncode}: {tail(proc.stderr, 3)}",
                "expectations": expectations,
                "rows": {k: list(v) for k, v in rows.items()},
            }
        if cfg.profile:
            profile_value = f'"{cfg.profile}"'
            expectations.append(
                self.expect(
                    rows,
                    "profile",
                    profile_value,
                    f"profile: {self.profile_path(cfg, cfg.profile)}",
                )
            )
        else:
            expectations.append(self.expect(rows, "profile", "(none)", "default"))
        if cfg.profile == "dev":
            expectations.append(
                self.expect(rows, "terminal.scrollback", "50000", "profile:")
            )
            expectations.append(self.expect(rows, "font.size", "13", "profile:"))
            expectations.append(self.expect(rows, "layout.gaps_out", "10", "profile:"))
        if cfg.profile == "minimal":
            expectations.append(
                self.expect(rows, "terminal.scrollback", "10000", "default")
            )
            expectations.append(self.expect(rows, "font.size", "12", "default"))
        if cfg.profile == "settings":
            expectations.extend(
                [
                    self.expect(rows, "terminal.scrollback", "100000", "profile:"),
                    self.expect(rows, "font.size", "14", "profile:"),
                    self.expect(rows, "window.opacity", "0.96", "profile:"),
                    self.expect(rows, "window.padding", "12", "profile:"),
                    self.expect(rows, "layout.gaps_in", "4", "profile:"),
                    self.expect(rows, "layout.gaps_out", "12", "profile:"),
                ]
            )
        if cfg.profile == "bar-off":
            # `workspace.show_bar` is a reserved input and `config check`
            # does not print it; the visible result is the bar-mount row
            # plus the plugin settings file (`show = false`).
            pass
        expected_theme = cfg.theme_cli or cfg.theme_file
        if cfg.theme_cli:
            expectations.append(
                self.expect(rows, "appearance.theme", f'"{cfg.theme_cli}"', "cli")
            )
        elif cfg.theme_file:
            expectations.append(
                self.expect(rows, "appearance.theme", f'"{cfg.theme_file}"', "file:")
            )
        else:
            # Value-only: the harness always writes an init.lua layer (for
            # animations, session, and plugin axes), so the reported source is
            # `file:` even when that layer does not override the theme. The
            # value must still be the built-in default.
            expectations.append(self.expect(rows, "appearance.theme", "(default)"))
        if cfg.bg:
            assert self.selected_wallpaper is not None
            expectations.append(
                self.expect(
                    rows,
                    "window.background_image",
                    f'"{self.selected_wallpaper}"',
                    "file:",
                )
            )
            expectations.append(
                self.expect(rows, "window.background_fit", cfg.bg, "file:")
            )
        else:
            expectations.append(
                self.expect(rows, "window.background_image", "(unset)", "default")
            )
        enabled_value = "false" if cfg.animations is False else "true"
        if cfg.animations is False:
            enabled_source = "file:"
        elif cfg.profile == "settings":
            enabled_source = "profile:"
        else:
            enabled_source = "default"
        expectations.append(
            self.expect(
                rows,
                "appearance.animations.enabled",
                enabled_value,
                enabled_source,
            )
        )
        if cfg.custom_palette or cfg.theme_file:
            expectations.append(
                self.expect(rows, "theme.contrast.result", "pass", "policy")
            )
        failed = [e for e in expectations if not e["ok"]]
        return {
            "status": "PASS" if not failed else "FAIL",
            "reason": "; ".join(
                f"{e['key']} expected {e['expected_value']} "
                f"({e['expected_source']}) got {e['actual']}"
                for e in failed
            ),
            "expectations": expectations,
            "theme_expected": expected_theme,
            "rows": {k: list(v) for k, v in rows.items()},
            "config_check_exit": proc.returncode,
        }

    def profile_path(self, cfg: MatrixConfig, name: str) -> Path:
        return self.store_for(cfg).profiles / f"{name}.lua"

    def check_theme_pixels(self, cfg: MatrixConfig, shot: Path) -> dict:
        mode = dominant_color(shot)
        if cfg.custom_palette:
            expected_hex = cfg.custom_palette.get("background")
            expected_key = "custom palette"
        else:
            expected_key = cfg.theme_cli or cfg.theme_file or "(default)"
            expected_hex = self.theme_backgrounds.get(expected_key)
        if not expected_hex:
            return {"status": "NOT_EXERCISED", "reason": "unknown theme background"}
        expected = hex_to_rgb(expected_hex)
        distance = max(abs(mode[i] - expected[i]) for i in range(3))
        return {
            "status": "PASS" if distance <= THEME_BG_TOLERANCE else "FAIL",
            "dominant_rgb": list(mode),
            "expected_hex": expected_hex,
            "expected_key": expected_key,
            "distance": distance,
        }

    def check_background_pixels(self, shot: Path) -> dict:
        assert self.selected_wallpaper is not None
        bg_hex = self.theme_backgrounds.get("(default)", "#000000")
        stats = background_coverage(shot, hex_to_rgb(bg_hex))
        ok = stats["coverage"] >= BG_COVERAGE_MIN
        if ok:
            ok = stats["center_coverage"] >= BG_CENTER_COVERAGE_MIN
        return {
            "status": "PASS" if ok else "FAIL",
            "theme_bg": bg_hex,
            "coverage": round(stats["coverage"], 4),
            "center_coverage": round(stats["center_coverage"], 4),
            "thresholds": {
                "coverage_min": BG_COVERAGE_MIN,
                "center_coverage_min": BG_CENTER_COVERAGE_MIN,
            },
        }

    # -- per-config execution ---------------------------------------------

    def run_config(self, cfg: MatrixConfig) -> None:
        self.write_profiles()
        self.write_config(cfg)
        store = self.store_for(cfg)
        clear_session(store.session_file)
        self.current_client = None

        if cfg.fail_closed == "inventory":
            self.run_inventory_probe(cfg)
            return
        if cfg.fail_closed == "profile":
            self.run_profile_fail_probe(cfg)
            return
        if cfg.fail_closed == "bg-fit":
            self.run_bg_fit_probe(cfg)
            return
        if cfg.fail_closed == "theme":
            self.run_theme_fail_probe(cfg)
            return
        if cfg.fail_closed == "statusline":
            self.run_statusline_probe(cfg)
            return
        if cfg.fail_closed == "extensions":
            self.run_extension_inventory_probe(cfg)
            return
        if cfg.fail_closed == "extension-matrix":
            self.run_extension_matrix_probe(cfg)
            return
        if cfg.fail_closed == "palette":
            self.run_palette_fail_probe(cfg)
            return
        if cfg.profile_env and not cfg.gui:
            self.run_profile_env_probe(cfg)
            return

        installs = self.install_plugins(cfg) if cfg.gui else []
        extension_installs: dict = {}
        cycle: dict = {}
        if cfg.gui:
            if cfg.graphics_cycle:
                install_cfg = dataclasses.replace(
                    cfg, extensions=("graphics",), graphics_cycle=False
                )
                cycle["install"] = self.install_extensions(
                    install_cfg, log_prefix="cycle-"
                )
                cycle["before"] = self.extension_list(cfg)
                cycle["remove"] = self.remove_extension(cfg, "graphics")
                cycle["after"] = self.extension_list(cfg)
            extension_installs = self.install_extensions(cfg)
            self.install_local_plugins(cfg)
        config_result = self.check_config(cfg)
        self.config_results[cfg.cid] = config_result
        self.config_results[cfg.cid]["extensions"] = {
            "requested": list(cfg.extensions),
            "installs": extension_installs,
            "cycle": cycle,
        }
        self.add_row(
            cfg,
            kind="core",
            row_id="config",
            check="bitty config check: resolved values and source attribution",
            status=config_result["status"],
            reason=config_result["reason"],
            evidence={
                "expectations": config_result["expectations"],
                "headless": True,
            },
        )
        if not cfg.gui:
            return

        proc, client, info = self.launch(cfg)
        if proc is None or client is None:
            if proc is not None:
                self.stop_instance(proc.pid, None)
            self.add_row(
                cfg,
                kind="core",
                row_id="launch",
                check="window mapped on workspace 5, first paint, no panic",
                status="FAIL",
                reason="no window mapped before timeout or process exited",
                evidence={"launch": info, "log_tail": tail_file(Path(info["log"]), 20)},
            )
            return
        launch_shot = self.screenshot(cfg, "launch", window=False)
        window_shot = self.screenshot(cfg, "launch-window", window=True)
        log_path = Path(info["log"])
        panic = read_last_match(
            log_path, re.compile(r"panicked at|thread \'main\' panicked")
        )
        paint = (
            first_paint_ok(Path(window_shot))
            if window_shot
            else {"status": "FAIL", "reason": "no window screenshot"}
        )
        class_ok = client.get("class") == EXPECTED_WINDOW_CLASS
        instance_ok = "--instance" in info["argv"] and any(
            token.startswith(f"{INSTANCE_PREFIX}-") for token in info["argv"]
        )
        workspace_ok = (client.get("workspace") or {}).get("id") == CAMPAIGN_WORKSPACE
        launch_status = "PASS"
        launch_reason = ""
        if panic:
            launch_status, launch_reason = "FAIL", "panic in app log"
        elif not class_ok:
            launch_status, launch_reason = (
                "FAIL",
                f"unexpected window class {client.get('class')!r}",
            )
        elif not instance_ok:
            launch_status, launch_reason = "FAIL", "launch argv lacks --instance marker"
        elif not workspace_ok:
            launch_status, launch_reason = (
                "FAIL",
                f"window landed on workspace {(client.get('workspace') or {}).get('id')}",
            )
        elif paint.get("status") != "PASS":
            launch_status, launch_reason = (
                "FAIL",
                paint.get("reason", "blank first paint"),
            )
        self.add_row(
            cfg,
            kind="core",
            row_id="launch",
            check="window mapped on workspace 5, first paint, no panic",
            status=launch_status,
            reason=launch_reason,
            screenshots=[s for s in (launch_shot, window_shot) if s],
            evidence={
                "pid": proc.pid,
                "client": client,
                "argv": info["argv"],
                "socket": info["socket"],
                "panic": panic,
                "first_paint": paint,
                "window_class": client.get("class"),
                "class_ok": class_ok,
                "instance_marker_ok": instance_ok,
                "workspace": (client.get("workspace") or {}).get("id"),
                "workspace_ok": workspace_ok,
                "log_tail": tail_file(log_path, 10),
            },
        )

        # Quick theme rows: launch, verify palette, close, no plugin/ipc work.
        if cfg.quick:
            self.record_theme_rows(cfg, window_shot, config_result)
            self.record_close(cfg, proc, client, log_path, window_shot)
            return

        # Session (fresh): exactly one view and no restore line at launch.
        if not cfg.session_restore:
            rc, payload, err = self.ctl(
                cfg,
                ["view", "list", "--format", "json"],
                log=f"{sanitize(cfg.cid)}.ctl-view-initial.json",
            )
            views = (payload or {}).get("result", {}).get("views", [])
            restored = read_last_match(log_path, re.compile(r"session restored"))
            ok = len(views) == 1 and not restored
            self.add_row(
                cfg,
                kind="core",
                row_id="session-fresh",
                check="fresh launch starts from one view and ignores prior state",
                status="PASS" if ok else "FAIL",
                reason=""
                if ok
                else f"views={len(views)} restored_log={bool(restored)} ctl_err={err}",
                screenshots=[launch_shot] if launch_shot else [],
                evidence={"views": views, "restored_log": restored, "ctl_exit": rc},
            )

        # Theme axis: dominant color, contrast result, and light/dark family.
        self.record_theme_rows(cfg, window_shot, config_result)
        if cfg.spot_check:
            self.record_ansi_spot_check(cfg, log_path)

        # Background axis: the wallpaper must cover the window, not the theme bg.
        if cfg.bg:
            bg_check = (
                self.check_background_pixels(Path(window_shot))
                if window_shot
                else {"status": "FAIL", "reason": "no screenshot"}
            )
            self.add_row(
                cfg,
                kind="core",
                row_id="background-rendered",
                check="window.background_image is painted under the grid",
                status=bg_check["status"],
                reason=bg_check.get("reason", ""),
                screenshots=[window_shot] if window_shot else [],
                evidence=bg_check,
            )

        ipc = self.run_ipc_checks(cfg) if not cfg.quick else []
        for row in ipc:
            if not row.get("screenshots") and launch_shot:
                row["screenshots"] = [launch_shot]
            self.add_row(cfg, **row)

        if cfg.kitty_image:
            self.run_kitty_check(cfg)
        self.record_graphics_rows(cfg, log_path)
        self.record_bar_checks(cfg, log_path)
        self.record_url_hover_check(cfg, log_path)

        # Plugin command probes (bounded, one per active plugin).
        probes = self.run_plugin_probes(cfg) if cfg.interactions else {}
        activation = self.plugin_activation(cfg, log_path, probes)
        for plugin in self.plugin_set(cfg):
            install = next((i for i in installs if i["plugin"] == plugin.id), None)
            act = activation.get(plugin.id, {})
            row_status = act.get("status", "FAIL")
            reason = act.get("reason", "not registered")
            if install and install.get("install_exit") is None:
                row_status = "FAIL"
                reason = f"source unavailable: {install.get('error')}"
            elif install and install.get("install_exit") not in (0, None):
                row_status = "FAIL"
                detail = " ".join(install.get("stderr_tail") or [])[-300:]
                reason = f"install refused (exit {install['install_exit']}): {detail}"
            shots: list[str] = []
            evidence = {
                "plugin_id": plugin.id,
                "repository": plugin.repository,
                "registry_entry": f"registry/official/{plugin.slug}.toml",
                "source_sha": plugin.head,
                "origin_main_sha": plugin.remote_main,
                "source_clean": plugin.clean,
                "source_ok": plugin.source_ok,
                "manifest_hash": plugin.manifest_hash,
                "manifest_hash_actual": plugin.manifest_hash_actual,
                "manifest_hash_ok": plugin.manifest_hash_ok,
                "install": install,
                "activation": act,
                "screenshot_config": launch_shot,
            }
            if act.get("probe"):
                evidence["command_probe"] = act["probe"]
                if act["probe"].get("screenshot"):
                    shots.append(act["probe"]["screenshot"])
            elif not cfg.interactions:
                evidence["command_probe"] = {
                    "status": "NOT_EXERCISED",
                    "reason": "interaction probes disabled for this configuration",
                }
            self.add_row(
                cfg,
                kind="plugin",
                row_id=f"plugin-{sanitize(plugin.slug)}",
                check="activation state; first-command probe and screenshot where a chord exists",
                status=row_status,
                reason=reason,
                screenshots=shots or ([launch_shot] if launch_shot else []),
                evidence=evidence,
            )

        # Failed-dispatch negative probe (generic named error class).
        if probes.get("failure"):
            probe = probes["failure"]
            self.add_row(
                cfg,
                kind="core",
                row_id="plugin-failure-path",
                check="unknown plugin command is refused with a named error",
                status=probe.get("status", "FAIL"),
                reason=probe.get("reason", ""),
                screenshots=[probe["screenshot"]] if probe.get("screenshot") else [],
                evidence=probe,
            )

        self.record_plugin_surfaces(cfg, probes)
        self.record_plugin_without_extension(cfg, log_path, activation)
        self.record_net_checks(cfg, log_path)

        storage_shot = self.screenshot(cfg, "ext-storage", window=True)
        first_close = self.record_close(cfg, proc, client, log_path, storage_shot)

        if cfg.session_restore:
            proc, client, info = self.launch(cfg)
            if proc is None or client is None:
                self.add_row(
                    cfg,
                    kind="core",
                    row_id="session-restored",
                    check="session restore across relaunch",
                    status="FAIL",
                    reason="relaunch produced no window",
                    evidence={
                        "first_close": first_close,
                        "log_tail": tail_file(Path(info["log"]), 20),
                    },
                )
                return
            restore_log = Path(info["log"])
            restored = read_last_match(
                restore_log, re.compile(r"session restored \(([^)]*)\)")
            )
            rc, payload, err = self.ctl(
                cfg,
                ["view", "list", "--format", "json"],
                log=f"{sanitize(cfg.cid)}.ctl-view-restored.json",
            )
            views = (payload or {}).get("result", {}).get("views", [])
            shot = self.screenshot(cfg, "session-restored", window=True)
            storage_shot = shot
            ok = bool(restored) and len(views) >= 2
            self.add_row(
                cfg,
                kind="core",
                row_id="session-restored",
                check="session restore across relaunch: layout and view count",
                status="PASS" if ok else "FAIL",
                reason=""
                if ok
                else f"restored_log={restored!r} views={len(views)} ctl_err={err}",
                screenshots=[shot] if shot else [],
                evidence={
                    "first_close": first_close,
                    "restored_log": restored,
                    "views": views,
                    "ctl_exit": rc,
                },
            )
            self.stop_instance(proc.pid, client)
            self.current_client = None
            self.run_extension_rows(cfg, restore_log, ipc, launch_shot, storage_shot)
            return

        self.run_extension_rows(cfg, log_path, ipc, launch_shot, storage_shot)

    def record_theme_rows(
        self, cfg: MatrixConfig, window_shot: str | None, config_result: dict
    ) -> None:
        if not (cfg.theme_file or cfg.theme_cli or cfg.custom_palette):
            return
        theme_check = (
            self.check_theme_pixels(cfg, Path(window_shot))
            if window_shot
            else {"status": "FAIL", "reason": "no screenshot"}
        )
        self.add_row(
            cfg,
            kind="core",
            row_id="theme-pixels",
            check="dominant window color matches the resolved theme background",
            status=theme_check["status"],
            reason=theme_check.get("reason", ""),
            screenshots=[window_shot] if window_shot else [],
            evidence=theme_check,
        )
        rows = config_result.get("rows", {})
        contrast = rows.get("theme.contrast.result")
        contrast_ok = bool(contrast and contrast[0] == "pass")
        self.add_row(
            cfg,
            kind="core",
            row_id="theme-contrast",
            check="theme.contrast.result = pass (policy) for the active palette",
            status="PASS" if contrast_ok else "FAIL",
            reason=""
            if contrast_ok
            else f"theme.contrast.result={contrast} expected pass (policy)",
            evidence={
                "contrast": contrast,
                "theme": cfg.theme_file or cfg.theme_cli or "(default)",
                "category": cfg.theme_category,
                "custom_palette": bool(cfg.custom_palette),
                "citation": DOC_CITATIONS["theme"],
                "headless": True,
            },
        )
        if cfg.theme_category:
            self.add_row(
                cfg,
                kind="core",
                row_id="theme-family",
                check="preset belongs to the shipped light/dark family",
                status="PASS"
                if cfg.theme_category in {"dark", "light", "custom"}
                else "FAIL",
                reason=""
                if cfg.theme_category in {"dark", "light", "custom"}
                else f"unexpected category {cfg.theme_category!r}",
                evidence={
                    "category": cfg.theme_category,
                    "shipped_themes": len(self.theme_catalog),
                },
            )

    def record_ansi_spot_check(self, cfg: MatrixConfig, log_path: Path) -> None:
        """Vim / `ls --color` spot check: the palette reaches real consumers."""
        if not cfg.spot_check:
            return
        if not self.current_client:
            self.add_row(
                cfg,
                kind="core",
                row_id=f"ansi-{cfg.spot_check}",
                check=f"{cfg.spot_check} renders ANSI colors under the preset",
                status="NOT_EXERCISED",
                reason="no window client for the spot check",
            )
            return
        self.ctl(cfg, ["view", "focus", "v:1"])
        _rc, payload, _err = self.ctl(cfg, ["terminal", "list", "--format", "json"])
        terminals = (payload or {}).get("result", {}).get("terminals", [])
        diagnostics = (payload or {}).get("result", {}).get("diagnostics", [])
        attached = next((d for d in diagnostics if d.get("view_id") == "v:1"), None)
        terminal = terminals[0] if terminals else {}
        terminal_id = (
            attached.get("terminal_id") if attached else terminal.get("id") or "t:1"
        )
        home = self.store_for(cfg).home
        if cfg.spot_check == "vim":
            target = home / "spot.py"
            write_text(
                target,
                "#!/usr/bin/env python3\n"
                "# comment: syntax colors\n"
                '"""docstring"""\n'
                "import os\n\n\n"
                "class Spot:\n"
                "    def run(self, value=42):\n"
                '        text = "string literal"\n'
                "        if value > 1 and os.name:\n"
                "            return text, value\n"
                "        return None\n",
            )
            command = (
                f"clear; vim -N -u NONE -c 'syntax on' -c 'set t_Co=256' {target}\n"
            )
        else:
            command = "clear; ls --color=always -la /usr/bin | head -40\n"
        self.ctl(
            cfg,
            ["terminal", "send", terminal_id, command],
            log=f"{sanitize(cfg.cid)}.ctl-{cfg.spot_check}-send.txt",
        )
        time.sleep(2.0)
        shot = self.screenshot(cfg, f"ansi-{cfg.spot_check}", window=True)
        bg = hex_to_rgb(
            cfg.custom_palette.get("background")
            or self.theme_backgrounds.get(cfg.theme_file or "(default)")
            or "#000000"
        )
        colors = (
            distinct_color_count(Path(shot), bg, minimum=ANSI_SPOT_MIN_PIXELS)
            if shot
            else 0
        )
        threshold = ANSI_SPOT_MIN_COLORS.get(cfg.spot_check, 6)
        ok = colors >= threshold
        self.add_row(
            cfg,
            kind="core",
            row_id=f"ansi-{cfg.spot_check}",
            check=(
                "Vim syntax rendering shows distinct ANSI colors"
                if cfg.spot_check == "vim"
                else "ls --color output shows distinct ANSI colors"
            ),
            status="PASS" if ok else "FAIL",
            reason="" if ok else f"distinct_colors={colors} < {threshold}",
            screenshots=[shot] if shot else [],
            evidence={
                "spot_check": cfg.spot_check,
                "distinct_colors": colors,
                "threshold": threshold,
                "min_pixels": ANSI_SPOT_MIN_PIXELS,
                "background": [bg[0], bg[1], bg[2]],
                "theme": cfg.theme_file or "(default)",
                "theme_family": cfg.theme_category,
                "command": command.strip(),
                "panic": read_last_match(
                    log_path, re.compile(r"panicked at|thread 'main' panicked")
                ),
            },
        )
        if cfg.spot_check == "vim":
            self.ctl(
                cfg,
                ["terminal", "send", terminal_id, "\x1b:q!\n"],
                log=f"{sanitize(cfg.cid)}.ctl-vim-quit.txt",
            )

    def record_close(
        self,
        cfg: MatrixConfig,
        proc: subprocess.Popen,
        client: dict,
        log_path: Path,
        shot: str | None,
    ) -> dict:
        storage_shot = shot or self.screenshot(cfg, "ext-storage", window=True)
        first_close = self.stop_instance(proc.pid, client)
        self.current_client = None
        self.add_row(
            cfg,
            kind="core",
            row_id="close",
            check="window close exits cleanly and saves session state",
            status="PASS" if first_close["exit"] != "SIGKILL" else "FAIL",
            reason="" if first_close["exit"] != "SIGKILL" else "process needed SIGKILL",
            screenshots=[storage_shot] if storage_shot else [],
            evidence={
                "close": first_close,
                "capture": "window state immediately before the close request",
                "session_saved": bool(
                    read_last_match(log_path, re.compile(r"session saved"))
                ),
            },
        )
        return first_close

    def prepare_bar_workspaces(self, cfg: MatrixConfig) -> list[dict]:
        # Three workspaces make the default lone-workspace hide rule moot and
        # give the click/scroll probes an inactive pill to target.
        for _ in range(2):
            self.ctl(cfg, ["workspace", "new"], log=None)
            time.sleep(0.3)
        return self.workspace_state(cfg)

    def workspace_state(self, cfg: MatrixConfig) -> list[dict]:
        _rc, payload, _ = self.ctl(cfg, ["workspace", "list", "--format", "json"])
        result = (payload or {}).get("result", {})
        raw = result.get("workspaces", [])
        active_id = result.get("active_id")
        rows = []
        for item in raw:
            if isinstance(item, str):
                rows.append({"id": item, "active": item == active_id})
            else:
                rows.append(item)
        return rows

    def record_bar_checks(self, cfg: MatrixConfig, log_path: Path) -> None:
        if not cfg.bar_check:
            return
        workspaces = self.prepare_bar_workspaces(cfg)
        time.sleep(0.8)
        shot = self.screenshot(cfg, "bar-band", window=True)
        expected_edge = cfg.bar_edge or "bottom"
        stats = (
            band_stats(Path(shot))
            if shot
            else {"top": 0, "bottom": 0, "top_row": None, "bottom_row": None}
        )
        profile = (
            band_profile(Path(shot), expected_edge)
            if shot and expected_edge in {"top", "bottom"}
            else {"accent": 0, "thirds": {"left": 0, "center": 0, "right": 0}}
        )
        box = profile.get("accent_box")
        accent_ok = bool(
            box and 10 <= box[2] - box[0] <= 400 and 5 <= box[3] - box[1] <= 80
        )
        if expected_edge == "none":
            # No bar plugin (or bar hidden): neither edge may paint a pill.
            top_profile = band_profile(Path(shot), "top") if shot else {"accent": 0}
            bottom_profile = (
                band_profile(Path(shot), "bottom") if shot else {"accent": 0}
            )
            ok = (
                top_profile.get("accent", 0) < 20
                and bottom_profile.get("accent", 0) < 20
            )
            reason = (
                ""
                if ok
                else (
                    f"accent pixels without the bar: top="
                    f"{top_profile.get('accent')} bottom="
                    f"{bottom_profile.get('accent')}"
                )
            )
            profile = {"accent": 0, "thirds": {"left": 0, "center": 0, "right": 0}}
            stats = {
                **stats,
                "top_profile": top_profile,
                "bottom_profile": bottom_profile,
            }
        else:
            other = "bottom" if expected_edge == "top" else "top"
            other_profile = band_profile(Path(shot), other) if shot else {"accent": 0}
            other_box = other_profile.get("accent_box")
            other_ok = bool(
                other_box
                and 10 <= other_box[2] - other_box[0] <= 400
                and 5 <= other_box[3] - other_box[1] <= 80
            )
            ok = accent_ok and (
                not other_ok
                or profile.get("accent", 0) > other_profile.get("accent", 0)
            )
            reason = (
                ""
                if ok
                else (
                    f"expected accent at {expected_edge}, got "
                    f"box={box} accent={profile.get('accent')} "
                    f"other={other_profile.get('accent')}"
                )
            )
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-mount",
            check=f"bar band mounts at edge={expected_edge!r} and not the opposite edge",
            status="PASS" if ok else "FAIL",
            reason=reason,
            screenshots=[shot] if shot else [],
            evidence={
                "expected_edge": expected_edge,
                "edge_pixels": stats,
                "edge_profile": profile,
                "threshold": BAR_EDGE_MIN_PX,
                "workspaces": workspaces,
                "screenshot_config": shot,
            },
        )
        if expected_edge == "none":
            for row_id, check in (
                ("bar-alignment", "left/center/right true alignment"),
                ("bar-hover", "pointer-enter detail overlay"),
            ):
                self.add_row(
                    cfg,
                    kind="bar",
                    row_id=row_id,
                    check=check,
                    status="NOT_APPLICABLE",
                    reason="bar plugin absent or hidden in this configuration",
                )
            if cfg.bar_absent_probe:
                self.record_bar_absent_hitregions(
                    cfg, log_path, shot, workspaces, expected_edge
                )
            return
        if self.bar_anchors.get("state") == "merged":
            thirds = profile.get("thirds", {})
            size_px = profile.get("size", [1, 1])[0]
            left_anchored = bool(
                accent_ok
                and box
                and box[0] < size_px // 6
                and thirds.get("left", 0) > 20
            )
            right_anchored = thirds.get("right", 0) > 40
            aligned = left_anchored and right_anchored
            self.add_row(
                cfg,
                kind="bar",
                row_id="bar-alignment",
                check="left/center/right groups align to true edges",
                status="PASS" if aligned else "FAIL",
                reason=""
                if aligned
                else (
                    f"left_anchored={left_anchored} right_anchored={right_anchored} "
                    f"accent_box={box} thirds={thirds}"
                ),
                screenshots=[shot] if shot else [],
                evidence={
                    "groups": list(cfg.bar_groups),
                    "band_profile": profile,
                    "dependency": BAR_ANCHORS_BLOCKER,
                    "bar_anchors": self.bar_anchors,
                },
            )
        else:
            self.add_row(
                cfg,
                kind="bar",
                row_id="bar-alignment",
                check="left/center/right groups align to true edges",
                status="BLOCKED",
                reason=BAR_ALIGNMENT_BLOCKER,
                evidence={
                    "groups": list(cfg.bar_groups),
                    "observed_order": "modules_left -> modules_center -> modules_right",
                    "dependency": BAR_ALIGNMENT_BLOCKER,
                    "bar_anchors": self.bar_anchors,
                },
            )
        self.record_bar_hover_check(cfg, log_path, shot, profile, accent_ok)
        # Hit-region click: move across the left group until the active
        # workspace changes; only a recorded client is targeted.
        active_before = next((w.get("id") for w in workspaces if w.get("active")), None)
        click_evidence = {
            "active_before": active_before,
            "attempts": [],
            "chord": "ydotool click 0x00",
        }
        clicked = False
        if self.current_client and active_before is not None:
            at = self.current_client.get("at") or [0, 0]
            size = self.current_client.get("size") or [0, 0]
            shot_width = stats.get("size", [size[0], size[1]])[0]
            scale = shot_width / max(1, size[0])
            box = profile.get("accent_box") if accent_ok else None
            if box:
                y = at[1] + int((box[1] + box[3]) / 2 / scale)
            else:
                y = at[1] + (25 if expected_edge == "top" else size[1] - 25)
            for dx in range(14, min(shot_width - 20, 360), 18):
                x = at[0] + int(dx / scale)
                pointer_click(x, y)
                time.sleep(0.45)
                rows = self.workspace_state(cfg)
                active = next((w.get("id") for w in rows if w.get("active")), None)
                click_evidence["attempts"].append(
                    {"x": x, "y": y, "active_after": active}
                )
                if active is not None and active != active_before:
                    clicked = True
                    break
        click_shot = self.screenshot(cfg, "bar-click", window=True)
        font_scale = 2
        bands = {"top": 0, "bottom": 0}
        if click_shot:
            bands = band_stats(Path(click_shot))
        size = self.current_client.get("size") or [0, 0] if self.current_client else []
        top_y = 10
        bottom_y = max(0, size[1] - 10) if size else 0
        click_evidence.update(
            {
                "band_top_px": bands.get("top"),
                "band_bottom_px": bands.get("bottom"),
                "top_probe_y": top_y,
                "bottom_probe_y": bottom_y,
                "font_scale": font_scale,
            }
        )
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-click",
            check="left click on an inactive workspace pill changes the active workspace",
            status="PASS" if clicked else "FAIL",
            reason="" if clicked else "no pill hit changed the active workspace",
            screenshots=[click_shot] if click_shot else [],
            evidence=click_evidence,
        )
        # Wheel hit region: only a module with on_scroll (pulseaudio) can
        # claim it. bar#31 merged, so a dispatch is expected and a miss is
        # recorded as NOT_EXERCISED with the reason, never silently excused.
        scroll_shot = shot
        scroll_evidence = {
            "dependency": BAR_SCROLL_DEPENDENCY,
            "module": "pulseaudio",
            "delta": -1,
        }
        scroll_seen = False
        if self.current_client and "pulseaudio" in ",".join(cfg.bar_groups):
            at = self.current_client.get("at") or [0, 0]
            size = self.current_client.get("size") or [0, 0]
            shot_width = stats.get("size", [size[0], size[1]])[0]
            scale = shot_width / max(1, size[0])
            x = at[0] + int(max(20 * scale, shot_width - 80) / scale)
            box = profile.get("accent_box") if accent_ok else None
            y = (
                at[1] + int((box[1] + box[3]) / 2 / scale)
                if box
                else at[1] + (25 if expected_edge == "top" else size[1] - 25)
            )
            pointer_wheel(x, y, -1)
            scroll_seen = bool(
                self.wait_log(
                    log_path,
                    r"keymap command:bitty-terminal\.bar:audio-step (?:->|refused)",
                    timeout=1.5,
                )
            )
            scroll_evidence.update({"x": x, "y": y, "dispatch_seen": scroll_seen})
        has_scroll_module = "pulseaudio" in ",".join(cfg.bar_groups)
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-scroll",
            check="wheel over a scroll-bound module dispatches on_scroll",
            status="PASS" if scroll_seen else "NOT_EXERCISED",
            reason=""
            if scroll_seen
            else (
                "no on_scroll dispatch observed after the wheel event; "
                f"{BAR_SCROLL_DEPENDENCY}"
                if has_scroll_module
                else f"no scroll-bearing module in this plan; {BAR_SCROLL_DEPENDENCY}"
            ),
            screenshots=[scroll_shot] if scroll_shot else [],
            evidence={**scroll_evidence, "dependency": BAR_SCROLL_DEPENDENCY},
        )
        # Group order/visibility: accent pill on the left, text on the right.
        # OCR is attempted only as extra evidence; tesseract may lack language
        # data, so the row never depends on it.
        if cfg.bar_groups:
            left_text = ocr_band(Path(shot), expected_edge, half="left") if shot else ""
            right_text = (
                ocr_band(Path(shot), expected_edge, half="right") if shot else ""
            )
            thirds = profile.get("thirds", {})
            accent_box = profile.get("accent_box")
            left_ok = bool(
                accent_ok
                and accent_box
                and accent_box[0] < profile.get("size", [1, 1])[0] // 3
                and thirds.get("left", 0) > 20
            )
            right_ok = bool(accent_ok and thirds.get("right", 0) > 40)
            self.add_row(
                cfg,
                kind="bar",
                row_id="bar-groups",
                check="declared left/center/right groups are painted in order",
                status="PASS" if left_ok and right_ok else "NOT_EXERCISED",
                reason=""
                if left_ok and right_ok
                else "band profile did not resolve left/right group content",
                screenshots=[shot] if shot else [],
                evidence={
                    "declared_groups": list(cfg.bar_groups),
                    "band_profile": profile,
                    "left_text": left_text,
                    "right_text": right_text,
                    "left_has_workspaces": left_ok,
                    "right_has_content": right_ok,
                    "ocr_language_available": ocr_language_available(),
                    "alignment_blocker": BAR_ALIGNMENT_BLOCKER,
                },
            )
        # Bar contrast uses the preset policy result plus visible band pixels.
        rows = self.config_results.get(cfg.cid, {}).get("rows", {})
        contrast = rows.get("theme.contrast.result")
        contrast_ok = bool(contrast and contrast[0] == "pass")
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-contrast",
            check="bar band is visible and the active palette contrast policy passes",
            status="PASS" if contrast_ok and accent_ok else "FAIL",
            reason=""
            if contrast_ok and accent_ok
            else f"contrast={contrast} accent_pixels={profile.get('accent')}",
            screenshots=[shot] if shot else [],
            evidence={
                "contrast": contrast,
                "accent_pixels": profile.get("accent"),
                "edge": expected_edge,
                "theme": cfg.theme_file or cfg.theme_cli or "(default)",
                "theme_family": cfg.theme_category or "dark",
            },
        )

    def record_bar_hover_check(
        self,
        cfg: MatrixConfig,
        log_path: Path,
        shot: str | None,
        profile: dict,
        accent_ok: bool,
    ) -> None:
        """Hover probe: real when #2411/#2423 has merged, BLOCKED until then."""
        if self.bar_anchors.get("state") != "merged":
            self.add_row(
                cfg,
                kind="bar",
                row_id="bar-hover",
                check="pointer-enter shows module detail and leave hides it",
                status="BLOCKED",
                reason=BAR_HOVER_BLOCKER,
                evidence={
                    "dependency": BAR_HOVER_BLOCKER,
                    "bar_anchors": self.bar_anchors,
                },
            )
            return
        if not (self.current_client and shot):
            self.add_row(
                cfg,
                kind="bar",
                row_id="bar-hover",
                check="pointer-enter shows module detail and leave hides it",
                status="NOT_EXERCISED",
                reason="no window client or screenshot for the hover probe",
                evidence={"bar_anchors": self.bar_anchors},
            )
            return
        before = self.screenshot(cfg, "bar-hover-before", window=True)
        at = self.current_client.get("at") or [0, 0]
        size = self.current_client.get("size") or [0, 0]
        edge = cfg.bar_edge or "bottom"
        box = profile.get("accent_box") if accent_ok else None
        shot_width = profile.get("size", [size[0], size[1]])[0]
        scale = shot_width / max(1, size[0])
        x = at[0] + int(((box[0] + box[2]) / 2 if box else size[0] / 2) / scale)
        y = at[1] + (
            int((box[1] + box[3]) / 2 / scale)
            if box
            else (25 if edge == "top" else max(0, size[1] - 25))
        )
        pointer_move(x, y)
        time.sleep(1.0)
        entered = self.screenshot(cfg, "bar-hover-entered", window=True)
        pointer_move(at[0] + size[0] // 2, at[1] + size[1] // 2)
        time.sleep(0.6)
        left = self.screenshot(cfg, "bar-hover-left", window=True)
        entered_px = (
            pixel_diff(Path(before), Path(entered)) if before and entered else 0
        )
        left_px = pixel_diff(Path(before), Path(left)) if before and left else 0
        ok = entered_px >= BAR_EDGE_CONTRAST_PX
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-hover",
            check="pointer-enter shows module detail and leave hides it",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"entered_delta_px={entered_px} left_delta_px={left_px}",
            screenshots=[s for s in (before, entered, left) if s],
            evidence={
                "pointer": [x, y],
                "entered_delta_px": entered_px,
                "left_delta_px": left_px,
                "threshold_px": BAR_EDGE_CONTRAST_PX,
                "bar_anchors": self.bar_anchors,
            },
        )

    def record_bar_absent_hitregions(
        self,
        cfg: MatrixConfig,
        log_path: Path,
        shot: str | None,
        workspaces: list[dict],
        expected_edge: str,
    ) -> None:
        """Hit regions without the bar plugin: no band, no switch, no panic."""
        if not self.current_client:
            for row_id, check in (
                ("bar-absent-click", "clicks at both edges change nothing"),
                ("bar-absent-scroll", "wheel at both edges dispatches nothing"),
            ):
                self.add_row(
                    cfg,
                    kind="bar",
                    row_id=row_id,
                    check=check,
                    status="NOT_EXERCISED",
                    reason="no window client for the hit-region probe",
                )
            return
        at = self.current_client.get("at") or [0, 0]
        size = self.current_client.get("size") or [0, 0]
        active_before = next((w.get("id") for w in workspaces if w.get("active")), None)
        attempts: list[dict] = []
        for name, y in (("top", at[1] + 10), ("bottom", at[1] + max(0, size[1] - 10))):
            for dx in (40, size[0] // 2, max(40, size[0] - 60)):
                pointer_click(at[0] + dx, y)
                time.sleep(0.35)
                rows = self.workspace_state(cfg)
                active = next((w.get("id") for w in rows if w.get("active")), None)
                attempts.append(
                    {"edge": name, "x": at[0] + dx, "y": y, "active_after": active}
                )
        active_after = next(
            (w.get("id") for w in self.workspace_state(cfg) if w.get("active")), None
        )
        changes = sum(1 for a in attempts if a["active_after"] != active_before)
        panic = read_last_match(
            log_path, re.compile(r"panicked at|thread 'main' panicked")
        )
        ok = changes <= BAR_ABSENT_MAX_WORKSPACE_CHANGES and not panic
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-absent-click",
            check="clicks at both window edges change nothing without the bar plugin",
            status="PASS" if ok else "FAIL",
            reason="" if ok else f"workspace_changes={changes} panic={bool(panic)}",
            screenshots=[shot] if shot else [],
            evidence={
                "active_before": active_before,
                "active_after": active_after,
                "attempts": attempts,
                "workspace_changes": changes,
                "panic": panic,
                "expected_edge": expected_edge,
            },
        )
        wheel: list[dict] = []
        for name, y in (("top", at[1] + 10), ("bottom", at[1] + max(0, size[1] - 10))):
            pointer_wheel(at[0] + size[0] // 2, y, -1)
            time.sleep(0.4)
            rows = self.workspace_state(cfg)
            wheel.append(
                {
                    "edge": name,
                    "active_after": next(
                        (w.get("id") for w in rows if w.get("active")), None
                    ),
                }
            )
        wheel_changes = sum(1 for w in wheel if w["active_after"] != active_before)
        ok = wheel_changes <= BAR_ABSENT_MAX_WORKSPACE_CHANGES and not panic
        self.add_row(
            cfg,
            kind="bar",
            row_id="bar-absent-scroll",
            check="wheel at both window edges dispatches nothing without the bar plugin",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"workspace_changes={wheel_changes} panic={bool(panic)}",
            screenshots=[shot] if shot else [],
            evidence={
                "active_before": active_before,
                "attempts": wheel,
                "workspace_changes": wheel_changes,
                "panic": panic,
            },
        )

    def record_url_hover_check(self, cfg: MatrixConfig, log_path: Path) -> None:
        if not cfg.hover_url:
            return
        assert self.current_client is not None
        before = self.screenshot(cfg, "url-before", window=True)
        self.ctl(cfg, ["view", "focus", "v:1"])
        _rc, payload, _err = self.ctl(cfg, ["terminal", "list", "--format", "json"])
        terminals = (payload or {}).get("result", {}).get("terminals", [])
        diagnostics = (payload or {}).get("result", {}).get("diagnostics", [])
        attached = next(
            (d for d in diagnostics if d.get("view_id") == "v:1"),
            None,
        )
        terminal = terminals[0] if terminals else {}
        terminal_id = (
            attached.get("terminal_id") if attached else terminal.get("id") or "t:1"
        )
        command = (
            "clear; printf '\\033[6;9Hhttps://example.com/ctx1344'; "
            "printf '\\n'; sleep 300\n"
        )
        self.ctl(
            cfg,
            ["terminal", "send", terminal_id, command],
            log=f"{sanitize(cfg.cid)}.ctl-url-send.txt",
        )
        time.sleep(1.0)
        cols = terminal.get("cols") or terminal.get("columns") or 80
        rows = terminal.get("rows") or 24
        at = self.current_client.get("at") or [0, 0]
        size = self.current_client.get("size") or [0, 0]
        cell_w = max(1.0, (size[0] - 16) / max(1, cols))
        cell_h = max(1.0, (size[1] - 16) / max(1, rows))
        x = int(at[0] + 8 + (9 + 12) * cell_w)
        y = int(at[1] + 8 + 5.5 * cell_h)
        pointer_move(x, y)
        pointer_key("ctrl", True)
        time.sleep(0.8)
        after = self.screenshot(cfg, "url-hover", window=True)
        pointer_key("ctrl", False)
        time.sleep(0.3)
        released = self.screenshot(cfg, "url-release", window=True)
        changed = pixel_diff(Path(before), Path(after)) if before and after else 0
        reverted = (
            pixel_diff(Path(before), Path(released)) if before and released else 0
        )
        ok = changed >= URL_HOVER_MIN_PX
        self.add_row(
            cfg,
            kind="core",
            row_id="url-hover",
            check="Ctrl+hover over a safe plaintext URL paints an underline and clears on release",
            status="PASS" if ok else "NOT_EXERCISED",
            reason=""
            if ok
            else "no Ctrl+hover delta observed (pointer synthesis or modifier)",
            screenshots=[s for s in (before, after, released) if s],
            evidence={
                "pointer": [x, y],
                "cell": [round(cell_w, 2), round(cell_h, 2)],
                "changed_px": changed,
                "released_px": reverted,
                "threshold": URL_HOVER_MIN_PX,
                "terminal_id": terminal_id,
                "citation": URL_DETECTOR_CITATION,
                "log_lines": tail_file(log_path, 5),
            },
        )

    def record_net_checks(self, cfg: MatrixConfig, log_path: Path) -> None:
        if not cfg.net_checks:
            return
        # One server per net configuration: a stale /ping from an earlier
        # config must never make an absent-component row look served.
        self.stop_component_server()
        self.component_server_log = None
        self.ensure_component_server()
        assert self.current_client is not None
        results: dict[str, str | None] = {}

        def dispatch(suffix: str) -> str | None:
            action = next(
                (
                    action
                    for _chord, action in cfg.extra_keymaps
                    if action.endswith(suffix)
                ),
                None,
            )
            chord = next(
                (chord for chord, act in cfg.extra_keymaps if act == action), None
            )
            if not action or not chord:
                return None
            focus_window(self.current_client["address"])
            press_chord(chord)
            line = self.wait_log(
                log_path,
                rf"keymap command:{re.escape(action.removeprefix('command:'))} (?:->|refused)",
                timeout=3.0,
            )
            results[action] = line
            return line

        dispatch(":go")
        # The broker fulfils asynchronously; wait for the local server with a
        # bounded deadline before polling the ticket.
        deadline = time.monotonic() + (
            COMPONENT_REQUEST_TIMEOUT_S if cfg.net_expected else 2.0
        )
        while time.monotonic() < deadline:
            if self.component_server_log and self.component_server_log.is_file():
                text = self.component_server_log.read_text(
                    encoding="utf-8", errors="replace"
                )
                if "/ping" in text or "/denied" in text:
                    break
            time.sleep(0.5)
        dispatch(":poll")
        time.sleep(0.6)
        dispatch(":deny")
        time.sleep(0.6)
        server_log = ""
        if self.component_server_log and self.component_server_log.is_file():
            server_log = self.component_server_log.read_text(
                encoding="utf-8", errors="replace"
            )
        served_ping = "/ping" in server_log
        served_denied = "/denied" in server_log
        shot = self.screenshot(cfg, "net-result", window=True)
        installed = cfg.net_expected is True
        installs = (
            self.config_results.get(cfg.cid, {})
            .get("extensions", {})
            .get("installs", {})
        )
        gated = (
            isinstance(installs.get("net"), dict)
            and installs["net"].get("status") == "NOT_EXERCISED"
        )
        if installed and gated:
            self.add_row(
                cfg,
                kind="extension",
                row_id="ext-net-broker",
                check="net component request/response and typed denial",
                status="NOT_EXERCISED",
                reason="pass --network-extensions to install the pinned artifact",
                evidence={"installs": installs, "citation": NET_CITATION},
            )
            return
        if installed:
            request_ok = served_ping and "-> invoked" in (
                results.get("command:acme.acceptance-allowed:go") or ""
            )
            denial_ok = not served_denied
            status = "PASS" if request_ok and denial_ok else "FAIL"
            reason = ""
            if not request_ok:
                reason = "allowed component request did not reach the local server"
            elif not denial_ok:
                reason = "denied request reached the local server"
        else:
            # Absent component: the request must fail typed and never reach
            # the server; the app stays alive and the window renders.
            request_ok = not served_ping
            denial_ok = not served_denied
            status = "PASS" if request_ok and denial_ok else "FAIL"
            reason = (
                "" if request_ok else "component request reached server without net"
            )
        self.add_row(
            cfg,
            kind="extension",
            row_id="ext-net-broker",
            check=(
                "net component request/response and typed denial"
                if installed
                else "net component absent: request degrades without reaching the network"
            ),
            status=status,
            reason=reason,
            screenshots=[shot] if shot else [],
            evidence={
                "expected_present": installed,
                "install": self.config_results.get(cfg.cid, {})
                .get("extensions", {})
                .get("installs", {}),
                "dispatch_lines": results,
                "server_log": tail(server_log, 8),
                "served_ping": served_ping,
                "served_denied": served_denied,
                "citation": NET_CITATION,
            },
        )
        self.add_row(
            cfg,
            kind="extension",
            row_id="ext-net-denial",
            check="request without network authority is denied and never served",
            status="PASS" if not served_denied else "FAIL",
            reason="" if not served_denied else "/denied reached the local server",
            screenshots=[shot] if shot else [],
            evidence={
                "deny_dispatch": results.get("command:acme.acceptance-denied:deny"),
                "served_denied": served_denied,
                "citation": NET_CITATION,
            },
        )

    def record_graphics_rows(self, cfg: MatrixConfig, log_path: Path) -> None:
        if cfg.graphics_expected is None and not cfg.graphics_cycle:
            return
        extension_state = self.config_results.get(cfg.cid, {}).get("extensions", {})
        cycle = extension_state.get("cycle", {})
        installs = extension_state.get("installs", {})
        gated = any(
            isinstance(item, dict) and item.get("status") == "NOT_EXERCISED"
            for item in installs.values()
        )
        _rc, components, _err = self.extension_list(cfg)
        names = {c.get("name") or c.get("id") for c in components}
        if cfg.graphics_cycle:
            cycle_install = cycle.get("install", {}).get("graphics", {})
            if cycle_install.get("status") == "NOT_EXERCISED":
                self.add_row(
                    cfg,
                    kind="extension",
                    row_id="ext-graphics-removal",
                    check="remove graphics: inventory drops it, no stale cache or panic",
                    status="NOT_EXERCISED",
                    reason="pass --network-extensions to install the pinned artifact",
                    evidence={"install": cycle_install, "citation": GRAPHICS_CITATION},
                )
                return
            before = cycle.get("before", (0, [], ""))[1]
            after = cycle.get("after", (0, [], ""))[1]
            remove = cycle.get("remove", {})
            before_names = {c.get("name") or c.get("id") for c in before}
            after_names = {c.get("name") or c.get("id") for c in after}
            ok = (
                "graphics" in before_names
                and "graphics" not in after_names
                and remove.get("exit") == 0
            )
            self.add_row(
                cfg,
                kind="extension",
                row_id="ext-graphics-removal",
                check="remove graphics: inventory drops it, no stale cache or panic",
                status="PASS" if ok else "FAIL",
                reason=""
                if ok
                else f"before={sorted(before_names)} after={sorted(after_names)} "
                f"remove_exit={remove.get('exit')}",
                screenshots=[],
                evidence={
                    "before": sorted(before_names),
                    "after": sorted(after_names),
                    "remove": remove,
                    "panic": read_last_match(
                        log_path, re.compile(r"panicked at|thread 'main' panicked")
                    ),
                    "citation": GRAPHICS_CITATION,
                },
            )
            # Stale-artifact sweep: the removed component must leave no
            # install directory and no kitty placement cache behind.
            store = self.store_for(cfg)
            residue = []
            for base in (store.data / "bitty" / "extensions", store.cache / "bitty"):
                if not base.is_dir():
                    continue
                for path in base.rglob("*"):
                    if "graphics" in path.name.lower() or "kitty" in path.name.lower():
                        residue.append(str(path.relative_to(store.root)))
            self.add_row(
                cfg,
                kind="extension",
                row_id="ext-graphics-residue",
                check="removed graphics component leaves no install dir or kitty cache",
                status="PASS" if len(residue) <= GRAPHICS_RESIDUE_MAX else "FAIL",
                reason=""
                if len(residue) <= GRAPHICS_RESIDUE_MAX
                else f"residual paths: {residue[:10]}",
                evidence={
                    "residue": residue,
                    "scan_roots": [
                        str(store.data / "bitty" / "extensions"),
                        str(store.cache / "bitty"),
                    ],
                    "threshold": GRAPHICS_RESIDUE_MAX,
                    "panic": read_last_match(
                        log_path, re.compile(r"panicked at|thread 'main' panicked")
                    ),
                },
            )
        expected = cfg.graphics_expected
        present = "graphics" in names
        if expected is not None:
            status = "PASS" if present == expected else "FAIL"
            reason = (
                ""
                if present == expected
                else f"expected present={expected} actual={present}"
            )
            if expected and gated:
                status = "NOT_EXERCISED"
                reason = "pass --network-extensions to install the pinned artifact"
            self.add_row(
                cfg,
                kind="extension",
                row_id="ext-graphics-state",
                check="graphics component inventory matches the requested state",
                status=status,
                reason=reason,
                evidence={
                    "components": sorted(names),
                    "expected_present": expected,
                    "installs": installs,
                    "citation": GRAPHICS_CITATION,
                },
            )
        if cfg.kitty_image or cfg.bg:
            row_index = {(r["config"], r["id"]): r for r in self.rows}
            kitty = row_index.get((cfg.cid, "image-kitty"))
            background = row_index.get((cfg.cid, "background-rendered"))
            scene_ok = (
                (kitty is None or kitty["status"] == "PASS")
                and (background is None or background["status"] == "PASS")
                and (kitty is not None or background is not None)
            )
            shots: list[str] = []
            if kitty:
                shots.extend(kitty.get("screenshots") or [])
            if background:
                shots.extend(background.get("screenshots") or [])
            self.add_row(
                cfg,
                kind="extension",
                row_id="graphics-scene",
                check="kitty image and background image reach the present path",
                status="PASS" if scene_ok else "FAIL",
                reason=""
                if scene_ok
                else f"kitty={kitty and kitty['status']} "
                f"background={background and background['status']}",
                screenshots=shots,
                evidence={
                    "kitty_row": kitty["status"] if kitty else None,
                    "background_row": background["status"] if background else None,
                    "citation": GRAPHICS_CITATION,
                },
            )

    def record_plugin_without_extension(
        self, cfg: MatrixConfig, log_path: Path, activation: dict
    ) -> None:
        """Each named plugin alone with no native component: degrade, no panic.

        The 0.0.x official set declares no required components, so the row
        asserts the degraded path directly: no extension installed, plugin
        still activates (or refuses with a typed error), no panic, and the
        window stays alive.
        """
        for plugin in self.plugin_set(cfg):
            if plugin.id not in NAMED_PLUGIN_SURFACES and plugin.id != BAR_PLUGIN_ID:
                continue
            if cfg.extensions:
                continue
            _rc, components, _err = self.extension_list(cfg)
            names = sorted({c.get("name") or c.get("id") for c in components})
            act = activation.get(plugin.id, {})
            panic = read_last_match(
                log_path, re.compile(r"panicked at|thread 'main' panicked")
            )
            alive = bool(
                self.current_client
                and running(self.current_client.get("pid") or 0, self.procs)
            )
            status_ok = act.get("status") in {"PASS", "NOT_EXERCISED"}
            ok = status_ok and not panic and alive
            self.add_row(
                cfg,
                kind="plugin",
                row_id=f"no-extension-{sanitize(plugin.slug)}",
                check=(
                    "plugin runs with no native extension installed: "
                    "degrades without panic"
                ),
                status="PASS" if ok else "FAIL",
                reason=""
                if ok
                else (
                    f"activation={act.get('status')} panic={bool(panic)} "
                    f"alive={alive} components={names}"
                ),
                screenshots=[act["probe"]["screenshot"]]
                if (act.get("probe") or {}).get("screenshot")
                else [],
                evidence={
                    "plugin_id": plugin.id,
                    "installed_components": names,
                    "activation": act,
                    "panic": panic,
                    "window_alive": alive,
                    "expectation": "degrade with a typed error or no-op, never panic",
                },
            )

    def record_plugin_surfaces(self, cfg: MatrixConfig, probes: dict) -> None:
        for plugin in self.plugin_set(cfg):
            probe = probes.get(plugin.id)
            if not probe or plugin.id == BAR_PLUGIN_ID:
                continue
            if plugin.id not in {
                PALETTE_PLUGIN_ID,
                COMPOSER_PLUGIN_ID,
                FILE_MANAGER_PLUGIN_ID,
            }:
                continue
            changed = probe.get("changed_px", 0)
            if plugin.id in {PALETTE_PLUGIN_ID, COMPOSER_PLUGIN_ID}:
                status = (
                    "PASS"
                    if probe.get("status") == "PASS" and changed >= OVERLAY_DIFF_MIN_PX
                    else "FAIL"
                )
                reason = (
                    ""
                    if status == "PASS"
                    else f"probe={probe.get('status')} changed_px={changed}"
                )
                check = "command opens a visible overlay surface"
            else:
                # file-manager has no listing when the scope is unavailable;
                # that is an honest NOT_EXERCISED, never a fabricated pass.
                status = (
                    "PASS"
                    if probe.get("status") == "PASS" and changed >= OVERLAY_DIFF_MIN_PX
                    else "NOT_EXERCISED"
                )
                reason = (
                    ""
                    if status == "PASS"
                    else "no visible listing surface (E_SCOPE_UNAVAILABLE path)"
                )
                check = "command paints a visible listing surface"
            self.add_row(
                cfg,
                kind="plugin",
                row_id=f"surface-{sanitize(plugin.slug)}",
                check=check,
                status=status,
                reason=reason,
                screenshots=[probe["screenshot"]] if probe.get("screenshot") else [],
                evidence={
                    "plugin_id": plugin.id,
                    "command": probe.get("command"),
                    "changed_px": changed,
                    "threshold": OVERLAY_DIFF_MIN_PX,
                    "probe": probe,
                },
            )

    def run_statusline_probe(self, cfg: MatrixConfig) -> None:
        manifest = (
            self.catalog.registry_repo / "plugins" / "statusline" / "bitty-plugin.toml"
        )
        compat = None
        if manifest.is_file():
            with manifest.open("rb") as handle:
                compat = tomllib.load(handle).get("compat", {}).get("bitty")
        registered = any(p.id == STATUSLINE_PLUGIN_ID for p in self.catalog.plugins)
        self.add_row(
            cfg,
            kind="plugin",
            row_id="plugin-statusline",
            check="statusline surface availability",
            status="NOT_APPLICABLE",
            reason=STATUSLINE_BLOCKER,
            evidence={
                "registered": registered,
                "checkout_manifest": str(manifest) if manifest.is_file() else None,
                "compat_bitty": compat,
                "citation": DOC_CITATIONS["plugin_registry"],
                "headless": True,
            },
        )

    def run_extension_inventory_probe(self, cfg: MatrixConfig) -> None:
        _rc, components, err = self.extension_list(cfg)
        rows = []
        for extension in self.extensions:
            published = PUBLISHED_EXTENSIONS.get(extension.slug)
            rows.append(
                {
                    "slug": extension.slug,
                    "repo": extension.repo,
                    "published_version": published,
                    "installable": published is not None,
                }
            )
        self.add_row(
            cfg,
            kind="extension",
            row_id="catalog-extensions",
            check="workspace extension roster vs published component artifacts",
            status="PASS" if not err else "FAIL",
            reason="" if not err else err,
            evidence={
                "extensions": rows,
                "installed": components,
                "published": PUBLISHED_EXTENSIONS,
                "citation": DOC_CITATIONS["plugin_registry"],
                "headless": True,
            },
        )

    def run_extension_matrix_probe(self, cfg: MatrixConfig) -> None:
        """One present/absent row pair per official extension (#2413).

        Compiled-in extensions have no absent configuration; published
        components are a real axis whose runtime rows live in the graphics-off
        and net-off configurations.
        """
        _rc, installed, err = self.extension_list(cfg)
        present_names = {c.get("name") or c.get("id") for c in installed}
        row_index = {(r["config"], r["id"]): r for r in self.rows}

        def component_install(component: str) -> dict:
            """Return the install record from the configuration that owns it."""
            base = "graphics-on" if component == "graphics" else "net-on"
            extensions = self.config_results.get(base, {}).get("extensions") or {}
            return {
                "config": base,
                "install": (extensions.get("installs") or {}).get(component, {}),
            }

        for extension in self.extensions:
            slug = extension.slug
            spec = EXTENSION_CHECKS.get(slug, {})
            component_name = COMPONENT_NAMES.get(slug, slug)
            if slug in COMPILED_EXTENSIONS:
                self.add_row(
                    cfg,
                    kind="extension",
                    row_id=f"matrix-{slug}-present",
                    check=f"{slug}: present (compiled into the binary)",
                    status="PASS",
                    evidence={
                        "repo": extension.repo,
                        "linkage": COMPILED_EXTENSIONS[slug],
                        "components": sorted(present_names),
                        "citation": spec.get("check"),
                        "absent_configuration": "not shippable in 0.0.x",
                    },
                )
                self.add_row(
                    cfg,
                    kind="extension",
                    row_id=f"matrix-{slug}-absent",
                    check=f"{slug}: absent",
                    status="NOT_APPLICABLE",
                    reason=(
                        f"compiled into the 0.0.x binary ({COMPILED_EXTENSIONS[slug]}); "
                        "there is no installable artifact and no feature flag for an "
                        "absent configuration"
                    ),
                    evidence={
                        "repo": extension.repo,
                        "linkage": COMPILED_EXTENSIONS[slug],
                    },
                )
            elif component_name in COMPONENT_EXTENSIONS:
                runtime_cfg = (
                    "graphics-off" if component_name == "graphics" else "net-off"
                )
                runtime_row = row_index.get((runtime_cfg, f"ext-{slug}"))
                runtime_state = (
                    (runtime_row["status"], runtime_row.get("reason", ""))
                    if runtime_row
                    else (
                        "NOT_EXERCISED",
                        "runtime absent configuration not in this plan",
                    )
                )
                owner = component_install(component_name)
                install = owner["install"]
                installed = component_name in present_names or install.get("exit") == 0
                if installed:
                    present_status = "PASS"
                    present_reason = ""
                elif install.get("exit") not in (None, 0):
                    present_status = "FAIL"
                    present_reason = (
                        f"install failed in {owner['config']} "
                        f"(exit {install.get('exit')}): "
                        + " ".join(install.get("stderr_tail") or [])[-200:]
                    )
                elif install.get("status") == "NOT_EXERCISED":
                    present_status = "NOT_EXERCISED"
                    present_reason = str(
                        install.get("reason", "artifact not installed")
                    )
                else:
                    present_status = "NOT_EXERCISED"
                    present_reason = (
                        f"present configuration {owner['config']} is not in this plan"
                    )
                self.add_row(
                    cfg,
                    kind="extension",
                    row_id=f"matrix-{slug}-present",
                    check=f"{slug}: present (published native component)",
                    status=present_status,
                    reason=present_reason,
                    evidence={
                        "repo": extension.repo,
                        "published": PUBLISHED_EXTENSIONS.get(component_name),
                        "component_name": component_name,
                        "components_here": sorted(present_names),
                        "install": install,
                        "install_config": owner["config"],
                        "runtime_config": runtime_cfg,
                    },
                )
                self.add_row(
                    cfg,
                    kind="extension",
                    row_id=f"matrix-{slug}-absent",
                    check=f"{slug}: absent (degraded, no panic)",
                    status=runtime_state[0],
                    reason=""
                    if runtime_state[0] == "PASS"
                    else runtime_state[1] or f"see {runtime_cfg} runtime rows",
                    evidence={
                        "repo": extension.repo,
                        "runtime_config": runtime_cfg,
                        "runtime_row": f"ext-{slug}",
                        "components": sorted(present_names),
                    },
                )
            else:
                surface = spec.get("surface", "library-only")
                for state in ("present", "absent"):
                    self.add_row(
                        cfg,
                        kind="extension",
                        row_id=f"matrix-{slug}-{state}",
                        check=f"{slug}: {state} ({surface})",
                        status="NOT_APPLICABLE",
                        reason=spec.get("reason", "no runtime surface in this build"),
                        evidence={"repo": extension.repo, "surface": surface},
                    )
        if err:
            self.add_row(
                cfg,
                kind="extension",
                row_id="matrix-inventory-error",
                check="extension inventory is readable",
                status="FAIL",
                reason=err,
            )

    def run_palette_fail_probe(self, cfg: MatrixConfig) -> None:
        store = self.store_for(cfg)
        bad = CUSTOM_PALETTE | {"background": "#GGGGGG"}
        text = "return {\n  appearance = { colors = " + lua_table(bad, 1) + " },\n}\n"
        write_text(store.bitty_config / "init.lua", text)
        proc = self.headless(
            ["config", "check"],
            store.env(f"{INSTANCE_PREFIX}-palette-fail"),
            f"{sanitize(cfg.cid)}.log",
        )
        ok = proc.returncode == 2 and "appearance.colors.background" in proc.stderr
        self.add_row(
            cfg,
            kind="core",
            row_id="fail-closed",
            check="malformed custom palette fails closed with a field diagnostic",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"exit={proc.returncode} stderr={tail(proc.stderr, 3)}",
            evidence={"exit": proc.returncode, "stderr": proc.stderr, "headless": True},
        )

    def run_inventory_probe(self, cfg: MatrixConfig) -> None:
        ids = [p.id for p in self.catalog.plugins]
        ok = (
            self.catalog.registry_revision != "unknown"
            and len(ids) == len(set(ids))
            and len(ids) == len(list(self.catalog.registry_dir.glob("*.toml")))
        )
        self.add_row(
            cfg,
            kind="core",
            row_id="catalog-registry",
            check="official plugin roster is derived from registry/official",
            status="PASS" if ok else "FAIL",
            reason="" if ok else "registry revision or entry count mismatch",
            evidence={
                "registry_revision": self.catalog.registry_revision,
                "registry_official_dirty": self.catalog.registry_dirty,
                "registry_index_dirty": self.catalog.registry_index_dirty,
                "official_entries": len(ids),
                "plugin_ids": ids,
                "manifest_hash_mismatches": [
                    p.slug for p in self.catalog.plugins if not p.manifest_hash_ok
                ],
                "excluded_checkouts": self.catalog.excluded_checkouts,
                "retired": self.catalog.retired,
                "source_policy": (
                    "git clone --depth=1 --branch main <registry repository>; "
                    "assert HEAD == origin/main at fetch time"
                ),
                "citation": DOC_CITATIONS["plugin_registry"],
                "headless": True,
            },
        )
        for slug, meta in self.catalog.retired.items():
            self.add_row(
                cfg,
                kind="plugin",
                row_id=f"plugin-{slug}",
                check="official plugin set membership",
                status="NOT_APPLICABLE",
                reason=meta["reason"],
                evidence={
                    "retired_checkout": True,
                    "archived_at": meta["archived_at"],
                    "repository": f"https://github.com/bitty-terminal/{slug}",
                    "registry_entry": None,
                    "official_entries": len(ids),
                    "excluded_checkouts": self.catalog.excluded_checkouts,
                    "citation": DOC_CITATIONS["plugin_registry"],
                    "headless": True,
                },
            )

    def run_profile_fail_probe(self, cfg: MatrixConfig) -> None:
        env = self.store_for(cfg).env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        proc = self.headless(
            ["--profile", "ctx1344-missing", "--headless"],
            env,
            f"{sanitize(cfg.cid)}.log",
        )
        ok = proc.returncode == 2 and "not found" in proc.stderr
        self.add_row(
            cfg,
            kind="core",
            row_id="fail-closed",
            check="requested-but-missing profile exits 2 (no fallback)",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"exit={proc.returncode} stderr={tail(proc.stderr, 3)}",
            evidence={
                "exit": proc.returncode,
                "stderr": proc.stderr,
                "citation": DOC_CITATIONS["profile_selection"],
                "headless": True,
            },
        )

    def run_bg_fit_probe(self, cfg: MatrixConfig) -> None:
        self.write_config(cfg)
        proc = self.config_check(cfg)
        detail = proc.stderr.strip()
        ok = proc.returncode == 2 and "window.background_fit" in detail
        self.add_row(
            cfg,
            kind="core",
            row_id="not-applicable",
            check=f"requested background_fit={cfg.bg_requested!r} is not a shipped value",
            status="NOT_APPLICABLE" if ok else "FAIL",
            reason=(
                f"documented enum is {', '.join(DOCUMENTED_BG_FITS)}; "
                "cover/contain are rejected fail-closed"
            ),
            evidence={
                "requested": cfg.bg_requested,
                "exit": proc.returncode,
                "stderr": detail,
                "documented_values": list(DOCUMENTED_BG_FITS),
                "citation": DOC_CITATIONS["background_fit"],
                "substitute_config": "bg-fill"
                if cfg.bg_requested == "cover"
                else "bg-fit",
                "headless": True,
            },
        )

    def run_theme_fail_probe(self, cfg: MatrixConfig) -> None:
        env = self.store_for(cfg).env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        proc = self.headless(
            ["--theme", "ctx1344-not-a-theme", "config", "check"],
            env,
            f"{sanitize(cfg.cid)}.log",
        )
        ok = proc.returncode == 2 and "invalid --theme" in proc.stderr
        self.add_row(
            cfg,
            kind="core",
            row_id="fail-closed",
            check="unknown --theme is a CLI usage error (exit 2)",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"exit={proc.returncode} stderr={tail(proc.stderr, 3)}",
            evidence={
                "exit": proc.returncode,
                "stderr": proc.stderr,
                "citation": DOC_CITATIONS["theme"],
                "headless": True,
            },
        )

    def run_profile_env_probe(self, cfg: MatrixConfig) -> None:
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        if cfg.profile_env:
            env["BITTY_PROFILE"] = cfg.profile_env
        argv = [str(self.binary)]
        if cfg.profile:
            argv += ["--profile", cfg.profile]
        argv += ["config", "check"]
        proc = run_cmd(argv, env=env, timeout=30.0)
        rows = self.parse_config_rows(proc.stdout)
        expected_name = cfg.profile or cfg.profile_env
        expected_source = f"profile: {store.profiles / f'{expected_name}.lua'}"
        check = self.expect(rows, "profile", f'"{expected_name}"', expected_source)
        ok = proc.returncode == 0 and check["ok"]
        self.add_row(
            cfg,
            kind="core",
            row_id="selection",
            check="profile selection: --profile over BITTY_PROFILE over none",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"exit={proc.returncode} profile_row={check['actual']}",
            evidence={
                "expectation": check,
                "cli_profile": cfg.profile,
                "env_profile": cfg.profile_env,
                "citation": DOC_CITATIONS["profile_selection"],
                "headless": True,
            },
        )

    # -- interaction checks ------------------------------------------------

    def run_ipc_checks(self, cfg: MatrixConfig) -> list[dict]:
        rows: list[dict] = []
        store = self.store_for(cfg)
        env = store.env(f"{INSTANCE_PREFIX}-{sanitize(cfg.cid)}")
        instance_proc = run_cmd(
            [str(self.binary), "list", "instances", "--format", "json"],
            env=env,
            timeout=15.0,
        )
        window_rc, _window_payload, window_err = self.ctl(
            cfg,
            ["window", "list", "--format", "json"],
            log=f"{sanitize(cfg.cid)}.ctl-window.json",
        )
        view_rc, view_payload, view_err = self.ctl(
            cfg,
            ["view", "list", "--format", "json"],
            log=f"{sanitize(cfg.cid)}.ctl-view.json",
        )
        workspace_rc, _workspace_payload, workspace_err = self.ctl(
            cfg,
            ["workspace", "list", "--format", "json"],
            log=f"{sanitize(cfg.cid)}.ctl-workspace.json",
        )
        terminal_rc, terminal_payload, terminal_err = self.ctl(
            cfg,
            ["terminal", "list", "--format", "json"],
            log=f"{sanitize(cfg.cid)}.ctl-terminal.json",
        )
        views = (view_payload or {}).get("result", {}).get("views", [])
        focused = next(
            (v for v in views if v.get("focused")), views[0] if views else None
        )
        terminals = (terminal_payload or {}).get("result", {}).get("terminals", [])
        terminal_id = None
        if focused:
            diagnostics = (
                (terminal_payload or {}).get("result", {}).get("diagnostics", [])
            )
            match = next(
                (d for d in diagnostics if d.get("view_id") == focused.get("id")), None
            )
            if match:
                terminal_id = match.get("terminal_id")
        if terminal_id is None and terminals:
            terminal_id = terminals[0].get("id")
        ok = all(rc == 0 for rc in (window_rc, view_rc, workspace_rc, terminal_rc))
        rows.append(
            {
                "kind": "extension",
                "row_id": "ext-ipc",
                "check": "ctl window/view/workspace/terminal list against the live instance",
                "status": "PASS" if ok else "FAIL",
                "reason": ""
                if ok
                else "; ".join(
                    filter(None, [window_err, view_err, workspace_err, terminal_err])
                ),
                "evidence": {
                    "instance_list_exit": instance_proc.returncode,
                    "window_exit": window_rc,
                    "view_exit": view_rc,
                    "workspace_exit": workspace_rc,
                    "terminal_exit": terminal_rc,
                    "views": views,
                    "terminals": terminals,
                    "focused_view": focused,
                    "terminal_id": terminal_id,
                    "screenshot_config": None,
                },
            }
        )

        # terminal send + bounded read-back
        send_ok = False
        transcript = ""
        if terminal_id:
            rc, _, err = self.ctl(
                cfg,
                ["terminal", "send", terminal_id, "printf 'CTX1344_IPC_OK\\n'\n"],
                log=f"{sanitize(cfg.cid)}.ctl-send.txt",
            )
            if rc == 0:
                deadline = time.monotonic() + SHELL_READY_TIMEOUT_S
                while time.monotonic() < deadline:
                    _trc, tpayload, _ = self.ctl(
                        cfg,
                        ["terminal", "text", terminal_id, "--format", "json"],
                    )
                    transcript = (tpayload or {}).get("result", {}).get("text", "")
                    if "CTX1344_IPC_OK" in transcript:
                        send_ok = True
                        break
                    time.sleep(0.25)
        rows.append(
            {
                "kind": "extension",
                "row_id": "ext-ipc-send",
                "check": "ctl terminal send reaches the focused PTY and is readable",
                "status": "PASS" if send_ok else "FAIL",
                "reason": "" if send_ok else "marker not observed in terminal text",
                "evidence": {
                    "terminal_id": terminal_id,
                    "transcript_tail": tail(transcript, 5),
                },
            }
        )

        # split
        rc, _payload, err = self.ctl(
            cfg,
            ["view", "split", "--right"],
            log=f"{sanitize(cfg.cid)}.ctl-split.txt",
        )
        view_rc, view_payload, _ = self.ctl(
            cfg,
            ["view", "list", "--format", "json"],
            log=f"{sanitize(cfg.cid)}.ctl-view-split.json",
        )
        views_after = (view_payload or {}).get("result", {}).get("views", [])
        split_ok = rc == 0 and len(views_after) == 2
        split_shot = (
            self.screenshot(cfg, "ipc-split", window=True) if split_ok else None
        )
        rows.append(
            {
                "kind": "extension",
                "row_id": "ext-ipc-split",
                "check": "ctl view split creates a second view",
                "status": "PASS" if split_ok else "FAIL",
                "reason": ""
                if split_ok
                else f"exit={rc} views={len(views_after)} err={err}",
                "screenshots": [split_shot] if split_shot else [],
                "evidence": {"views_after": views_after, "exit": rc, "stderr": err},
            }
        )
        return rows

    def run_plugin_probes(self, cfg: MatrixConfig) -> dict:
        out: dict = {}
        assert self.current_client is not None
        for plugin in self.plugin_set(cfg):
            command = self.catalog.probe_command(plugin)
            if not command:
                out[plugin.id] = {
                    "status": "NOT_EXERCISED",
                    "reason": "manifest declares no lazy commands",
                }
                continue
            chord = self.chord_for(cfg, command)
            if not chord:
                out[plugin.id] = {
                    "status": "NOT_EXERCISED",
                    "reason": "no free probe chord",
                }
                continue
            before = log_line_count(self.logs / f"{sanitize(cfg.cid)}.app.log")
            if self.current_client:
                focus_window(self.current_client["address"])
            before_shot = self.screenshot(
                cfg, f"plugin-{plugin.slug}-before", window=True
            )
            press_chord(chord)
            # A dispatch either logs `-> invoked` or `refused (...)`; both
            # end the probe and the refusal text is the named error class.
            line = self.wait_log(
                self.logs / f"{sanitize(cfg.cid)}.app.log",
                rf"keymap command:{re.escape(command)} (?:->|refused)",
            )
            time.sleep(0.5)
            shot = self.screenshot(cfg, f"plugin-{plugin.slug}", window=True)
            changed = (
                pixel_diff(Path(before_shot), Path(shot)) if before_shot and shot else 0
            )
            release_focus()
            if line and "-> invoked" in line:
                status = "PASS"
                reason = ""
            elif line and "args do not satisfy the command schema" in line:
                status = "NOT_EXERCISED"
                reason = (
                    "command requires arguments; keymap dispatch cannot supply them"
                )
            else:
                status = "FAIL"
                reason = (
                    line
                    if line
                    else f"no dispatch line for {chord} (swallowed or unbound)"
                )
            out[plugin.id] = {
                "status": status,
                "reason": reason,
                "command": command,
                "chord": chord,
                "log_line": line,
                "lines_after": log_line_count(
                    self.logs / f"{sanitize(cfg.cid)}.app.log"
                )
                - before,
                "screenshot": shot,
                "before_screenshot": before_shot,
                "changed_px": changed,
            }
        # Negative probe: unknown command must be refused with a named detail.
        probe_chord = next(
            (
                chord
                for chord, action in self.probe_keymaps(cfg)
                if action == "command:ctx1344.absent:probe"
            ),
            None,
        )
        if probe_chord:
            if self.current_client:
                focus_window(self.current_client["address"])
            press_chord(probe_chord)
            line = self.wait_log(
                self.logs / f"{sanitize(cfg.cid)}.app.log",
                r"warning: keymap command:ctx1344\.absent:probe refused \((?P<detail>[^)]*)\)",
            )
            shot = self.screenshot(cfg, "plugin-failure-path", window=True)
            release_focus()
            match = FAILURE_PATH_PATTERN.search(line or "")
            out["failure"] = {
                "status": "PASS" if match else "FAIL",
                "chord": probe_chord,
                "log_line": line,
                "error_class": match.group("detail") if match else None,
                "screenshot": shot,
                "reason": "" if match else "no named refusal line observed",
            }
        return out

    def plugin_activation(
        self, cfg: MatrixConfig, log_path: Path, probes: dict
    ) -> dict:
        text = log_path.read_text(encoding="utf-8", errors="replace")
        active = {}
        failed = {}
        for match in re.finditer(
            r"^bitty: plugin '([^']+)' active \((.*)\)$", text, re.MULTILINE
        ):
            active[match.group(1)] = match.group(2)
        for match in re.finditer(
            r"^bitty: plugin '([^']+)' failed: (.*)$", text, re.MULTILINE
        ):
            failed[match.group(1)] = match.group(2)
        out = {}
        for plugin in self.plugin_set(cfg):
            if plugin.id in failed:
                out[plugin.id] = {
                    "status": "FAIL",
                    "reason": failed[plugin.id],
                }
            elif plugin.id in active:
                probe = probes.get(plugin.id)
                entry = {
                    "status": "PASS",
                    "reason": "",
                    "detail": active[plugin.id],
                }
                if probe:
                    entry["probe"] = probe
                    if probe.get("status") == "FAIL":
                        entry["status"] = "FAIL"
                        entry["reason"] = (
                            probe.get("reason")
                            or probe.get("log_line")
                            or "command probe failed"
                        )
                out[plugin.id] = entry
            else:
                out[plugin.id] = {
                    "status": "FAIL",
                    "reason": "no activation or failure line in the app log",
                }
        return out

    def run_extension_rows(
        self,
        cfg: MatrixConfig,
        log_path: Path,
        ipc_rows: list[dict],
        launch_shot: str | None,
        storage_shot: str | None,
    ) -> None:
        _exit, installed, list_err = self.extension_list(cfg)
        log_text = log_path.read_text(encoding="utf-8", errors="replace")
        row_index = {(r["config"], r["id"]): r for r in self.rows}
        for extension in self.extensions:
            if extension.slug == "ipc":
                # Covered by the three ext-ipc* rows from run_ipc_checks.
                continue
            spec = EXTENSION_CHECKS.get(extension.slug, {})
            component_name = COMPONENT_NAMES.get(extension.slug, extension.slug)
            component = next(
                (
                    c
                    for c in installed
                    if c.get("id") == component_name or c.get("name") == component_name
                ),
                None,
            )
            if extension.slug == "storage":
                saved = bool(read_last_match(log_path, re.compile(r"session saved")))
                status = "PASS" if saved else "FAIL"
                reason = "" if saved else "no session snapshot on close"
                status_reason = STORAGE_CITATION
            elif extension.slug == "url-detector":
                hover = row_index.get((cfg.cid, "url-hover"))
                if hover is None:
                    status = "NOT_APPLICABLE"
                    reason = (
                        "no installable component artifact; hover row not run in "
                        "this configuration"
                    )
                else:
                    status = "PASS" if hover["status"] == "PASS" else "NOT_EXERCISED"
                    reason = (
                        ""
                        if status == "PASS"
                        else "Ctrl+hover delta not observed; see url-hover row"
                    )
                status_reason = URL_DETECTOR_CITATION
            elif extension.slug == "graphics":
                gfx_state = row_index.get((cfg.cid, "ext-graphics-state"))
                gfx_removed = row_index.get((cfg.cid, "ext-graphics-removal"))
                if gfx_removed is not None:
                    status = gfx_removed["status"]
                    reason = gfx_removed.get("reason", "")
                elif gfx_state is not None:
                    status = gfx_state["status"]
                    reason = gfx_state.get("reason", "")
                elif component is not None:
                    status = "PASS"
                    reason = ""
                else:
                    status = "NOT_APPLICABLE"
                    reason = "not installed in this configuration; see graphics-off"
                status_reason = GRAPHICS_CITATION
            elif extension.slug == "network":
                net = row_index.get((cfg.cid, "ext-net-broker"))
                if net is not None:
                    status = net["status"]
                    reason = net.get("reason", "")
                elif component is not None:
                    status = "PASS"
                    reason = ""
                else:
                    status = "NOT_APPLICABLE"
                    reason = "not installed in this configuration; see net-off"
                status_reason = NET_CITATION
            elif spec.get("surface") == "library-only":
                status = "NOT_APPLICABLE"
                reason = spec.get("reason", "no runtime surface")
                status_reason = None
            else:
                status = "NOT_EXERCISED"
                reason = spec.get("reason", "check not implemented")
                status_reason = None
            self.add_row(
                cfg,
                kind="extension",
                row_id=f"ext-{extension.slug}",
                check=spec.get("check", extension.role),
                status=status,
                reason=reason,
                screenshots=(
                    [storage_shot]
                    if extension.slug == "storage" and storage_shot
                    else ([launch_shot] if launch_shot else [])
                ),
                evidence={
                    "repo": extension.repo,
                    "role": extension.role,
                    "installed_component": component,
                    "extension_list_exit": _exit,
                    "extension_list_error": list_err,
                    "screenshot_config": launch_shot,
                    "citation": status_reason,
                    "log_evidence": {
                        "session_saved": bool(
                            read_last_match(log_path, re.compile(r"session saved"))
                        ),
                        "lines": len(log_text.splitlines()),
                    },
                },
            )

    def run_kitty_check(self, cfg: MatrixConfig) -> None:
        assert self.crop_png and self.send_image_script
        assert self.current_client is not None
        before = self.screenshot(cfg, "kitty-before", window=True)
        # Focus the first view so the send targets the visible shell.
        self.ctl(cfg, ["view", "focus", "v:1"])
        _rc, payload, _err = self.ctl(
            cfg,
            ["terminal", "list", "--format", "json"],
        )
        terminals = (payload or {}).get("result", {}).get("terminals", [])
        diagnostics = (payload or {}).get("result", {}).get("diagnostics", [])
        attached = next((d for d in diagnostics if d.get("view_id") == "v:1"), None)
        terminal_id = (
            attached.get("terminal_id")
            if attached
            else (terminals[0]["id"] if terminals else "t:1")
        )
        command = f"python3 {self.send_image_script} {self.crop_png}"
        send_rc, _, send_err = self.ctl(
            cfg,
            ["terminal", "send", terminal_id, command + "\n"],
            log=f"{sanitize(cfg.cid)}.ctl-kitty-send.txt",
        )
        changed = 0
        deadline = time.monotonic() + SHELL_READY_TIMEOUT_S
        after = None
        while time.monotonic() < deadline:
            time.sleep(0.4)
            after = self.screenshot(cfg, "kitty-after", window=True)
            if after and before:
                changed = pixel_diff(before, after)
                if changed >= KITTY_DIFF_MIN_PX:
                    break
        ok = send_rc == 0 and after is not None and changed >= KITTY_DIFF_MIN_PX
        self.add_row(
            cfg,
            kind="core",
            row_id="image-kitty",
            check="kitty graphics protocol displays a PNG crop from the wallpaper",
            status="PASS" if ok else "FAIL",
            reason=""
            if ok
            else f"send_exit={send_rc} changed_px={changed} err={send_err}",
            screenshots=[s for s in (before, after) if s],
            evidence={
                "png": str(self.crop_png),
                "png_sha256": sha256_file(self.crop_png),
                "wallpaper": str(self.selected_wallpaper),
                "crop_box": list(self.crop_box),
                "terminal_id": terminal_id,
                "changed_px": changed,
                "threshold_px": KITTY_DIFF_MIN_PX,
                "send_exit": send_rc,
                "citation": DOC_CITATIONS["kitty"],
            },
        )

    # -- orchestration -----------------------------------------------------

    def record_cross_config_rows(self, configs: list[MatrixConfig]) -> None:
        """Coverage rows that need more than one configuration.

        These rows are the audit surface for the owner matrix: they compare
        configurations with each other and assert that every requested
        dimension produced a row, so a missing axis cannot hide behind a
        green single-configuration run.
        """
        cids = {cfg.cid for cfg in configs}
        rows = {(r["config"], r["id"]): r for r in self.rows}
        full_plan = getattr(self.args, "plan", "") == "expanded"

        def subset_note(what: str) -> str:
            return (
                f"subset plan ({getattr(self.args, 'plan', 'unknown')}) does not "
                f"assert {what}; the expanded campaign does"
            )

        # Profile visible delta: the settings-heavy profile must change pixels.
        pairs = [
            ("profile-minimal", "profile-settings"),
            ("profile-default", "profile-settings"),
        ]
        compared = [(a, b) for a, b in pairs if a in cids and b in cids]
        for left, right in compared:
            left_shot = self.shots / f"{sanitize(left)}-launch-window.png"
            right_shot = self.shots / f"{sanitize(right)}-launch-window.png"
            if not (left_shot.is_file() and right_shot.is_file()):
                self.add_row(
                    self.plan_row_cfg("profile-visible-delta"),
                    kind="core",
                    row_id=f"delta-{left}-{right}",
                    check=f"{right} renders visibly differently from {left}",
                    status="NOT_EXERCISED",
                    reason="launch screenshots missing for the comparison",
                )
                continue
            changed = pixel_diff(left_shot, right_shot)
            ok = changed >= PROFILE_DELTA_MIN_PX
            self.add_row(
                self.plan_row_cfg("profile-visible-delta"),
                kind="core",
                row_id=f"delta-{left}-{right}",
                check=f"{right} renders visibly differently from {left}",
                status="PASS" if ok else "FAIL",
                reason="" if ok else f"changed_px={changed} < {PROFILE_DELTA_MIN_PX}",
                screenshots=[str(left_shot), str(right_shot)],
                evidence={
                    "left": left,
                    "right": right,
                    "changed_px": changed,
                    "threshold": PROFILE_DELTA_MIN_PX,
                    "citation": DOC_CITATIONS["profile_selection"],
                },
            )

        # Theme coverage: every shipped preset plus the custom palette.
        covered = {
            cfg.theme_file
            for cfg in configs
            if cfg.cid.startswith("theme-") and cfg.theme_file and not cfg.spot_check
        }
        if covered:
            shipped = {t["name"] for t in self.theme_catalog}
            missing = sorted(shipped - covered)
            extra = sorted(covered - shipped)
            families = {
                t["category"] for t in self.theme_catalog if t["name"] in covered
            }
            pixel_rows = [
                r
                for (cid, rid), r in rows.items()
                if rid == "theme-pixels" and cid in cids
            ]
            pixel_fail = [r["config"] for r in pixel_rows if r["status"] == "FAIL"]
            ok = (
                len(shipped) >= THEME_TOTAL_MIN
                and not missing
                and not extra
                and {"light", "dark"} <= families
                and not pixel_fail
            )
            self.add_row(
                self.plan_row_cfg("theme-coverage"),
                kind="core",
                row_id="theme-coverage",
                check=(
                    f"all shipped presets ({THEME_TOTAL_MIN}+) and one custom palette "
                    "rendered in light and dark families"
                ),
                status="PASS" if ok else "NOT_EXERCISED" if not full_plan else "FAIL",
                reason=""
                if ok
                else subset_note("the full preset set")
                if not full_plan
                else (
                    f"shipped={len(shipped)} covered={len(covered)} "
                    f"missing={missing[:5]} extra={extra[:5]} families={sorted(families)} "
                    f"pixel_fail={pixel_fail[:5]}"
                ),
                evidence={
                    "shipped": len(shipped),
                    "covered": sorted(covered),
                    "missing": missing,
                    "families": sorted(families),
                    "pixel_fail": pixel_fail,
                    "custom_palette": "theme-custom-palette" in cids,
                },
            )

        # Bar contrast at both edges and both families.
        contrast = [
            r for (cid, rid), r in rows.items() if rid == "bar-contrast" and cid in cids
        ]
        if contrast:
            combos = {
                (r["evidence"].get("edge"), r["evidence"].get("theme_family"))
                for r in contrast
            }
            wanted = {
                ("top", "dark"),
                ("bottom", "dark"),
                ("top", "light"),
                ("bottom", "light"),
            }
            failing = [r["config"] for r in contrast if r["status"] != "PASS"]
            ok = wanted <= combos and not failing
            self.add_row(
                self.plan_row_cfg("bar-contrast-matrix"),
                kind="bar",
                row_id="bar-contrast-matrix",
                check="bar contrast passes at top and bottom in light and dark families",
                status="PASS" if ok else "NOT_EXERCISED" if not full_plan else "FAIL",
                reason=""
                if ok
                else subset_note("every (edge, family) combination")
                if not full_plan
                else f"missing={sorted(wanted - combos)} failing={failing[:5]}",
                evidence={
                    "combos": sorted(map(str, combos)),
                    "wanted": sorted(map(str, wanted)),
                },
            )

        # Extension present/absent coverage.
        matrix_rows = [
            r
            for (cid, rid), r in rows.items()
            if rid.startswith("matrix-") and cid.startswith("probe-")
        ]
        if matrix_rows:
            slugs = {e.slug for e in self.extensions}
            present = {
                r["id"].removeprefix("matrix-").removesuffix("-present")
                for r in matrix_rows
                if r["id"].endswith("-present")
            }
            absent = {
                r["id"].removeprefix("matrix-").removesuffix("-absent")
                for r in matrix_rows
                if r["id"].endswith("-absent")
            }
            missing = sorted(slugs - present) + sorted(slugs - absent)
            self.add_row(
                self.plan_row_cfg("extension-coverage"),
                kind="extension",
                row_id="extension-coverage",
                check="every official extension has present and absent rows",
                status="PASS" if not missing else "FAIL",
                reason=""
                if not missing
                else f"missing rows for: {sorted(set(missing))}",
                evidence={
                    "extensions": sorted(slugs),
                    "present_rows": sorted(present),
                    "absent_rows": sorted(absent),
                },
            )

        # Session coverage: fresh and restored where state persists.
        fresh = [
            r
            for (cid, rid), r in rows.items()
            if rid == "session-fresh" and cid in cids
        ]
        restored = [
            r
            for (cid, rid), r in rows.items()
            if rid == "session-restored" and cid in cids
        ]
        if fresh or restored:
            failing = [
                r["config"] for r in [*fresh, *restored] if r["status"] != "PASS"
            ]
            self.add_row(
                self.plan_row_cfg("session-coverage"),
                kind="core",
                row_id="session-coverage",
                check="fresh sessions start clean and restored sessions reload layout",
                status="PASS" if fresh and restored and not failing else "FAIL",
                reason=""
                if fresh and restored and not failing
                else f"fresh={len(fresh)} restored={len(restored)} failing={failing[:5]}",
                evidence={
                    "fresh_configs": [r["config"] for r in fresh],
                    "restored_configs": [r["config"] for r in restored],
                },
            )

        # Plugin surface coverage for the named plugins.
        if any(cfg.plugin_id for cfg in configs):
            covered_plugins = {
                (r["evidence"].get("plugin_id") or "")
                for (cid, rid), r in rows.items()
                if rid.startswith("plugin-") and cid in cids
            }
            named = set(NAMED_PLUGIN_SURFACES) | {BAR_PLUGIN_ID}
            missing = sorted(named - covered_plugins)
            ok = not missing
            self.add_row(
                self.plan_row_cfg("plugin-surface-coverage"),
                kind="plugin",
                row_id="plugin-surface-coverage",
                check="named plugins (bar/palette/composer/statusline/file-manager) covered",
                status="PASS" if ok else "NOT_EXERCISED" if not full_plan else "FAIL",
                reason=""
                if ok
                else subset_note("every named plugin surface")
                if not full_plan
                else f"no row for: {missing}",
                evidence={
                    "named": sorted(named),
                    "covered": sorted(p for p in covered_plugins if p),
                },
            )

        # Graphics coverage for the installed/absent/removed triangle.
        if {"graphics-on", "graphics-off"} <= cids:
            removed = "graphics-removed" in cids
            state_rows = [
                r for (cid, rid), r in rows.items() if rid == "ext-graphics-state"
            ]
            residue = [
                r for (cid, rid), r in rows.items() if rid == "ext-graphics-residue"
            ]
            ok = all(r["status"] in {"PASS", "NOT_EXERCISED"} for r in state_rows) and (
                removed and residue and all(r["status"] == "PASS" for r in residue)
            )
            self.add_row(
                self.plan_row_cfg("graphics-coverage"),
                kind="extension",
                row_id="graphics-coverage",
                check="graphics installed/absent/removed paths all produce rows",
                status="PASS" if ok else "FAIL",
                reason=""
                if ok
                else (
                    f"states={[r['status'] for r in state_rows]} "
                    f"removed={removed} residue={[r['status'] for r in residue]}"
                ),
                evidence={
                    "state_rows": len(state_rows),
                    "residue_rows": len(residue),
                    "removed_config_present": removed,
                },
            )

    def plan_row_cfg(self, cid: str) -> MatrixConfig:
        """Synthetic config used by coverage rows (never launched)."""
        return MatrixConfig(cid=cid, axes={"note": "cross-configuration coverage row"})

    def run(self, configs: list[MatrixConfig], probes: list[MatrixConfig]) -> None:
        self.ensure_layout()
        self.desktop_before = desktop_state()
        write_json(self.out / "desktop-before.json", self.desktop_before)
        try:
            self.prepare()
            if self.args.require_clean_sources:
                unpinned = [p.slug for p in self.catalog.plugins if not p.source_ok]
                if unpinned:
                    raise SystemExit(
                        "refusing unpinned plugin sources: " + ", ".join(unpinned)
                    )
            self.write_profiles()
            # Pre-install each store's plugins once (cached per store).
            for cfg in configs:
                if cfg.gui and not cfg.fail_closed:
                    self.install_plugins(cfg)
            for cfg in [*configs, *probes]:
                print(f"[matrix] {cfg.cid}", flush=True)
                try:
                    self.run_config(cfg)
                # A harness defect is recorded and the campaign continues; a
                # product failure never reaches this handler.
                except Exception as exc:  # noqa: BLE001
                    self.errors.append(f"{cfg.cid}: {exc!r}")
                    self.add_row(
                        cfg,
                        kind="core",
                        row_id="harness-error",
                        check="harness execution",
                        status="BLOCKED",
                        reason=repr(exc),
                    )
                self.flush()
            self.record_cross_config_rows(configs)
            self.flush()
        finally:
            self.restore()

    def flush(self) -> None:
        write_json(
            self.out / "verification.json",
            {
                "schema_version": SCHEMA_VERSION,
                "harness": HARNESS_VERSION,
                "task": {"id": "CTX-2413", "issue": 2413, "date": self.started_at[:10]},
                "plan": getattr(self.args, "plan", "unknown"),
                "candidate": {
                    "binary": str(self.binary),
                    "binary_sha256": self.binary_sha256(),
                    "provenance": str(self.out / "provenance.txt"),
                },
                "environment": {
                    "xdg_roots": str(self.xdg_root),
                    "workspace": CAMPAIGN_WORKSPACE,
                    "clipboard_used": False,
                    "network_installs": self.network_installs,
                    "source_edits": 0,
                    "bar_anchors": self.bar_anchors,
                },
                "desktop_before": self.desktop_before,
                "desktop_after": self.desktop_after,
                "pids": self.pids,
                "configs": self.config_results,
                "rows": self.rows,
                "harness_errors": self.errors,
            },
        )
        write_json(self.out / "pids.json", self.pids)

    def restore(self) -> None:
        self.stop_component_server()
        # Close any window this harness left behind (recorded PIDs only).
        try:
            clients = hypr_json("clients")
        except RuntimeError:
            clients = []
        recorded = {entry["pid"] for entry in self.pids}
        for entry in list(self.pids):
            if entry["exit"] is None and not running(entry["pid"], self.procs):
                self.record_exit(entry["pid"], "exited")
            if entry["exit"] is None:
                client = next(
                    (c for c in clients if c.get("pid") == entry["pid"]), None
                )
                self.stop_instance(entry["pid"], client)
        # Never touch foreign clients; only verify the campaign workspace.
        try:
            leftover = [
                c
                for c in hypr_json("clients")
                if (c.get("workspace") or {}).get("id") == CAMPAIGN_WORKSPACE
                and c.get("pid") in recorded
            ]
        except RuntimeError:
            leftover = []
        restore_info = restore_desktop(self.desktop_before)
        self.desktop_after = {
            **restore_info["after"],
            "leftover_campaign_windows": leftover,
            "restore": restore_info,
        }
        write_json(self.out / "desktop-after.json", self.desktop_after)
        self.flush()


# ---------------------------------------------------------------------------
# Image helpers
# ---------------------------------------------------------------------------


def hex_to_rgb(value: str) -> tuple[int, int, int]:
    value = value.lstrip("#")
    return (int(value[0:2], 16), int(value[2:4], 16), int(value[4:6], 16))


def dominant_color(path: Path) -> tuple[int, int, int]:
    from PIL import Image

    with Image.open(path) as image:
        colors = image.convert("RGB").getcolors(maxcolors=1 << 24) or []
    colors.sort(reverse=True)
    return colors[0][1]


def first_paint_ok(path: Path) -> dict:
    from PIL import Image

    with Image.open(path) as image:
        rgb = image.convert("RGB")
        colors = rgb.getcolors(maxcolors=1 << 24) or []
    count = len(colors)
    return {
        "status": "PASS" if count >= MIN_DISTINCT_COLORS else "FAIL",
        "distinct_colors": count,
        "reason": "" if count >= MIN_DISTINCT_COLORS else "window appears blank",
    }


def _non_bg(raw: bytes, bg: tuple[int, int, int]) -> tuple[int, int]:
    total = len(raw) // 3
    hits = 0
    for index in range(0, len(raw), 3):
        if (
            abs(raw[index] - bg[0]) <= 6
            and abs(raw[index + 1] - bg[1]) <= 6
            and abs(raw[index + 2] - bg[2]) <= 6
        ):
            hits += 1
    return total, total - hits


def background_coverage(path: Path, bg: tuple[int, int, int]) -> dict:
    from PIL import Image

    with Image.open(path) as image:
        rgb = image.convert("RGB")
        width, height = rgb.size
        total, non_bg = _non_bg(rgb.tobytes(), bg)
        # Center half: the fit/contain letterbox bars live at the edges.
        center = rgb.crop(
            (width // 4, height // 4, (width * 3) // 4, (height * 3) // 4)
        )
        center_total, center_non_bg = _non_bg(center.tobytes(), bg)
    return {
        "coverage": non_bg / total if total else 0.0,
        "center_coverage": center_non_bg / center_total if center_total else 0.0,
    }


def pixel_diff(before: Path, after: Path) -> int:
    from PIL import Image, ImageChops

    with Image.open(before) as a, Image.open(after) as b:
        diff = ImageChops.difference(a.convert("RGB"), b.convert("RGB"))
        colors = diff.getcolors(maxcolors=1 << 24) or []
    return sum(count for count, color in colors if color != (0, 0, 0))


def distinct_color_count(
    path: Path, bg: tuple[int, int, int], minimum: int = 40
) -> int:
    """Count colors that differ from the theme background and cover >= minimum px.

    ANSI text is anti-aliased, so a raw distinct-color count overstates real
    color slots; only colors with at least ``minimum`` pixels count. The outer
    12 px are cropped so the focused-view outline and window border cannot
    inflate the count.
    """
    from PIL import Image

    with Image.open(path) as image:
        rgb = image.convert("RGB")
        width, height = rgb.size
        inset = 12
        if width > 3 * inset and height > 3 * inset:
            rgb = rgb.crop((inset, inset, width - inset, height - inset))
        colors = rgb.getcolors(maxcolors=1 << 24) or []
    total = 0
    for count, value in colors:
        if count < minimum:
            continue
        if all(abs(value[i] - bg[i]) <= 6 for i in range(3)):
            continue
        total += 1
    return total


def image_pixels(image) -> list:
    """Pillow 12 deprecates getdata; use the flattened accessor when present."""
    getter = getattr(image, "get_flattened_data", None)
    return list(getter()) if getter is not None else list(image.getdata())


def band_stats(path: Path, edge_rows: int = 64) -> dict:
    """Count non-background pixels in the top/bottom edge bands."""
    from PIL import Image

    with Image.open(path) as image:
        rgb = image.convert("RGB")
        width, height = rgb.size
        pixels = image_pixels(rgb)
        colors = rgb.getcolors(maxcolors=1 << 24) or []
        background = max(colors, key=lambda item: item[0])[1] if colors else (0, 0, 0)
        stats = {}
        for edge in ("top", "bottom"):
            rows = (
                range(min(edge_rows, height))
                if edge == "top"
                else range(max(0, height - edge_rows), height)
            )
            count = 0
            first = None
            last = None
            for y in rows:
                row = pixels[y * width : (y + 1) * width]
                non_bg = sum(
                    1
                    for px in row
                    if max(abs(px[i] - background[i]) for i in range(3)) > 3
                )
                count += non_bg
                if non_bg > max(2, width // 200):
                    first = y if first is None else first
                    last = y
            stats[edge] = count
            stats[f"{edge}_row"] = [first, last] if first is not None else None
        stats["background"] = list(background)
        stats["size"] = [width, height]
        return stats


def band_profile(path: Path, edge: str, edge_rows: int = 64) -> dict:
    """Positional band analysis that does not depend on OCR language data.

    The bar's active workspace pill paints a solid, saturated block of one
    color. The detector picks the most frequent saturated color cluster in the
    band (theme-agnostic: dark and light presets both work) instead of a
    hardcoded accent hue, and keeps the blue rule as an additional candidate.
    """
    from PIL import Image

    with Image.open(path) as image:
        rgb = image.convert("RGB")
        width, height = rgb.size
        y0, y1 = (
            (0, min(edge_rows, height))
            if edge == "top"
            else (max(0, height - edge_rows), height)
        )
        crop = rgb.crop((0, y0, width, y1))
        pixels = image_pixels(crop)
        colors = rgb.getcolors(maxcolors=1 << 24) or []
        background = max(colors, key=lambda item: item[0])[1] if colors else (0, 0, 0)
        third = max(1, width // 3)
        thirds = {"left": 0, "center": 0, "right": 0}
        accent = 0
        accent_box = [width, height, 0, 0]
        boxes: dict[tuple[int, int, int], list[int]] = {}
        counts: dict[tuple[int, int, int], int] = {}
        for index, px in enumerate(pixels):
            x = index % width
            y = index // width + y0
            # Exclude the focused-view outline (a 1-2 px accent border that
            # runs the full window and would otherwise dominate the band).
            if x < 12 or x >= width - 12 or y < y0 + 20 or y >= y1 - 20:
                continue
            distance = max(abs(px[i] - background[i]) for i in range(3))
            if distance > 3:
                key = "left" if x < third else "center" if x < 2 * third else "right"
                thirds[key] += 1
            if distance <= 3:
                continue
            color = (px[0], px[1], px[2])
            counts[color] = counts.get(color, 0) + 1
            box = boxes.get(color)
            if box is None:
                boxes[color] = [x, y, x, y]
            else:
                box[0] = min(box[0], x)
                box[1] = min(box[1], y)
                box[2] = max(box[2], x)
                box[3] = max(box[3], y)
        candidates: list[tuple[int, list[int]]] = []
        for color, count in counts.items():
            if count < 30:
                continue
            box = boxes[color]
            box_width = box[2] - box[0] + 1
            box_height = box[3] - box[1] + 1
            if not (10 <= box_width <= 400 and 5 <= box_height <= 80):
                continue
            saturation = max(color) - min(color)
            blue_rule = (
                color[2] > 150 and color[2] > color[0] + 40 and color[2] > color[1] + 20
            )
            if saturation >= 40 or blue_rule:
                candidates.append((count, box))
        if candidates:
            candidates.sort(key=lambda item: item[0], reverse=True)
            accent, accent_box = candidates[0]
        return {
            "edge": edge,
            "thirds": thirds,
            "accent": accent,
            "accent_box": accent_box if accent else None,
            "background": list(background),
            "size": [width, height],
        }


_OCR_LANG: bool | None = None


def ocr_language_available() -> bool:
    """True when tesseract can load English traineddata."""
    global _OCR_LANG
    if _OCR_LANG is None:
        proc = run_cmd(["tesseract", "--list-langs"], timeout=10.0)
        _OCR_LANG = proc.returncode == 0 and "eng" in proc.stdout
    return _OCR_LANG


def ocr_band(path: Path, edge: str, half: str | None = None) -> str:
    """OCR one edge band; return an empty string when tesseract is unavailable."""
    from PIL import Image, ImageOps

    if not shutil.which("tesseract") or not ocr_language_available():
        return ""
    height_cut = 72
    with Image.open(path) as image:
        rgb = image.convert("RGB")
        width, height = rgb.size
        box = (
            (0, 0, width, min(height_cut, height))
            if edge == "top"
            else (0, max(0, height - height_cut), width, height)
        )
        crop = rgb.crop(box)
        if half == "left":
            crop = crop.crop((0, 0, crop.width // 2, crop.height))
        elif half == "right":
            crop = crop.crop((crop.width // 2, 0, crop.width, crop.height))
        gray = ImageOps.grayscale(crop)
        if sum(image_pixels(gray)) / max(1, gray.width * gray.height) < 128:
            gray = ImageOps.invert(gray)
        gray = gray.resize((gray.width * 3, gray.height * 3), Image.LANCZOS)
        target = path.with_name(f"{path.stem}.{edge}.{half or 'full'}.ocr.png")
        gray.save(target)
    proc = run_cmd(["tesseract", str(target), "stdout", "--psm", "7"], timeout=20.0)
    text = (proc.stdout or "").strip().replace("\n", " ")
    return text if proc.returncode == 0 else ""


def cursor_position() -> tuple[int, int] | None:
    proc = run_cmd(["hyprctl", "cursorpos"], timeout=5.0)
    match = re.match(r"\s*(-?\d+)\s*,\s*(-?\d+)", proc.stdout or "")
    return (int(match.group(1)), int(match.group(2))) if match else None


def pointer_move(x: int, y: int) -> None:
    # ydotool's absolute device maps roughly 2 logical pixels per unit on this
    # host, and it overwrites a compositor warp with its own last position.
    # Drive it from measured cursor feedback instead of assuming a fixed map.
    target = (int(x), int(y))
    candidate = [max(0, target[0] // 2), max(0, target[1] // 2)]
    for _ in range(5):
        proc = run_cmd(
            [
                "ydotool",
                "mousemove",
                "-a",
                "-x",
                str(candidate[0]),
                "-y",
                str(candidate[1]),
            ],
            timeout=5.0,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"pointer move failed: {proc.stderr.strip()}")
        actual = cursor_position()
        if actual is None:
            return
        dx = target[0] - actual[0]
        dy = target[1] - actual[1]
        if abs(dx) <= 2 and abs(dy) <= 2:
            return
        candidate[0] = max(0, candidate[0] + dx // 2)
        candidate[1] = max(0, candidate[1] + dy // 2)


def pointer_click(x: int, y: int, button: str = "0xC0") -> None:
    pointer_move(x, y)
    proc = run_cmd(["ydotool", "click", button], timeout=5.0)
    if proc.returncode != 0:
        raise RuntimeError(f"ydotool click failed: {proc.stderr.strip()}")


def pointer_wheel(x: int, y: int, delta: int) -> None:
    pointer_move(x, y)
    proc = run_cmd(
        ["ydotool", "mousemove", "-w", "-x", "0", "-y", str(delta)], timeout=5.0
    )
    if proc.returncode != 0:
        raise RuntimeError(f"ydotool wheel failed: {proc.stderr.strip()}")


def pointer_key(name: str, down: bool) -> None:
    codes = {"ctrl": 29, "shift": 42, "alt": 56}
    code = codes.get(name)
    if code is None:
        return
    proc = run_cmd(["ydotool", "key", f"{code}:{1 if down else 0}"], timeout=5.0)
    if proc.returncode != 0:
        raise RuntimeError(f"ydotool key failed: {proc.stderr.strip()}")


def clear_session(path: Path) -> None:
    for candidate in (path, path.with_suffix(".tmp")):
        try:
            candidate.unlink()
        except FileNotFoundError:
            pass


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def running(pid: int, procs: dict[int, subprocess.Popen]) -> bool:
    """True while a recorded child is not reaped; zombies count as exited."""
    proc = procs.get(pid)
    if proc is not None:
        return proc.poll() is None
    return process_alive(pid)


def tail(text: str, lines: int) -> list[str]:
    return text.splitlines()[-lines:]


def tail_file(path: Path, lines: int) -> list[str]:
    try:
        return path.read_text(encoding="utf-8", errors="replace").splitlines()[-lines:]
    except FileNotFoundError:
        return []


def log_line_count(path: Path) -> int:
    try:
        return len(path.read_text(encoding="utf-8", errors="replace").splitlines())
    except FileNotFoundError:
        return 0


def read_last_match(path: Path, regex: re.Pattern) -> str | None:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return None
    matches = [line for line in text.splitlines() if regex.search(line)]
    return matches[-1] if matches else None


def press_chord(chord: str) -> None:
    parts = chord.split("+")
    key = parts[-1]
    mods = parts[:-1]
    key_map = {
        "escape": "Escape",
        "return": "Return",
        "space": "space",
        "minus": "minus",
        "plus": "plus",
    }
    argv = ["wtype"]
    for mod in mods:
        argv += ["-M", mod]
    argv += ["-k", key_map.get(key, key)]
    for mod in reversed(mods):
        argv += ["-m", mod]
    proc = run_cmd(argv, timeout=10.0)
    if proc.returncode != 0:
        raise RuntimeError(f"wtype {chord} failed: {proc.stderr.strip()}")


def release_focus() -> None:
    """Close any transient overlay and let capture release before the next probe."""
    for _ in range(2):
        proc = run_cmd(["wtype", "-k", "Escape"], timeout=10.0)
        if proc.returncode != 0:
            raise RuntimeError(f"wtype Escape failed: {proc.stderr.strip()}")
        time.sleep(0.35)


SEND_IMAGE_SCRIPT = '''\
"""Emit one PNG through the kitty graphics protocol (CTX-2413 matrix harness)."""
import base64
import sys

path = sys.argv[1]
data = open(path, "rb").read()
payload = base64.b64encode(data).decode("ascii")
out = sys.stdout.buffer
chunk = 4096
first = True
for index in range(0, len(payload), chunk):
    part = payload[index : index + chunk]
    more = 1 if index + chunk < len(payload) else 0
    if first:
        out.write(b"\\x1b_Ga=T,f=100,t=d,q=2,m=%d;" % more)
        first = False
    else:
        out.write(b"\\x1b_Gm=%d;" % more)
    out.write(part.encode("ascii"))
    out.write(b"\\x1b\\\\")
out.flush()
'''


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def load_theme_catalog(binary: Path, xdg_root: Path) -> list[dict]:
    """Enumerate shipped presets for the expanded plan (headless, isolated)."""
    store = Store(xdg_root, "theme-catalog")
    env = store.env(f"{INSTANCE_PREFIX}-theme-catalog")
    proc = run_cmd(
        [str(binary), "list", "themes", "--format", "json"], env=env, timeout=20.0
    )
    if proc.returncode != 0:
        raise SystemExit(f"list themes failed: {proc.stderr.strip()}")
    return json.loads(proc.stdout)["result"]["themes"]


def render_results(out: Path) -> Path:
    """Render RESULTS.md from a verification.json payload."""
    data = read_json(out / "verification.json")
    rows = data.get("rows", [])
    candidate = data.get("candidate", {})
    env = data.get("environment", {})
    provenance = {}
    prov_path = Path(candidate.get("provenance", ""))
    if prov_path.is_file():
        for line in prov_path.read_text(encoding="utf-8").splitlines():
            if ": " in line:
                key, value = line.split(": ", 1)
                provenance[key.strip()] = value.strip()
    counts: dict[str, int] = {}
    for row in rows:
        counts[row["status"]] = counts.get(row["status"], 0) + 1
    lines = [
        "# CTX-2413 expanded runtime matrix - results",
        "",
        f"- plan: `{data.get('plan', 'unknown')}`",
        f"- candidate: `{provenance.get('candidate_version', '(unknown)')}`",
        f"- source head: `{provenance.get('source_head', '(unknown)')}`",
        f"- binary: `{candidate.get('binary', '(unknown)')}`",
        f"- binary sha256: `{candidate.get('binary_sha256', '(unknown)')}`",
        f"- isolated XDG root: `{env.get('xdg_roots', '(unknown)')}`",
        f"- workspace: {env.get('workspace')}",
        f"- network installs: {env.get('network_installs')}",
        (
            f"- bar anchor dependency: "
            f"{env.get('bar_anchors', {}).get('state', 'unknown')} "
            f"({env.get('bar_anchors', {}).get('source', 'unknown')})"
        ),
        f"- rows: {len(rows)}",
        "",
        "## Summary",
        "",
        "| status | rows |",
        "| --- | ---: |",
    ]
    for status in (
        "PASS",
        "FAIL",
        "BLOCKED",
        "NOT_EXERCISED",
        "NOT_APPLICABLE",
    ):
        lines.append(f"| {status} | {counts.get(status, 0)} |")
    lines.append("")

    def table(title: str, predicate) -> None:
        selected = [r for r in rows if predicate(r)]
        if not selected:
            return
        lines.extend(
            [
                f"## {title}",
                "",
                "| config | row | status | detail | evidence |",
                "| --- | --- | --- | --- | --- |",
            ]
        )
        for row in selected:
            shots = row.get("screenshots") or []
            evidence = ""
            if shots:
                shot = Path(shots[0])
                try:
                    link = shot.relative_to(out)
                except ValueError:
                    link = Path("shots") / shot.name
                evidence = f"[{shot.name}]({link})"
            detail = (row.get("reason") or "").replace("|", "\\|")[:180]
            lines.append(
                f"| `{row['config']}` | `{row['id']}` | {row['status']} | "
                f"{detail} | {evidence} |"
            )
        lines.append("")

    table(
        "Profiles",
        lambda r: (
            (
                r["config"].startswith("profile-")
                and r["id"]
                in {
                    "config",
                    "launch",
                    "session-fresh",
                    "session-restored",
                    "close",
                    "theme-pixels",
                    "theme-contrast",
                    "theme-family",
                }
            )
            or r["id"].startswith("delta-")
        ),
    )
    table(
        "Bar edges, groups, hit regions",
        lambda r: r["kind"] == "bar",
    )
    table(
        "Themes",
        lambda r: r["id"].startswith("theme-") or r["id"].startswith("ansi-"),
    )
    table(
        "Extensions (present / absent / removed)",
        lambda r: r["kind"] == "extension",
    )
    table(
        "Plugins",
        lambda r: r["kind"] == "plugin",
    )
    table(
        "Graphics and images",
        lambda r: r["id"] in {"image-kitty", "background-rendered", "graphics-scene"},
    )
    table(
        "Session",
        lambda r: r["id"].startswith("session-"),
    )
    coverage = [
        r
        for r in rows
        if r["id"].endswith("-coverage") or r["id"] == "bar-contrast-matrix"
    ]
    if coverage:
        lines.extend(
            [
                "## Coverage audit",
                "",
                "| row | status | detail |",
                "| --- | --- | --- |",
            ]
        )
        for row in coverage:
            detail = (row.get("reason") or "").replace("|", "\\|")[:200]
            lines.append(f"| `{row['id']}` | {row['status']} | {detail} |")
        lines.append("")

    failures = [r for r in rows if r["status"] in {"FAIL", "BLOCKED"}]
    lines.extend(["## Failures and blocked rows", ""])
    if not failures:
        lines.append("None.")
    else:
        for row in failures:
            lines.append(
                f"- **{row['status']}** `{row['config']}/{row['id']}`: "
                f"{row.get('reason') or 'no reason recorded'}"
            )
    lines.append("")
    errors = data.get("harness_errors") or []
    lines.extend(["## Harness errors", ""])
    lines.append("None." if not errors else "\n".join(f"- `{e}`" for e in errors))
    lines.append("")
    path = out / "RESULTS.md"
    write_text(path, "\n".join(lines))
    return path


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path(os.environ.get("BITTY_MATRIX_BINARY", "")).expanduser()
        if os.environ.get("BITTY_MATRIX_BINARY")
        else None,
        help="candidate bitty binary (required except for matrix/help)",
    )
    parser.add_argument("--workspace", type=Path, default=None)
    parser.add_argument("--source", type=Path, default=None, help="candidate checkout")
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(os.environ.get("BITTY_MATRIX_OUT", DEFAULT_OUT)),
    )
    parser.add_argument(
        "--xdg-root",
        type=Path,
        default=Path(os.environ.get("BITTY_MATRIX_XDG", DEFAULT_XDG_ROOT)),
    )
    parser.add_argument(
        "--wallpapers",
        type=Path,
        default=Path.home() / "Pictures" / "Wallpapers",
        help="wallpaper directory (owner-provided default)",
    )
    parser.add_argument("--wallpaper", type=Path, default=None)
    parser.add_argument("--wallpaper-index", type=int, default=None)
    parser.add_argument("--build-profile", default="dev")
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument(
        "--reset-xdg",
        action="store_true",
        help="remove <xdg-root>/stores before prepare/run (CTX-2413 scratch only)",
    )
    parser.add_argument(
        "--require-clean-sources",
        action="store_true",
        help="refuse plugin checkouts with tracked modifications",
    )
    parser.add_argument(
        "--network-extensions",
        action="store_true",
        help=(
            "allow the pinned published extension installs (graphics 0.0.24, "
            "net 0.0.1) from cdn.bitty.run for present/absent rows"
        ),
    )
    parser.add_argument(
        "--reuse-sources",
        action="store_true",
        help="reuse existing plugin source clones under <xdg-root>/sources",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit 1 when any row is FAIL (default: product failures are data)",
    )
    parser.add_argument(
        "--bar-anchors",
        choices=["auto", "merged", "open"],
        default="auto",
        help=(
            "band anchor/hover dependency state (#2411/#2423). auto probes "
            "GitHub once and degrades to 'open'; only 'merged' runs the hover "
            "probe, otherwise the rows stay BLOCKED"
        ),
    )
    parser.add_argument(
        "--plan",
        choices=["smoke", "smoke2413", "full", "expanded"],
        default="smoke",
        help="run plan (default smoke)",
    )
    parser.add_argument(
        "--only",
        default=None,
        help="comma-separated config ids to run (default: all in the plan)",
    )
    parser.add_argument(
        "command", choices=["matrix", "prepare", "run", "report", "restore-desktop"]
    )
    return parser


def plan_configs(
    plan: str, catalog: PluginCatalog, xdg_root: Path, binary: Path | None
) -> tuple[list[MatrixConfig], list[MatrixConfig]]:
    if plan == "expanded":
        if binary is None:
            raise SystemExit("--plan expanded requires --binary")
        themes = load_theme_catalog(binary, xdg_root)
        return build_expanded_matrix(catalog, themes), expanded_probes()
    if plan == "full":
        return build_matrix(catalog), list(PROBE_CONFIGS)
    if plan == "smoke2413":
        if binary is None:
            raise SystemExit("--plan smoke2413 requires --binary")
        return smoke2413_configs(), smoke2413_probes()
    return smoke_configs(), list(PROBE_CONFIGS)


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    workspace = (
        args.workspace.resolve() if args.workspace else find_workspace(SCRIPT_DIR)
    )
    catalog = PluginCatalog(workspace)

    if args.command == "matrix":
        configs, probes = plan_configs(
            args.plan, catalog, args.xdg_root.resolve(), args.binary
        )
        for cfg in [*configs, *probes]:
            print(
                f"{cfg.cid}\tstore={cfg.store}\tprofile={cfg.profile or '-'}"
                f"\tbg={cfg.bg or '-'}\trequested={cfg.bg_requested or '-'}"
                f"\ttheme_file={cfg.theme_file or '-'}\ttheme_cli={cfg.theme_cli or '-'}"
                f"\tanimations={cfg.animations}\tsession="
                f"{'restored' if cfg.session_restore else 'fresh'}"
                f"\tbar={cfg.bar_edge or '-'}\textensions={','.join(cfg.extensions) or '-'}"
                f"\tgui={cfg.gui}"
            )
        print(
            f"\n{len(configs)} configurations + {len(probes)} headless probes; "
            f"{len(catalog.plugins)} plugins; "
            f"{len(load_extensions(workspace))} extensions"
        )
        return 0

    if args.command == "restore-desktop":
        state_path = args.out.resolve() / "desktop-before.json"
        if not state_path.is_file():
            print(f"no recorded desktop state at {state_path}", file=sys.stderr)
            return 3
        info = restore_desktop(read_json(state_path))
        print(json.dumps(info, indent=2))
        return 0

    if args.command == "report":
        out = args.out.resolve()
        if not (out / "verification.json").is_file():
            print(f"no verification.json under {out}", file=sys.stderr)
            return 3
        path = render_results(out)
        print(f"wrote {path}")
        return 0

    if args.binary is None:
        parser.error("--binary (or BITTY_MATRIX_BINARY) is required")

    harness = Harness(args)

    if args.reset_xdg:
        stores = harness.xdg_root / "stores"
        if stores.exists():
            shutil.rmtree(stores)
            print(f"reset isolated stores under {stores}")

    if args.command == "prepare":
        harness.prepare()
        if args.require_clean_sources:
            unpinned = [p.slug for p in harness.catalog.plugins if not p.source_ok]
            if unpinned:
                print(
                    "refusing unpinned plugin sources: " + ", ".join(unpinned),
                    file=sys.stderr,
                )
                return 3
        print(f"prepared {harness.out} (xdg root {harness.xdg_root})")
        return 0

    configs, probes = plan_configs(
        args.plan, harness.catalog, harness.xdg_root, harness.binary
    )
    if args.only:
        wanted = {part.strip() for part in args.only.split(",") if part.strip()}
        unknown = wanted - {c.cid for c in [*configs, *probes]}
        if unknown:
            parser.error(f"unknown config ids: {', '.join(sorted(unknown))}")
        configs = [c for c in configs if c.cid in wanted]
        probes = [c for c in probes if c.cid in wanted]
    harness.run(configs, probes)
    fail = [r for r in harness.rows if r["status"] == "FAIL"]
    blocked = [r for r in harness.rows if r["status"] == "BLOCKED"]
    print(
        f"rows={len(harness.rows)} pass={sum(1 for r in harness.rows if r['status'] == 'PASS')} "
        f"fail={len(fail)} not_applicable="
        f"{sum(1 for r in harness.rows if r['status'] == 'NOT_APPLICABLE')} "
        f"not_exercised={sum(1 for r in harness.rows if r['status'] == 'NOT_EXERCISED')} "
        f"blocked={len(blocked)}"
    )
    report = render_results(harness.out)
    print(f"report: {report}")
    print(f"evidence: {harness.out}")
    if harness.errors:
        print("harness errors:", file=sys.stderr)
        for error in harness.errors:
            print(f"  {error}", file=sys.stderr)
        return 3
    if args.strict and fail:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
