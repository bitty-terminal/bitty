#!/usr/bin/env bash
# preview-linux-smoke.sh — run the Linux preview archive in a clean container.
#
# The preview gate (P-07) is a download-and-run test, not a distributable
# package install: the archive is expanded inside a fresh ubuntu container and
# the extracted `bin/bitty` must report the preview channel and complete the
# deterministic `--headless` smoke. The container installs only the runtime
# libraries declared by nfpm.yaml (fontconfig, FreeType, libgcc).
#
# Usage:
#   scripts/preview-linux-smoke.sh --archive PATH --base-version X.Y.Z --topdir NAME
#
# Options:
#   --archive PATH      preview `.tar.zst` archive (required)
#   --base-version X.Y.Z  expected bare version (required)
#   --topdir NAME       top-level directory inside the archive (required)
#   --image IMAGE       container image (default: ubuntu:24.04)
#   -h, --help          show this help
#
# Exit codes: 0 = smoke passed, 1 = smoke/environment failure, 2 = usage error.
set -euo pipefail

ARCHIVE=""
BASE_VERSION=""
TOPDIR=""
IMAGE="ubuntu:24.04"

usage() {
  cat <<'EOF'
Usage: scripts/preview-linux-smoke.sh --archive PATH --base-version X.Y.Z --topdir NAME

  --archive PATH        preview .tar.zst archive
  --base-version X.Y.Z  expected bare version
  --topdir NAME         top-level directory inside the archive
  --image IMAGE         container image (default: ubuntu:24.04)
  -h, --help            show this help
EOF
}

usage_error() {
  printf 'preview-linux-smoke: %s\n' "$*" >&2
  printf 'run with --help\n' >&2
  exit 2
}

die() {
  printf 'preview-linux-smoke[error]: %s\n' "$*" >&2
  exit 1
}

while (($#)); do
  case "$1" in
  --archive | --base-version | --topdir | --image)
    [[ $# -ge 2 ]] || usage_error "$1 needs a value"
    case "$1" in
    --archive) ARCHIVE="$2" ;;
    --base-version) BASE_VERSION="$2" ;;
    --topdir) TOPDIR="$2" ;;
    --image) IMAGE="$2" ;;
    esac
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage_error "unknown argument: $1"
    ;;
  esac
done

[[ -n "$ARCHIVE" ]] || usage_error "--archive is required"
[[ -f "$ARCHIVE" ]] || die "archive not found: $ARCHIVE"
[[ "$BASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  usage_error "--base-version must look like X.Y.Z, got: $BASE_VERSION"
[[ -n "$TOPDIR" ]] || usage_error "--topdir is required"
[[ "$TOPDIR" =~ ^bitty-[0-9]+\.[0-9]+\.[0-9]+-[a-z0-9_-]+$ ]] ||
  die "unexpected archive top-level directory: $TOPDIR"
command -v docker >/dev/null 2>&1 || die "docker not found"

ARCHIVE_DIR="$(cd "$(dirname "$ARCHIVE")" && pwd)"
ARCHIVE_NAME="$(basename "$ARCHIVE")"

# shellcheck disable=SC2016 # the inner script expands inside the container
docker run --rm \
  -e "ARCHIVE=${ARCHIVE_NAME}" \
  -e "TOPDIR=${TOPDIR}" \
  -e "BASE_VERSION=${BASE_VERSION}" \
  -v "${ARCHIVE_DIR}:/dist:ro" \
  "$IMAGE" \
  bash -c '
    set -euo pipefail
    apt-get update -qq
    apt-get install -y --no-install-recommends \
      zstd libfontconfig1 libfreetype6 ca-certificates >/dev/null
    tmp="$(mktemp -d)"
    trap "rm -rf \"$tmp\"" EXIT
    zstd -dc "/dist/${ARCHIVE}" | tar -xf - -C "$tmp"
    exe="${tmp}/${TOPDIR}/bin/bitty"
    if [[ ! -x "$exe" ]]; then
      echo "extracted bin/bitty is missing or not executable: $exe" >&2
      exit 1
    fi
    out="$("$exe" --version)"
    echo "bitty --version => ${out}"
    if [[ "$out" != "bitty ${BASE_VERSION} (preview "*")" ]]; then
      echo "version/channel mismatch: ${out}" >&2
      exit 1
    fi
    timeout 120 "$exe" --headless
  '

echo "preview-linux-smoke: PASS (${ARCHIVE_NAME})"
