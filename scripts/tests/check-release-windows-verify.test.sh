#!/usr/bin/env bash
# check-release-windows-verify.test.sh — CTX-1916 enumerated fixture suite for
# the Windows verification gate; CTX-1918 (bitty#3281) extended it to the
# preview workflow, so the fixtures now cover both workflow files, and
# CTX-1917 (bitty#3279) added the scoop-shim rule-3 case.
#
# The suite never hand-picks its cases: it enumerates EVERY directory under
# scripts/tests/fixtures/release-windows-verify/ in sorted order and exercises
# each one against scripts/check-release-windows-verify.sh --root.
#
#   * FIXTURES_MUST_PASS   — fixtures the gate must ACCEPT (exit 0). One that
#     gets rejected fails the suite ("unexpected reject").
#   * FIXTURES_MUST_REJECT — fixtures the gate must REJECT (non-zero exit),
#     registered as "<fixture>:<rule>" with the rule the gate must report.
#     One that gets accepted fails the suite ("unexpected pass") — and so does
#     ANY fixture that passes while not in FIXTURES_MUST_PASS.
#   * Anything else — a directory on disk registered in neither list — fails
#     the suite loudly as "unregistered".
#
# Why the registry: the first draft of this harness invoked each fixture from
# its own hand-written line, so a fixture directory added later without a
# matching line was silently unexercised while the suite stayed green.
# Enumeration removes the silent path: registration is now the deliberate act
# that declares a fixture's expectation, and the pin works in both directions —
# a registered name whose directory is missing fails the suite too.
#
# Fixture inventory (pinned against the v0.0.46 release run, 44718678246, and
# the nightly-preview defect class bitty#3276/#3281):
#   clean                          -> gate passes
#   comment-after-suffix           -> rule-2: only a trailing comment ends in `\s*\z`
#   correct-with-comment           -> gate passes; a correct assertion may carry a comment
#   lookup-root                    -> rule-1: loop root renamed, the gate must follow the lookup
#   newline                        -> rule-2: assertion needs a newline
#   newline-suffix                 -> rule-2: newline before `\s*\z`
#   pattern-variable-name          -> rule-2: the assertion is bound to `$pat`, not `$pattern`
#   preview-clean                  -> gate passes; the preview workflow's post-#3278 shape
#   preview-derived-pattern        -> rule-2: `$proxyPattern = $pattern + ...` cannot be parsed
#   preview-here-string            -> rule-2: the assertion moved into a here-string
#   preview-newline                -> rule-2: the pre-#3278 newline-anchored preview pattern
#   preview-pattern-variable-name  -> rule-2: the regression is bound to `$pat`, not `$pattern`
#   preview-undefined-operand       -> rule-2: the comparison operand no assignment defines
#   scoop-shim                     -> rule-3: the pre-fix leg omits '.com' and the companion binding
#   stale-comment                  -> rule-1: a stale comment cites the pre-rename root
#   zip-path                       -> rule-1: download path differs from the loop root
#
# Every fixture directory carries BOTH .github/workflows/release.yml and
# .github/workflows/preview.yml, because the gate scans both files under
# --root (bitty#3281): the release-side fixtures pair their release.yml variant
# with the clean preview.yml copy, and the preview-side fixtures pair their
# preview.yml variant with the clean release.yml copy. A fixture root missing
# either file is rejected by the gate itself (fail closed), never tolerated.
#
# The fixtures cover the rule shapes; the real workflows are checked separately
# by `just release-windows-verify` (part of `just check`) and by CI, so a
# regression in .github/workflows/release.yml or preview.yml cannot slip
# through even when it matches no fixture.
set -euo pipefail

# Byte-stable ordering for the sorted enumeration and all matching below.
export LC_ALL=C

cd "$(dirname "$0")/../.."

GATE=./scripts/check-release-windows-verify.sh
FIXTURES=scripts/tests/fixtures/release-windows-verify
FAIL=0
SEEN=""

