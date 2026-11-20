#!/usr/bin/env bash
# check-release-windows-verify.test.sh — CTX-1916 fixture test for the release
# workflow's Windows verification gate.
#
# The v0.0.46 release run (44718678246) failed in two Windows verification jobs
# after every build succeeded. Both defects are pinned here against synthetic
# workflow fixtures, so the gate is proven to reject them:
#
#   fixtures/release-windows-verify/clean       -> gate passes
#   fixtures/release-windows-verify/zip-path    -> Mismatched ZIP directory
#   fixtures/release-windows-verify/newline     -> assertion needs a newline
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

zip_out="$("$GATE" --root "$FIXTURES/zip-path" 2>&1)" || zip_status=$?
zip_status="${zip_status:-0}"
if ((zip_status == 0)); then
  fail "mismatched portable-ZIP directory was accepted"
fi
grep -q 'rule-1' <<<"$zip_out" || fail "ZIP mismatch did not report rule-1: $zip_out"

newline_out="$("$GATE" --root "$FIXTURES/newline" 2>&1)" || newline_status=$?
newline_status="${newline_status:-0}"
if ((newline_status == 0)); then
  fail "newline-requiring version assertion was accepted"
fi
grep -q 'rule-2' <<<"$newline_out" || fail "newline assertion did not report rule-2: $newline_out"

if ((FAIL != 0)); then
  exit 1
fi
echo "check-release-windows-verify.test.sh: all cases passed"
