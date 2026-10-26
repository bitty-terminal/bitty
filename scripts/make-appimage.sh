#!/usr/bin/env bash
# make-appimage.sh — assemble the portable Linux AppImage bundle.
#
# Packages the Linux binary as `usr/bin/bitty` together with standard AppDir
# assets (AppRun launcher, desktop file, icons, and AppStream metainfo) into a
# single-file portable AppImage via `appimagetool` (issue #2555; CTX-1468).
#
# Target-triple binaries and compressed tarballs continue shipping in parallel;
# AppImage provides the universal standalone single-file format for Linux
# distributions.
#
# Arguments:
#   --version VERSION  release version embedded in metadata (X.Y.Z)
#   --target TARGET    rust target triple (e.g. x86_64-unknown-linux-gnu)
#   --binary PATH      Linux bitty binary to pack as usr/bin/bitty
#   --output PATH      AppImage output path (e.g. dist/bitty-x86_64-unknown-linux-gnu.AppImage)
#   --dry-run          validate and stage AppDir only; do not invoke appimagetool
#
# Environment:
#   APPIMAGETOOL       explicit appimagetool executable path; skips PATH discovery,
#                      the /tmp cache, and the download fallback
#
# Usage:
#   scripts/make-appimage.sh --version 0.0.27 \
#     --target x86_64-unknown-linux-gnu \
#     --binary target/release/bitty \
#     --output dist/bitty-x86_64-unknown-linux-gnu.AppImage
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAYLOAD=("LICENSE" "README.md" "CHANGELOG.md")
DESKTOP_FILE="packaging/run.bitty.Bitty.desktop"
METAINFO_FILE="packaging/run.bitty.Bitty.metainfo.xml"
TERMINFO_FILE="terminfo/bitty.ti"
DRY_RUN=0

VERSION=""
TARGET=""
BINARY=""
OUTPUT=""

usage() {
  cat <<'EOF'
Usage: scripts/make-appimage.sh --version VERSION --target TARGET --binary PATH --output PATH [--dry-run]

  --version VERSION  release version embedded in metadata (X.Y.Z)
  --target TARGET    rust target triple (e.g. x86_64-unknown-linux-gnu)
  --binary PATH      Linux bitty binary to pack as usr/bin/bitty
  --output PATH      AppImage output path (e.g. dist/bitty-x86_64-unknown-linux-gnu.AppImage)
  --dry-run          validate and stage the AppDir only; do not build the AppImage

Environment:
  APPIMAGETOOL       explicit appimagetool executable path; skips PATH discovery,
                     the /tmp cache, and the download fallback
EOF
}

die() {
  echo "make-appimage: ERROR: $1" >&2
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
[[ -x "$BINARY" ]] || die "--binary is not executable: $BINARY"
[[ -n "$OUTPUT" ]] || die "--output is required"

# Map target triple to AppImage architecture
case "$TARGET" in
x86_64-*)
  ARCH="x86_64"
  ;;
aarch64-*)
  ARCH="aarch64"
  ;;
*)
  die "unsupported AppImage target architecture: $TARGET"
  ;;
esac

# Stage AppDir under ephemeral project scratch
SCRATCH_ROOT="${REPO_ROOT}/tmp"
if [[ ! -d "$SCRATCH_ROOT" ]]; then
  SCRATCH_ROOT="/tmp/bitty"
fi
mkdir -p "$SCRATCH_ROOT"

APPDIR="$(mktemp -d "${SCRATCH_ROOT}/bitty-appdir-XXXXXX")"
cleanup() {
  rm -rf "$APPDIR"
}
trap cleanup EXIT

mkdir -p "${APPDIR}/usr/bin"
mkdir -p "${APPDIR}/usr/share/applications"
mkdir -p "${APPDIR}/usr/share/metainfo"
mkdir -p "${APPDIR}/usr/share/icons/hicolor"
mkdir -p "${APPDIR}/usr/share/terminfo"
mkdir -p "${APPDIR}/usr/share/doc/bitty"

# 1. Binary payload
cp "$BINARY" "${APPDIR}/usr/bin/bitty"
chmod +x "${APPDIR}/usr/bin/bitty"

# 2. Desktop integration and metainfo
[[ -f "${REPO_ROOT}/${DESKTOP_FILE}" ]] || die "missing desktop file: ${DESKTOP_FILE}"
cp "${REPO_ROOT}/${DESKTOP_FILE}" "${APPDIR}/run.bitty.Bitty.desktop"
cp "${REPO_ROOT}/${DESKTOP_FILE}" "${APPDIR}/usr/share/applications/run.bitty.Bitty.desktop"

