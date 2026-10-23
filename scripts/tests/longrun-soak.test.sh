#!/usr/bin/env bash
# longrun-soak.test.sh — contract test for scripts/longrun-soak.sh (#2483).
#
# Headless-safe: every assertion goes through --help, --dry-run, or argument
# validation, so no display, Xvfb, GPU, or built binary is required. The live
# path is exercised only by the soak harness itself on a host with Xvfb.
set -euo pipefail

cd "$(dirname "$0")/../.."

SOAK=./scripts/longrun-soak.sh
FAIL=0
# Issue #2497: keep every test artifact out of the repository's recording
# directory; a live-path invocation still creates --out-dir before it fails.
TEST_ROOT="$(mktemp -d /tmp/bitty-longrun-soak-test.XXXXXX)"
TEST_OUT="$TEST_ROOT/output.txt"
trap 'rm -rf "$TEST_ROOT"' EXIT

expect_exit() {
  want="$1"
  shift
  got=0
  "$@" >"$TEST_OUT" 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    echo "FAIL: expected exit $want, got $got: $*" >&2
    cat "$TEST_OUT" >&2 || true
    FAIL=1
  fi
}

expect_stdout() {
  pattern="$1"
  shift
  if ! "$@" >"$TEST_OUT" 2>/dev/null; then
    echo "FAIL: command failed: $*" >&2
    FAIL=1
    return
  fi
  if ! grep -qF -- "$pattern" "$TEST_OUT"; then
    echo "FAIL: stdout missing [$pattern]: $*" >&2
    FAIL=1
  fi
}

# --help lists the contract surface and never needs a display.
expect_exit 0 "$SOAK" --help
expect_stdout "--dry-run" "$SOAK" --help
expect_stdout "--stall-probe" "$SOAK" --help
expect_stdout "--interval-secs" "$SOAK" --help

# Unknown flags and invalid knobs fail closed with exit 2.
expect_exit 2 "$SOAK" --bogus
expect_exit 2 "$SOAK" --dry-run --workload rm-rf
expect_exit 2 "$SOAK" --dry-run --duration-secs 0
expect_exit 2 "$SOAK" --dry-run --duration-secs 999999
expect_exit 2 "$SOAK" --dry-run --duration-secs banana
expect_exit 2 "$SOAK" --dry-run --interval-secs 0
expect_exit 2 "$SOAK" --dry-run --interval-secs 99999
expect_exit 2 "$SOAK" --dry-run --stall-ms 10
expect_exit 2 "$SOAK" --dry-run --stall-ms 999999
expect_exit 2 "$SOAK" --dry-run --probe-writes 99
expect_exit 2 "$SOAK" --dry-run --binary /nonexistent/bitty

# A plan that would exceed the sample cap fails closed instead of truncating.
expect_exit 2 "$SOAK" --dry-run --duration-secs 86400 --interval-secs 1

# Issue #2497: the live launcher clears inherited Bitty overrides. The
# headless contract cannot start the app, so assert the exact unset list.
UNSET_BLOCK="$(sed -n '/^  unset BITTY_CONFIG /,/BITTY_CTL_ELEVATE$/p' "$SOAK")"
for var in BITTY_CONFIG BITTY_PROFILE BITTY_PLUGIN_DIR BITTY_COMPONENTS_DIR \
  BITTY_COMPONENT_STATE_DIR BITTY_HEADLESS BITTY_DEMO_PUMP BITTY_CTL_ELEVATE; do
  case "$UNSET_BLOCK" in
  *"$var"*) ;;
  *)
    echo "FAIL: launcher does not clear inherited $var" >&2
    FAIL=1
    ;;
  esac
done

# Default plan math and the bounded knobs are visible before any process runs.
expect_stdout "samples=61" "$SOAK" --dry-run --binary /bin/true
expect_stdout "duration=600s interval=10s" "$SOAK" --dry-run --binary /bin/true
expect_stdout "workload=mixed" "$SOAK" --dry-run --binary /bin/true
expect_stdout "watchdog=on threshold=2000ms" "$SOAK" --dry-run --binary /bin/true
expect_stdout "stall-probe=1 expect-stall=1" \
  "$SOAK" --dry-run --binary /bin/true --stall-probe --expect-stall
expect_stdout "samples=11" "$SOAK" --dry-run --binary /bin/true --duration-secs 100 --interval-secs 10

# The live path refuses to invent a display server, and must not write into
# the repository while doing so.
expect_exit 2 "$SOAK" --binary /bin/true --xvfb /nonexistent/Xvfb \
  --duration-secs 1 --out-dir "$TEST_ROOT/run"

if [ "$FAIL" != 0 ]; then
  echo "longrun-soak contract test: FAIL" >&2
  exit 1
fi
echo "longrun-soak contract test: PASS"
