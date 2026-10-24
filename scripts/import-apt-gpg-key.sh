#!/usr/bin/env bash
# import-apt-gpg-key.sh — import and verify the APT archive signing key.
#
# The release secret has been provisioned in several shapes over time:
#   * ASCII-armored OpenPGP private key block
#   * base64 of the armored or binary key (possibly line-wrapped)
#   * armored block with literal "\n" or "\\n" escapes
#   * a quoted value, with a UTF-8 BOM, CRLF line endings, or surrounding text
#
# This helper normalizes those shapes, imports the key into GNUPGHOME, and
# verifies that the pinned signing fingerprint is present as a *secret* key.
# It never prints key material; failures include a bounded shape diagnostic and
# fail closed.
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
BEGIN_ARMOR="-----BEGIN PGP PRIVATE KEY BLOCK-----"
END_ARMOR="-----END PGP PRIVATE KEY BLOCK-----"
BOM=$'\xEF\xBB\xBF'

usage() {
  cat <<'EOF'
Usage: import-apt-gpg-key.sh [--key-env NAME] [--fingerprint FINGERPRINT]

Reads private key material from the named environment variable (default:
APT_GPG_PRIVATE_KEY). Accepts an ASCII-armored private key, a base64-encoded
armored or binary key, escaped newlines, surrounding quotes/whitespace/BOM,
CRLF line endings, and an armored block embedded in surrounding text. The key
is imported into GNUPGHOME and the expected fingerprint is verified to be
present as a secret key.

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

trim_ws() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

strip_quotes() {
  local value
  value="$(trim_ws "$1")"
  while [[ ${#value} -ge 2 ]]; do
    case "$value" in
    \"*\") value="${value:1:${#value}-2}" ;;
    \'*\') value="${value:1:${#value}-2}" ;;
    *) break ;;
    esac
    value="$(trim_ws "$value")"
  done
  printf '%s' "$value"
}

normalize_text() {
  local value="$1"
  value="${value#"$BOM"}"
  value="$(strip_quotes "$value")"
  value="${value#"$BOM"}"
  value="${value//$'\r'/}"
  trim_ws "$value"
}

unescape_newlines() {
  local value="$1"
  # Double-escaped first, then single-escaped, so both JSON layers collapse.
  value="${value//\\\\n/$'\n'}"
  value="${value//\\n/$'\n'}"
  printf '%s' "$value"
}

extract_armor() {
  awk -v begin="$BEGIN_ARMOR" -v end="$END_ARMOR" '
    {
      if (!copy) {
        p = index($0, begin)
        if (p > 0) {
          copy = 1
          $0 = substr($0, p)
        }
      }
      if (copy) {
        q = index($0, end)
        if (q > 0) {
          print substr($0, 1, q + length(end) - 1)
          exit
        }
        print
      }
    }
  ' <<<"$1"
}

shape_diagnostic() {
  local value="$1" bytes decode decoded_bytes magic first
  bytes="$(printf '%s' "$value" | wc -c | tr -d '[:space:]')"
  echo "APT key shape diagnostic (value is never printed):" >&2
  echo "  length: ${bytes} bytes" >&2
  if [[ "$value" == *"BEGIN PGP"* ]]; then
    echo "  contains BEGIN PGP: yes" >&2
  else
    echo "  contains BEGIN PGP: no" >&2
  fi
  if printf '%s' "$value" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1; then
    echo "  valid UTF-8: yes" >&2
  else
    echo "  valid UTF-8: no" >&2
  fi

  decode=no
  decoded_bytes=0
  magic=no
  rm -f "$tmp/shape-decoded"
  if [[ -n "$compact" && "$compact" != *"-----BEGIN"* ]] &&
    printf '%s' "$compact" | base64 --decode >"$tmp/shape-decoded" 2>/dev/null; then
    decode=yes
    decoded_bytes="$(wc -c <"$tmp/shape-decoded" | tr -d '[:space:]')"
    first="$(od -An -tu1 -N1 "$tmp/shape-decoded" | tr -d '[:space:]')"
    if [[ -n "$first" ]] && ((first >= 128)); then
      magic=yes
    fi
  fi
  echo "  base64 decode: ${decode} (${decoded_bytes} bytes)" >&2
  echo "  decoded OpenPGP packet magic: ${magic}" >&2
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

# Normalize the provisioning shapes. Base64 is decoded to a file (below) so
# binary payloads survive NUL bytes that command substitution would drop.
base="$(normalize_text "$material")"
literal="$(normalize_text "$(unescape_newlines "$base")")"
armored="$(extract_armor "$literal")"
compact="$(printf '%s' "$literal" | tr -d '[:space:]')"

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

for candidate in "$base" "$literal" "$armored"; do
  [[ -n "$candidate" ]] || continue
  printf '%s' "$candidate" >"$tmp/key"
  try_import && exit 0
done

# Base64 of an armored or binary OpenPGP payload. Decode to a file so NUL
# bytes in a binary payload are preserved.
if [[ -n "$compact" && "$compact" != *"-----BEGIN"* ]] &&
  printf '%s' "$compact" | base64 --decode >"$tmp/decoded" 2>/dev/null &&
  [[ -s "$tmp/decoded" ]]; then
  cp "$tmp/decoded" "$tmp/key"
  try_import && exit 0

  # A decoded armored block may still carry surrounding text.
  if iconv -f UTF-8 -t UTF-8 "$tmp/decoded" >/dev/null 2>&1; then
    decoded_text="$(<"$tmp/decoded")"
    decoded_armor="$(extract_armor "$(normalize_text "$decoded_text")")"
    if [[ -n "$decoded_armor" ]]; then
      printf '%s' "$decoded_armor" >"$tmp/key"
      try_import && exit 0
    fi
  fi
fi

if ((imported)); then
  found="$(secret_fingerprints | sort -u | paste -sd ', ' -)"
  error "imported secret key(s) [${found:-none}] do not include the pinned APT signing fingerprint ${EXPECTED_FP}"
fi

shape_diagnostic "$material"
error "could not parse ${KEY_ENV} as an armored, base64-encoded, or escaped-newline OpenPGP private key (see shape diagnostic above)"
