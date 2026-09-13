#!/usr/bin/env bash
# check-scratch-paths.test.sh — CTX-0379 fixture test for the scratch-path gate.
#
# Runs scripts/check-scratch-paths.sh --root against committed fixture trees:
#   fixtures/scratch-paths/clean       -> gate must pass (exit 0, no findings)
#   fixtures/scratch-paths/violations  -> gate must fail (exit 1) with one
#                                         finding for every rule pattern.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-scratch-paths.sh
FIXTURES=scripts/tests/fixtures/scratch-paths
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
	'scratch-paths[recordings]' \
	'scratch-paths[tmp-write]' \
	'scratch-paths[abs-path]' \
	'scratch-paths[doc-abs-path]'; do
	if ! rg -qF "$tag" <<<"$violations_out"; then
		echo "FAIL: violations fixture did not trip $tag:" >&2
		printf '%s\n' "$violations_out" >&2
		FAIL=1
	fi
done

if ((FAIL)); then
	echo "scratch-paths-test: FAIL" >&2
	exit 1
fi
echo "scratch-paths-test: OK"
