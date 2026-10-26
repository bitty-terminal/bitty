#!/usr/bin/env bash
# check-vendored-winit.test.sh — CTX-1605 fixture test for the vendored-winit
# resolution gate.
#
# The production state must pass, and every way the patch can be lost must
# fail closed:
#   metadata-registry.json   -> winit resolved from crates.io (source set)
#   metadata-outside.json    -> winit resolved from a path outside the tree
#   Cargo.lock.registry      -> lock records a registry source/checksum
#   Cargo.lock.checksum-only -> lock records a checksum without a source
#   PATCHES.md (edited)      -> tree digest does not match
#   PATCHES.md (no row)      -> tree digest is not recorded
#   missing tree manifest    -> vendored tree is not there
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-vendored-winit.sh
FIXTURES=scripts/tests/fixtures/vendored-winit
TREE=$PWD/vendor/winit
PATCHES=$TREE/PATCHES.md
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
FAIL=0

render() {
  sed -e "s|@TREE@|$TREE|g" -e "s|@REPO@|$PWD|g" "$1" >"$2"
}

pass_case() {
  local name=$1
  shift
  local out status
  out=$("$@" 2>&1) || status=$?
  status="${status:-0}"
  if ((status != 0)); then
    echo "FAIL: $name should pass, exited $status:" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

fail_case() {
  local name=$1 pattern=$2
  shift 2
  local out status
  out=$("$@" 2>&1) || status=$?
  status="${status:-0}"
  if ((status == 0)); then
    echo "FAIL: $name should fail, exited 0:" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  elif ! grep -qF "$pattern" <<<"$out"; then
    echo "FAIL: $name failed without '$pattern':" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# Production state: the lock, metadata, and tree all agree.
pass_case "repository state" "$GATE"

# Synthetic path-resolved metadata: the gate accepts the path criteria without
# running cargo (this is how the fixture cases isolate one failure each).
render "$FIXTURES/metadata-path.json" "$TMP/metadata-path.json"
pass_case "path-resolved metadata" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.path"

render "$FIXTURES/metadata-registry.json" "$TMP/metadata-registry.json"
fail_case "registry-resolved metadata" "resolved from" \
  "$GATE" --metadata "$TMP/metadata-registry.json" --lock "$FIXTURES/Cargo.lock.path"

render "$FIXTURES/metadata-outside.json" "$TMP/metadata-outside.json"
fail_case "metadata pointing outside the tree" "expected" \
  "$GATE" --metadata "$TMP/metadata-outside.json" --lock "$FIXTURES/Cargo.lock.path"

render "$FIXTURES/metadata-path.json" "$TMP/metadata-path.json"
fail_case "registry-resolved lock" "records a \`source\`" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.registry"
fail_case "lock with a checksum but no source" "records a \`checksum\`" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.checksum-only"

# Digest drift: every upstream file edit must update PATCHES.md in the same
# commit, so an old digest fails.
sed 's/`c0c4343e576b91e6c1e88f7f1f2ba3c1d0cd9519533d0b554492385ee0510a47`/`0000000000000000000000000000000000000000000000000000000000000000`/' \
  "$PATCHES" >"$TMP/PATCHES.bad-digest.md"
if cmp -s "$PATCHES" "$TMP/PATCHES.bad-digest.md"; then
  echo "FAIL: digest substitution did not change PATCHES.md (row reworded?)" >&2
  FAIL=1
fi
fail_case "stale tree digest" "tree digest is" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.path" \
  --patches "$TMP/PATCHES.bad-digest.md"

grep -v 'Tree digest' "$PATCHES" >"$TMP/PATCHES.no-digest.md"
fail_case "PATCHES.md without a digest row" "no \`Tree digest\` row" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.path" \
  --patches "$TMP/PATCHES.no-digest.md"

fail_case "missing vendored manifest" "missing vendored manifest" \
  "$GATE" --metadata "$TMP/metadata-path.json" --lock "$FIXTURES/Cargo.lock.path" \
  --tree "$TMP/absent"

if ((FAIL)); then
  echo "check-vendored-winit-test: FAIL" >&2
  exit 1
fi
echo "check-vendored-winit-test: OK"
