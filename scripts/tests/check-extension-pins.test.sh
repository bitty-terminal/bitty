#!/usr/bin/env bash
# Fixture test for scripts/check-extension-pins.sh (CTX-1363, #2255).
#
# Builds a throwaway upstream repository (file:// remote) so the gate logic is
# pinned without network: reachable-in-sync, over-threshold drift, --strict,
# unreachable pin, malformed rev, and offline skip.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
checker="$script_dir/../check-extension-pins.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null
export GIT_CONFIG_COUNT=3
export GIT_CONFIG_KEY_0=init.defaultBranch
export GIT_CONFIG_VALUE_0=main
export GIT_CONFIG_KEY_1=user.name
export GIT_CONFIG_VALUE_1=fixture
export GIT_CONFIG_KEY_2=user.email
export GIT_CONFIG_VALUE_2=fixture@example.invalid

repos="$tmp/repos"
work="$tmp/work"
mkdir -p "$repos" "$work" "$tmp/cargo/crates/fixture"

git init --quiet --bare "$repos/bitty-fixture"
git -C "$work" init --quiet
git -C "$work" config user.name fixture
git -C "$work" config user.email fixture@example.invalid
printf 'one\n' >"$work/file"
git -C "$work" add file
git -C "$work" commit --quiet -m one
pin="$(git -C "$work" rev-parse --short HEAD)"
git -C "$work" push --quiet "$repos/bitty-fixture" main

printf 'two\n' >>"$work/file"
git -C "$work" commit --quiet -am two
printf 'three\n' >>"$work/file"
git -C "$work" commit --quiet -am three
git -C "$work" push --quiet "$repos/bitty-fixture" main

git -C "$work" checkout --quiet --orphan side
git -C "$work" rm --quiet -rf .
printf 'side\n' >"$work/side"
git -C "$work" add side
git -C "$work" commit --quiet -m side
side="$(git -C "$work" rev-parse --short HEAD)"
git -C "$work" push --quiet "$repos/bitty-fixture" side

# The checker parses manifests textually; phodopus is a library pin and must
# stay out of the extracted-extension policy.
write_manifest() {
  cat >"$tmp/cargo/crates/fixture/Cargo.toml" <<EOF
[package]
name = "fixture"
version = "0.0.1"

[dependencies]
bitty-fixture = { version = "0.0.1", git = "https://github.com/bitty-terminal/bitty-fixture", rev = "$1" }
phodopus = { version = "0.2.0-alpha.1", git = "https://github.com/bitty-terminal/phodopus", rev = "f6aa59a" }
EOF
}

run_gate() {
  local cache="$1"
  shift
  BITTY_PIN_CACHE="$cache" \
    BITTY_PIN_GIT_BASE="file://$repos/" \
    bash "$checker" --root "$tmp/cargo" "$@"
}

fail() {
  echo "FAIL: $*" >&2
  if [ -s "$tmp/out" ]; then
    echo '--- gate output ---' >&2
    cat "$tmp/out" >&2
  fi
  exit 1
}

expect_contains() {
  grep -qF -- "$1" "$tmp/out" || fail "expected output to contain '$1'"
}

expect_absent() {
  ! grep -qF -- "$1" "$tmp/out" || fail "expected output not to contain '$1'"
}

# 1. In sync with a clean policy: exit 0, no drift.
write_manifest "$(git -C "$work" rev-parse --short main)"
code=0
run_gate "$tmp/cache-sync" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 0 ] || fail "in-sync run exited $code"
expect_contains 'ok'
expect_contains 'pins: 1 checked'
expect_absent 'phodopus'

# 2. Two commits behind, threshold 10: warning-free pass.
write_manifest "$pin"
code=0
run_gate "$tmp/cache-behind" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 0 ] || fail "within-threshold run exited $code"
expect_contains '2'

# 3. Two commits behind, threshold 1: annotate (warning) but still pass.
code=0
PIN_DRIFT_WARN=1 run_gate "$tmp/cache-behind-strict" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 0 ] || fail "over-threshold warning run exited $code"
expect_contains 'DRIFT'
expect_contains 'commits behind upstream main'

# 4. --strict turns over-threshold drift into a failure.
code=0
PIN_DRIFT_WARN=1 run_gate "$tmp/cache-strict" --strict >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 1 ] || fail "--strict over-threshold run exited $code, want 1"

# 5. A pin outside upstream main fails.
write_manifest "$side"
code=0
run_gate "$tmp/cache-unreachable" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 1 ] || fail "unreachable pin run exited $code, want 1"
expect_contains 'NOT REACHABLE'

# 6. A branch pin (no inline rev) is rejected by the coverage guard.
cat >"$tmp/cargo/crates/fixture/Cargo.toml" <<EOF
[package]
name = "fixture"
version = "0.0.1"

[dependencies]
bitty-fixture = { version = "0.0.1", git = "https://github.com/bitty-terminal/bitty-fixture", branch = "main" }
EOF
code=0
run_gate "$tmp/cache-branch" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 1 ] || fail "branch pin run exited $code, want 1"
expect_contains 'lack an inline exact'

# 7. Offline upstream skips by default and fails under --require-network.
write_manifest "$pin"
code=0
BITTY_PIN_CACHE="$tmp/cache-offline" \
  BITTY_PIN_GIT_BASE="file://$tmp/missing/" \
  bash "$checker" --root "$tmp/cargo" >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 0 ] || fail "offline run exited $code, want 0"
expect_contains 'SKIP (offline)'

code=0
BITTY_PIN_CACHE="$tmp/cache-offline-required" \
  BITTY_PIN_GIT_BASE="file://$tmp/missing/" \
  bash "$checker" --root "$tmp/cargo" --require-network >"$tmp/out" 2>&1 || code=$?
[ "$code" -eq 1 ] || fail "offline --require-network run exited $code, want 1"

echo "check-extension-pins.test.sh: ok"
