#!/usr/bin/env bash
# check-desktop-integration.test.sh — CTX-0452 fixture test for the desktop
# integration gate.
#
# Copies the real desktop/metainfo/packaging inputs into a temporary root and
# mutates one identity link at a time; the gate must fail on every drift.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-desktop-integration.sh
APP_ID=run.bitty.Bitty
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# make_root <dir>: a valid copy of every file the gate inspects.
make_root() {
	local root="$1"
	mkdir -p "$root/packaging/icons/hicolor/scalable/apps" "$root/crates/bitty-platform/src"
	cp "packaging/$APP_ID.desktop" "packaging/$APP_ID.metainfo.xml" "$root/packaging/"
	cp crates/bitty-platform/src/app.rs "$root/crates/bitty-platform/src/app.rs"
	cp nfpm.yaml "$root/nfpm.yaml"
	cp packaging/PKGBUILD packaging/PKGBUILD.bin "$root/packaging/"
	cp packaging/icons/hicolor/scalable/apps/bitty.svg \
		"$root/packaging/icons/hicolor/scalable/apps/"
}

# expect_exit <exit> <label> <gate args...>
expect_exit() {
	local want="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status != want)); then
		echo "FAIL: $label: expected exit $want, got $status" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# expect_fail_tag <tag> <label> <gate args...>
expect_fail_tag() {
	local tag="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status == 0)); then
		echo "FAIL: $label: gate passed but exit $tag was expected" >&2
		FAIL=1
	elif ! grep -qF "$tag" <<<"$out"; then
		echo "FAIL: $label: missing tag '$tag'" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# 1. The real repository passes.
expect_exit 0 'real repository' --root .

# 2. A clean copy passes.
root="$TMP/clean"
make_root "$root"
expect_exit 0 'clean copy' --root "$root"

# 3. The legacy desktop file name is retired.
root="$TMP/legacy"
make_root "$root"
: >"$root/packaging/bitty.desktop"
expect_fail_tag 'still exists' 'legacy desktop name' --root "$root"

# 4. StartupWMClass drift fails.
root="$TMP/wmclass"
make_root "$root"
sed -i 's/^StartupWMClass=.*/StartupWMClass=bitty/' "$root/packaging/$APP_ID.desktop"
expect_fail_tag 'StartupWMClass' 'StartupWMClass drift' --root "$root"

# 5. Metainfo component ID drift fails.
root="$TMP/id"
make_root "$root"
sed -i "s|<id>$APP_ID</id>|<id>run.example.Wrong</id>|" "$root/packaging/$APP_ID.metainfo.xml"
expect_fail_tag 'must declare' 'metainfo id drift' --root "$root"

# 6. Launchable desktop-id drift fails.
root="$TMP/launchable"
make_root "$root"
sed -i "s|desktop-id\">$APP_ID.desktop|desktop-id\">bitty.desktop|" \
	"$root/packaging/$APP_ID.metainfo.xml"
expect_fail_tag 'must launch' 'launchable drift' --root "$root"

# 7. Window identity const drift in code fails.
root="$TMP/code"
make_root "$root"
sed -i "s|const APP_ID: &str = \"$APP_ID\";|const APP_ID: \&str = \"bitty\";|" \
	"$root/crates/bitty-platform/src/app.rs"
expect_fail_tag 'must define' 'code const drift' --root "$root"

# 8. A package format that stops installing the metainfo fails.
root="$TMP/pkg"
make_root "$root"
sed -i "/$APP_ID.metainfo.xml/d" "$root/nfpm.yaml"
expect_fail_tag 'must install' 'nfpm missing metainfo install' --root "$root"

# 9. Malformed XML fails (xmllint path; appstreamcli also rejects it).
if command -v xmllint >/dev/null 2>&1; then
	root="$TMP/xml"
	make_root "$root"
	sed -i 's|</component>|<extra/>|' "$root/packaging/$APP_ID.metainfo.xml"
	expect_fail_tag 'xmllint rejects' 'malformed metainfo' --root "$root"
fi

if ((FAIL)); then
	echo "check-desktop-integration-test: FAIL" >&2
	exit 1
fi
echo "check-desktop-integration-test: OK"
