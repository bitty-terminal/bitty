#!/usr/bin/env bash
# make-unix-bundle.sh — assemble the versioned Linux binary bundle.
#
# Packs the Linux binary as `bin/bitty` together with the desktop entry,
# icons, and AppStream metainfo under `share/`, plus LICENSE, README.md and
# CHANGELOG.md, into `bitty-<version>-<target>.tar.zst` (REL-07 / issue
# #1110; binary-compression section of release-distribution.md). Bare
# target-triple binaries keep shipping for now; the versioned bundle is the
# primary long-term artifact. Windows uses the portable ZIP and macOS the
# Universal DMG, so this script covers Linux (gnu + musl) targets only.
#
# The archive wraps everything in one top-level `bitty-<version>-<target>/`
# directory so extraction never pollutes the current directory. The payload
# list is fixed (explicit members, no globs). Version and target arrive as
# arguments that the release workflow derives from repo metadata
# (`scripts/check-release-version.sh --print` + the build matrix) — never
# hardcoded here.
#
# `--dry-run` validates arguments, stages the payload and prints the archive
# command without creating the bundle. Assembly needs only `tar` and `zstd`.
#
# Usage:
#   scripts/make-unix-bundle.sh --version 0.0.21 \
#     --target x86_64-unknown-linux-gnu \
#     --binary dist/bitty-x86_64-unknown-linux-gnu \
#     --output dist/bitty-0.0.21-x86_64-unknown-linux-gnu.tar.zst
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAYLOAD=("LICENSE" "README.md" "CHANGELOG.md")
ICON_SIZES=(16 32 64 128 256 512)
DESKTOP_FILE="packaging/run.bitty.Bitty.desktop"
METAINFO_FILE="packaging/run.bitty.Bitty.metainfo.xml"
DRY_RUN=0

VERSION=""
TARGET=""
BINARY=""
OUTPUT=""

usage() {
	cat <<'EOF'
Usage: scripts/make-unix-bundle.sh --version VERSION --target TARGET --binary PATH --output PATH [--dry-run]

  --version VERSION  release version embedded in the bundle name (X.Y.Z)
  --target TARGET    rust target triple (e.g. x86_64-unknown-linux-gnu)
  --binary PATH      Linux bitty binary to pack as bin/bitty
  --output PATH      bundle path to create (e.g. dist/bitty-0.0.21-x86_64-unknown-linux-gnu.tar.zst)
  --dry-run          validate and stage the payload only; do not write the bundle
EOF
}

die() {
	echo "make-unix-bundle: ERROR: $1" >&2
	exit 1
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--version)
		VERSION="${2:-}"
		shift 2
		;;
	--target)
		TARGET="${2:-}"
		shift 2
		;;
	--binary)
		BINARY="${2:-}"
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
[[ -n "$TARGET" ]] || die "--target is required"
[[ "$TARGET" =~ ^[a-z0-9_]+(-[a-z0-9_]+)+$ ]] || die "--target must look like a rust target triple, got: $TARGET"
[[ -n "$BINARY" ]] || die "--binary is required"
[[ -f "$BINARY" ]] || die "--binary not found: $BINARY"
[[ -n "$OUTPUT" ]] || die "--output is required"
command -v tar >/dev/null 2>&1 || die "tar not found"
command -v zstd >/dev/null 2>&1 || die "zstd not found"

