#!/usr/bin/env bash
# check-release-version.sh — align Cargo workspace version with release tags.
#
# Modes:
#   scripts/check-release-version.sh
#       Assert in-repo consistency: workspace version in Cargo.toml equals
#       pkgver in PKGBUILD, packaging/PKGBUILD, packaging/PKGBUILD.bin and
#       version in nfpm.yaml.
#   scripts/check-release-version.sh --tag v0.0.20
#   scripts/check-release-version.sh 0.0.20
#       Additionally assert the in-repo version equals the given release tag
#       version (leading `v` optional). The release workflow runs this mode on
#       tag pushes so `bitty --version` always matches the release tag
#       (issue #227: version used to lie at 0.0.1 while packages moved on).
#
# Fails non-zero on any mismatch. Bumps happen by changing the workspace
# version (single source of truth) plus the packaging files together, in the
# release that ships them.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED_TAG_VERSION=""

if [[ "${1:-}" == "--tag" ]]; then
	EXPECTED_TAG_VERSION="${2:-}"
	shift 2 2>/dev/null || true
elif [[ -n "${1:-}" ]]; then
	EXPECTED_TAG_VERSION="$1"
	shift
fi
EXPECTED_TAG_VERSION="${EXPECTED_TAG_VERSION#v}"

fail() {
	echo "check-release-version: FAIL: $1" >&2
	exit 1
}

workspace_version() {
	# Single source of truth: [workspace.package] version in root Cargo.toml.
	local ver
	ver="$(awk '/^\[workspace\.package\]/{flag=1;next}/^\[/{flag=0}flag && /^version[[:space:]]*=/{gsub(/.*=[[:space:]]*"/,"");gsub(/".*/,"");print;exit}' "$REPO_ROOT/Cargo.toml")"
	[[ -n "$ver" ]] || fail "could not parse workspace version from Cargo.toml"
	printf '%s' "$ver"
}

pkgbuild_pkgver() {
	local file="$1" ver
	ver="$(awk -F= '/^pkgver=/{gsub(/["'"'"']/,"",$2);print $2;exit}' "$file")"
	[[ -n "$ver" ]] || fail "could not parse pkgver from $file"
	printf '%s' "$ver"
}

nfpm_version() {
	local ver
	ver="$(awk -F': ' '/^version:/{gsub(/[[:space:]"\047]/,"",$2);print $2;exit}' "$REPO_ROOT/nfpm.yaml")"
	[[ -n "$ver" ]] || fail "could not parse version from nfpm.yaml"
	printf '%s' "$ver"
}

WS_VER="$(workspace_version)"
echo "check-release-version: workspace version = $WS_VER"

check_equal() {
	local name="$1" actual="$2"
	if [[ "$actual" != "$WS_VER" ]]; then
		fail "$name version $actual != workspace version $WS_VER"
	fi
	echo "check-release-version: $name = $actual (ok)"
}

check_equal "root PKGBUILD" "$(pkgbuild_pkgver "$REPO_ROOT/PKGBUILD")"
check_equal "packaging/PKGBUILD" "$(pkgbuild_pkgver "$REPO_ROOT/packaging/PKGBUILD")"
check_equal "packaging/PKGBUILD.bin" "$(pkgbuild_pkgver "$REPO_ROOT/packaging/PKGBUILD.bin")"
check_equal "nfpm.yaml" "$(nfpm_version)"

if [[ -n "$EXPECTED_TAG_VERSION" ]]; then
	if [[ "$WS_VER" != "$EXPECTED_TAG_VERSION" ]]; then
		fail "tag v$EXPECTED_TAG_VERSION != workspace version $WS_VER (bump Cargo.toml + packaging together)"
	fi
	echo "check-release-version: tag v$EXPECTED_TAG_VERSION matches (ok)"
fi

echo "check-release-version: PASS"
