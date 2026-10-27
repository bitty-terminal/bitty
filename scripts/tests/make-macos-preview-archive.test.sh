#!/usr/bin/env bash
# make-macos-preview-archive.test.sh — fixture test for the macOS preview packer.
#
# Uses an ephemeral shell-script fake binary plus the repo's own
# LICENSE/README/CHANGELOG (no network, no macOS, no cargo build): dry-run
# staging, real assembly, the fixed member list, the executable bit, and the
# argument-validation failures that keep the accepted asset name and arm64-only
# matrix from drifting. The fake is not a Mach-O, so fixture invocations pass
# --skip-arch-check; the production workflow never does.
# Follows the PASS:/FAIL: convention of the other scripts/tests/*.test.sh gates.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/make-macos-preview-archive.sh
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

command -v tar >/dev/null 2>&1 || {
  echo "SKIP: tar not found"
  exit 0
}
command -v zstd >/dev/null 2>&1 || {
  echo "SKIP: zstd not found"
  exit 0
}

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/make-macos-preview-archive-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

VERSION="0.0.30"
TARGET="aarch64-apple-darwin"
TOPDIR="bitty-$VERSION-$TARGET"
OUT="$TMP_BASE/$TOPDIR.tar.zst"

FAKE_BIN="$TMP_BASE/bitty-fixture"
printf '#!/bin/sh\necho "bitty %s"\n' "$VERSION" >"$FAKE_BIN"
chmod +x "$FAKE_BIN"

# --- dry-run stages the payload without writing the archive ---
if "$SCRIPT" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" --dry-run --skip-arch-check >/dev/null 2>&1; then
  if [[ -e "$OUT" ]]; then
    fail "dry-run wrote the archive"
  else
    pass "dry-run validates and stages without writing"
  fi
else
  fail "dry-run exited non-zero"
fi

# --- real assembly ---
if ! "$SCRIPT" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" --skip-arch-check >/dev/null 2>&1; then
  fail "assembly exited non-zero"
fi
[[ -f "$OUT" ]] || fail "archive not created"

# --- fixed member list (files only, directories excluded) ---
EXPECTED="$(
  {
    echo "$TOPDIR/bin/bitty"
    echo "$TOPDIR/LICENSE"
    echo "$TOPDIR/README.md"
    echo "$TOPDIR/CHANGELOG.md"
  } | LC_ALL=C sort
)"
ACTUAL="$(zstd -dc "$OUT" | tar -tf - | grep -v '/$' | LC_ALL=C sort)"
if [[ "$ACTUAL" == "$EXPECTED" ]]; then
  pass "archive carries exactly the macOS preview payload"
else
  fail "archive payload mismatch:
$ACTUAL"
fi

# --- one top-level directory and an executable bin/bitty ---
if [[ "$(zstd -dc "$OUT" | tar -tf - | cut -d/ -f1 | sort -u)" == "$TOPDIR" ]]; then
  pass "archive extracts under one top-level directory"
else
  fail "archive does not extract under $TOPDIR"
fi
DEST="$TMP_BASE/extract"
mkdir -p "$DEST"
zstd -dc "$OUT" | tar -xf - -C "$DEST"
if [[ -x "$DEST/$TOPDIR/bin/bitty" ]]; then
  pass "bin/bitty keeps the executable bit"
else
  fail "bin/bitty is missing or not executable"
fi
if "$DEST/$TOPDIR/bin/bitty" --version | grep -qF "bitty $VERSION"; then
  pass "extracted binary runs"
else
  fail "extracted binary did not report $VERSION"
fi

# --- the fixture-only arch opt-out is explicit and loud ---
if "$SCRIPT" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$TMP_BASE/skip/$TOPDIR.tar.zst" --dry-run --skip-arch-check \
  >"$TMP_BASE/skip.log" 2>&1 &&
  grep -qF -- "--skip-arch-check: arm64 slice assertion skipped" "$TMP_BASE/skip.log"; then
  pass "--skip-arch-check opts out of lipo with a warning"
else
  fail "--skip-arch-check did not opt out loudly"
fi

# --- accepted matrix and asset name are enforced ---
if "$SCRIPT" --version "$VERSION" --target "x86_64-apple-darwin" \
  --binary "$FAKE_BIN" --output "$TMP_BASE/wrong-$TARGET.tar.zst" >/dev/null 2>&1; then
  fail "non-arm64 target was accepted"
else
  pass "non-arm64 target is rejected"
fi
if "$SCRIPT" --version "$VERSION" --binary "$FAKE_BIN" \
  --output "$TMP_BASE/bitty-$VERSION-macos.tar.zst" >/dev/null 2>&1; then
  fail "unexpected output basename was accepted"
else
  pass "unexpected output basename is rejected"
fi
if "$SCRIPT" --version "0.0" --binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
  fail "non-X.Y.Z version was accepted"
else
  pass "non-X.Y.Z version is rejected"
fi
if "$SCRIPT" --version "$VERSION" --binary "$TMP_BASE/missing" --output "$OUT" >/dev/null 2>&1; then
  fail "missing binary was accepted"
else
  pass "missing binary is rejected"
fi
if "$SCRIPT" --help >/dev/null 2>&1; then
  pass "--help exits zero"
else
  fail "--help exited non-zero"
fi

if ((FAIL)); then
  echo "make-macos-preview-archive-test: FAIL" >&2
  exit 1
fi
echo "make-macos-preview-archive-test: OK"
