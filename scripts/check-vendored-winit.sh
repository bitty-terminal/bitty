#!/usr/bin/env bash
# check-vendored-winit.sh — CTX-1605 resolution gate for the patched winit.
#
# Usage: scripts/check-vendored-winit.sh [OPTIONS]
#
# The workspace takes `winit 0.30.13` from `vendor/winit` through
# `[patch.crates-io]`. If the requirement or the lock resolve back to the
# crates.io registry, cargo only prints `warning: patch for `winit` was not
# used in the crate graph`, the build stays green, and the Wayland
# `WindowEvent::Occluded` edge this repository patches in silently disappears.
# This gate fails closed on that state and on any drift of the vendored tree
# from the digest recorded in `vendor/winit/PATCHES.md`.
#
# Checks (no build; needs the local cargo index/cache):
#   1. `Cargo.lock` records no `source` or `checksum` for `winit`.
#   2. `cargo metadata --locked` resolves `winit` with a null `source` and a
#      manifest at `<tree>/Cargo.toml`.
#   3. The whole vendored tree hashes to the `Tree digest` pinned in
#      `PATCHES.md` (files are hashed as `sha256sum` prints them, sorted under
#      `LC_ALL=C`; `PATCHES.md` and `patches/` are metadata, not vendored
#      source, and are excluded).
#   4. The recorded patch file (`Patch` row) hashes to the `Patch SHA-256`
#      pinned in `PATCHES.md`. The tree digest deliberately excludes
#      `patches/`, so this row is what protects the recorded diff itself.
#
# Options (fixture tests use them; the defaults are the repository state):
#   --tree DIR       vendored crate directory (default: vendor/winit)
#   --lock FILE      lockfile to inspect (default: Cargo.lock)
#   --patches FILE   PATCHES.md carrying the tree digest and patch hash
#   --metadata FILE  pre-generated `cargo metadata --format-version 1` JSON
#                    (default: run `cargo metadata --locked` in this checkout)
#   -h, --help       print this usage
set -euo pipefail

cd "$(dirname "$0")/.."

tree=vendor/winit
lock=Cargo.lock
patches="$tree/PATCHES.md"
metadata=""

usage() {
  awk 'NR > 3 && /^set -euo pipefail$/ { exit } NR > 3 { sub(/^# ?/, ""); print }' "$0"
}

while (($#)); do
  case "$1" in
  --tree)
    tree=${2:?--tree needs a value}
    shift 2
    ;;
  --lock)
    lock=${2:?--lock needs a value}
    shift 2
    ;;
  --patches)
    patches=${2:?--patches needs a value}
    shift 2
    ;;
  --metadata)
    metadata=${2:?--metadata needs a value}
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "check-vendored-winit: unknown argument: $1" >&2
    usage >&2
    exit 2
    ;;
  esac
done

tmp=""
cleanup() {
  if [[ -n $tmp ]]; then
    rm -f "$tmp"
  fi
}
trap cleanup EXIT INT TERM

if [[ -z $metadata ]]; then
  tmp="$(mktemp)"
  if ! cargo metadata --locked --format-version 1 >"$tmp"; then
    echo "check-vendored-winit: cargo metadata --locked failed" >&2
    exit 1
  fi
  metadata=$tmp
fi

for path in "$metadata" "$lock" "$patches"; do
  if [[ ! -f $path ]]; then
    echo "check-vendored-winit: missing file: $path" >&2
    exit 1
  fi
done
if [[ ! -f "$tree/Cargo.toml" ]]; then
  echo "check-vendored-winit: missing vendored manifest: $tree/Cargo.toml" >&2
  exit 1
fi

python3 - "$metadata" "$lock" "$tree" "$patches" <<'PY'
import hashlib
import json
import os
import pathlib
import re
import sys

metadata_path, lock_path, tree_path, patches_path = sys.argv[1:5]
failures = []


def fail(message):
    failures.append(message)


# 1. Resolution through cargo metadata: the patch must be the source of winit.
try:
    metadata = json.loads(pathlib.Path(metadata_path).read_text(encoding="utf-8"))
except (OSError, ValueError) as error:
    fail(f"cannot read cargo metadata {metadata_path}: {error}")
    metadata = None
