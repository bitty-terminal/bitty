#!/usr/bin/env bash
# make-macos-dmg.sh — assemble Bitty.app and package Bitty-<version>-universal.dmg.
#
# Fuses the x86_64 and arm64 Mach-O slices of the `bitty` binary into one
# Universal 2 executable with `lipo`, writes the bundle's Info.plist, and
# creates a compressed DMG with `hdiutil` so macOS users never pick a target
# triple (CTX-0454 / 034 item 9). Codesign/notarization is intentionally not
# part of this step (034 item 11): the produced app is unsigned.
#
# `--dry-run` validates arguments and assembles the bundle tree (including the
# rendered Info.plist) without invoking `lipo`/`hdiutil`, so the non-platform
# logic is exercisable on a Linux host. Real DMG creation requires macOS.
#
# Usage:
#   scripts/make-macos-dmg.sh --version 0.0.21 \
#     --arm64 dist/bitty-aarch64-apple-darwin \
#     --x86_64 dist/bitty-x86_64-apple-darwin \
#     --output dist/Bitty-0.0.21-universal.dmg
set -euo pipefail

APP_NAME="Bitty"
APP_BUNDLE_ID="run.bitty.Bitty"
VOLUME_NAME="Bitty"
# The arm64 slice already requires macOS 11, so the app as a whole does too.
MINIMUM_MACOS="11.0"
DRY_RUN=0

VERSION=""
ARM64_BIN=""
X86_64_BIN=""
OUTPUT=""

usage() {
	cat <<'EOF'
Usage: scripts/make-macos-dmg.sh --version VERSION --arm64 PATH --x86_64 PATH --output PATH [--dry-run]

  --version VERSION  release version embedded in the bundle and plist
  --arm64 PATH       aarch64-apple-darwin bitty binary
  --x86_64 PATH      x86_64-apple-darwin bitty binary
  --output PATH      DMG path to create (e.g. dist/Bitty-0.0.21-universal.dmg)
  --dry-run          validate and assemble the bundle tree only; no lipo/hdiutil
EOF
}

die() {
	echo "make-macos-dmg: ERROR: $1" >&2
	exit 1
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--version)
		VERSION="${2:-}"
		shift 2
		;;
	--arm64)
		ARM64_BIN="${2:-}"
		shift 2
		;;
	--x86_64)
		X86_64_BIN="${2:-}"
		shift 2
		;;
	--output)
		OUTPUT="${2:-}"
		shift 2
		;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage >&2
		die "unknown argument: $1"
		;;
	esac
done

[[ -n "$VERSION" ]] || die "--version is required"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version must look like X.Y.Z, got: $VERSION"
[[ -n "$ARM64_BIN" ]] || die "--arm64 is required"
[[ -f "$ARM64_BIN" ]] || die "--arm64 binary not found: $ARM64_BIN"
[[ -n "$X86_64_BIN" ]] || die "--x86_64 is required"
[[ -f "$X86_64_BIN" ]] || die "--x86_64 binary not found: $X86_64_BIN"
[[ -n "$OUTPUT" ]] || die "--output is required"

if [[ "$DRY_RUN" -eq 0 ]]; then
	command -v lipo >/dev/null 2>&1 || die "lipo not found (macOS only)"
	command -v hdiutil >/dev/null 2>&1 || die "hdiutil not found (macOS only)"
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-dmg.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

APP_DIR="$STAGE/$APP_NAME.app"
APP_BIN="$APP_DIR/Contents/MacOS/bitty"
mkdir -p "$APP_DIR/Contents/MacOS"

cat >"$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleExecutable</key>
	<string>bitty</string>
	<key>CFBundleIdentifier</key>
	<string>$APP_BUNDLE_ID</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSMinimumSystemVersion</key>
	<string>$MINIMUM_MACOS</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

if [[ "$DRY_RUN" -eq 1 ]]; then
	echo "make-macos-dmg: dry-run bundle tree:"
	find "$STAGE" -print | sort
	echo "make-macos-dmg: dry-run Info.plist:"
	cat "$APP_DIR/Contents/Info.plist"
	echo "make-macos-dmg: dry-run would run:"
	echo "  lipo -create $ARM64_BIN $X86_64_BIN -output $APP_BIN"
	echo "  hdiutil create -volname $VOLUME_NAME -srcfolder $STAGE -ov -format UDZO -fs HFS+ $OUTPUT"
	echo "make-macos-dmg: dry-run PASS (version=$VERSION)"
	exit 0
fi

# Universal 2: one fat binary carrying both slices.
lipo -create "$ARM64_BIN" "$X86_64_BIN" -output "$APP_BIN"
chmod 755 "$APP_BIN"
ARCHS="$(lipo -archs "$APP_BIN")"
echo "make-macos-dmg: fused slices: $ARCHS"
if [[ "$ARCHS" != *arm64* || "$ARCHS" != *x86_64* ]]; then
	die "fused binary is not Universal 2: $ARCHS"
fi

# Standard drag-to-install layout: the Applications symlink keeps the DMG
# useful without a custom Finder layout.
ln -s /Applications "$STAGE/Applications"

mkdir -p "$(dirname "$OUTPUT")"
hdiutil create \
	-volname "$VOLUME_NAME" \
	-srcfolder "$STAGE" \
	-ov \
	-format UDZO \
	-fs HFS+ \
	"$OUTPUT"

ls -lh "$OUTPUT"
echo "make-macos-dmg: PASS (version=$VERSION, slices=$ARCHS, output=$OUTPUT)"
