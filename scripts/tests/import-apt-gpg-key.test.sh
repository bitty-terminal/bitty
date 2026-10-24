#!/usr/bin/env bash
# import-apt-gpg-key.test.sh — hermetic tests for scripts/import-apt-gpg-key.sh.
#
# Generates throwaway keys in temporary GnuPG homes (no network, no user
# keyring) and proves the importer accepts every provisioned secret shape:
#   * ASCII-armored private key
#   * base64 of the armored key and of the binary export
#   * literal "\n" and double-escaped "\\n" newlines
#   * UTF-8 BOM, surrounding quotes, surrounding whitespace, CRLF, and an
#     armored block embedded in text
#   * whitespace-wrapped base64
# and fails closed for:
#   * a key whose fingerprint does not match the expected pinned key
#   * public-only key material (no secret key)
#   * unparseable input, with a bounded shape diagnostic
# Every output must never echo key material.
set -euo pipefail

cd "$(dirname "$0")/../.."

IMPORT=./scripts/import-apt-gpg-key.sh
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/import-apt-gpg-key-test.XXXXXX")"
BOM=$'\xEF\xBB\xBF'

cleanup() {
  for home in "$TMP"/home-*; do
    [[ -d "$home" ]] || continue
    GNUPGHOME="$home" gpgconf --kill gpg-agent >/dev/null 2>&1 || true
  done
  rm -rf "$TMP"
}
trap cleanup EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

gen_key() { # <gnupg-home> <uid>
  local home="$1" uid="$2"
  mkdir -p "$home"
  chmod 700 "$home"
  GNUPGHOME="$home" gpg --batch --pinentry-mode loopback --passphrase '' \
    --quick-generate-key "$uid" ed25519 sign 0 2>/dev/null
  GNUPGHOME="$home" gpg --batch --with-colons --list-secret-keys |
    awk -F: '$1 == "fpr" { print $10; exit }'
}

run_import() { # <gnupg-home> <key-material> [extra args...]
  local home="$1" material="$2"
  shift 2
  GNUPGHOME="$home" APT_GPG_PRIVATE_KEY="$material" "$IMPORT" "$@"
}

has_secret_fpr() { # <gnupg-home> <fingerprint>
  GNUPGHOME="$1" gpg --batch --with-colons --list-secret-keys 2>/dev/null |
    awk -F: '$1 == "fpr" { print $10 }' |
    grep -qxF "$2"
}

assert_no_key_material() { # <label> <output>
  if [[ "$2" == *"-----BEGIN PGP"* ]] || [[ "$2" == *"PRIVATE KEY BLOCK"* ]]; then
    fail "$1 leaked key material"
  else
    pass "$1 does not print key material"
  fi
}

success_case() { # <slug> <label> <material>
  local home="$TMP/home-$1" label="$2" material="$3" out=""
  if out="$(run_import "$home" "$material" --fingerprint "$KEY_FP" 2>&1)"; then
    pass "$label imports"
  else
    fail "$label import failed: $out"
  fi
  if has_secret_fpr "$home" "$KEY_FP"; then
    pass "$label verifies as a secret key"
  else
    fail "$label is not listed as a secret key"
  fi
  assert_no_key_material "$label import" "$out"
}

KEY_HOME="$TMP/home-source"
KEY_FP="$(gen_key "$KEY_HOME" 'Bitty Release Test (throwaway) <release-test@example.invalid>')"
[[ -n "$KEY_FP" ]] || {
  echo "FAIL: could not generate throwaway key" >&2
  exit 1
}

OTHER_HOME="$TMP/home-other"
OTHER_FP="$(gen_key "$OTHER_HOME" 'Bitty Release Other (throwaway) <release-other@example.invalid>')"
[[ -n "$OTHER_FP" && "$OTHER_FP" != "$KEY_FP" ]] || {
  echo "FAIL: could not generate distinct throwaway key" >&2
  exit 1
}

ARMORED="$(GNUPGHOME="$KEY_HOME" gpg --batch --armor --export-secret-keys "$KEY_FP")"
PUBLIC_ARMORED="$(GNUPGHOME="$KEY_HOME" gpg --batch --armor --export "$KEY_FP")"
BASE64="$(printf '%s' "$ARMORED" | base64 --wrap=0)"
BINARY_BASE64="$(GNUPGHOME="$KEY_HOME" gpg --batch --export-secret-keys "$KEY_FP" | base64 --wrap=0)"
ESCAPED="$(printf '%s' "$ARMORED" | awk 'BEGIN { ORS = "" } { print $0 "\\n" }')"
DOUBLE_ESCAPED="${ARMORED//$'\n'/\\\\n}"
BOM_ARMORED="${BOM}${ARMORED}"
SINGLE_QUOTED="'${ARMORED}'"
DOUBLE_QUOTED="\"${ARMORED}\""
WHITESPACE_ARMORED=$'\n\t  '"${ARMORED}"$'\r\n  '
CRLF_ARMORED="${ARMORED//$'\n'/$'\r\n'}"
SURROUNDED_ARMORED="prefix text"$'\n'"${ARMORED}"$'\n'"suffix text"
WRAPPED_BASE64="$(printf '%s' "$BASE64" | fold -w 64 | sed 's/$/  /')"
GARBAGE_BASE64="$(printf '%s' 'this is not an OpenPGP key' | base64 --wrap=0)"

