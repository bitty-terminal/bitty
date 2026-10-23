#!/usr/bin/env bash
# import-apt-gpg-key.test.sh — hermetic tests for scripts/import-apt-gpg-key.sh.
#
# Generates throwaway keys in temporary GnuPG homes (no network, no user
# keyring) and proves the importer accepts every provisioned secret shape:
#   1. ASCII-armored private key
#   2. base64-encoded armored key
#   3. armored key with literal "\n" escapes
# and fails closed for:
#   4. a key whose fingerprint does not match the expected pinned key
#   5. public-only key material (no secret key)
#   6. unparseable input
# Every failure must be explicit and must never echo key material.
set -euo pipefail

cd "$(dirname "$0")/../.."

IMPORT=./scripts/import-apt-gpg-key.sh
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/import-apt-gpg-key-test.XXXXXX")"

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

[[ "$ARMORED" == *"-----BEGIN PGP PRIVATE KEY BLOCK-----"* ]] ||
  fail "fixture armored export is missing the private key header"
[[ "$ESCAPED" != *$'\n'* ]] || fail "fixture escaped form still contains real newlines"

# --- 1. armored -------------------------------------------------------------
out=""
if out="$(run_import "$TMP/home-armored" "$ARMORED" --fingerprint "$KEY_FP" 2>&1)"; then
  pass "armored key imports"
else
  fail "armored key import failed: $out"
fi
if has_secret_fpr "$TMP/home-armored" "$KEY_FP"; then
  pass "armored key verifies as a secret key"
else
  fail "armored key is not listed as a secret key"
fi
assert_no_key_material "armored import" "$out"

# --- 2. base64 --------------------------------------------------------------
out=""
if out="$(run_import "$TMP/home-base64" "$BASE64" --fingerprint "$KEY_FP" 2>&1)"; then
  pass "base64-encoded key imports"
else
  fail "base64-encoded key import failed: $out"
fi
if has_secret_fpr "$TMP/home-base64" "$KEY_FP"; then
  pass "base64-encoded key verifies as a secret key"
else
  fail "base64-encoded key is not listed as a secret key"
fi
assert_no_key_material "base64 import" "$out"

# --- 3. escaped newlines ----------------------------------------------------
out=""
if out="$(run_import "$TMP/home-escaped" "$ESCAPED" --fingerprint "$KEY_FP" 2>&1)"; then
  pass "escaped-newline key imports"
else
  fail "escaped-newline key import failed: $out"
fi
if has_secret_fpr "$TMP/home-escaped" "$KEY_FP"; then
  pass "escaped-newline key verifies as a secret key"
else
  fail "escaped-newline key is not listed as a secret key"
fi
assert_no_key_material "escaped-newline import" "$out"

# --- 3b. base64 of the binary export ---------------------------------------
out=""
if out="$(run_import "$TMP/home-binary-base64" "$BINARY_BASE64" --fingerprint "$KEY_FP" 2>&1)"; then
  pass "base64-encoded binary key imports"
else
  fail "base64-encoded binary key import failed: $out"
fi
if has_secret_fpr "$TMP/home-binary-base64" "$KEY_FP"; then
  pass "base64-encoded binary key verifies as a secret key"
else
  fail "base64-encoded binary key is not listed as a secret key"
fi
assert_no_key_material "base64 binary import" "$out"

# --- 4. wrong fingerprint ---------------------------------------------------
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

# --- 5. public-only material ------------------------------------------------
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

# --- 6. unparseable input ---------------------------------------------------
out=""
if out="$(run_import "$TMP/home-garbage" 'not a key at all' --fingerprint "$KEY_FP" 2>&1)"; then
  fail "unparseable key material was accepted"
else
  pass "unparseable key material is rejected"
fi
if [[ "$out" == *"could not parse APT_GPG_PRIVATE_KEY"* ]]; then
  pass "unparseable failure names the accepted input shapes"
else
  fail "unparseable failure is unclear: $out"
fi

# --- 7. empty secret --------------------------------------------------------
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
