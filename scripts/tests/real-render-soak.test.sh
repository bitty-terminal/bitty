#!/usr/bin/env bash
# real-render-soak.test.sh — CTX-0642 contract test for scripts/real-render-soak.sh.
#
# Headless-safe: every assertion runs through --help, --dry-run,
# --print-systemd, or --print-cron, so no display, Hyprland session, or
# built binary is required. The live run path is only asserted for its
# honest UNAVAILABLE refusal (exit 2) when the gate/session is absent.
set -euo pipefail

cd "$(dirname "$0")/../.."

SOAK=./scripts/real-render-soak.sh
FAIL=0
TEST_OUT="$(mktemp /tmp/bitty-soak-test-out.XXXXXX.txt)"

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
	if ! "$@" 2>/dev/null | grep -qF -- "$pattern"; then
		echo "FAIL: stdout missing [$pattern]: $*" >&2
		FAIL=1
	fi
}

# --help lists the contract surface.
expect_exit 0 "$SOAK" --help
expect_stdout "--dry-run" "$SOAK" --help
expect_stdout "--print-systemd" "$SOAK" --help

# Unknown flags and invalid knobs fail closed with exit 2.
expect_exit 2 "$SOAK" --bogus
expect_exit 2 "$SOAK" --dry-run --workload rm-rf
expect_exit 2 "$SOAK" --dry-run --duration-secs 10
expect_exit 2 "$SOAK" --dry-run --duration-secs 999999
expect_exit 2 "$SOAK" --dry-run --duration-secs banana
expect_exit 2 "$SOAK" --dry-run --interval-secs 5
expect_exit 2 "$SOAK" --dry-run --interval-secs 99999
expect_exit 2 "$SOAK" --dry-run --workspace 11
expect_exit 2 "$SOAK" --dry-run --workspace studio

# Default plan mirrors the PB-3 four-hour window: 14400/300 + t=0 = 49.
expect_stdout "captures=49" "$SOAK" --dry-run
expect_stdout "workload=mixed" "$SOAK" --dry-run
expect_stdout "workspace=4" "$SOAK" --dry-run --out-dir recording/real-soak

# Custom plan math: 600/300 gives captures at 0, 300, 600.
expect_stdout "captures=3" "$SOAK" --dry-run --duration-secs 600 --interval-secs 300
expect_stdout "step 300s" "$SOAK" --dry-run --duration-secs 600 --interval-secs 300

# Extreme cadence widens the interval instead of overflowing (512 cap).
expect_stdout "captures=512" "$SOAK" --dry-run --duration-secs 86400 --interval-secs 30
expect_stdout "widened" "$SOAK" --dry-run --duration-secs 86400 --interval-secs 30

# --write-schedule dumps a reviewable schedule document (needs jq).
if command -v jq >/dev/null 2>&1; then
	sched_out="$(mktemp /tmp/bitty-soak-sched.XXXXXX.json)"
	"$SOAK" --dry-run --duration-secs 600 --interval-secs 300 --write-schedule "$sched_out" >/dev/null
	for key in '"task": "CTX-0642"' '"status": "scheduled"' '"planned_captures": 3' '"budget_ref"'; do
		if ! grep -qF -- "$key" "$sched_out"; then
			echo "FAIL: schedule missing [$key]" >&2
			FAIL=1
		fi
	done
	if ! jq -e . "$sched_out" >/dev/null 2>&1; then
		echo "FAIL: schedule is not valid JSON" >&2
		FAIL=1
	fi
	if ! jq -e '.issues == [1063] and .soak.planned_captures == 3' "$sched_out" >/dev/null 2>&1; then
		echo "FAIL: schedule issues/plan mismatch" >&2
		FAIL=1
	fi
	# No host checkout path may leak into the schedule (patterns are built
	# from parts so this assertion itself stays gate-clean).
	host_leak=0
	for stem in home mnt Users; do
		if grep -qF "/$stem/" "$sched_out"; then
			host_leak=1
		fi
	done
	if [ "$host_leak" = "1" ]; then
		echo "FAIL: schedule embeds a host path" >&2
		FAIL=1
	fi
	rm -f "$sched_out"
else
	echo "SKIP: jq absent, schedule JSON assertions skipped" >&2
fi

# Schedulers render from caller-supplied dirs only; repo-dir is required.
expect_exit 2 "$SOAK" --print-systemd --out-dir recording/real-soak
expect_exit 2 "$SOAK" --print-cron --out-dir recording/real-soak
expect_stdout "ExecStart=" "$SOAK" --print-systemd --repo-dir /repo/under/test --out-dir recording/real-soak
expect_stdout "OnCalendar=daily" "$SOAK" --print-systemd --repo-dir /repo/under/test --out-dir recording/real-soak
expect_stdout "systemctl --user" "$SOAK" --print-systemd --repo-dir /repo/under/test --out-dir recording/real-soak
expect_stdout "0 2 * * *" "$SOAK" --print-cron --repo-dir /repo/under/test --out-dir recording/real-soak
expect_stdout "real-render-soak.sh" "$SOAK" --print-cron --repo-dir /repo/under/test --out-dir recording/real-soak

# Live run without the opt-in gate/session refuses honestly (exit 2).
got=0
env -u BITTY_PERF_REAL_SOAK -u HYPRLAND_INSTANCE_SIGNATURE "$SOAK" --out-dir recording/real-soak >"$TEST_OUT" 2>&1 || got=$?
if [ "$got" != "2" ]; then
	echo "FAIL: live run without gate/session must exit 2, got $got" >&2
	FAIL=1
fi
if ! grep -qF "UNAVAILABLE" "$TEST_OUT"; then
	echo "FAIL: live run refusal must say UNAVAILABLE" >&2
	FAIL=1
fi
rm -f "$TEST_OUT"

if [ "$FAIL" != "0" ]; then
	echo "real-render-soak-test: FAIL" >&2
	exit 1
fi
echo "real-render-soak-test: OK"