[[ "$ARMORED" == *"-----BEGIN PGP PRIVATE KEY BLOCK-----"* ]] ||
  fail "fixture armored export is missing the private key header"
[[ "$ESCAPED" != *$'\n'* ]] || fail "fixture escaped form still contains real newlines"
[[ "$DOUBLE_ESCAPED" != *$'\n'* ]] || fail "fixture double-escaped form still contains real newlines"
[[ "$DOUBLE_ESCAPED" == *'\\n'* ]] || fail "fixture double-escaped form lost its two-backslash sequences"
[[ "$SURROUNDED_ARMORED" == prefix* ]] || fail "fixture surrounding-text form lost its prefix"

# --- Success shapes ---------------------------------------------------------
success_case armored "armored key" "$ARMORED"
success_case base64 "base64-encoded armored key" "$BASE64"
success_case binary-base64 "base64-encoded binary key" "$BINARY_BASE64"
success_case escaped "escaped-newline key" "$ESCAPED"
success_case double-escaped "double-escaped-newline key" "$DOUBLE_ESCAPED"
success_case bom "BOM-prefixed key" "$BOM_ARMORED"
success_case single-quoted "single-quoted key" "$SINGLE_QUOTED"
success_case double-quoted "double-quoted key" "$DOUBLE_QUOTED"
success_case whitespace "whitespace-wrapped key" "$WHITESPACE_ARMORED"
success_case crlf "CRLF key" "$CRLF_ARMORED"
success_case surrounded "armored key with surrounding text" "$SURROUNDED_ARMORED"
success_case wrapped-base64 "whitespace-wrapped base64 key" "$WRAPPED_BASE64"

# --- Fail-closed cases ------------------------------------------------------
out=""
if out="$(run_import "$TMP/home-wrong" "$ARMORED" --fingerprint "$OTHER_FP" 2>&1)"; then
  fail "import with a mismatched expected fingerprint succeeded"
else
  pass "import with a mismatched expected fingerprint fails"
fi
if [[ "$out" == *"do not include the pinned APT signing fingerprint"* ]]; then
  pass "wrong-fingerprint failure names the pinned fingerprint check"
else
  fail "wrong-fingerprint failure is unclear: $out"
fi
assert_no_key_material "wrong-fingerprint failure" "$out"

out=""
if out="$(run_import "$TMP/home-public" "$PUBLIC_ARMORED" --fingerprint "$KEY_FP" 2>&1)"; then
  fail "public-only key material was accepted as a signing key"
else
  pass "public-only key material is rejected"
fi
if [[ "$out" == *"secret key(s)"* ]]; then
  pass "public-only failure reports the missing secret key"
else
  fail "public-only failure is unclear: $out"
fi
assert_no_key_material "public-only failure" "$out"

out=""
if out="$(run_import "$TMP/home-garbage" '%not-a-key%' --fingerprint "$KEY_FP" 2>&1)"; then
  fail "unparseable key material was accepted"
else
  pass "unparseable key material is rejected"
fi
for needle in \
  "APT key shape diagnostic" \
  "length: " \
  "contains BEGIN PGP: no" \
  "valid UTF-8: yes" \
  "base64 decode: no (0 bytes)" \
  "decoded OpenPGP packet magic: no"; do
  if [[ "$out" == *"$needle"* ]]; then
    pass "shape diagnostic includes '$needle'"
  else
    fail "shape diagnostic missing '$needle': $out"
  fi
done
if [[ "$out" == *"%not-a-key%"* ]]; then
  fail "shape diagnostic echoed the secret value"
else
  pass "shape diagnostic does not echo the secret value"
fi

out=""
if out="$(run_import "$TMP/home-garbage-b64" "$GARBAGE_BASE64" --fingerprint "$KEY_FP" 2>&1)"; then
  fail "base64 garbage was accepted"
else
  pass "base64 garbage is rejected"
fi
if [[ "$out" == *"base64 decode: yes ("* ]] &&
  [[ "$out" == *"decoded OpenPGP packet magic: no"* ]]; then
  pass "shape diagnostic reports base64 success without OpenPGP magic"
else
  fail "base64 shape diagnostic is unclear: $out"
fi
assert_no_key_material "base64 garbage failure" "$out"

out=""
if out="$(run_import "$TMP/home-empty" '' --fingerprint "$KEY_FP" 2>&1)"; then
  fail "empty key material was accepted"
else
  pass "empty key material is rejected"
fi
if [[ "$out" == *"APT_GPG_PRIVATE_KEY is empty"* ]]; then
  pass "empty failure names the environment variable"
else
  fail "empty failure is unclear: $out"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "import-apt-gpg-key.test: FAIL" >&2
  exit 1
fi
echo "import-apt-gpg-key.test: PASS"
