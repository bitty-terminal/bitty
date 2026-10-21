#!/usr/bin/env bash
# verify-cdn-sidecar.sh — fetch a release asset plus its published .sha256
# sidecar and fail closed unless the sidecar describes the downloaded bytes.
#
# CTX-1433 (#2364). One implementation for the tag-time publishers in
# `.github/workflows/release.yml` (`aur-bin`, `homebrew`, `scoop`). Each job
# used to inline the same download/validate/compare block, which no gate could
# exercise: a locally computed digest compared against a locally written
# sidecar passed the string-based checks. Pinning the digest to the published
# `<base-url>/<asset>.sha256` here makes the published sidecar authoritative
# and lets `scripts/tests/check-package-manifests.test.sh` run the real logic
# against a file:// fixture.
#
# Usage: scripts/verify-cdn-sidecar.sh <base-url> <asset> [<dest-dir>]
#   <base-url>  release base URL (https://, http://, or file:// for fixtures)
#   <asset>     bare asset file name; must not contain a directory component
#   <dest-dir>  optional scratch dir; defaults to a fresh `mktemp -d`
#
# stdout: the verified lowercase 64-hex digest, one line, nothing else.
# stderr: failure diagnostics.
# Exit: 0 only when the published sidecar and the downloaded bytes agree;
# 1 on usage error, transport failure, malformed sidecar, filename mismatch,
# or digest mismatch. Callers under `set -e` therefore fail closed.
set -euo pipefail

usage() {
  printf 'usage: %s <base-url> <asset> [<dest-dir>]\n' \
    "$(basename "${BASH_SOURCE[0]}")" >&2
  exit 1
}

fail() {
  printf 'verify-cdn-sidecar: %s\n' "$1" >&2
  exit 1
}

[[ $# -ge 2 && $# -le 3 ]] || usage

BASE_URL="${1%/}"
ASSET="$2"
DEST_DIR="${3:-}"

[[ -n "$BASE_URL" ]] || usage
# Bare file name only: no traversal, no nested path, no leading dash.
[[ "$ASSET" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] ||
  fail "asset name outside [A-Za-z0-9._+-]: ${ASSET}"
case "$BASE_URL" in
  https://* | http://* | file://*) ;;
  *) fail "unsupported base URL scheme: ${BASE_URL}" ;;
esac

if [[ -z "$DEST_DIR" ]]; then
  DEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/verify-cdn-sidecar.XXXXXX")"
fi
mkdir -p "$DEST_DIR"
ASSET_PATH="$DEST_DIR/$ASSET"
SIDECAR_PATH="${ASSET_PATH}.sha256"

fetch() { # <url> <dest>
  curl --fail --location --silent --show-error \
    --proto '=https,http,file' --proto-redir '=https,http,file' \
    --output "$2" "$1"
}

fetch "${BASE_URL}/${ASSET}" "$ASSET_PATH" ||
  fail "could not download ${BASE_URL}/${ASSET}"
fetch "${BASE_URL}/${ASSET}.sha256" "$SIDECAR_PATH" ||
  fail "could not download ${BASE_URL}/${ASSET}.sha256 (published sidecar required)"

EXPECTED="$(awk 'NR == 1 { print $1 }' "$SIDECAR_PATH")"
printf '%s' "$EXPECTED" | grep -Eq '^[0-9a-f]{64}$' ||
  fail "sidecar ${ASSET}.sha256 did not yield a 64-hex digest: ${EXPECTED:-<empty>}"

# GNU `sha256sum` format is `<hex>  <filename>`; the filename column may carry
# the build-time `dist/` prefix, so only the basename is compared.
NAMED="$(awk 'NR == 1 { print $2 }' "$SIDECAR_PATH")"
if [[ -n "$NAMED" && "$(basename -- "$NAMED")" != "$ASSET" ]]; then
  fail "sidecar ${ASSET}.sha256 names a different file: ${NAMED}"
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$ASSET_PATH" | awk '{ print $1 }')"
else
  ACTUAL="$(shasum -a 256 "$ASSET_PATH" | awk '{ print $1 }')"
fi

if [[ "$ACTUAL" != "$EXPECTED" ]]; then
  fail "digest mismatch for ${ASSET}: sidecar says ${EXPECTED}, downloaded bytes hash to ${ACTUAL}"
fi

printf '%s\n' "$EXPECTED"
