#!/usr/bin/env bash
# Bitty APT repository setup script
# Supports: Debian, Ubuntu, Linux Mint, Pop!_OS, Kali Linux, and derivatives
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Error: this setup script must be run as root (e.g. curl -fsSL https://cdn.bitty.run/apt/setup.sh | sudo bash)" >&2
  exit 1
fi

KEYRING_DIR="/etc/apt/keyrings"
KEYRING_PATH="${KEYRING_DIR}/bitty-archive-keyring.gpg"
SOURCES_PATH="/etc/apt/sources.list.d/bitty.sources"
CDN_BASE="https://cdn.bitty.run/apt"

echo "==> Setting up Bitty APT repository..."

# Ensure prerequisites are available
if ! command -v curl >/dev/null 2>&1 || ! command -v gpg >/dev/null 2>&1; then
  echo "==> Installing prerequisites (curl, ca-certificates, gnupg)..."
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends curl ca-certificates gnupg
fi

# Install signing keyring
mkdir -p "${KEYRING_DIR}"
chmod 0755 "${KEYRING_DIR}"
echo "==> Installing GPG keyring to ${KEYRING_PATH}..."
curl -fsSL "${CDN_BASE}/bitty-archive-keyring.gpg" -o "${KEYRING_PATH}"
chmod 0644 "${KEYRING_PATH}"

# Install deb822 sources definition
echo "==> Installing source definition to ${SOURCES_PATH}..."
cat >"${SOURCES_PATH}" <<'EOF'
Types: deb
URIs: https://cdn.bitty.run/apt
Suites: stable
Components: main
Architectures: amd64 arm64
Signed-By: /etc/apt/keyrings/bitty-archive-keyring.gpg
EOF
chmod 0644 "${SOURCES_PATH}"

# Refresh repository cache
echo "==> Updating APT package cache..."
apt-get update -o Dir::Etc::sourcelist="sources.list.d/bitty.sources" \
  -o Dir::Etc::sourceparts="-" \
  -o APT::Get::List-Cleanup="0" 2>/dev/null || apt-get update

echo "==> Bitty APT repository configured successfully!"
echo "    Install Bitty with: sudo apt install -y bitty"
