#!/usr/bin/env bash
# make-windows-zip.test.sh — pin the Windows portable-ZIP payload contract.
#
# bitty#3224: `bitty.exe` is a GUI-subsystem executable, so a shell launch never
# blocks on it. The archive must therefore carry `bitty.com` (the console
# companion proxy, which `%PATHEXT%` resolves first) next to `bitty.exe`, for
# both architectures. This gate runs the real packer on dummy payloads, so a
# dropped `--com` requirement, a renamed payload member, or an arch/archive-name
# mismatch fails here instead of at tag time.
#
# Runs on any host with `zip`/`unzip`; no Windows and no Rust toolchain needed.
set -euo pipefail

cd "$(dirname "$0")/../.."

PACKER=./scripts/make-windows-zip.sh
FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/make-windows-zip-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

expect_reject() { # <label> <args...>
  local label="$1"
  shift
  if "$PACKER" "$@" >"$TMP/rejected.out" 2>"$TMP/rejected.err"; then
    fail "packer accepted $label"
  else
    pass "packer rejects $label"
  fi
}

[[ -x "$PACKER" ]] || {
  echo "FAIL: packer is missing or not executable: $PACKER" >&2
  exit 1
}
bash -n "$PACKER" || fail "bash -n rejects $PACKER"

VERSION=0.0.24
# Dummy payloads: the packer only requires non-empty files.
printf 'placeholder terminal bytes\n' >"$TMP/bitty.exe"
printf 'placeholder proxy bytes\n' >"$TMP/bitty-com.exe"

X64_ZIP="$TMP/bitty-${VERSION}-windows-x86_64.zip"
ARM64_ZIP="$TMP/bitty-${VERSION}-windows-aarch64.zip"

# --- Negative cases ---------------------------------------------------------
expect_reject "a missing --com payload" \
  --version "$VERSION" --arch x86_64 --exe "$TMP/bitty.exe" --output "$X64_ZIP"
expect_reject "a missing --arch" \
  --version "$VERSION" --exe "$TMP/bitty.exe" --com "$TMP/bitty-com.exe" --output "$X64_ZIP"
expect_reject "an unknown --arch" \
  --version "$VERSION" --arch riscv64 --exe "$TMP/bitty.exe" \
  --com "$TMP/bitty-com.exe" --output "$TMP/bitty-${VERSION}-windows-riscv64.zip"
expect_reject "an archive name that disagrees with --arch" \
  --version "$VERSION" --arch aarch64 --exe "$TMP/bitty.exe" \
  --com "$TMP/bitty-com.exe" --output "$X64_ZIP"
expect_reject "a non-existent --com path" \
  --version "$VERSION" --arch x86_64 --exe "$TMP/bitty.exe" \
  --com "$TMP/absent.exe" --output "$X64_ZIP"

# --- Dry run ---------------------------------------------------------------
for arch in x86_64 aarch64; do
  out="$TMP/dry-run-$arch.log"
  if "$PACKER" --version "$VERSION" --arch "$arch" --exe "$TMP/bitty.exe" \
    --com "$TMP/bitty-com.exe" \
    --output "$TMP/bitty-${VERSION}-windows-${arch}.zip" --dry-run >"$out" 2>&1; then
    pass "dry-run succeeds for $arch"
  else
    fail "dry-run failed for $arch: $(tail -n 2 "$out")"
  fi
  for member in bitty.exe bitty.com bitty.ico LICENSE README.md CHANGELOG.md; do
    if grep -qF "/$member" "$out"; then
      pass "dry-run $arch payload carries $member"
    else
      fail "dry-run $arch payload is missing $member"
    fi
  done
done

# --- Real assembly ---------------------------------------------------------
for pair in "x86_64:$X64_ZIP" "aarch64:$ARM64_ZIP"; do
  arch="${pair%%:*}"
  zip_path="${pair#*:}"
  if "$PACKER" --version "$VERSION" --arch "$arch" --exe "$TMP/bitty.exe" \
    --com "$TMP/bitty-com.exe" --output "$zip_path" >"$TMP/zip-$arch.log" 2>&1; then
    pass "packer assembles the $arch archive"
  else
    fail "packer failed for $arch: $(tail -n 3 "$TMP/zip-$arch.log")"
    continue
  fi
  [[ "$(basename "$zip_path")" == "bitty-${VERSION}-windows-${arch}.zip" ]] &&
    pass "$arch archive keeps the versioned name" ||
    fail "$arch archive name is wrong"
  EXPECTED="$(printf '%s\n' CHANGELOG.md LICENSE README.md bitty.com bitty.exe bitty.ico | LC_ALL=C sort)"
  ACTUAL="$(unzip -Z1 "$zip_path" | LC_ALL=C sort)"
  if [[ "$ACTUAL" == "$EXPECTED" ]]; then
    pass "$arch archive payload is exactly the six expected members"
  else
    fail "$arch archive payload mismatch:
$ACTUAL"
  fi
  # The proxy is staged under its shipped name, byte for byte.
  if unzip -p "$zip_path" bitty.com | cmp -s - "$TMP/bitty-com.exe"; then
    pass "$arch archive stages the proxy as bitty.com"
  else
    fail "$arch archive does not stage the proxy bytes as bitty.com"
  fi
  if unzip -p "$zip_path" bitty.exe | cmp -s - "$TMP/bitty.exe"; then
    pass "$arch archive stages the terminal as bitty.exe"
  else
    fail "$arch archive does not stage the terminal bytes as bitty.exe"
  fi
done

if [[ "$FAIL" -ne 0 ]]; then
  echo "make-windows-zip.test: FAIL" >&2
  exit 1
fi
echo "make-windows-zip.test: PASS"
