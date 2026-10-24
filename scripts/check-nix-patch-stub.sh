#!/usr/bin/env bash
# check-nix-patch-stub.sh — reject crane dummy-source caching while the
# workspace patches external dependencies onto local crates (#2591).
#
# `Cargo.toml` redirects the external `bitty-execution` dependency's
# `bitty-pty` and `bitty-plugin-host` edges to local workspace crates:
#
#   [patch."https://github.com/bitty-terminal/bitty"]
#   bitty-pty = { path = "crates/bitty-pty" }
#   bitty-plugin-host = { path = "crates/bitty-plugin-host" }
#
# crane's dependency-only cache compiles from `mkDummySrc`, which replaces
# every workspace crate with an empty stub. `bitty-execution` then compiles
# against those stubs, which either fails with unresolved imports or, after a
# future stub grows the right names, silently links a fake PTY crate. The
# flake must therefore build from the real sources (`buildPackage` with
# `cargoArtifacts = null`) while at least one such patch path exists. Merely
# removing `cargoArtifacts` is not enough: `buildPackage` creates a
# dependency-only derivation internally unless the attribute is explicitly
# null.
#
# This guard runs without Nix or network access. It:
#   1. extracts the `[patch."https://github.com/bitty-terminal/bitty"]` paths;
#   2. fails when a patched path is missing or looks like a crane dummy stub;
#   3. fails when flake.nix calls buildDepsOnly/mkDummySrc or omits
#      `cargoArtifacts = null`.
#
# Usage: scripts/check-nix-patch-stub.sh [--root DIR]
set -euo pipefail

usage() {
  echo "Usage: scripts/check-nix-patch-stub.sh [--root DIR]"
}

fail() {
  echo "check-nix-patch-stub: FAIL: $1" >&2
  exit 1
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
while (($#)); do
  case "$1" in
  --root)
    [[ $# -ge 2 ]] || {
      usage >&2
      exit 2
    }
    ROOT="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    fail "unknown argument: $1"
    ;;
  esac
done

CARGO_TOML="$ROOT/Cargo.toml"
FLAKE="$ROOT/flake.nix"
[[ -f "$CARGO_TOML" ]] || fail "missing $CARGO_TOML"
[[ -d "$ROOT/crates" ]] || fail "missing $ROOT/crates"

# Extract "name<TAB>path" for every entry in the core-repository patch table.
# Parsing stays in awk so the guard needs no TOML library and no network.
patch_entries() {
  awk '
    /^\[patch\."https:\/\/github\.com\/bitty-terminal\/bitty"\]/ { in_patch = 1; next }
    /^\[/ { in_patch = 0 }
    in_patch && /path[[:space:]]*=/ {
      line = $0
      name = line
      sub(/[[:space:]]*=.*/, "", name)
      sub(/^[[:space:]]*/, "", name)
      path = line
      sub(/.*path[[:space:]]*=[[:space:]]*"/, "", path)
      sub(/".*$/, "", path)
      print name "\t" path
    }
  ' "$CARGO_TOML"
}

# A crane stub is the generated one-file crate: it carries the dummy marker
# and is smaller than any real workspace crate (the stub observed in #2591 was
# 525 bytes). Both conditions together avoid flagging small real files.
is_crane_stub() {
  local file="$1"
  [[ -f "$file" ]] || return 1
  (($(wc -c <"$file") < 1024)) || return 1
  grep -q "unused_extern_crates" "$file" &&
    grep -q "pub fn main() {}" "$file"
}

entries="$(patch_entries)"
if [[ -z "$entries" ]]; then
  echo "check-nix-patch-stub: no local core-repository patch paths; dependency caching is allowed"
  echo "check-nix-patch-stub: PASS"
  exit 0
fi

count=0
while IFS=$'\t' read -r name path; do
  [[ -n "$name" ]] || continue
  count=$((count + 1))
  crate_dir="$ROOT/$path"
  lib="$crate_dir/src/lib.rs"
  [[ -d "$crate_dir" ]] || fail "$name patches to missing path: $path"
  [[ -f "$lib" ]] || fail "$name patches to $path without src/lib.rs"
  if is_crane_stub "$lib"; then
    fail "$name at $path is a crane dummy stub; the external patched dependency would compile against it"
  fi
  grep -Rqs "pub " "$crate_dir/src" ||
    fail "$name at $path has no public API; refusing a stubbed dependency"
done <<<"$entries"

echo "check-nix-patch-stub: $count local patch path(s) require real workspace sources"

[[ -f "$FLAKE" ]] || fail "missing $FLAKE"
# Comments are allowed to explain the rule; only executable lines are matched.
if grep -vE '^[[:space:]]*#' "$FLAKE" | grep -q "buildDepsOnly"; then
  fail "flake.nix calls buildDepsOnly while $count workspace patch path(s) exist; it would compile external crates against crane stubs"
fi
if grep -vE '^[[:space:]]*#' "$FLAKE" | grep -q "mkDummySrc"; then
  fail "flake.nix calls mkDummySrc while $count workspace patch path(s) exist; it would stub patched workspace crates"
fi
if ! grep -qE '^[[:space:]]*cargoArtifacts[[:space:]]*=[[:space:]]*null[[:space:]]*;' "$FLAKE"; then
  fail "flake.nix must set 'cargoArtifacts = null;' while $count workspace patch path(s) exist (buildPackage creates a stub-source dependency cache otherwise)"
fi

echo "check-nix-patch-stub: flake.nix builds from real sources"
echo "check-nix-patch-stub: PASS"
