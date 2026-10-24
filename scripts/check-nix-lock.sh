#!/usr/bin/env bash
# check-nix-lock.sh — resolve flake.lock and fail on any hash/metadata drift.
#
# The v0.0.27 release failed in the Cachix publish job with
#
#   error: NAR hash mismatch in input
#   'github:NixOS/nixpkgs/5adc50831c259f87faeb09ddf930d43597197473?...',
#   expected 'sha256-hVMDyAXU0kxRuG3GygbZkzf5x+aBNEC0NYEJ6KwHtD4='
#   but got 'sha256-Q/8KLB6jNfpaUN1HAedE/u/Et9feE/zYSsutBLKrCFk='
#
# because flake.lock carried a stale nixpkgs narHash (and stale lastModified
# values for every github input). The Nix job only runs on release tags, so PR
# CI never resolved the lock and the defect reached the tag (#2568).
#
# This gate is deliberately build-free. It:
#   1. rejects malformed or placeholder narHash values in flake.lock;
#   2. resolves every locked input (`nix flake metadata --json`) so an
#      unfetchable or unreachable input fails with a clear message;
#   3. evaluates the x86_64-linux package graph, which is the step that makes
#      Nix compare every locked narHash and rev/lastModified value with the
#      fetched tree (`nix flake metadata` alone performs neither check);
#   4. fails when resolving the flake rewrote flake.lock, so a stale lock is
#      never silently "fixed" in CI instead of committed.
#
# Usage: scripts/check-nix-lock.sh [--root DIR]
set -euo pipefail

usage() {
  echo "Usage: scripts/check-nix-lock.sh [--root DIR]"
}

fail() {
  echo "check-nix-lock: FAIL: $1" >&2
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

LOCK="$ROOT/flake.lock"
[[ -f "$LOCK" ]] || fail "missing $LOCK"

# --- 1. Static sanity: every locked narHash must be a real SRI sha256 --------
# A placeholder (the retired `sha256-AAAA...` lock) or a truncated value fails
# here without touching the network. Parsing stays in pure bash so the guard
# also runs in minimal Nix images that ship neither sed nor coreutils.
checked=0
while IFS= read -r line; do
  [[ $line == *'"narHash"'* ]] || continue
  hash="${line#*\"sha256-}"
  [[ $hash != "$line" ]] || fail "malformed narHash line in flake.lock: $line"
  hash="sha256-${hash%%\"*}"
  checked=$((checked + 1))
  [[ ${#hash} -eq 51 ]] || fail "malformed narHash in flake.lock: $hash"
  [[ $hash =~ ^sha256-[A-Za-z0-9+/]{43}=$ ]] || fail "malformed narHash in flake.lock: $hash"
  # A real hash draws from the 64-character base64 alphabet; the retired
  # placeholder lock used one repeated character (`sha256-AAAA...`).
  body="${hash#sha256-}"
  seen=""
  for ((i = 0; i < ${#body}; i++)); do
    ch="${body:i:1}"
    [[ $seen == *"$ch"* ]] || seen+="$ch"
  done
  ((${#seen} > 4)) || fail "placeholder narHash in flake.lock: $hash"
done <"$LOCK"
((checked > 0)) || fail "flake.lock has no narHash entries"
echo "check-nix-lock: $checked locked narHash value(s) look well-formed"

# --- 2./3. Let Nix verify the lock ------------------------------------------
command -v nix >/dev/null 2>&1 ||
  fail "nix is required to resolve flake.lock; install Nix with flakes enabled"

NIX=(nix --extra-experimental-features "nix-command flakes")
TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-nix-lock.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "check-nix-lock: resolving flake inputs (nix flake metadata)"
"${NIX[@]}" flake metadata --json "$ROOT" >"$TMP/metadata.json" ||
  fail "flake.lock does not resolve; run 'nix flake lock' in the repository root, fix any reported hash, and commit flake.lock"

# Summarize the resolved inputs for the CI log. python3 is used by the release
# workflow already; a missing interpreter only skips the summary.
if command -v python3 >/dev/null 2>&1; then
  python3 - "$TMP/metadata.json" <<'PY' || true
import json, sys
meta = json.load(open(sys.argv[1]))
for name, node in sorted(meta.get("locks", {}).get("nodes", {}).items()):
    if name == "root":
        continue
    locked = node.get("locked", {})
    print(f"  {name}: {locked.get('rev', '?')} ({locked.get('narHash', '?')})")
PY
fi

echo "check-nix-lock: evaluating .#packages.x86_64-linux.bitty (no build)"
"${NIX[@]}" eval --raw "$ROOT#packages.x86_64-linux.bitty.drvPath" >/dev/null ||
  fail "flake evaluation rejects flake.lock (narHash or rev/lastModified drift); run 'nix flake lock' and commit the result"

# --- 4. The lock must already be canonical ----------------------------------
if git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  if ! git -C "$ROOT" diff --quiet -- flake.lock; then
    git -C "$ROOT" --no-pager diff -- flake.lock >&2 || true
    fail "resolving the flake rewrote flake.lock; commit the regenerated lock"
  fi
else
  echo "check-nix-lock: note: $ROOT is not a git work tree; skipping the lock-rewrite check"
fi

echo "check-nix-lock: PASS"
