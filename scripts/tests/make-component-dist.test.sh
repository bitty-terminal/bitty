#!/usr/bin/env bash
# make-component-dist.test.sh — CTX-1063 fixture test for
# scripts/make-component-dist.sh (issue #1792, R2 prebuilt component dist).
#
# No network, no cargo build, no uploads: an ephemeral staged component dir
# (descriptor + fake executable) drives pack/manifest/URL legs, and a
# hostile corpus pins the fail-closed URL allowlist. Follows the PASS:/FAIL:
# convention of the other scripts/tests/*.test.sh gates.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/make-component-dist.sh
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

command -v tar >/dev/null 2>&1 || {
  echo "SKIP: tar not found"
  exit 0
}
command -v gzip >/dev/null 2>&1 || {
  echo "SKIP: gzip not found"
  exit 0
}
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "SKIP: no sha256 tool found"
  exit 0
fi

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/make-component-dist-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

NAME="net"
VERSION="0.0.99"
TARGET="x86_64-unknown-linux-gnu"
TARGET2="aarch64-unknown-linux-gnu"

SRC="$TMP_BASE/src"
OUT="$TMP_BASE/out"
mkdir -p "$SRC" "$OUT"

# Fake executable: reports the fixture version when run.
printf '#!/bin/sh\necho "bitty-net %s"\n' "$VERSION" >"$SRC/bitty-net"
chmod +x "$SRC/bitty-net"

write_descriptor() { # [extra lines...] — base valid descriptor plus extras
  {
    echo "[component]"
    echo "name = \"$NAME\""
    echo "version = \"$VERSION\""
    echo "protocol = [1, 1]"
    echo "executable = \"bitty-$NAME\""
    for extra in "$@"; do
      printf '%s\n' "$extra"
    done
  } >"$SRC/bitty-component.toml"
}

write_descriptor

# --- dry-run validates and stages without writing ---
if "$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz" --dry-run >/dev/null 2>&1; then
  if [[ -e "$OUT/$TARGET.tar.gz" ]]; then
    fail "dry-run wrote the tarball"
  else
    pass "dry-run validates and stages without writing"
  fi
else
  fail "dry-run exited non-zero"
fi

# --- real assembly ---
if ! "$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz" >/dev/null 2>&1; then
  fail "assembly exited non-zero"
fi
[[ -f "$OUT/$TARGET.tar.gz" ]] || fail "tarball not created"
[[ -f "$OUT/$TARGET.tar.gz.sha256" ]] || fail "sidecar not created"
[[ -f "$OUT/SHA256SUMS" ]] || fail "SHA256SUMS not created"

# --- exact member list at the archive root (no wrapper dir) ---
EXPECTED="$(printf '%s\n%s\n' "bitty-component.toml" "bitty-$NAME" | LC_ALL=C sort)"
ACTUAL="$(tar -tzf "$OUT/$TARGET.tar.gz" | LC_ALL=C sort)"
if [[ "$ACTUAL" == "$EXPECTED" ]]; then
  pass "member list is exactly descriptor + executable at root"
else
  fail "member list mismatch:
$ACTUAL"
fi

# --- sidecar matches the tarball digest (GNU sha256sum format) ---
WANT="$(sha256sum "$OUT/$TARGET.tar.gz" | cut -d' ' -f1)"
GOT="$(cut -d' ' -f1 "$OUT/$TARGET.tar.gz.sha256")"
if [[ "$WANT" == "$GOT" ]]; then
  pass "sidecar digest matches the tarball"
else
  fail "sidecar digest mismatch: want $WANT, got $GOT"
fi

# --- aggregate manifest verifies ---
if (cd "$OUT" && sha256sum -c SHA256SUMS >/dev/null 2>&1); then
  pass "SHA256SUMS verifies"
else
  fail "SHA256SUMS failed to verify"
fi

# --- a tampered tarball breaks the manifest ---
printf 'tamper' >>"$OUT/$TARGET.tar.gz"
if (cd "$OUT" && sha256sum -c SHA256SUMS >/dev/null 2>&1); then
  fail "tampered tarball still verifies"
else
  pass "tampered tarball breaks SHA256SUMS"
fi
# Rebuild the honest tarball for the legs below.
"$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz" >/dev/null 2>&1

