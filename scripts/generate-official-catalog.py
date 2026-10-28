#!/usr/bin/env python3
"""Generate the embedded official plugin catalog for Core.

Input (never fetched at runtime):

- ``<registry>/registry/official/*.toml``: id, repository, description,
  compatibility, license, and the registry's raw ``manifest_hash``.
- ``<registry>`` Git metadata: the ``plugins/<name>`` gitlink pin for each
  entry, the registry commit the catalog was generated from, and its origin
  remote (the rendered source repository; no URL is hardcoded here).

Output: ``crates/bitty-plugin-host/src/official_catalog.rs`` (checked in).

Usage:
    scripts/generate-official-catalog.py [--registry DIR] [--output FILE]
    scripts/generate-official-catalog.py --check [--registry DIR]
    scripts/generate-official-catalog.py --check-pins [--revision REF]

The registry directory is resolved from ``--registry``, then
``BITTY_PLUGINS_DIR``, then ``$BITTY_WORKSPACE/bitty-plugins``, then
``<repo>/../bitty-plugins``, ``<repo>/external/bitty-plugins``, and the
worktree-relative sibling. An explicit ``--registry`` or ``BITTY_PLUGINS_DIR``
is authoritative: when it does not name a usable checkout the command fails
instead of falling back to another tree. ``--check`` is read-only and exits
non-zero when the checked-in file differs from a fresh rendering (the strict
``just check-registry`` gate). ``--check-pins`` compares only the compiled
``pin`` fields with the registry gitlinks at ``--revision``; point it at
``origin/main`` (``just check-registry-fresh``) to catch a plugin code change
whose manifest hash, and therefore digest verification, stayed unchanged.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

import tomllib

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = REPO_ROOT / "crates/bitty-plugin-host/src/official_catalog.rs"
GITLINK_RE = re.compile(r"^160000 commit ([0-9a-f]{40})\tplugins/(.+)$")
HASH_RE = re.compile(r"^sha256:([0-9a-f]{64})$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")

# Repository-location variables never inherited by `git`. `git` lets them
# override `-C`, so a generator run from a hook that exports them (for
# example `pre-commit`, which sets `GIT_INDEX_FILE`) would read the caller's
# repository instead of the registry checkout. Mirrors the seven-variable
# scrub in `crates/bitty-terminal/src/plugin.rs`; gitconfig, proxy, SSH-agent,
# and CA handling still come free from the inherited environment.
GIT_REPO_ENV_VARS = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
)


def git_env() -> dict[str, str]:
    """Inherited environment with repository-location overrides removed."""
    env = dict(os.environ)
    for name in GIT_REPO_ENV_VARS:
        env.pop(name, None)
    return env


class CatalogError(RuntimeError):
    """A registry input cannot be rendered safely."""


def rust_str(value: str) -> str:
    """Render a Rust string literal (supports escapes JSON does not)."""
    out = ['"']
    for ch in value:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append(f"\\u{{{ord(ch):X}}}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def option(value: str | None) -> str:
    return "None" if value is None else f"Some({rust_str(value)})"


def run_git(registry: Path, *args: str) -> str:
    try:
        return subprocess.run(
            ["git", "-C", str(registry), *args],
            check=True,
            capture_output=True,
            text=True,
            env=git_env(),
        ).stdout
    except FileNotFoundError as error:
        raise CatalogError("`git` is required to read registry pins") from error
    except subprocess.CalledProcessError as error:
        detail = error.stderr.strip() or error.stdout.strip() or str(error)
        raise CatalogError(f"git {' '.join(args)} failed: {detail}") from error


def validation_pin(name: str) -> str | None:
    """Read one KEY=VALUE from validation-pins.env (CI/local pin source)."""
    path = REPO_ROOT / "validation-pins.env"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith(f"{name}="):
            return line.split("=", 1)[1].strip()
    return None


def resolve_revision(registry: Path, explicit: str | None) -> str:
    """Resolve an immutable registry commit (never a branch or tag result)."""
    revision = (
        explicit
        or os.environ.get("BITTY_PLUGINS_REV")
        or validation_pin("BITTY_PLUGINS_REV")
        or "HEAD"
    )
    sha = run_git(registry, "rev-parse", revision).strip()
    if not SHA_RE.match(sha):
        raise CatalogError(f"registry revision {revision!r} is not a commit: {sha!r}")
    return sha


def require_registry(candidate: Path, source: str, hint: str) -> Path:
    """Resolve one requested registry, or fail without trying another tree."""
    registry = candidate.expanduser().resolve()
    if not (registry / "registry/official").is_dir():
        reason = "no registry/official directory"
    elif not (registry / ".git").exists():
        reason = "not a Git checkout (missing .git)"
    else:
        return registry
    raise CatalogError(
        f"requested registry {registry} (via {source}) is not a usable "
        f"bitty-plugins checkout ({reason}); {hint}"
    )


def resolve_registry(explicit: str | None) -> Path:
    # An explicitly requested registry is authoritative: never fall back to a
    # different checkout, so a typo or unprovisioned path fails here instead of
    # silently validating the catalog against an unrelated tree. CI passes
    # --registry with the pinned checkout.
    if explicit:
        return require_registry(
            Path(explicit),
            "--registry",
            "correct the path or choose another checkout",
        )
    configured = os.environ.get("BITTY_PLUGINS_DIR")
    if configured:
        return require_registry(
            Path(configured),
            "BITTY_PLUGINS_DIR",
            "correct the path or unset BITTY_PLUGINS_DIR",
        )
    candidates: list[Path] = []
    if os.environ.get("BITTY_WORKSPACE"):
        candidates.append(Path(os.environ["BITTY_WORKSPACE"]) / "bitty-plugins")
    candidates.append(REPO_ROOT.parent / "bitty-plugins")
    # CI checks out the pinned registry here for the strict registry gate; a
    # normal checkout can provision the same in-repository path.
    candidates.append(REPO_ROOT / "external" / "bitty-plugins")
    # Worktrees live at <main-checkout>/.worktrees/<task>; the workspace
    # checkout is three levels up from the worktree root.
    candidates.append(REPO_ROOT.parent.parent.parent / "bitty-plugins")
    for candidate in candidates:
        registry = candidate.expanduser().resolve()
        if (registry / "registry/official").is_dir() and (registry / ".git").exists():
            return registry
    tried = ", ".join(str(candidate) for candidate in candidates)
    raise CatalogError(
        "bitty-plugins checkout not found (tried: "
        f"{tried}); the strict registry gate (`just check-registry`) needs a "
        "registry: pass --registry DIR, set BITTY_PLUGINS_DIR, or clone "
        "bitty-plugins beside this checkout (see CONTRIBUTING.md). "
        "`just check` does not need the registry."
    )


def source_repository(registry: Path, explicit: str | None) -> str:
    """Resolve the registry repository URL from its checkout, never a literal."""
    if explicit:
        value = explicit.strip()
    else:
        value = run_git(registry, "remote", "get-url", "origin").strip()
    if value.startswith("git@") and ":" in value:
        host, path = value[4:].split(":", 1)
        value = f"https://{host}/{path}"
    value = value.rstrip("/").removesuffix(".git")
    if not value.startswith("https://"):
        raise CatalogError(
            f"registry repository must be an https:// URL (got {value!r}); "
            "pass --source-repository"
        )
    return value


def reject_controls(value: str, key: str, path: Path) -> str:
    if any(ord(ch) < 0x20 or 0x7F <= ord(ch) <= 0x9F for ch in value):
        raise CatalogError(f"{path}: `{key}` must not contain control characters")
    return value


def repository_url(value: object, path: Path) -> str:
    if not isinstance(value, str) or not value.startswith("https://"):
        raise CatalogError(f"{path}: `repository` must be an https:// URL")
    url = reject_controls(value.strip(), "repository", path).rstrip("/")
    if not url.endswith(".git"):
        url += ".git"
    return url


def required_string(entry: dict[str, object], key: str, path: Path) -> str:
    value = entry.get(key)
    if not isinstance(value, str) or not value.strip():
        raise CatalogError(f"{path}: `{key}` must be a non-empty string")
    return reject_controls(value.strip(), key, path)


def optional_string(entry: dict[str, object], key: str, path: Path) -> str | None:
    value = entry.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise CatalogError(f"{path}: `{key}` must be a non-empty string when set")
    return reject_controls(value.strip(), key, path)


def gitmodules_url(registry: Path, revision: str, path: str) -> str:
    text = run_git(registry, "show", f"{revision}:.gitmodules")
    section: dict[str, str] = {}
    sections: list[dict[str, str]] = []
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("[submodule"):
            section = {}
            sections.append(section)
        elif "=" in line and section is not None:
            key, value = line.split("=", 1)
            section[key.strip()] = value.strip()
    for section in sections:
        if section.get("path") == path:
            return section.get("url", "")
    raise CatalogError(f".gitmodules has no submodule at `{path}`")


def pin_for(
    registry: Path, revision: str, repo_name: str, repository: str, path: Path
) -> str:
    submodule = f"plugins/{repo_name}"
    listing = run_git(registry, "ls-tree", revision, "--", f"{submodule}/")
    match = GITLINK_RE.match(listing.strip())
    if not match:
        raise CatalogError(
            f"{path}: no pinned gitlink at `{submodule}` (official entries must be pinned)"
        )
    pin = match.group(1)
    if not SHA_RE.match(pin):
        raise CatalogError(f"{path}: malformed gitlink pin {pin!r}")
    declared = gitmodules_url(registry, revision, submodule)
    if declared.rstrip("/").removesuffix(".git") != repository.removesuffix(".git"):
        raise CatalogError(
            f"{path}: .gitmodules URL {declared!r} does not match registry repository {repository!r}"
        )
    return pin


def render_entry(
    entry: dict[str, object], path: Path, registry: Path, revision: str
) -> str:
    if entry.get("kind") not in (None, "plugin"):
        raise CatalogError(f"{path}: official entry kind must be `plugin`")
    plugin_id = required_string(entry, "id", path)
    repository = repository_url(entry.get("repository"), path)
    repo_name = repository.removesuffix(".git").rsplit("/", 1)[-1]
    pin = pin_for(registry, revision, repo_name, repository, path)
    manifest_hash = required_string(entry, "manifest_hash", path)
    match = HASH_RE.match(manifest_hash)
    if not match:
        raise CatalogError(
            f"{path}: `manifest_hash` must be `sha256:` plus 64 lowercase hex chars"
        )
    compatibility = entry.get("compatibility")
    if not isinstance(compatibility, dict):
        raise CatalogError(f"{path}: missing `[compatibility]` table")
    compat_bitty = optional_string(compatibility, "bitty", path)
    if compat_bitty is None:
        raise CatalogError(f"{path}: `compatibility.bitty` is required")
    compat_sdk = optional_string(compatibility, "sdk", path)
    return "\n".join(
        [
            "    OfficialPlugin {",
            f"        id: {rust_str(plugin_id)},",
            f"        name: {rust_str(required_string(entry, 'name', path))},",
            f"        repository: {rust_str(repository)},",
            f"        pin: {rust_str(pin)},",
            f"        manifest_hash: {rust_str(manifest_hash)},",
            f"        compat_bitty: Some({rust_str(compat_bitty)}),",
            f"        compat_plugin_api: {option(compat_sdk)},",
            f"        description: {rust_str(required_string(entry, 'description', path))},",
            f"        license: {option(optional_string(entry, 'license', path))},",
            "    },",
        ]
    )


def render(
    registry: Path, revision_arg: str | None, source_repository_arg: str | None
) -> tuple[str, int, str]:
    revision = resolve_revision(registry, revision_arg)
    source = source_repository(registry, source_repository_arg)
    listing = run_git(
        registry, "ls-tree", "--name-only", revision, "registry/official/"
    )
    names = sorted(
        line.strip() for line in listing.splitlines() if line.strip().endswith(".toml")
    )
    if not names:
        raise CatalogError(
            f"registry commit {revision}: no registry/official/*.toml entries"
        )
    entries: list[tuple[str, str]] = []
    for name in names:
        path = Path(name)
        body = run_git(registry, "show", f"{revision}:{name}")
        try:
            entry = tomllib.loads(body)
        except tomllib.TOMLDecodeError as error:
            raise CatalogError(f"{path}: cannot parse entry: {error}") from error
        rendered = render_entry(entry, path, registry, revision)
        plugin_id = required_string(entry, "id", path)
        entries.append((plugin_id, rendered))
    entries.sort(key=lambda item: item[0])
    body = "\n".join(rendered for _, rendered in entries)
    text = f"""//! Generated official plugin catalog. Do not edit by hand.
