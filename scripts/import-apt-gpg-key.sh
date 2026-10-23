#!/usr/bin/env bash
# import-apt-gpg-key.sh — import and verify the APT archive signing key.
#
# The release secret has been provisioned in several shapes over time:
#   * ASCII-armored OpenPGP private key block
#   * base64 of the armored block (possibly one long line)
#   * armored block with literal "\n" escapes instead of real newlines
#
# This helper normalizes those shapes, imports the key into GNUPGHOME, and
# verifies that the pinned signing fingerprint is present as a *secret* key.
# It never prints key material and fails closed with a clear error.
#
# Usage:
#   APT_GPG_PRIVATE_KEY=... GNUPGHOME=/path/to/gnupg \
#     scripts/import-apt-gpg-key.sh [--fingerprint <40-hex-fp>]
#
# The workflow sets GNUPGHOME to a fresh per-run directory; locally the
# default GnuPG home is used when GNUPGHOME is unset.
set -euo pipefail

PINNED_FINGERPRINT="BCB87D7B489A64168080D900DF4751B6EA711CB8"
KEY_ENV="APT_GPG_PRIVATE_KEY"
EXPECTED_FP="$PINNED_FINGERPRINT"

usage() {
  cat <<'EOF'
Usage: import-apt-gpg-key.sh [--key-env NAME] [--fingerprint FINGERPRINT]

Reads private key material from the named environment variable (default:
APT_GPG_PRIVATE_KEY). Accepts an ASCII-armored private key, a base64-encoded
armored key, or an armored key with literal "\n" escapes. The key is imported
into GNUPGHOME and the expected fingerprint is verified to be present as a
secret key.

Options:
  --key-env NAME              Environment variable holding the key material
  --fingerprint FINGERPRINT   40-hex fingerprint that must be imported
                              (default: the pinned Bitty archive key)
  -h, --help                  Show this help
EOF
}

error() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    echo "::error title=APT signing key import failed::$*" >&2
  else
    echo "error: $*" >&2
  fi
  exit 1
}

while (($# > 0)); do
  case "$1" in
  --key-env)
    (($# >= 2)) || error "--key-env requires a value"
    KEY_ENV="$2"
    shift 2
    ;;
  --fingerprint)
    (($# >= 2)) || error "--fingerprint requires a value"
    EXPECTED_FP="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    error "unknown argument: $1"
    ;;
  esac
done

[[ "$KEY_ENV" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] ||
  error "invalid environment variable name: $KEY_ENV"
EXPECTED_FP="$(printf '%s' "$EXPECTED_FP" | tr '[:lower:]' '[:upper:]')"
[[ "$EXPECTED_FP" =~ ^[0-9A-F]{40}$ ]] ||
  error "expected fingerprint must be 40 hex characters"

[[ -n "${!KEY_ENV:-}" ]] || error "$KEY_ENV is empty"
material="${!KEY_ENV}"

umask 077
tmp="$(mktemp -d "${TMPDIR:-/tmp}/import-apt-gpg-key.XXXXXX")"
cleanup() {
  if [[ -f "$tmp/key" ]]; then
    if command -v shred >/dev/null 2>&1; then
      shred -u "$tmp/key" 2>/dev/null || rm -f "$tmp/key"
    else
      rm -f "$tmp/key"
    fi
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

gnupg_home="${GNUPGHOME:-${HOME:-}/.gnupg}"
[[ -n "$gnupg_home" && "$gnupg_home" != "/.gnupg" ]] ||
  error "GNUPGHOME or HOME must be set"
mkdir -p "$gnupg_home"
chmod 700 "$gnupg_home"
export GNUPGHOME="$gnupg_home"

# Normalize literal "\n" escapes and carriage returns. Base64 is decoded
# directly to a file (below) so binary payloads survive NUL bytes that command
# substitution would drop.
literal_newlines="${material//\\n/$'\n'}"
normalized="${literal_newlines//$'\r'/}"
compact="$(printf '%s' "$normalized" | tr -d '[:space:]')"

secret_fingerprints() {
  gpg --batch --with-colons --list-secret-keys 2>/dev/null |
    awk -F: '$1 == "fpr" { print toupper($10) }'
}

imported=0
try_import() {
  if gpg --batch --quiet --import "$tmp/key" 2>"$tmp/import.err"; then
    imported=1
    while IFS= read -r fpr; do
      if [[ "$fpr" == "$EXPECTED_FP" ]]; then
        echo "imported and verified secret APT signing key ${EXPECTED_FP}"
        return 0
      fi
    done < <(secret_fingerprints)
  fi
  return 1
}

for candidate in "$material" "$literal_newlines" "$normalized"; do
  [[ -n "$candidate" ]] || continue
  printf '%s' "$candidate" >"$tmp/key"
  try_import && exit 0
done

# Base64 of an armored or binary OpenPGP payload. Decode to a file so NUL
# bytes in a binary payload are preserved.
if [[ -n "$compact" && "$compact" != *"-----BEGIN"* ]]; then
  if printf '%s' "$compact" | base64 --decode >"$tmp/key" 2>/dev/null &&
    [[ -s "$tmp/key" ]]; then
    try_import && exit 0
  fi
fi

if ((imported)); then
  found="$(secret_fingerprints | sort -u | paste -sd ', ' -)"
  error "imported secret key(s) [${found:-none}] do not include the pinned APT signing fingerprint ${EXPECTED_FP}"
fi

error "could not parse ${KEY_ENV} as an armored, base64-encoded, or escaped-newline OpenPGP private key"