# --- Registry: every fixture must be declared in exactly one list ------------
FIXTURES_MUST_PASS="clean correct-with-comment preview-clean"
FIXTURES_MUST_REJECT="comment-after-suffix:rule-2 lookup-root:rule-1 newline:rule-2 newline-suffix:rule-2 pattern-variable-name:rule-2 preview-derived-pattern:rule-2 preview-here-string:rule-2 preview-newline:rule-2 preview-pattern-variable-name:rule-2 preview-undefined-operand:rule-2 scoop-shim:rule-3 stale-comment:rule-1 zip-path:rule-1"

fail() {
  echo "FAIL: $*" >&2
  FAIL=1
}

# True when the exact word $2 appears in the space-separated list $1.
list_contains() {
  case " $1 " in
  *" $2 "*) return 0 ;;
  esac
  return 1
}

# Print how many times the exact word $2 appears in the list $1.
word_count() {
  local count=0 item
  for item in $1; do
    if [[ "$item" == "$2" ]]; then
      count=$((count + 1))
    fi
  done
  echo "$count"
}

# Print the rule registered for must-reject fixture $1; fail (no output) when
# $1 is not a registered must-reject fixture.
reject_rule() {
  local entry
  for entry in $FIXTURES_MUST_REJECT; do
    if [[ "${entry%%:*}" == "$1" ]]; then
      echo "${entry##*:}"
      return 0
    fi
  done
  return 1
}

[[ -d "$FIXTURES" ]] || fail "fixture root '$FIXTURES' does not exist"

# --- Registry integrity: pin the lists themselves ---------------------------
# Raw checks first: names may hold only [A-Za-z0-9._-] (plus the ':' rule
# separator and spaces between entries), so a corrupted list can never glob
# against the working directory when the loops below expand it.
case "$FIXTURES_MUST_PASS" in
*[!A-Za-z0-9._[:space:]-]*)
  fail "malformed FIXTURES_MUST_PASS: '$FIXTURES_MUST_PASS' (expected fixture names separated by spaces)"
  ;;
esac
case "$FIXTURES_MUST_REJECT" in
*[!A-Za-z0-9._[:space:]:-]*)
  fail "malformed FIXTURES_MUST_REJECT: '$FIXTURES_MUST_REJECT' (expected '<fixture>:<rule>' entries separated by spaces)"
  ;;
esac

REJECT_NAMES=""
for entry in $FIXTURES_MUST_REJECT; do
  REJECT_NAMES="$REJECT_NAMES ${entry%%:*}"
done

for name in $FIXTURES_MUST_PASS; do
  if (($(word_count "$FIXTURES_MUST_PASS" "$name") > 1)); then
    fail "duplicate FIXTURES_MUST_PASS entry '$name'"
  fi
done
for entry in $FIXTURES_MUST_REJECT; do
  if [[ "$entry" != *:* || -z "${entry%%:*}" || -z "${entry##*:}" ]]; then
    fail "malformed FIXTURES_MUST_REJECT entry '$entry' (expected '<fixture>:<rule>')"
  fi
done
for name in $REJECT_NAMES; do
  if (($(word_count "$REJECT_NAMES" "$name") > 1)); then
    fail "duplicate FIXTURES_MUST_REJECT registration for fixture '$name'"
  fi
  if list_contains "$FIXTURES_MUST_PASS" "$name"; then
    fail "fixture '$name' is registered in both FIXTURES_MUST_PASS and FIXTURES_MUST_REJECT"
  fi
done

