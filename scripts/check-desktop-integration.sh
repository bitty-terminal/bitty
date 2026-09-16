#!/usr/bin/env bash
# check-desktop-integration.sh — guard the fixed Linux application ID and the
# desktop/AppStream metadata (034 item 7 / CTX-0452).
#
# The one ID is run.bitty.Bitty (recorded in packaging/README.md):
#   packaging/run.bitty.Bitty.desktop       file name + StartupWMClass
#   packaging/run.bitty.Bitty.metainfo.xml  <id> + <launchable>
#   crates/bitty-platform/src/app.rs        Wayland app_id / X11 WM_CLASS
# Every package format installs both files; the icon theme name stays `bitty`.
# xmllint, appstreamcli, and desktop-file-validate run when available.
#
# Usage: scripts/check-desktop-integration.sh [--root DIR]
set -euo pipefail

APP_ID="run.bitty.Bitty"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
	echo "Usage: scripts/check-desktop-integration.sh [--root DIR]"
}

while (($#)); do
	case "$1" in
	--root)
		[[ $# -ge 2 ]] || {
			echo "check-desktop-integration: --root needs a value" >&2
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
		echo "check-desktop-integration: unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

fail() {
	echo "check-desktop-integration: FAIL: $1" >&2
	exit 1
}

DESKTOP="$ROOT/packaging/$APP_ID.desktop"
METAINFO="$ROOT/packaging/$APP_ID.metainfo.xml"
APP_RS="$ROOT/crates/bitty-platform/src/app.rs"

# 1. The fixed desktop file ID is the app ID; the legacy name is retired.
[[ -f "$DESKTOP" ]] || fail "missing packaging/$APP_ID.desktop"
[[ -f "$METAINFO" ]] || fail "missing packaging/$APP_ID.metainfo.xml"
[[ ! -e "$ROOT/packaging/bitty.desktop" ]] ||
	fail "packaging/bitty.desktop still exists; the desktop file ID is $APP_ID"

# 2. Desktop entry: the compositor class matches the app ID and its icon.
grep -qx "StartupWMClass=$APP_ID" "$DESKTOP" ||
	fail "$APP_ID.desktop must set StartupWMClass=$APP_ID"
ICON="$(sed -n 's/^Icon=//p' "$DESKTOP")"
[[ -n "$ICON" ]] || fail "$APP_ID.desktop must set Icon="
[[ -f "$ROOT/packaging/icons/hicolor/scalable/apps/$ICON.svg" ]] ||
	fail "desktop Icon=$ICON has no packaging/icons/hicolor/scalable/apps/$ICON.svg"

# 3. AppStream metainfo: component ID and launchable desktop ID.
grep -qF "<id>$APP_ID</id>" "$METAINFO" ||
	fail "metainfo must declare <id>$APP_ID</id>"
grep -qF "desktop-id\">$APP_ID.desktop</launchable>" "$METAINFO" ||
	fail "metainfo must launch $APP_ID.desktop"
if command -v xmllint >/dev/null 2>&1; then
	xmllint --noout "$METAINFO" || fail "xmllint rejects the metainfo"
fi

# 4. The window identity in code uses the same fixed ID.
grep -qF "const APP_ID: &str = \"$APP_ID\";" "$APP_RS" ||
	fail "app.rs must define const APP_ID: &str = \"$APP_ID\";"
grep -qF "with_name(attributes, APP_ID, APP_ID)" "$APP_RS" ||
	fail "app.rs must apply APP_ID on the Wayland and X11 window backends"

# 5. Every package format installs both files.
for config in "$ROOT/nfpm.yaml" "$ROOT/packaging/PKGBUILD" "$ROOT/packaging/PKGBUILD.bin"; do
	[[ -f "$config" ]] || fail "missing $config"
	rel="${config#"$ROOT"/}"
	grep -qF "$APP_ID.desktop" "$config" || fail "$rel must install $APP_ID.desktop"
	grep -qF "$APP_ID.metainfo.xml" "$config" || fail "$rel must install $APP_ID.metainfo.xml"
done

# 6. Run the spec validators when the host has them.
if command -v appstreamcli >/dev/null 2>&1; then
	appstreamcli validate --no-net "$METAINFO" || fail "appstreamcli validate rejects the metainfo"
else
	echo "check-desktop-integration: appstreamcli not available, skipped AppStream validation"
fi
if command -v desktop-file-validate >/dev/null 2>&1; then
	desktop-file-validate "$DESKTOP" || fail "desktop-file-validate rejects the desktop entry"
else
	echo "check-desktop-integration: desktop-file-validate not available, skipped desktop validation"
fi

echo "check-desktop-integration: PASS (app ID $APP_ID; appstreamcli/desktop-file-validate)"