if metadata is not None:
    packages = [p for p in metadata.get("packages", []) if p.get("name") == "winit"]
    if not packages:
        fail("cargo metadata reports no `winit` package")
    else:
        package = packages[0]
        source = package.get("source")
        if source is not None:
            fail(
                f"winit is resolved from {source}, not from the vendored path; "
                "restore [patch.crates-io] or follow vendor/winit/PATCHES.md"
            )
        expected = os.path.realpath(os.path.join(tree_path, "Cargo.toml"))
        actual = os.path.realpath(package.get("manifest_path") or "")
        if actual != expected:
            fail(f"winit resolves to {actual}, expected {expected}")

# 2. The lockfile records a registry source only for registry packages.
lock_text = pathlib.Path(lock_path).read_text(encoding="utf-8")
blocks = [block for block in lock_text.split("[[package]]") if re.search(r'^name = "winit"$', block, re.M)]
if not blocks:
    fail(f"{lock_path} has no `winit` package block")
elif len(blocks) > 1:
    fail(f"{lock_path} has {len(blocks)} `winit` package blocks")
else:
    block = blocks[0]
    if re.search(r"^source = ", block, re.M):
        fail(
            f"{lock_path} records a `source` for winit (registry resolution); "
            "the vendored patch is inactive"
        )
    if re.search(r"^checksum = ", block, re.M):
        fail(
            f"{lock_path} records a `checksum` for winit, which only registry packages carry"
        )

# 3. Whole-tree digest pinned in PATCHES.md.
patches_text = pathlib.Path(patches_path).read_text(encoding="utf-8")
match = re.search(r"\|\s*Tree digest\s*\|\s*`?([0-9a-f]{64})`?\s*\|", patches_text)
if match is None:
    fail(f"{patches_path} has no `Tree digest` row with a 64-hex value")
else:
    recorded = match.group(1)
    root = pathlib.Path(tree_path)
    entries = []
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            full = pathlib.Path(dirpath, name)
            relative = full.relative_to(root).as_posix()
            if relative == "PATCHES.md" or relative.startswith("patches/"):
                continue
            if full.is_symlink():
                continue
            entries.append(relative)
    entries.sort(key=lambda path: path.encode())
    digest = hashlib.sha256()
    for relative in entries:
        file_hash = hashlib.sha256((root / relative).read_bytes()).hexdigest()
        digest.update(f"{file_hash}  ./{relative}\n".encode())
    computed = digest.hexdigest()
    if computed != recorded:
        fail(
            f"vendored winit tree digest is {computed}, {patches_path} records {recorded}; "
            "recompute it after an intentional vendor change (procedure in PATCHES.md)"
        )

# 4. The recorded patch file itself: the tree digest above excludes `patches/`.
patch_row = re.search(r"\|\s*Patch\s*\|\s*`([^`]+)`\s*\|", patches_text)
patch_hash_row = re.search(
    r"\|\s*Patch SHA-256\s*\|\s*`?([0-9a-f]{64})`?\s*\|", patches_text
)
if patch_row is None:
    fail(f"{patches_path} has no `Patch` row naming the recorded diff")
elif patch_hash_row is None:
    fail(f"{patches_path} has no `Patch SHA-256` row with a 64-hex value")
else:
    patch_relative = patch_row.group(1)
    patch_file = pathlib.Path(tree_path, patch_relative)
    if not patch_file.is_file():
        fail(f"recorded patch {patch_relative} is missing under {tree_path}")
    else:
        computed_patch = hashlib.sha256(patch_file.read_bytes()).hexdigest()
        recorded_patch = patch_hash_row.group(1)
        if computed_patch != recorded_patch:
            fail(
                f"recorded patch {patch_relative} is {computed_patch}, "
                f"{patches_path} records {recorded_patch}; restore the fetched diff "
                "or update the `Patch SHA-256` row"
            )

if failures:
    for message in failures:
        print(f"check-vendored-winit: {message}", file=sys.stderr)
    sys.exit(1)

version = packages[0].get("version", "unknown") if metadata is not None else "unknown"
print(
    f"check-vendored-winit: winit {version} resolves to {tree_path}/Cargo.toml; "
    "lock is path-resolved; tree digest and patch hash match"
)
PY
