#!/usr/bin/env bash
# rust-channel.test.sh — CTX-0502 fixture test for the rust-toolchain channel
# reader used by the release workflow.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/rust-channel.sh
FIX=scripts/tests/fixtures/rust-channel
FAIL=0

# expect_out <expected> <label> <gate args...>
expect_out() {
	local want="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status != 0)); then
		echo "FAIL: $label: expected exit 0, got $status" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	elif [[ "$out" != "$want" ]]; then
		echo "FAIL: $label: expected '$want', got '$out'" >&2
		FAIL=1
	fi
}

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

# 1. The repo default file resolves to the pinned channel.
pinned="$(awk -F'"' '/^channel[[:space:]]*=/{print $2; exit}' rust-toolchain.toml)"
expect_out "$pinned" 'repo default' --file rust-toolchain.toml

# 2. Quoted and single-quoted channel values are unwrapped.
expect_out '1.98.1' 'double-quoted fixture' --file "$FIX/toolchain.toml"
expect_out 'nightly-2026-01-01' 'single-quoted fixture' --file "$FIX/single-quoted.toml"

# 3. Failure modes: no channel and missing file exit 1; bad usage exits 2.
expect_exit 1 'no channel' --file "$FIX/no-channel.toml"
expect_exit 1 'missing file' --file "$FIX/absent.toml"
expect_exit 2 'unknown argument' --bogus

if ((FAIL)); then
	echo "rust-channel-test: FAIL" >&2
	exit 1
fi
echo "rust-channel-test: OK"
