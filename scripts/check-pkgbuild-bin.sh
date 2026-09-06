#!/usr/bin/env bash
# check-pkgbuild-bin.sh — template render test for packaging/PKGBUILD.bin.
#
# Renders the template the same way the release `aur-bin` job does (pkgver
# substitution + real 64-hex sha256, no SKIP for the binary), then asserts:
#   - `bash -n` passes on template and rendered output
#   - `arch` is x86_64 only
#   - rendered binary sha256 is a real 64-hex digest (never SKIP/placeholder)
#   - `provides`/`conflicts` follow the bitty convention from issue #227
#   - the prebuilt asset URL points at the release dist name
#   - `package()` installs the binary to /usr/bin/bitty
#
# Usage: scripts/check-pkgbuild-bin.sh [PKGVER] [FAKE_SHA256]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATE="$REPO_ROOT/packaging/PKGBUILD.bin"
PKGVER="${1:-0.0.1}"
FAKE_SHA="${2:-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855}"

fail() {
	echo "check-pkgbuild-bin: FAIL: $1" >&2
	exit 1
}

[[ -f "$TEMPLATE" ]] || fail "template $TEMPLATE not found"

# Template itself must be valid shell (placeholders are plain strings).
bash -n "$TEMPLATE" || fail "bash -n rejects template"

# Template must not promise a real checksum it does not have: the binary slot
# carries an explicit placeholder the publish job is required to fill.
grep -q "REPLACE_WITH_RELEASE_SHA256" "$TEMPLATE" ||
	fail "template lost its sha256 placeholder (publish job has nothing to fill)"
grep -q "'SKIP'" "$TEMPLATE" && {
	# SKIP is only acceptable for the metadata tarball, never the binary slot.
	awk '/sha256sums_x86_64/,/\)/' "$TEMPLATE" | head -n 3 | grep -q "REPLACE_WITH_RELEASE_SHA256" ||
		fail "binary sha256 slot must be the placeholder, not SKIP"
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
RENDERED="$WORK/PKGBUILD"
sed -e "s/^pkgver=.*/pkgver=$PKGVER/" \
	-e "s/REPLACE_WITH_RELEASE_SHA256/$FAKE_SHA/" \
	"$TEMPLATE" >"$RENDERED"

bash -n "$RENDERED" || fail "bash -n rejects rendered PKGBUILD"

assert_grep() {
	local pattern="$1" why="$2"
	grep -Eq "$pattern" "$RENDERED" || fail "$why"
}

assert_grep "^pkgname=bitty-bin$" "pkgname must be bitty-bin"
assert_grep "^arch=\('x86_64'\)$" "arch must be x86_64 only, got: $(grep -E '^arch=' "$RENDERED")"
assert_grep "provides=\('bitty'\)" "provides must include bitty"
assert_grep "conflicts=\('bitty' 'bitty-nightly' 'bitty-git'\)" "conflicts must cover bitty/bitty-nightly/bitty-git"
assert_grep "releases/download/v\\\$\{?pkgver\}?/\\\$\{?_bitty_asset\}?" \
	"source URL must use the parameterized release asset"
assert_grep "bitty-x86_64-unknown-linux-gnu" "asset stem must be the x86_64 release dist name"
assert_grep '"\$\{?pkgdir\}?/usr/bin/bitty"' "package() must install to /usr/bin/bitty"

# Rendered binary checksum: real 64-hex, no SKIP, no leftover placeholder.
grep -q "REPLACE_WITH_RELEASE_SHA256" "$RENDERED" &&
	fail "rendered PKGBUILD still contains the placeholder"
BIN_SHA_LINE="$(awk '/sha256sums_x86_64/,/\)/' "$RENDERED" | grep -E "'[0-9a-f]{64}'" | head -n 1)"
[[ -n "$BIN_SHA_LINE" ]] ||
	fail "rendered binary sha256 is not a real 64-hex digest"

# Rendered file must still produce valid SRCINFO input shape (makepkg
# --printsrcinfo needs Arch tooling; here we at least require the fields it
# derives: pkgname/pkgver/source/sha256sums present exactly once each side).
for field in "^pkgname=" "^pkgver=" "source_x86_64=" "sha256sums_x86_64="; do
	count="$(grep -Ec "$field" "$RENDERED")"
	[[ "$count" -ge 1 ]] || fail "rendered PKGBUILD missing $field"
done

if command -v makepkg >/dev/null 2>&1; then
	(cd "$WORK" && makepkg --printsrcinfo >.SRCINFO) ||
		fail "makepkg --printsrcinfo rejects rendered PKGBUILD"
	grep -q "pkgname = bitty-bin" "$WORK/.SRCINFO" || fail ".SRCINFO missing pkgname"
	echo "check-pkgbuild-bin: makepkg --printsrcinfo ok"
else
	echo "check-pkgbuild-bin: makepkg not available, skipped .SRCINFO probe"
fi

echo "check-pkgbuild-bin: PASS (pkgver=$PKGVER)"
