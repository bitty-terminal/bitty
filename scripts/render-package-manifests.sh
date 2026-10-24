#!/usr/bin/env bash
# render-package-manifests.sh — render Homebrew/Scoop manifests for a release.
#
# The release workflow calls this after `r2-mirror` has published the immutable
# release payload. Both channels prefer the R2-backed CDN
# (https://cdn.bitty.run/releases/<TAG>/<asset>); GitHub Releases keeps the
# byte-identical fallback, documented inside each rendered file.
#
# Usage:
#   scripts/render-package-manifests.sh homebrew --version X.Y.Z \
#     --base-url https://cdn.bitty.run/releases/vX.Y.Z \
#     --intel-asset NAME --arm-asset NAME \
#     --sha-intel HEX --sha-arm HEX
#   scripts/render-package-manifests.sh scoop --version X.Y.Z \
#     --base-url https://cdn.bitty.run/releases/vX.Y.Z \
#     --x64-asset NAME --arm64-asset NAME \
#     --sha-x64 HEX --sha-arm64 HEX
#
# The rendered manifest goes to stdout. Inputs are validated before rendering:
# the version is X.Y.Z (optionally with a prerelease/build suffix), the base URL
# is exactly the R2/CDN release path, asset names are plain basenames, and every
# digest is a lowercase 64-hex SHA-256. Exit 2 on usage or validation errors.
set -euo pipefail

MODE=""
VERSION=""
BASE_URL=""
ASSET_A=""
ASSET_B=""
SHA_A=""
SHA_B=""

usage() {
  cat <<'EOF'
Usage:
  render-package-manifests.sh homebrew --version X.Y.Z --base-url URL \
    --intel-asset NAME --arm-asset NAME --sha-intel HEX --sha-arm HEX
  render-package-manifests.sh scoop --version X.Y.Z --base-url URL \
    --x64-asset NAME --arm64-asset NAME --sha-x64 HEX --sha-arm64 HEX

Writes the rendered manifest to stdout.
EOF
}

die() {
  echo "render-package-manifests: ERROR: $1" >&2
  exit 2
}

require_value() { # <flag> <remaining-args>
  [[ "$2" -ge 2 && -n "${3:-}" ]] || die "missing value for $1"
}

[[ "$#" -ge 1 ]] || {
  usage >&2
  exit 2
}

case "$1" in
homebrew | scoop)
  MODE="$1"
  shift
  ;;
-h | --help)
  usage
  exit 0
  ;;
*)
  die "unknown mode: $1"
  ;;
esac

while [[ "$#" -gt 0 ]]; do
  case "$1" in
  --version)
    require_value "$1" "$#" "${2:-}"
    VERSION="$2"
    shift 2
    ;;
  --base-url)
    require_value "$1" "$#" "${2:-}"
    BASE_URL="$2"
    shift 2
    ;;
  --intel-asset | --x64-asset)
    require_value "$1" "$#" "${2:-}"
    ASSET_A="$2"
    shift 2
    ;;
  --arm-asset | --arm64-asset)
    require_value "$1" "$#" "${2:-}"
    ASSET_B="$2"
    shift 2
    ;;
  --sha-intel | --sha-x64)
    require_value "$1" "$#" "${2:-}"
    SHA_A="$2"
    shift 2
    ;;
  --sha-arm | --sha-arm64)
    require_value "$1" "$#" "${2:-}"
    SHA_B="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    die "unknown option: $1"
    ;;
  esac
done

[[ -n "$VERSION" ]] || die "--version is required"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$ ]] ||
  die "version must look like X.Y.Z, got: $VERSION"
[[ -n "$BASE_URL" ]] || die "--base-url is required"
EXPECTED_BASE="https://cdn.bitty.run/releases/v${VERSION}"
[[ "$BASE_URL" == "$EXPECTED_BASE" ]] ||
  die "base URL must be ${EXPECTED_BASE} (R2/CDN primary source), got: $BASE_URL"
[[ -n "$ASSET_A" && -n "$ASSET_B" ]] || die "both architecture asset names are required"
for asset in "$ASSET_A" "$ASSET_B"; do
  [[ "$asset" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
    die "asset name must be a plain basename, got: $asset"
done
for sha in "$SHA_A" "$SHA_B"; do
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] ||
    die "digest must be a lowercase 64-hex SHA-256, got: $sha"
done

render_homebrew() {
  cat <<EOF
class Bitty < Formula
  desc "Bitty pre-alpha terminal workspace minimal correct terminal"
  homepage "https://github.com/bitty-terminal/bitty"
  version "${VERSION}"
  license any_of: ["MIT", "Apache-2.0"]

  # Primary download source: the R2-backed CDN. The same bytes are published
  # on GitHub Releases, documented here as the fallback:
  #   https://github.com/bitty-terminal/bitty/releases/download/v${VERSION}/${ASSET_A}
  #   https://github.com/bitty-terminal/bitty/releases/download/v${VERSION}/${ASSET_B}
  if OS.mac? && Hardware::CPU.intel?
    url "${BASE_URL}/${ASSET_A}"
    sha256 "${SHA_A}"
  elsif OS.mac? && Hardware::CPU.arm?
    url "${BASE_URL}/${ASSET_B}"
    sha256 "${SHA_B}"
  end

  def install
    bin.install "${ASSET_A}" => "bitty" if Hardware::CPU.intel?
    bin.install "${ASSET_B}" => "bitty" if Hardware::CPU.arm?
  end

  test do
    assert_match "${VERSION}", shell_output("#{bin}/bitty --version")
  end
end
EOF
}

render_scoop() {
  cat <<EOF
{
    "version": "${VERSION}",
    "description": "Bitty pre-alpha terminal workspace minimal correct terminal",
    "homepage": "https://github.com/bitty-terminal/bitty",
    "license": "MIT OR Apache-2.0",
    "notes": "Downloads use the R2-backed CDN (cdn.bitty.run). GitHub Releases fallback: https://github.com/bitty-terminal/bitty/releases/tag/v${VERSION}",
    "architecture": {
        "64bit": {
            "url": "${BASE_URL}/${ASSET_A}",
            "hash": "${SHA_A}",
            "bin": [["${ASSET_A}", "bitty"]]
        },
        "arm64": {
            "url": "${BASE_URL}/${ASSET_B}",
            "hash": "${SHA_B}",
            "bin": [["${ASSET_B}", "bitty"]]
        }
    },
    "checkver": {
        "github": "https://github.com/bitty-terminal/bitty"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://cdn.bitty.run/releases/v\$version/${ASSET_A}"
            },
            "arm64": {
                "url": "https://cdn.bitty.run/releases/v\$version/${ASSET_B}"
            }
        }
    }
}
EOF
}

case "$MODE" in
homebrew) render_homebrew ;;
scoop) render_scoop ;;
esac