//!
//! Rendered by `just catalog-generate` from
//! `registry/official/*.toml` plus each `plugins/<name>` gitlink in
//! <{source}> at `SOURCE_REVISION`. The checked-in output is
//! gated by `just catalog-check`; Core never fetches registry metadata at
//! runtime.
//!
//! @generated

use crate::official::OfficialPlugin;

/// Registry repository this catalog was generated from.
pub const SOURCE_REPOSITORY: &str = {rust_str(source)};

/// Exact registry commit this catalog was generated from.
pub const SOURCE_REVISION: &str = {rust_str(revision)};

/// Official plugin entries, sorted by canonical id.
pub static OFFICIAL_PLUGINS: &[OfficialPlugin] = &[
{body}
];
"""
    return text, len(entries), revision


COMPILED_ENTRY_RE = re.compile(r"^\s*OfficialPlugin \{$")
COMPILED_FIELD_RE = {
    "id": re.compile(r'^\s*id: "((?:[^"\\]|\\.)*)",$'),
    "repository": re.compile(r'^\s*repository: "((?:[^"\\]|\\.)*)",$'),
    "pin": re.compile(r'^\s*pin: "([0-9a-f]{40})",$'),
}


def parse_compiled_entries(path: Path) -> list[dict[str, str]]:
    """Read ``(id, repository, pin)`` from the generated Rust catalog."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise CatalogError(f"cannot read checked-in catalog {path}: {error}") from error
    entries: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for line in lines:
        if COMPILED_ENTRY_RE.match(line):
            current = {}
            entries.append(current)
            continue
        if current is None:
            continue
        if line.strip() == "},":
            current = None
            continue
        for field, pattern in COMPILED_FIELD_RE.items():
            match = pattern.match(line)
            if match:
                current[field] = match.group(1)
    if not entries:
        raise CatalogError(f"{path}: no OfficialPlugin entries found")
    for index, entry in enumerate(entries):
        missing = [field for field in ("id", "repository", "pin") if field not in entry]
        if missing:
            raise CatalogError(f"{path}: entry {index} is missing {', '.join(missing)}")
    return entries


