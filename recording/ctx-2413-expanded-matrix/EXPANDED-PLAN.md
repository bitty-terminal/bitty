# CTX-2413 expanded runtime acceptance matrix

Owner directive (bitty-terminal/bitty#2413, 2026-10-21): during testing,
exercise different profiles with different extensions, plugins, and themes;
test the bar mounted at the top as well as the bottom; compare
`bitty-graphics` present vs absent; and cover the same for the other
surface-changing extensions.

This plan turns every requested dimension into an automated row. A row is
produced by `harness.py`, carries its axes, status, reason, screenshots, and
evidence, and lands in `verification.json` plus `RESULTS.md`. Nothing in this
matrix is a manual note.

## Status vocabulary

| Status           | Meaning                                                                                                                                |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `PASS`           | The check ran and the product met its acceptance criterion.                                                                            |
| `FAIL`           | The check ran and the product missed the criterion (product defect).                                                                   |
| `BLOCKED`        | The criterion depends on an open issue/PR; the row names it and is never `PASS` until the dependency lands and the probe runs.         |
| `NOT_EXERCISED`  | The check was skipped for an environmental reason that is recorded (no network install, no window, no scroll module).                  |
| `NOT_APPLICABLE` | The dimension cannot exist in this build (library-only extension, retired plugin, compiled-in extension with no installable artifact). |

`BLOCKED` is reserved for product dependencies, never for harness defects
(those fail the run with exit 3 and a `harness-error` row).

## Plans

| Plan        | Purpose                                                                         | Size           |
| ----------- | ------------------------------------------------------------------------------- | -------------- |
| `smoke`     | CTX-1344 harness sanity (profile, background, plugin).                          | 3 + 7 probes   |
| `smoke2413` | One row per new #2413 dimension; the fast proof that the expanded rows execute. | 12 + 4 probes  |
| `expanded`  | The full acceptance campaign for the 0.0.26 candidate.                          | 70 + 11 probes |

Commands:

```sh
# print the planned matrix without touching the desktop
python3 harness.py matrix --plan expanded --binary <candidate>

# fast proof against the current binary
python3 harness.py run --plan smoke2413 --binary <candidate> \
  --out recording/ctx-2413-expanded-matrix/smoke-0.0.25

# full campaign (0.0.26 candidate)
python3 harness.py run --plan expanded --binary <candidate> \
  --out recording/ctx-2413-expanded-matrix/full-0.0.26 \
  --network-extensions --reset-xdg

# render RESULTS.md for an existing evidence directory
python3 harness.py report --out recording/ctx-2413-expanded-matrix/full-0.0.26
```

## Dimensions and rows

### 1. Profiles

Configurations: `profile-default` (no `--profile`), `profile-dev`,
`profile-minimal`, `profile-plugins`, `profile-settings` (settings-heavy),
`profile-settings-restored`, `profile-bar-off` (bar declared disabled).

Rows per configuration: `config` (resolved values and source attribution:
profile vs default vs file vs cli), `launch`, `session-fresh`, `close`, and
`theme-*`.

Cross-configuration rows:

- `delta-profile-minimal-profile-settings` and
  `delta-profile-default-profile-settings` — the settings-heavy profile must
  change at least `PROFILE_DELTA_MIN_PX` window pixels, so a profile that
  parses but never takes effect fails.
- `config` expectations pin the documented values: scrollback 100000,
  font 14/1.2, gaps 4/12, opacity 0.96, padding 12, steady-bar cursor.

### 2. Extensions (present vs absent)

Every extension in the workspace roster (derived from `workspace.toml`
`kind = "extension"`, never hardcoded) gets a present row and an absent row
from the headless `probe-extension-matrix`:

| Extension                                     | Present                                                                                                                                | Absent                                                                                                                                            |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `graphics`                                    | `graphics-on` installs the published 0.0.24 component; `ext-graphics-state` + `image-kitty` + `background-rendered` + `graphics-scene` | `graphics-off` (no component); `graphics-removed` (install, remove, render); `ext-graphics-state`, `ext-graphics-removal`, `ext-graphics-residue` |
| `net`                                         | `net-on` installs net 0.0.1; `ext-net-broker` proves an allowed request and a typed denial through the broker                          | `net-off`; `ext-net-broker` must degrade with a typed denial, no panic                                                                            |
| `storage`                                     | `ext-storage` — session snapshot written on close, restored by `all-on-restored`                                                       | compiled-in (`bitty-storage` git pin): absent is not shippable, row is `NOT_APPLICABLE` with the linkage named                                    |
| `url-detector`                                | `url-hover` — Ctrl+hover underline delta over a printed plaintext URL                                                                  | compiled-in (`bitty-url-detector` git pin): `NOT_APPLICABLE` with the linkage named                                                               |
| `ipc`                                         | `ext-ipc*` rows from `run_ipc_checks` (instance/window/view/terminal list, split, send)                                                | compiled-in (`bitty-ipc` git pin): `NOT_APPLICABLE`                                                                                               |
| `platform-services`                           | `NOT_EXERCISED` (notification probe chord not assigned)                                                                                | compiled-in: `NOT_APPLICABLE`                                                                                                                     |
| `observability`, `a11y`, `agent`, `execution` | library-only: both rows `NOT_APPLICABLE` with the reason                                                                               | same                                                                                                                                              |

`extension-coverage` asserts that every roster entry produced both rows.

### 3. Plugins

- `all-on` — every official plugin installed and enabled.
- `plugin-<slug>` — each official registry plugin alone (10 entries today:
  activity, bar, composer, copy-mode, file-manager, git-panel, history,
  palette, search, wheel).
- Named surfaces: `bar-*` configurations, `surface-palette`,
  `surface-composer`, `surface-file-manager` (visible overlay delta), and
  `plugin-statusline` (`NOT_APPLICABLE`: retired checkout, no registry entry,
  0.1 compatibility floor).
- `no-extension-<slug>` — the named plugins (bar, palette, composer,
  statusline, file-manager) installed with **no** native extension: the plugin
  must activate or refuse with a typed error, the window must stay alive, and
  the app log must contain no panic.
- `plugin-surface-coverage` asserts the named set produced rows.

### 4. Themes

- `theme-<preset>` for all 30 shipped presets (20 dark, 10 light) plus
  `theme-custom-palette` (complete inline `appearance.colors`).
- Rows per theme: `theme-pixels` (dominant window color equals the resolved
  background within tolerance), `theme-contrast` (`theme.contrast.result =
pass`), `theme-family`.
- ANSI spot checks: `theme-vim-dark`, `theme-vim-light` (`ansi-vim`),
  `theme-ls-dark`, `theme-ls-light` (`ansi-ls`). Each asserts at least
  `ANSI_SPOT_MIN_COLORS` distinct non-background colors with at least 40
  pixels each, so anti-aliasing cannot fake coverage.
- `theme-coverage` asserts 30+ presets, no missing/extra preset, both
  families, and no failing pixel row.
- Bar contrast at top and bottom in both families: `bar-top-dark`,
  `bar-bottom` (dark), `bar-top-light`, `bar-bottom-light`;
  `bar-contrast-matrix` asserts the four (edge, family) combinations exist and
  pass.

### 5. Bar edges and hit regions

| Row                                      | What it proves                                                                                                                                                         |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bar-mount`                              | The band is painted at the configured edge and not the opposite one (accent-pixel profile).                                                                            |
| `bar-alignment`                          | `BLOCKED` on bitty#2411 / PR #2423 until anchors land; the merged branch measures left/right anchoring.                                                                |
| `bar-hover`                              | `BLOCKED` on bitty#2411 / PR #2423 until pointer enter/leave lands; the merged branch measures the enter/leave pixel delta.                                            |
| `bar-click`                              | A left click on an inactive workspace pill changes the active workspace.                                                                                               |
| `bar-scroll`                             | A wheel over a scroll-bound module (`pulseaudio`) dispatches `on_scroll`; bar#31 is merged, so a miss is recorded as `NOT_EXERCISED` with the reason, never as a pass. |
| `bar-groups`                             | Declared left/center/right modules are painted in order.                                                                                                               |
| `bar-contrast`                           | Band accent pixels exist and the palette contrast policy passes.                                                                                                       |
| `bar-absent-click` / `bar-absent-scroll` | With the bar plugin absent or hidden, clicks and wheel at both window edges change nothing and never panic.                                                            |

Dependency probe: `--bar-anchors auto` performs one bounded `gh pr view 2423`
call at startup and degrades to `open` on any failure. Only `merged` runs the
hover/alignment probes; otherwise the rows stay `BLOCKED` with the dependency
named in `reason` and `evidence.bar_anchors`.

### 6. Graphics

- `graphics-on` — published `graphics` 0.0.24 component installed:
  `ext-graphics-state`, `image-kitty` (kitty protocol PNG), `background-rendered`
  (background image under the grid), `graphics-scene` (both reach the present
  path).
- `graphics-off` — no component: same kitty and background rows must still
  pass through Core-owned intake/placement/present, with `graphics_expected =
false` asserted against the extension inventory.
- `graphics-removed` — install, `extension remove graphics`, then launch:
  `ext-graphics-removal` (inventory drops it, no panic), `ext-graphics-residue`
  (no install directory and no kitty placement cache left behind),
  `image-kitty` and `background-rendered` still render.
- `graphics-coverage` asserts the installed/absent/removed triangle produced
  rows.

### 7. Session

- `session-fresh` — every non-restore configuration starts from exactly one
  view and logs no restore line.
- `session-restored` — `all-on-restored`, `bar-restored`,
  `profile-settings-restored` relaunch with `session.restore_on_startup`,
  restore the layout, and expose at least two views.
- `session-coverage` asserts both directions ran and passed.

## Smoke plan (`smoke2413`)

Sixteen configurations, one per new dimension, plus four headless probes:

| Config                 | Dimension                                                      |
| ---------------------- | -------------------------------------------------------------- |
| `profile-minimal`      | profile baseline for the visible delta                         |
| `profile-settings`     | settings-heavy profile                                         |
| `profile-bar-off`      | bar disabled by profile + plugin settings (absent hit regions) |
| `bar-top`              | bar mounted at the top (dark)                                  |
| `bar-bottom-light`     | bar mounted at the bottom (light)                              |
| `theme-github-light`   | shipped light preset                                           |
| `theme-custom-palette` | custom palette                                                 |
| `theme-vim-dark`       | Vim syntax colors under a dark preset                          |
| `theme-ls-light`       | `ls --color` ANSI slots under a light preset                   |
| `graphics-on`          | graphics component installed (kitty + background)              |
| `graphics-off`         | graphics absent: kitty + background still render               |
| `graphics-removed`     | install, remove, render, no residue                            |
| `net-off`              | net absent: typed denial, no panic                             |
| `plugin-palette`       | plugin alone with no extension                                 |
| `all-on-restored`      | fresh vs restored session                                      |
| `url-hover`            | url-detector visible delta                                     |

Probes: `probe-plugin-inventory`, `probe-extension-inventory`,
`probe-extension-matrix`, `probe-statusline`.

The smoke run passes `--network-extensions` so the graphics
present/absent/removed triangle executes against the published 0.0.24
component instead of being skipped.

## Environment and safety

- Isolated XDG roots: `$XDG_RUNTIME_DIR/ctx-2413/m26` (fallback
  `/tmp/ctx-2413/m26`), full socket path asserted under 100 bytes for
  AF_UNIX. The owner's config, plugin store, and session are never read or
  written.
- Hyprland workspace 5; `grim` window and monitor captures; desktop state
  recorded before and restored after (`desktop-before.json`,
  `desktop-after.json`, `harness.py restore-desktop`).
- No clipboard access, no pattern kills, no `pkill`/`killall`. Only PIDs
  recorded by the harness are signalled, and only after a window close and
  SIGTERM both fail.
- Extension downloads are opt-in (`--network-extensions`) and counted in
  `environment.network_installs`.
- The harness never edits product source. Product defects are rows; the
  harness exits 3 only for its own assumption failures.

## Evidence layout

```text
recording/ctx-2413-expanded-matrix/<run>/
  verification.json   # every row with axes, status, reason, evidence
  RESULTS.md          # rendered summary (report command)
  provenance.txt      # binary, source head, rustc, compositor, wallpaper
  desktop-before.json / desktop-after.json
  pids.json           # every PID the harness started and how it exited
  shots/              # per-row PNGs referenced from the rows
  logs/               # app logs, ctl JSON, extension list, fixtures
  assets/             # kitty crop, send-image script, component fixtures
```
