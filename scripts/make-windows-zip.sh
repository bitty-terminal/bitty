#!/usr/bin/env bash
# make-windows-zip.sh — assemble a Windows portable ZIP (x86_64 or aarch64).
#
# Packs the architecture's MSVC binaries as `bitty.exe` and `bitty.com` together
# with the Windows display icon (`bitty.ico`, from packaging/icons/bitty.ico),
# LICENSE, README.md and CHANGELOG.md at the archive root, so users unzip and run
# without an installer (CTX-0455 / 034 item 10). The icon lets install.ps1 point
# the Start Menu shortcut and the app-list DisplayIcon at the mascot instead of
# the generic executable icon (bitty#2926). The build artifact names
# (`bitty-<target>.exe`, `bitty-com-<target>.exe`) are build details, never the
# user-facing names. The archive is built from a fixed payload list, so no glob
# can pull in unexpected files.
#
# `bitty.com` (bitty#3224) is the console companion proxy: `bitty.exe` links as
# a GUI-subsystem executable, so Windows resolves a bare `bitty` to the `.COM`
# file first (`%PATHEXT%` order) and that proxy decides between a blocking
# console run and a detached window. The two files always travel together, so
# this packer requires `--com` and fails closed without it.
#
# `--dry-run` validates arguments, stages the payload and prints the archive
# command without creating the ZIP. Real assembly needs only `zip`/`unzip` and
# works on any host, not just Windows.
#
# Usage:
#   scripts/make-windows-zip.sh --version 0.0.46 --arch x86_64 \
#     --exe dist/bitty-x86_64-pc-windows-msvc.exe \
#     --com dist/bitty-com-x86_64-pc-windows-msvc.exe \
#     --output dist/bitty-0.0.46-windows-x86_64.zip
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAYLOAD=("LICENSE" "README.md" "CHANGELOG.md")
# Display icon archived as `bitty.ico` at the archive root; the source lives
# under packaging/icons/ so it stays next to the other platform icons.
ICON="packaging/icons/bitty.ico"
DRY_RUN=0

VERSION=""
ARCH=""
EXE=""
COM=""
OUTPUT=""

usage() {
	cat <<'EOF'
Usage: scripts/make-windows-zip.sh --version VERSION --arch ARCH \
         --exe PATH --com PATH --output PATH [--dry-run]

  --version VERSION  release version embedded in the archive name
  --arch ARCH        x86_64 | aarch64 (must match the archive name)
  --exe PATH         Windows bitty executable (bitty-<target>.exe build artifact)
  --com PATH         Windows bitty.com console proxy (bitty-com-<target>.exe artifact)
  --output PATH      ZIP path to create (e.g. dist/bitty-0.0.46-windows-x86_64.zip)
  --dry-run          validate and stage the payload only; do not write the ZIP
EOF
}

die() {
	echo "make-windows-zip: ERROR: $1" >&2
	exit 1
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--version)
		VERSION="${2:-}"
		shift 2
		;;
	--arch)
		ARCH="${2:-}"
		shift 2
		;;
	--exe)
		EXE="${2:-}"
		shift 2
		;;
	--com)
		COM="${2:-}"
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
[[ -n "$ARCH" ]] || die "--arch is required (x86_64|aarch64)"
case "$ARCH" in
x86_64 | aarch64) ;;
*) die "--arch must be x86_64 or aarch64, got: $ARCH" ;;
esac
[[ -n "$EXE" ]] || die "--exe is required"
[[ -f "$EXE" ]] || die "--exe not found: $EXE"
# bitty#3224: the proxy is not optional. A ZIP without bitty.com leaves a shell
# launch silently detached from `bitty init`, which is the defect it fixes.
[[ -n "$COM" ]] || die "--com is required (the bitty.com console proxy)"
[[ -f "$COM" ]] || die "--com not found: $COM"
[[ -n "$OUTPUT" ]] || die "--output is required"

EXPECTED_NAME="bitty-${VERSION}-windows-${ARCH}.zip"
if [[ "$(basename "$OUTPUT")" != "$EXPECTED_NAME" ]]; then
	die "output must be named ${EXPECTED_NAME} (got: $(basename "$OUTPUT"))"
fi

command -v zip >/dev/null 2>&1 || die "zip not found"
command -v unzip >/dev/null 2>&1 || die "unzip not found"

# The archive is created from inside the staging directory, so a relative
# --output must be anchored to the invocation directory first (the release job
# passes `dist/...zip` from the repository root).
if [[ "$OUTPUT" != /* ]]; then
	OUTPUT="$PWD/${OUTPUT#./}"
fi

for f in "${PAYLOAD[@]}"; do
	[[ -f "$REPO_ROOT/$f" ]] || die "payload file not found: $f"
done
[[ -f "$REPO_ROOT/$ICON" ]] || die "payload file not found: $ICON (regenerate with scripts/make-windows-ico.py)"

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-zip.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

cp "$EXE" "$STAGE/bitty.exe"
cp "$COM" "$STAGE/bitty.com"
cp "$REPO_ROOT/$ICON" "$STAGE/bitty.ico"
for f in "${PAYLOAD[@]}"; do
	cp "$REPO_ROOT/$f" "$STAGE/$f"
done
[[ -s "$STAGE/bitty.exe" ]] || die "staged bitty.exe is empty"
[[ -s "$STAGE/bitty.com" ]] || die "staged bitty.com is empty"

if [[ "$DRY_RUN" -eq 1 ]]; then
	echo "make-windows-zip: dry-run payload:"
	find "$STAGE" -maxdepth 1 -type f -print | sort
	echo "make-windows-zip: dry-run would run:"
	echo "  (cd $STAGE && zip -X -9 -q $OUTPUT bitty.exe bitty.com bitty.ico ${PAYLOAD[*]})"
	echo "make-windows-zip: dry-run PASS (version=$VERSION, arch=$ARCH)"
	exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
# zip updates an existing archive in place, so start from a clean path.
rm -f "$OUTPUT"
(cd "$STAGE" && zip -X -9 -q "$OUTPUT" bitty.exe bitty.com bitty.ico "${PAYLOAD[@]}")

EXPECTED="$(printf '%s\n' bitty.exe bitty.com bitty.ico "${PAYLOAD[@]}" | LC_ALL=C sort)"
ACTUAL="$(unzip -Z1 "$OUTPUT" | LC_ALL=C sort)"
if [[ "$ACTUAL" != "$EXPECTED" ]]; then
	die "unexpected archive entries:
$ACTUAL"
fi

ls -lh "$OUTPUT"
unzip -l "$OUTPUT"
echo "make-windows-zip: PASS (version=$VERSION, arch=$ARCH, output=$OUTPUT)"
