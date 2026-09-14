#!/usr/bin/env bash
# check-status-drift.test.sh — CTX-0437 fixture test for the status-drift gate.
#
# Runs scripts/check-status-drift.sh --root against committed fixture trees:
#   fixtures/status-drift/clean       -> gate must pass (exit 0, no findings)
#   fixtures/status-drift/violations  -> gate must fail (exit 1) with one
#                                         finding for every rule tag.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-status-drift.sh
FIXTURES=scripts/tests/fixtures/status-drift
FAIL=0

clean_out="$("$GATE" --root "$FIXTURES/clean" 2>&1)" || clean_status=$?
clean_status="${clean_status:-0}"
if ((clean_status != 0)); then
	echo "FAIL: clean fixture rejected by the gate (exit $clean_status):" >&2
	printf '%s\n' "$clean_out" >&2
	FAIL=1
fi

violations_out="$("$GATE" --root "$FIXTURES/violations" 2>&1)" || violations_status=$?
violations_status="${violations_status:-0}"
if ((violations_status == 0)); then
	echo "FAIL: violations fixture passed the gate:" >&2
	printf '%s\n' "$violations_out" >&2
	FAIL=1
fi

for tag in \
	'status-drift[oq-status]' \
	'status-drift[crate-count]' \
	'status-drift[submodule]' \
	'status-drift[rfc-status]'; do
	if ! grep -qF -e "$tag" <<<"$violations_out"; then
		echo "FAIL: violations fixture did not trip $tag:" >&2
		printf '%s\n' "$violations_out" >&2
		FAIL=1
	fi
done

if ((FAIL)); then
	echo "status-drift-test: FAIL" >&2
	exit 1
fi
echo "status-drift-test: OK"