[[ -f "${REPO_ROOT}/${METAINFO_FILE}" ]] || die "missing metainfo file: ${METAINFO_FILE}"
cp "${REPO_ROOT}/${METAINFO_FILE}" "${APPDIR}/usr/share/metainfo/run.bitty.Bitty.metainfo.xml"

# 3. Icons hierarchy
[[ -d "${REPO_ROOT}/packaging/icons/hicolor" ]] || die "missing icon directory: packaging/icons/hicolor"
cp -r "${REPO_ROOT}/packaging/icons/hicolor/"* "${APPDIR}/usr/share/icons/hicolor/"
cp "${REPO_ROOT}/packaging/icons/hicolor/256x256/apps/bitty.png" "${APPDIR}/bitty.png"
if [[ -f "${REPO_ROOT}/packaging/icons/hicolor/scalable/apps/bitty.svg" ]]; then
  cp "${REPO_ROOT}/packaging/icons/hicolor/scalable/apps/bitty.svg" "${APPDIR}/bitty.svg"
fi

# 4. Terminfo database (compiled via tic if available, fallback ignored)
if command -v tic >/dev/null 2>&1 && [[ -f "${REPO_ROOT}/${TERMINFO_FILE}" ]]; then
  tic -x -o "${APPDIR}/usr/share/terminfo" "${REPO_ROOT}/${TERMINFO_FILE}" 2>/dev/null || true
fi

# 5. Documentation payload
for doc in "${PAYLOAD[@]}"; do
  if [[ -f "${REPO_ROOT}/${doc}" ]]; then
    cp "${REPO_ROOT}/${doc}" "${APPDIR}/usr/share/doc/bitty/"
  fi
done

# 6. AppRun entrypoint script
cat <<'EOF' >"${APPDIR}/AppRun"
#!/bin/sh
set -e
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="${HERE}/usr/bin:${PATH}"
if [ -d "${HERE}/usr/share/terminfo" ]; then
	export TERMINFO="${HERE}/usr/share/terminfo:${TERMINFO:-}"
fi
exec "${HERE}/usr/bin/bitty" "$@"
EOF
chmod +x "${APPDIR}/AppRun"

echo "make-appimage: staged AppDir at ${APPDIR} (arch: ${ARCH})"

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "make-appimage: dry run complete (no bundle emitted)"
  exit 0
fi

# Locate or download appimagetool. An explicit APPIMAGETOOL path wins so tests
# and packagers can pin the tool; an unusable override fails closed instead of
# silently falling back to discovery, the cache, or the network.
TOOL_PATH="${APPIMAGETOOL:-}"
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
x86_64 | amd64)
  TOOL_ARCH="x86_64"
  ;;
aarch64 | arm64)
  TOOL_ARCH="aarch64"
  ;;
*)
  TOOL_ARCH="$HOST_ARCH"
  ;;
esac

if [[ -n "$TOOL_PATH" ]]; then
  [[ -f "$TOOL_PATH" && -x "$TOOL_PATH" ]] ||
    die "APPIMAGETOOL must point to an executable file, got: $TOOL_PATH"
elif command -v appimagetool >/dev/null 2>&1; then
  TOOL_PATH="$(command -v appimagetool)"
elif [[ -x "/tmp/bitty/appimagetool" ]]; then
  TOOL_PATH="/tmp/bitty/appimagetool"
else
  echo "make-appimage: downloading appimagetool continuous release for host ${TOOL_ARCH}..."
  mkdir -p "/tmp/bitty"
  TOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${TOOL_ARCH}.AppImage"
  curl -fsSL -o "/tmp/bitty/appimagetool" "$TOOL_URL" || die "failed to download appimagetool from ${TOOL_URL}"
  chmod +x "/tmp/bitty/appimagetool"
  TOOL_PATH="/tmp/bitty/appimagetool"
fi

mkdir -p "$(dirname "$OUTPUT")"

echo "make-appimage: generating ${OUTPUT} via ${TOOL_PATH}..."
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="${ARCH}" "$TOOL_PATH" "${APPDIR}" "${OUTPUT}"

[[ -f "$OUTPUT" ]] || die "output AppImage was not created: ${OUTPUT}"
[[ -s "$OUTPUT" ]] || die "output AppImage is empty: ${OUTPUT}"
chmod +x "$OUTPUT"

# Compute sha256 checksum sidecar
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$OUTPUT" | tee "${OUTPUT}.sha256"
elif command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$OUTPUT" | tee "${OUTPUT}.sha256"
fi

echo "make-appimage: successfully generated ${OUTPUT}"
