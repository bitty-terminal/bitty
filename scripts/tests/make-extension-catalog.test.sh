#!/usr/bin/env bash
# make-extension-catalog.test.sh — fixture test for
# scripts/make-extension-catalog.sh (issue #2440).
#
# No network, no R2 credentials, no uploads: a local `<name>/<version>/`
# tree drives discovery, artifact re-hashing, canonical ordering, and the
# append-only guards. The checked-in fixture also pins the golden bytes that
# the Rust parser test consumes, so the publisher and the client cannot drift
# apart silently.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/make-extension-catalog.sh
FIXTURE=./packaging/fixtures/extensions
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || {
  echo "SKIP: no sha256 tool found"
  exit 0
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/bitty/make-extension-catalog-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

# --- checked-in fixture reproduces the golden catalog ---
if "$SCRIPT" --source-dir "$FIXTURE" >"$TMP_BASE/golden.out" 2>"$TMP_BASE/golden.err"; then
  if cmp -s "$TMP_BASE/golden.out" "$FIXTURE/catalog.v1"; then
    pass "fixture generates the checked-in golden catalog"
  else
    fail "fixture output differs from $FIXTURE/catalog.v1"
  fi
else
  fail "fixture generation exited non-zero: $(cat "$TMP_BASE/golden.err")"
fi

# --- canonical ordering: name, numeric version, target ---
SRC="$TMP_BASE/order"
mkdir -p "$SRC"
write_extension() { # NAME VERSION TARGET
  local name="$1" version="$2" target="$3" dir
  dir="$SRC/$name/$version"
  mkdir -p "$dir"
  printf '%s-%s-%s fixture\n' "$name" "$version" "$target" >"$dir/$target.tar.gz"
  (cd "$dir" && printf '%s  %s.tar.gz\n' "$(sha256_file "$target.tar.gz")" "$target" >>SHA256SUMS)
}
write_extension graphics 0.0.10 x86_64-unknown-linux-gnu
write_extension graphics 0.0.9 x86_64-unknown-linux-gnu
write_extension graphics 0.0.9 aarch64-unknown-linux-gnu
write_extension net 0.0.1 x86_64-unknown-linux-gnu

if "$SCRIPT" --source-dir "$SRC" >"$TMP_BASE/order.out" 2>"$TMP_BASE/order.err"; then
  EXPECTED="$(printf '%s\n' \
    'extension graphics 0.0.9 aarch64-unknown-linux-gnu' \
    'extension graphics 0.0.9 x86_64-unknown-linux-gnu' \
    'extension graphics 0.0.10 x86_64-unknown-linux-gnu' \
    'extension net 0.0.1 x86_64-unknown-linux-gnu')"
  ACTUAL="$(grep '^extension ' "$TMP_BASE/order.out" | cut -d' ' -f1-5)"
  if [[ "$(printf '%s\n' "$ACTUAL" | cut -d' ' -f2-4)" == "$(printf '%s\n' "$EXPECTED" | cut -d' ' -f2-4)" ]]; then
    pass "records sort by name, numeric version (0.0.10 after 0.0.9), and target"
  else
    fail "unexpected record ordering:
$ACTUAL"
  fi
  if head -n 1 "$TMP_BASE/order.out" | grep -q '^# bitty extension catalog v1$' &&
    grep -qx 'schema 1' "$TMP_BASE/order.out"; then
    pass "catalog header carries the schema line"
  else
    fail "catalog header is missing or malformed"
  fi
else
  fail "ordering fixture generation exited non-zero: $(cat "$TMP_BASE/order.err")"
fi

# --- single record per SHA256SUMS entry, exact digest ---
EXPECTED_DIGEST="$(sha256_file "$SRC/graphics/0.0.9/x86_64-unknown-linux-gnu.tar.gz")"
if grep -qx "extension graphics 0.0.9 x86_64-unknown-linux-gnu $EXPECTED_DIGEST" "$TMP_BASE/order.out"; then
  pass "record digest is the re-hashed artifact digest"
else
  fail "record digest does not match the artifact"
fi

# --- digest mismatch fails closed ---
BAD="$TMP_BASE/bad-digest"
cp -R "$SRC" "$BAD"
printf 'tampered\n' >"$BAD/net/0.0.1/x86_64-unknown-linux-gnu.tar.gz"
if "$SCRIPT" --source-dir "$BAD" >/dev/null 2>"$TMP_BASE/bad.err"; then
  fail "digest mismatch was accepted"
else
  if grep -q 'digest mismatch' "$TMP_BASE/bad.err"; then
    pass "digest mismatch fails closed with an actionable error"
  else
    fail "digest mismatch error is not actionable: $(cat "$TMP_BASE/bad.err")"
  fi
fi

# --- missing artifact fails closed ---
MISSING="$TMP_BASE/missing"
cp -R "$SRC" "$MISSING"
rm "$MISSING/net/0.0.1/x86_64-unknown-linux-gnu.tar.gz"
if "$SCRIPT" --source-dir "$MISSING" >/dev/null 2>"$TMP_BASE/missing.err"; then
  fail "missing artifact was accepted"
else
  if grep -q 'missing artifact' "$TMP_BASE/missing.err"; then
    pass "missing artifact fails closed"
  else
    fail "missing artifact error is not actionable: $(cat "$TMP_BASE/missing.err")"
  fi
fi

# --- malformed manifest fails closed ---
MALFORMED="$TMP_BASE/malformed"
cp -R "$SRC" "$MALFORMED"
printf 'not-a-digest  x86_64-unknown-linux-gnu.tar.gz\n' >"$MALFORMED/net/0.0.1/SHA256SUMS"
if "$SCRIPT" --source-dir "$MALFORMED" >/dev/null 2>"$TMP_BASE/malformed.err"; then
  fail "malformed SHA256SUMS was accepted"
else
  if grep -q 'malformed SHA256SUMS' "$TMP_BASE/malformed.err"; then
    pass "malformed SHA256SUMS fails closed"
  else
    fail "malformed SHA256SUMS error is not actionable: $(cat "$TMP_BASE/malformed.err")"
  fi
fi

# --- duplicate manifest entry fails closed ---
DUPLICATE="$TMP_BASE/duplicate"
cp -R "$SRC" "$DUPLICATE"
cp "$DUPLICATE/net/0.0.1/SHA256SUMS" "$TMP_BASE/duplicate.manifest"
cat "$TMP_BASE/duplicate.manifest" >>"$DUPLICATE/net/0.0.1/SHA256SUMS"
if "$SCRIPT" --source-dir "$DUPLICATE" >/dev/null 2>"$TMP_BASE/duplicate.err"; then
  fail "duplicate SHA256SUMS entry was accepted"
else
  if grep -q 'duplicate SHA256SUMS entry' "$TMP_BASE/duplicate.err"; then
    pass "duplicate SHA256SUMS entry fails closed"
  else
    fail "duplicate entry error is not actionable: $(cat "$TMP_BASE/duplicate.err")"
  fi
fi

# --- empty discovery is refused unless explicitly allowed ---
EMPTY="$TMP_BASE/empty"
mkdir -p "$EMPTY"
if "$SCRIPT" --source-dir "$EMPTY" >/dev/null 2>"$TMP_BASE/empty.err"; then
  fail "empty discovery emitted a catalog by default"
else
  if grep -q 'refusing to emit an empty catalog' "$TMP_BASE/empty.err"; then
    pass "empty discovery refuses to emit a catalog"
  else
    fail "empty discovery error is not actionable: $(cat "$TMP_BASE/empty.err")"
  fi
fi
if "$SCRIPT" --source-dir "$EMPTY" --allow-empty >"$TMP_BASE/empty.out" 2>/dev/null; then
  if [[ "$(grep -c '^extension ' "$TMP_BASE/empty.out")" == "0" ]] && grep -qx 'schema 1' "$TMP_BASE/empty.out"; then
    pass "--allow-empty emits a schema-only catalog"
  else
    fail "--allow-empty output is not schema-only"
  fi
else
  fail "--allow-empty was refused"
fi

# --- --output writes byte-identical content ---
if "$SCRIPT" --source-dir "$FIXTURE" --output "$TMP_BASE/out.v1" >/dev/null 2>&1; then
  if cmp -s "$TMP_BASE/out.v1" "$FIXTURE/catalog.v1"; then
    pass "--output writes the same canonical bytes"
  else
    fail "--output content differs from stdout content"
  fi
else
  fail "--output mode exited non-zero"
fi

# --- usage guard: --check/--publish reject --output ---
if "$SCRIPT" --check --output "$TMP_BASE/x" >/dev/null 2>&1; then
  fail "--check accepted --output"
else
  pass "--check rejects --output"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "make-extension-catalog.test.sh: FAILED" >&2
  exit 1
fi
echo "make-extension-catalog.test.sh: OK"
