#!/usr/bin/env bash
# make-unix-bundle.test.sh — fixture test for scripts/make-unix-bundle.sh.
#
# Uses an ephemeral fake binary plus the repo's own LICENSE/README/CHANGELOG
# (no network, no cargo build): dry-run staging, real assembly, exact member
# listing, extraction with executable-bit check, `--version` smoke, and the
# argument-validation failures. Follows the PASS:/FAIL: convention of the
# other scripts/tests/*.test.sh gates.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/make-unix-bundle.sh
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

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/make-unix-bundle-test.XXXXXX")"
cleanup() {
	rm -rf "$TMP_BASE"
}
trap cleanup EXIT

VERSION="0.0.99"
TARGET="x86_64-unknown-linux-gnu"
TOPDIR="bitty-$VERSION-$TARGET"

# Fake binary: reports the fixture version like `bitty --version` does.
FAKE_BIN="$TMP_BASE/bitty-fixture"
printf '#!/bin/sh\necho "bitty %s"\n' "$VERSION" >"$FAKE_BIN"
chmod +x "$FAKE_BIN"

OUT="$TMP_BASE/$TOPDIR.tar.zst"

# --- dry-run stages the payload without writing the bundle ---
if "$SCRIPT" --version "$VERSION" --target "$TARGET" \
	--binary "$FAKE_BIN" --output "$OUT" --dry-run >/dev/null 2>&1; then
	if [[ -e "$OUT" ]]; then
		fail "dry-run wrote the bundle"
	else
		pass "dry-run validates and stages without writing"
	fi
else
	fail "dry-run exited non-zero"
fi

# --- real assembly ---
if ! "$SCRIPT" --version "$VERSION" --target "$TARGET" \
	--binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
	fail "assembly exited non-zero"
fi
[[ -f "$OUT" ]] || fail "bundle not created"

# --- exact member list (files only, directories excluded) ---
EXPECTED="$(
	{
		echo "$TOPDIR/bin/bitty"
		echo "$TOPDIR/LICENSE"
		echo "$TOPDIR/README.md"
		echo "$TOPDIR/CHANGELOG.md"
		echo "$TOPDIR/share/applications/run.bitty.Bitty.desktop"
		echo "$TOPDIR/share/metainfo/run.bitty.Bitty.metainfo.xml"
		for size in 16 32 64 128 256 512; do
			echo "$TOPDIR/share/icons/hicolor/${size}x${size}/apps/bitty.png"
		done
		echo "$TOPDIR/share/icons/hicolor/scalable/apps/bitty.svg"
	} | LC_ALL=C sort
)"
ACTUAL="$(tar -tf "$OUT" | grep -v '/$' | LC_ALL=C sort)"
if [[ "$ACTUAL" == "$EXPECTED" ]]; then
	pass "member list is exactly bin/share/LICENSE/README/CHANGELOG"
else
	fail "member list mismatch:
$ACTUAL"
fi

# --- single top-level directory (extraction never pollutes cwd) ---
TOPLEVELS="$(tar -tf "$OUT" | cut -d/ -f1 | sort -u)"
if [[ "$TOPLEVELS" == "$TOPDIR" ]]; then
	pass "single top-level directory $TOPDIR"
else
	fail "unexpected top-level entries: $TOPLEVELS"
fi

# --- extraction: executable bit, version smoke, doc payload ---
EXTRACT="$TMP_BASE/extract"
mkdir -p "$EXTRACT"
tar -xf "$OUT" -C "$EXTRACT"
if [[ -x "$EXTRACT/$TOPDIR/bin/bitty" ]]; then
	pass "bin/bitty is executable after extraction"
else
	fail "bin/bitty lost its executable bit"
fi
BIN_OUT="$("$EXTRACT/$TOPDIR/bin/bitty" --version)"
if [[ "$BIN_OUT" == *"$VERSION"* ]]; then
	pass "extracted binary reports version ($BIN_OUT)"
else
	fail "version mismatch: $BIN_OUT"
fi
for f in LICENSE README.md CHANGELOG.md; do
	if cmp -s "$f" "$EXTRACT/$TOPDIR/$f"; then
		pass "$f matches the repo file"
	else
		fail "$f differs from the repo file"
	fi
done

# --- argument validation failures ---
expect_fail() {
	local why="$1"
	shift
	if "$SCRIPT" "$@" >/dev/null 2>&1; then
		fail "accepted invalid input ($why)"
	else
		pass "rejects invalid input ($why)"
	fi
}
expect_fail "missing version" --target "$TARGET" --binary "$FAKE_BIN" --output "$OUT"
expect_fail "bad version" --version "0.0" --target "$TARGET" --binary "$FAKE_BIN" --output "$OUT"
expect_fail "missing target" --version "$VERSION" --binary "$FAKE_BIN" --output "$OUT"
expect_fail "bad target" --version "$VERSION" --target "not a triple!!" --binary "$FAKE_BIN" --output "$OUT"
expect_fail "missing binary" --version "$VERSION" --target "$TARGET" --binary "$TMP_BASE/nope" --output "$OUT"

if [[ "$FAIL" -ne 0 ]]; then
	echo "make-unix-bundle.test: FAIL" >&2
	exit 1
fi
echo "make-unix-bundle.test: PASS"
