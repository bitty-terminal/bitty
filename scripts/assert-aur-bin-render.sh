#!/usr/bin/env bash
# assert-aur-bin-render.sh — verify a rendered AUR `bitty-bin` recipe before
# it is pushed to the AUR.
#
# CTX-1436 (#2417). The `aur-bin` publish in `.github/workflows/release.yml`
# used to grep the rendered PKGBUILD for the literal needle
# `https://cdn.bitty.run/releases/v${pkgver}/${ASSET}` (ASSET expanded) while
# the template renders the download name through `${_bitty_asset}`. The needle
# could never match, so the healthy v0.0.25 run failed only this channel after
# every other publisher had gone green. This script is the single publish-time
# check: it resolves the recipe's variables the way `makepkg` expands them,
# then pins the R2/CDN host, the verified digest, and (when given) the
# `.SRCINFO` that ships to the AUR.
#
# The CDN host stays a literal on purpose: it is the channel contract
# (CTX-1413, #1847), and a recipe that can download from anywhere else must
# never reach the AUR. Only the version and asset segments are resolved.
#
# Usage: scripts/assert-aur-bin-render.sh <PKGBUILD> <version> <sha256> [<SRCINFO>]
#   <PKGBUILD>  rendered PKGBUILD (pkgver substituted, digest filled in)
#   <version>   release version without the leading `v` (for example 0.0.26)
#   <sha256>    verified lowercase 64-hex digest of the binary asset
#   <SRCINFO>   optional generated `.SRCINFO`; when present it must describe
#               the same binary URL and digest as the PKGBUILD
#
# stdout: one PASS line. stderr: failure diagnostics.
# Exit: 0 only when every check passes; 1 on usage errors or drift.
set -euo pipefail

usage() {
  printf 'usage: %s <PKGBUILD> <version> <sha256> [<SRCINFO>]\n' \
    "$(basename "${BASH_SOURCE[0]}")" >&2
  exit 1
}

fail() {
  printf 'assert-aur-bin-render: FAIL: %s\n' "$1" >&2
  exit 1
}

[[ $# -ge 3 && $# -le 4 ]] || usage

PKGBUILD_PATH="$1"
VERSION="$2"
SHA="$3"
SRCINFO_PATH="${4:-}"

# The pinned prefix, byte for byte. `${pkgver}` stays unexpanded here because
# that is what the template must carry; the resolved URL is checked below.
readonly CDN_PREFIX='https://cdn.bitty.run/releases/v${pkgver}/'
readonly CDN_HOST='https://cdn.bitty.run/releases/'

[[ -n "$VERSION" ]] || usage
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+-]*$ ]] ||
  fail "version is not a rendered semver without a leading v: ${VERSION}"
printf '%s' "$SHA" | grep -Eq '^[0-9a-f]{64}$' ||
  fail "sha256 is not a lowercase 64-hex digest: ${SHA:-<empty>}"
[[ -f "$PKGBUILD_PATH" ]] || fail "PKGBUILD not found: $PKGBUILD_PATH"

# The publish job substitutes these; a leftover placeholder or a stale pkgver
# means the sed step did not run against this copy.
grep -qxF "pkgver=$VERSION" "$PKGBUILD_PATH" ||
  fail "rendered PKGBUILD does not set pkgver=$VERSION"
if grep -qF 'REPLACE_WITH_RELEASE_SHA256' "$PKGBUILD_PATH"; then
  fail "rendered PKGBUILD still carries the sha256 placeholder"
fi

ASSET="$(awk -F'"' '/^_bitty_asset=/{print $2; exit}' "$PKGBUILD_PATH")"
[[ -n "$ASSET" ]] || fail "rendered PKGBUILD does not declare _bitty_asset"
grep -qxF "_bitty_asset=\"$ASSET\"" "$PKGBUILD_PATH" ||
  fail "rendered PKGBUILD declares an unexpected _bitty_asset"

# 1) The R2/CDN host and version-pinned path must be present literally. A
#    fully resolved URL alone would let the template drift to another host.
grep -qF "$CDN_PREFIX" "$PKGBUILD_PATH" ||
  fail "rendered PKGBUILD does not pin the R2/CDN prefix ${CDN_PREFIX}"

