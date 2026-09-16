#!/usr/bin/env bash
# make-windows-zip.sh — assemble the Windows x86_64 portable ZIP.
#
# Packs the x86_64 MSVC binary as `bitty.exe` together with LICENSE, README.md
# and CHANGELOG.md at the archive root, so users unzip and run without an
# installer (CTX-0455 / 034 item 10). The build artifact name
# (`bitty-x86_64-pc-windows-msvc.exe`) is a build detail, never the user-facing
# name. The archive is built from a fixed payload list, so no glob can pull in
# unexpected files.
#
# `--dry-run` validates arguments, stages the payload and prints the archive
# command without creating the ZIP. Real assembly needs only `zip`/`unzip` and
# works on any host, not just Windows.
#
# Usage:
#   scripts/make-windows-zip.sh --version 0.0.21 \
#     --exe dist/bitty-x86_64-pc-windows-msvc.exe \
#     --output dist/bitty-0.0.21-windows-x86_64.zip
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAYLOAD=("LICENSE" "README.md" "CHANGELOG.md")
DRY_RUN=0

VERSION=""
EXE=""
OUTPUT=""

usage() {
	cat <<'EOF'
Usage: scripts/make-windows-zip.sh --version VERSION --exe PATH --output PATH [--dry-run]

  --version VERSION  release version embedded in the archive name
  --exe PATH         x86_64-pc-windows-msvc bitty executable
  --output PATH      ZIP path to create (e.g. dist/bitty-0.0.21-windows-x86_64.zip)
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
	--exe)
		EXE="${2:-}"
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
[[ -n "$EXE" ]] || die "--exe is required"
[[ -f "$EXE" ]] || die "--exe not found: $EXE"
[[ -n "$OUTPUT" ]] || die "--output is required"
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

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-zip.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

cp "$EXE" "$STAGE/bitty.exe"
for f in "${PAYLOAD[@]}"; do
	cp "$REPO_ROOT/$f" "$STAGE/$f"
done

if [[ "$DRY_RUN" -eq 1 ]]; then
	echo "make-windows-zip: dry-run payload:"
	find "$STAGE" -maxdepth 1 -type f -print | sort
	echo "make-windows-zip: dry-run would run:"
	echo "  (cd $STAGE && zip -X -9 -q $OUTPUT bitty.exe ${PAYLOAD[*]})"
	echo "make-windows-zip: dry-run PASS (version=$VERSION)"
	exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
# zip updates an existing archive in place, so start from a clean path.
rm -f "$OUTPUT"
(cd "$STAGE" && zip -X -9 -q "$OUTPUT" bitty.exe "${PAYLOAD[@]}")

EXPECTED="$(printf '%s\n' bitty.exe "${PAYLOAD[@]}" | LC_ALL=C sort)"
ACTUAL="$(unzip -Z1 "$OUTPUT" | LC_ALL=C sort)"
if [[ "$ACTUAL" != "$EXPECTED" ]]; then
	die "unexpected archive entries:
$ACTUAL"
fi

ls -lh "$OUTPUT"
unzip -l "$OUTPUT"
echo "make-windows-zip: PASS (version=$VERSION, output=$OUTPUT)"
