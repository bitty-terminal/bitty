#!/usr/bin/env bash
# check-release-windows-verify.test.sh — CTX-1916 enumerated fixture suite for
# the release workflow's Windows verification gate.
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
# Fixture inventory (pinned against the v0.0.46 release run, 44718678246):
#   clean           -> gate passes
#   lookup-root     -> rule-1: loop root renamed, the gate must follow the lookup
#   newline         -> rule-2: assertion needs a newline
#   newline-suffix  -> rule-2: newline before `\s*\z`
#   zip-path        -> rule-1: download path differs from the loop root
#
# The real workflow's default-root check runs in CI via
# `scripts/check-release-windows-verify.sh`.
set -euo pipefail

# Byte-stable ordering for the sorted enumeration and all matching below.
export LC_ALL=C

cd "$(dirname "$0")/../.."

GATE=./scripts/check-release-windows-verify.sh
FIXTURES=scripts/tests/fixtures/release-windows-verify
FAIL=0
SEEN=""

# --- Registry: every fixture must be declared in exactly one list ------------
FIXTURES_MUST_PASS="clean"
FIXTURES_MUST_REJECT="lookup-root:rule-1 newline:rule-2 newline-suffix:rule-2 zip-path:rule-1"

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
  if (( $(word_count "$FIXTURES_MUST_PASS" "$name") > 1 )); then
    fail "duplicate FIXTURES_MUST_PASS entry '$name'"
  fi
done
for entry in $FIXTURES_MUST_REJECT; do
  if [[ "$entry" != *:* || -z "${entry%%:*}" || -z "${entry##*:}" ]]; then
    fail "malformed FIXTURES_MUST_REJECT entry '$entry' (expected '<fixture>:<rule>')"
  fi
done
for name in $REJECT_NAMES; do
  if (( $(word_count "$REJECT_NAMES" "$name") > 1 )); then
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
    if (( status != 0 )); then
      printf '  %-4s %-16s expect=pass   result=REJECTED (exit %s)\n' FAIL "$fixture" "$status"
      fail "unexpected reject: must-pass fixture '$fixture' was rejected by the gate (exit $status):"
      printf '%s\n' "$out" >&2
    elif grep -q 'PASS' <<<"$out"; then
      printf '  %-4s %-16s expect=pass   result=PASS (exit 0)\n' ok "$fixture"
    else
      printf '  %-4s %-16s expect=pass   result=exit 0 without reporting PASS\n' FAIL "$fixture"
      fail "must-pass fixture '$fixture' exited 0 but the gate did not report PASS: $out"
    fi
  elif rule="$(reject_rule "$fixture")"; then
    if (( status == 0 )); then
      printf '  %-4s %-16s expect=reject %s result=ACCEPTED (exit 0)\n' FAIL "$fixture" "$rule"
      fail "unexpected pass: must-reject fixture '$fixture' was accepted by the gate (no fixture outside FIXTURES_MUST_PASS may pass):"
      printf '%s\n' "$out" >&2
    elif grep -q "$rule" <<<"$out"; then
      printf '  %-4s %-16s expect=reject %s result=REJECTED (exit %s, reported %s)\n' ok "$fixture" "$rule" "$status" "$rule"
    else
      printf '  %-4s %-16s expect=reject %s result=REJECTED (exit %s) without reporting %s\n' FAIL "$fixture" "$rule" "$status" "$rule"
      fail "must-reject fixture '$fixture' was rejected (exit $status) but the gate did not report $rule: $out"
    fi
  else
    # Unregistered directory — the exact gap the enumeration exists to close.
    if (( status == 0 )); then
      printf '  %-4s %-16s expect=registered result=ACCEPTED (exit 0)\n' FAIL "$fixture"
      fail "unexpected pass: fixture '$fixture' is registered in neither list and the gate accepted it — register it in FIXTURES_MUST_PASS or FIXTURES_MUST_REJECT"
    else
      printf '  %-4s %-16s expect=registered result=REJECTED (exit %s)\n' FAIL "$fixture" "$status"
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

if (( FAIL != 0 )); then
  echo "check-release-windows-verify.test.sh: FAILED" >&2
  exit 1
fi
echo "check-release-windows-verify.test.sh: all cases passed"
