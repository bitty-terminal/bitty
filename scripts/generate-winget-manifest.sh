#!/usr/bin/env bash
# generate-winget-manifest.sh — generate Windows Package Manager (winget) manifests.
#
# Generates multi-file manifests (version, installer, defaultLocale) conforming
# to winget schema v1.6.0 for packaging under microsoft/winget-pkgs.
#
# Usage:
#   scripts/generate-winget-manifest.sh --version X.Y.Z \
#     --installer-sha256 HEX [--base-url URL] [--output-dir DIR]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

VERSION=""
INSTALLER_SHA=""
BASE_URL=""
OUTPUT_DIR=""

usage() {
  cat <<'EOF'
Usage:
  generate-winget-manifest.sh --version X.Y.Z --installer-sha256 HEX [OPTIONS]

Options:
  --base-url URL        Base URL hosting the setup installer
                        (default: https://cdn.bitty.run/releases/v<version>)
  --output-dir DIR      Destination directory for the manifest files
                        (default: packaging/windows/winget/manifests/b/BittyTerminal/Bitty/<version>)
  -h, --help            Show this help message
EOF
}

die() {
  echo "generate-winget-manifest: ERROR: $1" >&2
  exit 2
}

while [[ "$#" -gt 0 ]]; do
  case "$1" in
  --version)
    [[ "$#" -ge 2 && -n "${2:-}" ]] || die "missing value for --version"
    VERSION="$2"
    shift 2
    ;;
  --installer-sha256)
    [[ "$#" -ge 2 && -n "${2:-}" ]] || die "missing value for --installer-sha256"
    INSTALLER_SHA="$2"
    shift 2
    ;;
  --base-url)
    [[ "$#" -ge 2 && -n "${2:-}" ]] || die "missing value for --base-url"
    BASE_URL="$2"
    shift 2
    ;;
  --output-dir)
    [[ "$#" -ge 2 && -n "${2:-}" ]] || die "missing value for --output-dir"
    OUTPUT_DIR="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    die "unknown argument: $1"
    ;;
  esac
done

[[ -n "$VERSION" ]] || die "--version is required"
[[ -n "$INSTALLER_SHA" ]] || die "--installer-sha256 is required"

# Validate version format (X.Y.Z or X.Y.Z-prerelease)
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  die "invalid version format '$VERSION'; must be X.Y.Z or X.Y.Z-suffix"
fi

# Validate SHA256 (64 lowercase hex digits)
if ! [[ "$INSTALLER_SHA" =~ ^[0-9a-f]{64}$ ]]; then
  die "invalid sha256 '$INSTALLER_SHA'; must be exactly 64 lowercase hex digits"
fi

if [[ -z "$BASE_URL" ]]; then
  BASE_URL="https://cdn.bitty.run/releases/v${VERSION}"
fi

# Ensure base URL matches expected CDN host
if [[ "$BASE_URL" != "https://cdn.bitty.run/releases/v${VERSION}"* ]]; then
  die "base URL must start with https://cdn.bitty.run/releases/v${VERSION}"
fi

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="${REPO_ROOT}/packaging/windows/winget/manifests/b/BittyTerminal/Bitty/${VERSION}"
fi

mkdir -p "$OUTPUT_DIR"

INSTALLER_NAME="bitty-${VERSION}-windows-x86_64-setup.exe"
INSTALLER_URL="${BASE_URL}/${INSTALLER_NAME}"

# 1. Version manifest
cat >"${OUTPUT_DIR}/BittyTerminal.Bitty.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json

PackageIdentifier: BittyTerminal.Bitty
PackageVersion: ${VERSION}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
EOF

# 2. Installer manifest
cat >"${OUTPUT_DIR}/BittyTerminal.Bitty.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json

PackageIdentifier: BittyTerminal.Bitty
PackageVersion: ${VERSION}
MinimumOSVersion: 10.0.0.0
InstallerType: inno
Scope: user
InstallModes:
  - interactive
  - silent
  - silentWithProgress
InstallerSwitches:
  Silent: /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
  SilentWithProgress: /SILENT /SUPPRESSMSGBOXES /NORESTART
UpgradeBehavior: install
Commands:
  - bitty
ProductCode: '{3E8B4A2C-7F1D-4B6E-9A0C-5D2F8E1B7A40}_is1'
Installers:
  - Architecture: x64
    InstallerUrl: ${INSTALLER_URL}
    InstallerSha256: ${INSTALLER_SHA}
    AppsAndFeaturesEntries:
      - DisplayName: Bitty
        Publisher: Bitty Terminal
        DisplayVersion: ${VERSION}
        ProductCode: '{3E8B4A2C-7F1D-4B6E-9A0C-5D2F8E1B7A40}_is1'
ManifestType: installer
ManifestVersion: 1.6.0
EOF

# 3. Default locale manifest
cat >"${OUTPUT_DIR}/BittyTerminal.Bitty.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json

PackageIdentifier: BittyTerminal.Bitty
PackageVersion: ${VERSION}
PackageLocale: en-US
Publisher: Bitty Terminal
PublisherUrl: https://github.com/bitty-terminal/bitty
PublisherSupportUrl: https://github.com/bitty-terminal/bitty/issues
Author: Bitty Developers
PackageName: Bitty
PackageUrl: https://bitty.run
License: MIT OR Apache-2.0
LicenseUrl: https://github.com/bitty-terminal/bitty/blob/main/LICENSE
Copyright: Copyright (c) 2026 Bitty Developers
ShortDescription: Modern GPU-accelerated terminal emulator with panes and Lua extensions
Description: |-
  Bitty is a fast, keyboard-driven, GPU-accelerated terminal emulator designed
  for developer ergonomics, featuring native multiplexing (tabs and split panes),
  smooth rendering, and extensible configuration via Lua.
Moniker: bitty
Tags:
  - terminal
  - terminal-emulator
  - cli
  - console
  - shell
  - rust
ManifestType: defaultLocale
ManifestVersion: 1.6.0
EOF

echo "Generated winget manifests in ${OUTPUT_DIR}:"
ls -la "${OUTPUT_DIR}"
