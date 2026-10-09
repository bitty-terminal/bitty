#!/usr/bin/env bash
# mem-measure.test.sh — CTX-1036 contract test for scripts/mem-measure.sh.
#
# Headless-safe: every assertion runs through --help, --dry-run, or argument
# refusal, so no display, Hyprland session, grim capture, or launched window
# is required. The live headed path is exercised by hand on ws5 per the #1809
# method, never here.
set -euo pipefail

cd "$(dirname "$0")/../.."

SESSION=./scripts/mem-measure.sh
FAIL=0

expect_exit() {
  want="$1"
  shift
  got=0
  "$@" >/dev/null 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    echo "FAIL: expected exit $want, got $got: $*" >&2
    FAIL=1
  fi
}

expect_stdout() {
  # Capture first, then grep: piping straight into `grep -qF` lets the
  # early grep exit SIGPIPE the producer, which fails under the
  # caller's `pipefail`. Small outputs only; that is all we assert.
  pattern="$1"
  shift
  out="$("$@" 2>/dev/null)" || true
  if ! printf '%s\n' "$out" | grep -qF -- "$pattern"; then
    echo "FAIL: stdout missing [$pattern]: $*" >&2
    FAIL=1
  fi
}

# --help lists the contract surface.
expect_exit 0 "$SESSION" --help
expect_stdout "--dry-run" "$SESSION" --help
expect_stdout "--workspace ID" "$SESSION" --help
expect_stdout "--settle-secs N" "$SESSION" --help
expect_stdout "250" "$SESSION" --help

# --dry-run prints the plan and launches nothing.
expect_exit 0 "$SESSION" --dry-run --binary /bin/true
expect_stdout "mem-measure: plan" "$SESSION" --dry-run --binary /bin/true
expect_stdout "workspace=5" "$SESSION" --dry-run --binary /bin/true
expect_exit 0 "$SESSION" --dry-run --binary /bin/true --font-size 12.5

# Refusals: unknown flag, bad workspace, bad settle, bad font size, missing binary.
expect_exit 2 "$SESSION" --bogus-flag
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --workspace 11
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --settle-secs 3
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --settle-secs 61
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size abc
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size 0
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size 129
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size "12; echo pwned"
expect_exit 2 "$SESSION" --dry-run --binary /nonexistent-bitty-mem-probe

if [ "$FAIL" != "0" ]; then
  echo "mem-measure contract: FAIL" >&2
  exit 1
fi
echo "mem-measure contract: PASS"
