#!/usr/bin/env bash
# generate-apt-repo.sh — Wrapper around scripts/generate-apt-repo.py
# Generates Debian/Ubuntu APT static repository indices and signs Release files.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PYTHON_SCRIPT="${SCRIPT_DIR}/generate-apt-repo.py"

if [[ ! -f "${PYTHON_SCRIPT}" ]]; then
  echo "Error: Python generator not found: ${PYTHON_SCRIPT}" >&2
  exit 1
fi

exec python3 "${PYTHON_SCRIPT}" "$@"
