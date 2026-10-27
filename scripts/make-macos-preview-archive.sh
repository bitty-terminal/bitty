#!/usr/bin/env bash
# make-macos-preview-archive.sh — assemble the macOS arm64 preview archive.
#
# The accepted preview matrix (P-07) ships a macOS arm64 `.tar.zst` with just
# the CLI binary and the three repository documents. It is deliberately NOT a
# DMG, an `.app`, or a universal binary, and it carries no Linux desktop entry,
# icons, or terminfo members. The stable `scripts/make-unix-bundle.sh` packs
# those Linux members, so passing a Darwin binary through it would produce a
# bundle with the wrong shape (see the preview release contract).
#
# The archive wraps everything in one top-level
# `bitty-<version>-aarch64-apple-darwin/` directory. The payload list is fixed
# (explicit members, no globs), the archive name is enforced to the accepted
# asset name, and the member list is verified after assembly.
#
# `--dry-run` validates arguments and stages the payload without writing.
#
# Usage:
#   scripts/make-macos-preview-archive.sh --version 0.0.30 \
#     --binary dist/bitty-aarch64-apple-darwin \
#     --output dist/bitty-0.0.30-aarch64-apple-darwin.tar.zst
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ACCEPTED_TARGET="aarch64-apple-darwin"
PAYLOAD=("LICENSE" "README.md" "CHANGELOG.md")
DRY_RUN=0

VERSION=""
TARGET="$ACCEPTED_TARGET"
BINARY=""
OUTPUT=""

usage() {
  cat <<'EOF'
Usage: scripts/make-macos-preview-archive.sh --version VERSION --binary PATH --output PATH
                                             [--target TARGET] [--dry-run]

  --version VERSION  release base version embedded in the archive name (X.Y.Z)
  --binary PATH      aarch64-apple-darwin bitty binary to pack as bin/bitty
  --output PATH      archive path; the basename must be
                     bitty-<VERSION>-aarch64-apple-darwin.tar.zst
  --target TARGET    rust target triple (default and accepted: aarch64-apple-darwin)
  --dry-run          validate and stage the payload only; do not write the archive
EOF
}

die() {
  echo "make-macos-preview-archive: ERROR: $1" >&2
  exit 1
}

while (($#)); do
  case "$1" in
  --version | --target | --binary | --output)
    [[ $# -ge 2 ]] || {
      usage >&2
      die "$1 needs a value"
    }
    case "$1" in
    --version) VERSION="$2" ;;
    --target) TARGET="$2" ;;
    --binary) BINARY="$2" ;;
    --output) OUTPUT="$2" ;;
    esac
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
[[ "$TARGET" == "$ACCEPTED_TARGET" ]] ||
  die "the accepted preview matrix is $ACCEPTED_TARGET only, got: $TARGET (see P-07)"
[[ -n "$BINARY" ]] || die "--binary is required"
[[ -f "$BINARY" ]] || die "--binary not found: $BINARY"
[[ -n "$OUTPUT" ]] || die "--output is required"
command -v tar >/dev/null 2>&1 || die "tar not found"
command -v zstd >/dev/null 2>&1 || die "zstd not found"

# The accepted asset name carries the bare base version (P-15). Enforce it here
# so a workflow edit cannot publish a differently named preview archive.
EXPECTED_NAME="bitty-${VERSION}-${TARGET}.tar.zst"
[[ "$(basename "$OUTPUT")" == "$EXPECTED_NAME" ]] ||
  die "--output basename must be $EXPECTED_NAME, got: $(basename "$OUTPUT")"

# A Mach-O binary must carry exactly the arm64 slice. lipo exists on the macOS
# packaging runner; the fixture test runs on any host, so the check is skipped
# there with an explicit note rather than silently claiming success.
if command -v lipo >/dev/null 2>&1; then
  arches="$(lipo -archs "$BINARY" 2>/dev/null || true)"
  [[ "$arches" == "arm64" ]] ||
    die "binary is not an arm64-only Mach-O (lipo -archs: '${arches:-unknown}')"
  echo "make-macos-preview-archive: lipo -archs: $arches"
else
  echo "make-macos-preview-archive: note: lipo unavailable on this host; arm64 slice asserted by the macOS runner" >&2
fi

if [[ "$OUTPUT" != /* ]]; then
  OUTPUT="$PWD/${OUTPUT#./}"
fi

for f in "${PAYLOAD[@]}"; do
  [[ -f "$REPO_ROOT/$f" ]] || die "payload file not found: $f"
done

TOPDIR="bitty-$VERSION-$TARGET"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-macos-preview.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

ROOT="$STAGE/$TOPDIR"
mkdir -p "$ROOT/bin"
cp "$BINARY" "$ROOT/bin/bitty"
chmod 755 "$ROOT/bin/bitty"
for f in "${PAYLOAD[@]}"; do
  cp "$REPO_ROOT/$f" "$ROOT/$f"
done

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "make-macos-preview-archive: dry-run payload:"
  find "$ROOT" -mindepth 1 -print | sort
  echo "make-macos-preview-archive: dry-run would run:"
  echo "  (cd $STAGE && tar -cf - $TOPDIR | zstd -19 -T0 -o $OUTPUT)"
  echo "make-macos-preview-archive: dry-run PASS (version=$VERSION, target=$TARGET)"
  exit 0
fi

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"
(cd "$STAGE" && tar -cf - "$TOPDIR" | zstd -19 -T0 -q -o "$OUTPUT")

EXPECTED="$(
  {
    echo "$TOPDIR/bin/bitty"
    echo "$TOPDIR/LICENSE"
    echo "$TOPDIR/README.md"
    echo "$TOPDIR/CHANGELOG.md"
  } | LC_ALL=C sort
)"
ACTUAL="$(zstd -dc "$OUTPUT" | tar -tf - | grep -v '/$' | LC_ALL=C sort)"
if [[ "$ACTUAL" != "$EXPECTED" ]]; then
  die "unexpected archive entries:
$ACTUAL"
fi

ls -lh "$OUTPUT"
zstd -dc "$OUTPUT" | tar -tvf - | sort -k6
echo "make-macos-preview-archive: PASS (version=$VERSION, target=$TARGET, output=$OUTPUT)"