# --- second target extends the manifest (one entry per tarball) ---
if ! "$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET2" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET2.tar.gz" >/dev/null 2>&1; then
  fail "second-target assembly exited non-zero"
fi
ENTRIES="$(awk '{print $2}' "$OUT/SHA256SUMS" | LC_ALL=C sort)"
if [[ "$ENTRIES" == "$(printf '%s\n%s\n' "$TARGET.tar.gz" "$TARGET2.tar.gz" | LC_ALL=C sort)" ]]; then
  pass "SHA256SUMS covers both target tarballs"
else
  fail "SHA256SUMS entries mismatch:
$ENTRIES"
fi

# --- extraction: executable bit, descriptor round-trip, version smoke ---
EXTRACT="$TMP_BASE/extract"
mkdir -p "$EXTRACT"
tar -xzf "$OUT/$TARGET.tar.gz" -C "$EXTRACT"
if [[ -x "$EXTRACT/bitty-$NAME" ]]; then
  pass "bitty-$NAME is executable after extraction"
else
  fail "bitty-$NAME lost its executable bit"
fi
if cmp -s "$SRC/bitty-component.toml" "$EXTRACT/bitty-component.toml"; then
  pass "descriptor survives the round-trip byte-identical"
else
  fail "descriptor differs after extraction"
fi
BIN_OUT="$("$EXTRACT/bitty-$NAME" --version)"
if [[ "$BIN_OUT" == *"$VERSION"* ]]; then
  pass "extracted executable reports version ($BIN_OUT)"
else
  fail "version mismatch: $BIN_OUT"
fi

# --- descriptor with a correct sha256 is accepted ---
GOOD_DIGEST="$(sha256sum "$SRC/bitty-net" | cut -d' ' -f1)"
write_descriptor "sha256 = \"$GOOD_DIGEST\""
if "$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz" >/dev/null 2>&1; then
  pass "descriptor with matching sha256 accepted"
else
  fail "descriptor with matching sha256 refused"
fi
write_descriptor

# --- argument and descriptor validation failures (fail closed) ---
expect_fail() {
  local why="$1"
  shift
  if "$SCRIPT" "$@" >/dev/null 2>&1; then
    fail "accepted invalid input ($why)"
  else
    pass "rejects invalid input ($why)"
  fi
}
expect_fail "missing source" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz"
expect_fail "bad version" --source "$SRC" --version "0.0" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz"
expect_fail "missing binary" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$TMP_BASE/nope" --output "$OUT/$TARGET.tar.gz"
expect_fail "wrong output basename" --source "$SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$SRC/bitty-net" --output "$OUT/wrong-name.tar.gz"
expect_fail "bad target" --source "$SRC" --version "$VERSION" --target "not a triple!!" \
  --binary "$SRC/bitty-net" --output "$OUT/not a triple!!.tar.gz"

# Descriptor failures: swap in a bad descriptor, expect refusal, restore.
expect_bad_descriptor() {
  local why="$1"
  shift
  printf '%s\n' "$@" >"$SRC/bitty-component.toml"
  if "$SCRIPT" --source "$SRC" --version "$VERSION" --target "$TARGET" \
    --binary "$SRC/bitty-net" --output "$OUT/$TARGET.tar.gz" >/dev/null 2>&1; then
    fail "accepted bad descriptor ($why)"
  else
    pass "rejects bad descriptor ($why)"
  fi
  write_descriptor
}
expect_bad_descriptor "unknown table" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-$NAME\"" "[evil]" "x = \"1\""
expect_bad_descriptor "unknown key" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-$NAME\"" "evil = \"1\""
expect_bad_descriptor "bad name" \
  "[component]" "name = \"Not-A-Name\"" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-net\""
expect_bad_descriptor "version mismatch" \
  "[component]" "name = \"$NAME\"" "version = \"0.0.98\"" \
  "protocol = [1, 1]" "executable = \"bitty-$NAME\""
expect_bad_descriptor "inverted protocol" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "protocol = [3, 1]" "executable = \"bitty-$NAME\""
expect_bad_descriptor "executable mismatch" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-evil\""
expect_bad_descriptor "sha256 mismatch" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-$NAME\"" \
  "sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\""