def check_pins(registry: Path, revision_arg: str | None, catalog: Path) -> int:
    """Fail when a compiled pin differs from the registry gitlink at a revision.

    Unlike ``--check`` this ignores ``SOURCE_REVISION`` and unrelated registry
    metadata, so it can be pointed at ``origin/main`` to detect a registry-side
    code change whose manifest hash (and therefore digest verification) stayed
    the same. Issue #2775.
    """
    revision = resolve_revision(registry, revision_arg)
    # The gate reads the local ref (no fetch). Print the compared commit's date
    # so a stale `origin/main` is visible instead of silently passing.
    committed = run_git(registry, "show", "-s", "--format=%cI", revision).strip()
    entries = parse_compiled_entries(catalog)
    mismatches: list[str] = []
    for entry in entries:
        repository = entry["repository"]
        repo_name = repository.removesuffix(".git").rsplit("/", 1)[-1]
        expected = pin_for(
            registry,
            revision,
            repo_name,
            repository,
            Path(f"<catalog entry {entry['id']}>"),
        )
        if entry["pin"] != expected:
            mismatches.append(
                f"{entry['id']}: compiled {entry['pin']} != registry {expected}"
            )
    if mismatches:
        raise CatalogError(
            f"{catalog} pins are stale for {registry} at {revision} "
            f"(committed {committed}): "
            + "; ".join(mismatches)
            + " (run `just catalog-generate` and commit the result)"
        )
    print(
        f"official catalog pins: OK ({len(entries)} entries, registry "
        f"{revision} committed {committed})"
    )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", help="bitty-plugins checkout to read")
    parser.add_argument(
        "--revision",
        help="registry commit to read (default BITTY_PLUGINS_REV or HEAD)",
    )
    parser.add_argument(
        "--source-repository",
        help="registry repository URL (default: the checkout's origin remote)",
    )
    parser.add_argument(
        "--output", default=str(DEFAULT_OUTPUT), help="generated Rust file"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when the checked-in catalog is stale (no write)",
    )
    parser.add_argument(
        "--check-pins",
        action="store_true",
        help=(
            "fail when a compiled pin differs from the registry gitlink at "
            "--revision (ignores SOURCE_REVISION and manifest hashes)"
        ),
    )
    args = parser.parse_args(argv)
    try:
        registry = resolve_registry(args.registry)
        output = Path(args.output)
        if args.check_pins:
            if args.check:
                raise CatalogError("--check and --check-pins are mutually exclusive")
            return check_pins(registry, args.revision, output)
        text, count, revision = render(registry, args.revision, args.source_repository)
        if args.check:
            try:
                existing = output.read_text(encoding="utf-8")
            except OSError as error:
                raise CatalogError(
                    f"cannot read checked-in catalog {output}: {error}"
                ) from error
            if existing != text:
                raise CatalogError(
                    f"{output} is stale for {registry} at {revision}; "
                    "run `just catalog-generate` and commit the result"
                )
            print(f"official catalog: OK ({count} entries, source {revision})")
            return 0
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(text, encoding="utf-8")
        print(f"official catalog: wrote {count} entries from {revision} to {output}")
        return 0
    except CatalogError as error:
        print(f"official catalog: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