# The archive is created from inside the staging directory, so a relative
# --output must be anchored to the invocation directory first (the release
# job passes `dist/...tar.zst` from the repository root).
if [[ "$OUTPUT" != /* ]]; then
	OUTPUT="$PWD/${OUTPUT#./}"
fi

for f in "${PAYLOAD[@]}"; do
	[[ -f "$REPO_ROOT/$f" ]] || die "payload file not found: $f"
done
[[ -f "$REPO_ROOT/$DESKTOP_FILE" ]] || die "payload file not found: $DESKTOP_FILE"
[[ -f "$REPO_ROOT/$METAINFO_FILE" ]] || die "payload file not found: $METAINFO_FILE"
for size in "${ICON_SIZES[@]}"; do
	[[ -f "$REPO_ROOT/packaging/icons/hicolor/${size}x${size}/apps/bitty.png" ]] ||
		die "payload file not found: packaging/icons/hicolor/${size}x${size}/apps/bitty.png"
done
[[ -f "$REPO_ROOT/packaging/icons/hicolor/scalable/apps/bitty.svg" ]] ||
	die "payload file not found: packaging/icons/hicolor/scalable/apps/bitty.svg"

TOPDIR="bitty-$VERSION-$TARGET"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-bundle.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

ROOT="$STAGE/$TOPDIR"
mkdir -p "$ROOT/bin" "$ROOT/share/applications" "$ROOT/share/metainfo" \
	"$ROOT/share/icons/hicolor/scalable/apps"
for size in "${ICON_SIZES[@]}"; do
	mkdir -p "$ROOT/share/icons/hicolor/${size}x${size}/apps"
done

cp "$BINARY" "$ROOT/bin/bitty"
chmod 755 "$ROOT/bin/bitty"
for f in "${PAYLOAD[@]}"; do
	cp "$REPO_ROOT/$f" "$ROOT/$f"
done
cp "$REPO_ROOT/$DESKTOP_FILE" "$ROOT/share/applications/"
cp "$REPO_ROOT/$METAINFO_FILE" "$ROOT/share/metainfo/"
for size in "${ICON_SIZES[@]}"; do
	cp "$REPO_ROOT/packaging/icons/hicolor/${size}x${size}/apps/bitty.png" \
		"$ROOT/share/icons/hicolor/${size}x${size}/apps/bitty.png"
done
cp "$REPO_ROOT/packaging/icons/hicolor/scalable/apps/bitty.svg" \
	"$ROOT/share/icons/hicolor/scalable/apps/bitty.svg"

if [[ "$DRY_RUN" -eq 1 ]]; then
	echo "make-unix-bundle: dry-run payload:"
	find "$ROOT" -mindepth 1 -print | sort
	echo "make-unix-bundle: dry-run would run:"
	echo "  (cd $STAGE && tar -c -I 'zstd -19' -f $OUTPUT $TOPDIR)"
	echo "make-unix-bundle: dry-run PASS (version=$VERSION, target=$TARGET)"
	exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
# tar refuses to overwrite an existing archive member set in place, so start
# from a clean path (same reason the windows-zip script rm -f first).
rm -f "$OUTPUT"
(cd "$STAGE" && tar -c -I 'zstd -19' -f "$OUTPUT" "$TOPDIR")

# The member list is fixed: assert the archive holds exactly the staged
# payload (files only; directory entries excluded) so no glob or stray file
# can slip into a release bundle.
EXPECTED="$(
	{
		echo "$TOPDIR/bin/bitty"
		echo "$TOPDIR/LICENSE"
		echo "$TOPDIR/README.md"
		echo "$TOPDIR/CHANGELOG.md"
		echo "$TOPDIR/share/applications/run.bitty.Bitty.desktop"
		echo "$TOPDIR/share/metainfo/run.bitty.Bitty.metainfo.xml"
		for size in "${ICON_SIZES[@]}"; do
			echo "$TOPDIR/share/icons/hicolor/${size}x${size}/apps/bitty.png"
		done
		echo "$TOPDIR/share/icons/hicolor/scalable/apps/bitty.svg"
	} | LC_ALL=C sort
)"
ACTUAL="$(tar -tf "$OUTPUT" | grep -v '/$' | LC_ALL=C sort)"
if [[ "$ACTUAL" != "$EXPECTED" ]]; then
	die "unexpected archive entries:
$ACTUAL"
fi

ls -lh "$OUTPUT"
tar -tvf "$OUTPUT" | sort -k6
echo "make-unix-bundle: PASS (version=$VERSION, target=$TARGET, output=$OUTPUT)"