expect_bad_descriptor "missing protocol" \
  "[component]" "name = \"$NAME\"" "version = \"$VERSION\"" \
  "executable = \"bitty-$NAME\""
expect_bad_descriptor "key outside table" \
  "name = \"$NAME\"" "[component]" "version = \"$VERSION\"" \
  "protocol = [1, 1]" "executable = \"bitty-$NAME\""

# An empty binary fails closed.
EMPTY_SRC="$TMP_BASE/empty-src"
mkdir -p "$EMPTY_SRC"
cp "$SRC/bitty-component.toml" "$EMPTY_SRC/bitty-component.toml"
: >"$EMPTY_SRC/bitty-net"
expect_fail "empty binary" --source "$EMPTY_SRC" --version "$VERSION" --target "$TARGET" \
  --binary "$EMPTY_SRC/bitty-net" --output "$OUT/$TARGET.tar.gz"

# --- --print-url literal conformance (packaging/README.md layout table) ---
URL_OUT="$("$SCRIPT" --print-url --name net --version 0.0.23 --target "$TARGET")"
if [[ "$URL_OUT" == "$(printf 'https://cdn.bitty.run/bitty/components/net/0.0.23/%s.tar.gz\nhttps://cdn.bitty.run/bitty/components/net/0.0.23/SHA256SUMS' "$TARGET")" ]]; then
  pass "--print-url emits the literal R2 layout"
else
  fail "--print-url mismatch:
$URL_OUT"
fi

# --- hostile URL corpus: refused before any emission (zero spawn path) ---
expect_bad_url() {
  local why="$1"
  shift
  if "$SCRIPT" --print-url "$@" >/dev/null 2>&1; then
    fail "emitted a URL for hostile input ($why)"
  else
    pass "refuses hostile input ($why)"
  fi
}
expect_bad_url "name with slash" --name "../evil" --version 0.0.23 --target "$TARGET"
expect_bad_url "name with scheme" --name "https://evil.example/x" --version 0.0.23 --target "$TARGET"
expect_bad_url "uppercase name" --name "Net" --version 0.0.23 --target "$TARGET"
expect_bad_url "name with space" --name "ne t" --version 0.0.23 --target "$TARGET"
expect_bad_url "empty name" --name "" --version 0.0.23 --target "$TARGET"
expect_bad_url "v-prefixed version" --name net --version v0.0.23 --target "$TARGET"
expect_bad_url "version with slash" --name net --version "0.0.23/../../evil" --target "$TARGET"
expect_bad_url "version with query" --name net --version "0.0.23?x=1" --target "$TARGET"
expect_bad_url "target with pipe" --name net --version 0.0.23 --target "x86_64|evil"
expect_bad_url "target with traversal" --name net --version 0.0.23 --target ".."
expect_bad_url "target with space" --name net --version 0.0.23 --target "x86_64 evil"
expect_bad_url "http cdn base" --name net --version 0.0.23 --target "$TARGET" \
  --cdn-base "http://cdn.bitty.run"
expect_bad_url "lookalike cdn host" --name net --version 0.0.23 --target "$TARGET" \
  --cdn-base "https://cdn.bitty.run.evil.example"
expect_bad_url "cdn base with path" --name net --version 0.0.23 --target "$TARGET" \
  --cdn-base "https://cdn.bitty.run/evil"
expect_bad_url "foreign cdn host" --name net --version 0.0.23 --target "$TARGET" \
  --cdn-base "https://evil.example"

# A subdomain of this zone stays allowed (test override leg).
SUB_OUT="$("$SCRIPT" --print-url --name net --version 0.0.23 --target "$TARGET" \
  --cdn-base "https://staging.bitty.run" 2>/dev/null)"
if [[ "$SUB_OUT" == "$(printf 'https://staging.bitty.run/bitty/components/net/0.0.23/%s.tar.gz\nhttps://staging.bitty.run/bitty/components/net/0.0.23/SHA256SUMS' "$TARGET")" ]]; then
  pass "--cdn-base allows a same-zone staging host"
else
  fail "--cdn-base staging host mismatch:
$SUB_OUT"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "make-component-dist.test: FAIL" >&2
  exit 1
fi
echo "make-component-dist.test: PASS"
