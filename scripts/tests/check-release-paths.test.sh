#!/usr/bin/env bash
# check-release-paths.test.sh — fixture test for the builder-path gate
# (CTX-2402).
#
# Drives scripts/check-release-paths.sh against recorded fixtures
# (scripts/tests/fixtures/release-paths/): the gate reads file bytes, not
# executable metadata, so the logic runs identically on Linux, macOS, and
# git-bash without a cross toolchain or a cargo build.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-release-paths.sh
FIX=scripts/tests/fixtures/release-paths
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# expect_exit <exit> <label> <gate args...>
expect_exit() {
  local want="$1" label="$2"
  shift 2
  local out status=0
  out="$("$GATE" "$@" 2>&1)" || status=$?
  if ((status != want)); then
    echo "FAIL: $label: expected exit $want, got $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# expect_fail_text <text> <label> <gate args...>
expect_fail_text() {
  local text="$1" label="$2"
  shift 2
  local out status=0
  out="$("$GATE" "$@" 2>&1)" || status=$?
  if ((status == 0)); then
    echo "FAIL: $label: gate passed but failure was expected" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  elif ! grep -qF "$text" <<<"$out"; then
    echo "FAIL: $label: missing '$text'" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# 1. A binary with no builder path passes.
expect_exit 0 'clean binary' --binary "$FIX/clean.bin"

# 2. Remapped /build/... paths are the policy shape, not a leak.
expect_exit 0 'remapped build paths' --binary "$FIX/remapped.bin"

# 3. A Linux builder home is rejected.
# scratch-paths-exempt: asserts the gate's report against a fixture path, not a host path.
expect_fail_text '/home/runner/' 'linux builder home' --binary "$FIX/linux-leak.bin"

# 4. A macOS builder home is rejected.
# scratch-paths-exempt: asserts the gate's report against a fixture path, not a host path.
expect_fail_text '/Users/runner/' 'macos builder home' --binary "$FIX/macos-leak.bin"

# 5. A Windows user-profile builder home is rejected.
expect_fail_text 'Users' 'windows builder home' --binary "$FIX/windows-leak.bin"

# 6. A configured --prefix is rejected even when it is not a home path.
expect_fail_text '/opt/bitty-build' 'extra prefix' \
  --binary "$FIX/prefix-leak.bin" --prefix /opt/bitty-build

# 7. The derived $HOME is rejected (catches a remap that skipped a root).
mkdir -p "$TMP/fakehome"
printf 'embedded at %s/src/crates/bitty-vt/src/lib.rs\n' "$TMP/fakehome" >"$TMP/home-leak.bin"
HOME="$TMP/fakehome" expect_fail_text "$TMP/fakehome" 'derived HOME' --binary "$TMP/home-leak.bin"

# 8. One leaky binary fails the whole run.
expect_exit 1 'one leaky binary among clean ones' \
  --binary "$FIX/clean.bin" --binary "$FIX/linux-leak.bin" --binary "$FIX/macos-leak.bin"

# 9. A missing binary is an inspection failure, not a pass.
expect_fail_text 'binary not found' 'missing binary' --binary "$TMP/absent"

# 10. A relative CARGO_TARGET_DIR is resolved before matching, so the bare
# substring does not false-positive.
printf 'built under target/debug/deps\n' >"$TMP/relative-target.bin"
CARGO_TARGET_DIR=target expect_exit 0 'relative target dir' --binary "$TMP/relative-target.bin"

# 11. A grep error is an inspection failure, not a pass.
mkdir -p "$TMP/grep-shim"
printf '#!/bin/sh\nexit 2\n' >"$TMP/grep-shim/grep"
chmod +x "$TMP/grep-shim/grep"
PATH="$TMP/grep-shim:$PATH" expect_exit 1 'grep error is not a pass' --binary "$FIX/linux-leak.bin"

# 12. Usage errors.
expect_exit 2 'no arguments'
expect_exit 2 'unknown argument' --frobnicate

if ((FAIL)); then
  echo "release-paths-test: FAIL" >&2
  exit 1
fi
echo "release-paths-test: OK"