# --- Enumeration: every fixture directory, sorted, nothing skipped -----------
for fixture_dir in "$FIXTURES"/*; do
  [[ -d "$fixture_dir" ]] || continue
  fixture="${fixture_dir##*/}"
  SEEN="$SEEN $fixture"

  # Exercise the fixture: the gate's exit status is the verdict.
  status=0
  out="$("$GATE" --root "$fixture_dir" 2>&1)" || status=$?

  if list_contains "$FIXTURES_MUST_PASS" "$fixture"; then
    if ((status != 0)); then
      printf '  %-4s %-22s expect=pass   result=REJECTED (exit %s)\n' FAIL "$fixture" "$status"
      fail "unexpected reject: must-pass fixture '$fixture' was rejected by the gate (exit $status):"
      printf '%s\n' "$out" >&2
    elif grep -q 'PASS' <<<"$out"; then
      printf '  %-4s %-22s expect=pass   result=PASS (exit 0)\n' ok "$fixture"
    else
      printf '  %-4s %-22s expect=pass   result=exit 0 without reporting PASS\n' FAIL "$fixture"
      fail "must-pass fixture '$fixture' exited 0 but the gate did not report PASS: $out"
    fi
  elif rule="$(reject_rule "$fixture")"; then
    if ((status == 0)); then
      printf '  %-4s %-22s expect=reject %s result=ACCEPTED (exit 0)\n' FAIL "$fixture" "$rule"
      fail "unexpected pass: must-reject fixture '$fixture' was accepted by the gate (no fixture outside FIXTURES_MUST_PASS may pass):"
      printf '%s\n' "$out" >&2
    elif grep -q "$rule" <<<"$out"; then
      printf '  %-4s %-22s expect=reject %s result=REJECTED (exit %s, reported %s)\n' ok "$fixture" "$rule" "$status" "$rule"
    else
      printf '  %-4s %-22s expect=reject %s result=REJECTED (exit %s) without reporting %s\n' FAIL "$fixture" "$rule" "$status" "$rule"
      fail "must-reject fixture '$fixture' was rejected (exit $status) but the gate did not report $rule: $out"
    fi
  else
    # Unregistered directory — the exact gap the enumeration exists to close.
    if ((status == 0)); then
      printf '  %-4s %-22s expect=registered result=ACCEPTED (exit 0)\n' FAIL "$fixture"
      fail "unexpected pass: fixture '$fixture' is registered in neither list and the gate accepted it — register it in FIXTURES_MUST_PASS or FIXTURES_MUST_REJECT"
    else
      printf '  %-4s %-22s expect=registered result=REJECTED (exit %s)\n' FAIL "$fixture" "$status"
      fail "unregistered fixture '$fixture': the gate rejected it (exit $status), but a fixture registered in neither list is exercised only by accident — register it in FIXTURES_MUST_PASS or FIXTURES_MUST_REJECT"
    fi
  fi
done

[[ -n "$SEEN" ]] || fail "no fixture directories under '$FIXTURES' — the enumeration is vacuous"

# Reverse pin: a registered fixture whose directory is missing is loud too.
for name in $FIXTURES_MUST_PASS; do
  list_contains "$SEEN" "$name" || fail "FIXTURES_MUST_PASS entry '$name' has no directory under '$FIXTURES'"
done
for name in $REJECT_NAMES; do
  list_contains "$SEEN" "$name" || fail "FIXTURES_MUST_REJECT entry '$name' has no directory under '$FIXTURES'"
done

# --- scoop-shim specifics ----------------------------------------------------
# The registry pins the verdict (rejected, rule-3 reported); these pins carry
# the #3280 case's sharper expectations: the rejection must name the omitted
# '.com' extension, and the Scoop defect must be the fixture's only defect —
# a rule-1 or rule-2 failure on it would mean the fixture overstates the
# regression it pins.
scoop_status=0
scoop_out="$("$GATE" --root "$FIXTURES/scoop-shim" 2>&1)" || scoop_status=$?
if ((scoop_status == 0)); then
  fail "must-reject fixture 'scoop-shim' was accepted by the gate; the enumeration above should have caught this"
else
  grep -q "'\.com'" <<<"$scoop_out" ||
    fail "scoop-shim rejection did not name the missing '.com' extension: $scoop_out"
  if grep -q 'rule-1\|rule-2' <<<"$scoop_out"; then
    fail "scoop-shim fixture also broke an unrelated rule: $scoop_out"
  fi
fi

if ((FAIL != 0)); then
  echo "check-release-windows-verify.test.sh: FAILED" >&2
  exit 1
fi
echo "check-release-windows-verify.test.sh: all cases passed"
