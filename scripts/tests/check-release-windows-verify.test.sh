#!/usr/bin/env bash
# check-release-windows-verify.test.sh — CTX-1916 fixture test for the release
# workflow's Windows verification gate.
#
# The v0.0.46 release run (44718678246) failed in two Windows verification jobs
# after every build succeeded. Both defects — and the two near-misses the first
# draft of this gate would have accepted — are pinned against synthetic workflow
# fixtures, so each one is proven to be rejected:
#
#   fixtures/release-windows-verify/clean           -> gate passes
#   fixtures/release-windows-verify/zip-path        -> download path != loop root
#   fixtures/release-windows-verify/lookup-root     -> loop root renamed, gate
#                                                      must follow the lookup
#   fixtures/release-windows-verify/newline         -> assertion needs a newline
#   fixtures/release-windows-verify/newline-suffix  -> newline before `\s*\z`
#
# The real workflow's default-root check runs in CI via
# `scripts/check-release-windows-verify.sh`.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-release-windows-verify.sh
FIXTURES=scripts/tests/fixtures/release-windows-verify
FAIL=0

fail() {
  echo "FAIL: $*" >&2
  FAIL=1
}

clean_out="$("$GATE" --root "$FIXTURES/clean" 2>&1)" || clean_status=$?
clean_status="${clean_status:-0}"
if ((clean_status != 0)); then
  fail "clean fixture rejected (exit $clean_status):"
  printf '%s\n' "$clean_out" >&2
fi
grep -q 'PASS' <<<"$clean_out" || fail "clean fixture did not report PASS"

# expect_rejected <fixture> <rule> <description>
expect_rejected() {
  local fixture="$1" rule="$2" description="$3" out status
  out="$("$GATE" --root "$FIXTURES/$fixture" 2>&1)" || status=$?
  status="${status:-0}"
  if ((status == 0)); then
    fail "$description was accepted"
  fi
  grep -q "$rule" <<<"$out" || fail "$description did not report $rule: $out"
}

expect_rejected zip-path rule-1 "a download path that differs from the loop root"
expect_rejected lookup-root rule-1 "a renamed Join-Path lookup root"
expect_rejected newline rule-2 "an assertion requiring a trailing newline"
expect_rejected newline-suffix rule-2 "a mandatory newline before the \\s*\\z suffix"

if ((FAIL != 0)); then
  exit 1
fi
echo "check-release-windows-verify.test.sh: all cases passed"