# 2) Resolve the binary source entry the way makepkg expands it, then compare
#    the URL the recipe will actually download. This is the check the old
#    literal needle intended: it fails on a different host, a different asset
#    variable, or a stale version.
SOURCE_ENTRY="$(awk '/^source_x86_64=\(/,/^\)/' "$PKGBUILD_PATH" |
  grep -F '_bitty_src' | head -n 1)"
[[ -n "$SOURCE_ENTRY" ]] || fail "source_x86_64 has no _bitty_src entry"

SRC_NAME="$(awk -F'"' '/^_bitty_src=/{print $2; exit}' "$PKGBUILD_PATH")"
[[ -n "$SRC_NAME" ]] || fail "rendered PKGBUILD does not declare _bitty_src"
SRC_NAME="${SRC_NAME//\$\{pkgver\}/$VERSION}"

RESOLVED_ENTRY="${SOURCE_ENTRY//\$\{pkgver\}/$VERSION}"
RESOLVED_ENTRY="${RESOLVED_ENTRY//\$\{_bitty_asset\}/$ASSET}"
RESOLVED_ENTRY="${RESOLVED_ENTRY//\$\{_bitty_src\}/$SRC_NAME}"
RESOLVED_URL="${RESOLVED_ENTRY#*::}"
RESOLVED_URL="${RESOLVED_URL%\"}"
RESOLVED_URL="${RESOLVED_URL%"${RESOLVED_URL##*[![:space:]]}"}"

EXPECTED_URL="${CDN_PREFIX//\$\{pkgver\}/$VERSION}${ASSET}"
[[ "$RESOLVED_URL" == "$EXPECTED_URL" ]] ||
  fail "rendered binary source resolves to '${RESOLVED_URL}', expected '${EXPECTED_URL}'"
case "$RESOLVED_URL" in
"$CDN_HOST"*) ;;
*) fail "rendered binary source does not use the R2/CDN host: $RESOLVED_URL" ;;
esac

# 3) The digest the publish job verified must be the binary slot, not some
#    other array element, and never `SKIP` (makepkg would not verify).
BIN_SHA="$(awk '/^sha256sums_x86_64=\(/,/^\)/' "$PKGBUILD_PATH" |
  grep -oE '([0-9a-f]{64}|SKIP|REPLACE_WITH_RELEASE_SHA256)' | head -n 1 || true)"
[[ "$BIN_SHA" == "$SHA" ]] ||
  fail "binary sha256 slot is '${BIN_SHA:-<missing>}', expected the verified digest"

# 4) The `.SRCINFO` that ships to the AUR must describe the same download. A
#    stale generated file would publish a recipe macOS/Arch users cannot fetch.
if [[ -n "$SRCINFO_PATH" ]]; then
  [[ -f "$SRCINFO_PATH" ]] || fail "SRCINFO not found: $SRCINFO_PATH"
  srcinfo_has() { # <exact line, ignoring leading indentation>
    awk -v want="$1" '
      { line = $0; sub(/^[[:space:]]+/, "", line) }
      line == want { found = 1; exit }
      END { exit(found ? 0 : 1) }
    ' "$SRCINFO_PATH"
  }
  srcinfo_has "pkgname = bitty-bin" || fail ".SRCINFO does not declare pkgname = bitty-bin"
  srcinfo_has "pkgver = $VERSION" || fail ".SRCINFO pkgver is not $VERSION"
  srcinfo_has "source_x86_64 = ${SRC_NAME}::${EXPECTED_URL}" ||
    fail ".SRCINFO source_x86_64 does not resolve to ${SRC_NAME}::${EXPECTED_URL}"
  srcinfo_has "sha256sums_x86_64 = $SHA" ||
    fail ".SRCINFO does not carry the verified binary digest"
fi

printf 'assert-aur-bin-render: PASS (bitty-bin %s, asset %s, R2/CDN)\n' \
  "$VERSION" "$ASSET"
