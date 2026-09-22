#!/usr/bin/env bash
# dogfood-session.test.sh — CTX-0643 contract test for scripts/dogfood-session.sh.
#
# Headless-safe: every assertion runs through --help, --dry-run,
# --print-systemd, or --print-cron, so no display, Hyprland session, or
# built binary is required. The live run path is only asserted for its
# honest UNAVAILABLE refusal (exit 2) when the gate/session is absent.
set -euo pipefail

cd "$(dirname "$0")/../.."

SESSION=./scripts/dogfood-session.sh
FAIL=0
TEST_OUT="$(mktemp /tmp/bitty-dogfood-test-out.XXXXXX.txt)"

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
expect_exit 0 "$SESSION" --help
expect_stdout "--dry-run" "$SESSION" --help
expect_stdout "--print-systemd" "$SESSION" --help
expect_stdout "--apps" "$SESSION" --help

# Unknown flags and invalid knobs fail closed with exit 2.
expect_exit 2 "$SESSION" --bogus
expect_exit 2 "$SESSION" --dry-run --apps rm-rf
expect_exit 2 "$SESSION" --dry-run --duration-secs 10
expect_exit 2 "$SESSION" --dry-run --duration-secs 999999
expect_exit 2 "$SESSION" --dry-run --duration-secs banana
expect_exit 2 "$SESSION" --dry-run --cycle-secs 5
expect_exit 2 "$SESSION" --dry-run --cycle-secs 99999
expect_exit 2 "$SESSION" --dry-run --cycle-secs banana
expect_exit 2 "$SESSION" --dry-run --workspace 11
expect_exit 2 "$SESSION" --dry-run --workspace studio

# Default plan mirrors the PB-3 four-hour window: 14400/600 + t=0 = 25.
expect_stdout "cycles=25" "$SESSION" --dry-run
expect_stdout "apps=shell,cargo,git,nvim,tmux,ssh" "$SESSION" --dry-run
expect_stdout "workspace=4" "$SESSION" --dry-run --out-dir recording/dogfood-session

# Custom plan math: 600/300 gives cycles at 0, 300, 600.
expect_stdout "cycles=3" "$SESSION" --dry-run --duration-secs 600 --cycle-secs 300
expect_stdout "step 300s" "$SESSION" --dry-run --duration-secs 600 --cycle-secs 300

# App subsets canonicalize to drive order.
expect_stdout "apps=nvim,ssh" "$SESSION" --dry-run --apps ssh,nvim
expect_stdout "cycles=25" "$SESSION" --dry-run --apps nvim

# Extreme cadence widens the interval instead of overflowing (256 cap).
expect_stdout "cycles=256" "$SESSION" --dry-run --duration-secs 86400 --cycle-secs 60
expect_stdout "widened" "$SESSION" --dry-run --duration-secs 86400 --cycle-secs 60

# --write-schedule dumps a reviewable schedule document (needs jq).
if command -v jq >/dev/null 2>&1; then
	sched_out="$(mktemp /tmp/bitty-dogfood-sched.XXXXXX.json)"
	"$SESSION" --dry-run --duration-secs 600 --cycle-secs 300 --write-schedule "$sched_out" >/dev/null
	for key in '"task": "CTX-0643"' '"status": "scheduled"' '"planned_cycles": 3' '"budget_ref"'; do
		if ! grep -qF -- "$key" "$sched_out"; then
			echo "FAIL: schedule missing [$key]" >&2
			FAIL=1
		fi
	done
	if ! jq -e . "$sched_out" >/dev/null 2>&1; then
		echo "FAIL: schedule is not valid JSON" >&2
		FAIL=1
	fi
	if ! jq -e '.issues == [1064] and .session.planned_cycles == 3' "$sched_out" >/dev/null 2>&1; then
		echo "FAIL: schedule issues/plan mismatch" >&2
		FAIL=1
	fi
	if ! jq -e '.session.apps == ["shell","cargo","git","nvim","tmux","ssh"]' "$sched_out" >/dev/null 2>&1; then
		echo "FAIL: schedule apps mismatch" >&2
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
expect_exit 2 "$SESSION" --print-systemd --out-dir recording/dogfood-session
expect_exit 2 "$SESSION" --print-cron --out-dir recording/dogfood-session
expect_stdout "ExecStart=" "$SESSION" --print-systemd --repo-dir /repo/under/test --out-dir recording/dogfood-session
expect_stdout "OnCalendar=daily" "$SESSION" --print-systemd --repo-dir /repo/under/test --out-dir recording/dogfood-session
expect_stdout "systemctl --user" "$SESSION" --print-systemd --repo-dir /repo/under/test --out-dir recording/dogfood-session
expect_stdout "bitty-dogfood-session" "$SESSION" --print-systemd --repo-dir /repo/under/test --out-dir recording/dogfood-session
expect_stdout "0 3 * * *" "$SESSION" --print-cron --repo-dir /repo/under/test --out-dir recording/dogfood-session
expect_stdout "dogfood-session.sh" "$SESSION" --print-cron --repo-dir /repo/under/test --out-dir recording/dogfood-session

# Live run without the opt-in gate/session refuses honestly (exit 2).
got=0
env -u BITTY_PERF_DOGFOOD_SESSION -u HYPRLAND_INSTANCE_SIGNATURE "$SESSION" --out-dir recording/dogfood-session >"$TEST_OUT" 2>&1 || got=$?
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
	echo "dogfood-session-test: FAIL" >&2
	exit 1
fi
echo "dogfood-session-test: OK"
