#!/usr/bin/env bash
# real-render-soak.sh — Automated long-duration real-render soak (CTX-0642, PERF-09).
# Closes the manual hyprctl+grim capture leg: builds the bitty binary, launches
# it on a chosen Hyprland workspace, waits for the first frame, then collects
# periodic evidence through two legs per capture — the pixel leg
# (hyprctl+grim screenshot) and the DevTools-preferred introspection leg
# (`bitty ctl terminal text` grid snapshot) — plus a per-capture RSS sample
# for the PB-3 typical-session anchor. The window is never stranded: an EXIT
# trap closes it and restores the previously focused workspace.
#
# Evidence stays local and uncommitted under --out-dir (default
# recording/real-soak, gitignored): captures.csv, soak-evidence.json,
# per-capture PNG + grid JSON, and the bitty log. Screenshots are file names
# only inside the JSON; no absolute host path is recorded. Promoting an
# artifact to crates/bitty-perf/baselines/ is an explicit reviewed copy.
#
# Usage:
#   bash scripts/real-render-soak.sh --out-dir DIR [--duration-secs N]
#       [--interval-secs N] [--workspace ID] [--workload idle|mixed|input-spam]
#       [--binary PATH] [--release] [--wait-secs N] [--settle-secs N]
#   bash scripts/real-render-soak.sh --dry-run [--duration-secs N] [--interval-secs N] ...
#   bash scripts/real-render-soak.sh --print-systemd --repo-dir DIR --out-dir DIR [...]
#   bash scripts/real-render-soak.sh --print-cron --repo-dir DIR --out-dir DIR [...]

set -euo pipefail

OUT_DIR="recording/real-soak"
DURATION_SECS=14400
INTERVAL_SECS=300
SOAK_WS="4"
WORKLOAD="mixed"
BIN_OVERRIDE=""
RELEASE=0
WAIT_SECS=15
SETTLE_SECS=2
DRY_RUN=0
PRINT_SYSTEMD=0
PRINT_CRON=0
REPO_DIR_OVERRIDE=""
WRITE_SCHEDULE=""

while [ $# -gt 0 ]; do
	case "$1" in
	--out-dir)
		OUT_DIR="${2:-}"
		shift 2
		;;
	--duration-secs)
		DURATION_SECS="${2:-}"
		shift 2
		;;
	--interval-secs)
		INTERVAL_SECS="${2:-}"
		shift 2
		;;
	--workspace)
		SOAK_WS="${2:-}"
		shift 2
		;;
	--workload)
		WORKLOAD="${2:-}"
		shift 2
		;;
	--binary)
		BIN_OVERRIDE="${2:-}"
		shift 2
		;;
	--release)
		RELEASE=1
		shift
		;;
	--wait-secs)
		WAIT_SECS="${2:-15}"
		shift 2
		;;
	--settle-secs)
		SETTLE_SECS="${2:-2}"
		shift 2
		;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--print-systemd)
		PRINT_SYSTEMD=1
		shift
		;;
	--print-cron)
		PRINT_CRON=1
		shift
		;;
	--repo-dir)
		REPO_DIR_OVERRIDE="${2:-}"
		shift 2
		;;
	--write-schedule)
		WRITE_SCHEDULE="${2:-}"
		shift 2
		;;
	--help | -h)
		sed -n '2,20p' "$0"
		echo "  --out-dir DIR      evidence directory (default recording/real-soak)"
		echo "  --duration-secs N  soak wall time 60..86400 (default 14400 = 4 h)"
		echo "  --interval-secs N  capture cadence 30..3600 (default 300 = 5 min)"
		echo "  --workspace ID     Hyprland workspace 1..10 (default 4)"
		echo "  --workload NAME    idle|mixed|input-spam (default mixed)"
		echo "  --binary PATH      explicit bitty binary (default: target build)"
		echo "  --release          build and measure the release binary"
		echo "  --dry-run          print the capture plan; launch nothing"
		echo "  --print-systemd    emit user service+timer (needs --repo-dir)"
		echo "  --print-cron       emit a cron line (needs --repo-dir)"
		exit 0
		;;
	*)
		echo "unknown flag $1" >&2
		exit 2
		;;
	esac
done

is_uint() {
	case "$1" in
	'' | *[!0-9]*) return 1 ;;
	*) return 0 ;;
	esac
}

fail_usage() {
	echo "real-render-soak: $1" >&2
	exit 2
}

is_uint "$DURATION_SECS" || fail_usage "--duration-secs must be a positive integer"
is_uint "$INTERVAL_SECS" || fail_usage "--interval-secs must be a positive integer"
[ "$DURATION_SECS" -ge 60 ] && [ "$DURATION_SECS" -le 86400 ] || fail_usage "--duration-secs must be 60..86400"
[ "$INTERVAL_SECS" -ge 30 ] && [ "$INTERVAL_SECS" -le 3600 ] || fail_usage "--interval-secs must be 30..3600"
case "$SOAK_WS" in
1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10) ;;
*) fail_usage "--workspace must be 1..10" ;;
esac
case "$WORKLOAD" in
idle | mixed | input-spam) ;;
*) fail_usage "--workload must be idle|mixed|input-spam" ;;
esac

# Bounded capture plan (mirrors bitty_perf::real_soak::plan_captures):
# capture 0 fires at soak start, then every effective interval while
# offset <= duration; the count never exceeds 512 (interval widens instead).
EFFECTIVE_INTERVAL="$INTERVAL_SECS"
CAPTURES=$((DURATION_SECS / EFFECTIVE_INTERVAL + 1))
if [ "$CAPTURES" -gt 512 ]; then
	EFFECTIVE_INTERVAL=$(((DURATION_SECS + 511) / 512))
	[ "$EFFECTIVE_INTERVAL" -lt 1 ] && EFFECTIVE_INTERVAL=1
	CAPTURES=$((DURATION_SECS / EFFECTIVE_INTERVAL + 1))
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [ "$PRINT_SYSTEMD" = "1" ] || [ "$PRINT_CRON" = "1" ]; then
	[ -n "$REPO_DIR_OVERRIDE" ] || fail_usage "--print-systemd/--print-cron need --repo-dir DIR"
	[ -n "$OUT_DIR" ] || fail_usage "--print-systemd/--print-cron need --out-dir DIR"
	RUN_LINE="$REPO_DIR_OVERRIDE/scripts/real-render-soak.sh --out-dir $OUT_DIR --duration-secs $DURATION_SECS --interval-secs $INTERVAL_SECS --workspace $SOAK_WS --workload $WORKLOAD"
	if [ "$PRINT_SYSTEMD" = "1" ]; then
		cat <<EOF
# bitty real-render soak (CTX-0642, PERF-09) — install with:
#   mkdir -p ~/.config/systemd/user
#   bash scripts/real-render-soak.sh --print-systemd --repo-dir <repo> --out-dir <dir> > ~/.config/systemd/user/bitty-real-soak.service
#   (split the service/timer blocks below into bitty-real-soak.service and bitty-real-soak.timer)
#   systemctl --user daemon-reload && systemctl --user enable --now bitty-real-soak.timer
[Unit]
Description=bitty real-render soak evidence capture
After=graphical-session.target

[Service]
Type=oneshot
ExecStart=$RUN_LINE

[Install]
WantedBy=default.target
---
[Unit]
Description=bitty real-render soak schedule (daily)

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
EOF
	else
		cat <<EOF
# bitty real-render soak (CTX-0642, PERF-09) — install with: crontab -e
# Daily 02:00 capture; output stays under the given directory.
0 2 * * * $RUN_LINE
EOF
	fi
	exit 0
fi

# Portable host context (environment-derived; no checkout path, username,
# or hostname is ever recorded).
host_os="$(uname -s | tr '[:upper:]' '[:lower:]')"
host_arch="$(uname -m)"
host_cpus="$(nproc 2>/dev/null || echo 0)"
host_mem_mb="$(awk '/^MemTotal:/ { printf "%d", $2 / 1024 }' /proc/meminfo 2>/dev/null || echo null)"
[ -n "$host_mem_mb" ] || host_mem_mb="null"
host_toolchain="$(rustc --version 2>/dev/null || echo unrecorded)"
capture_date="$(date -u +%F)"
revision="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unrecorded)"
record_command="scripts/real-render-soak.sh --duration-secs $DURATION_SECS --interval-secs $INTERVAL_SECS --workspace $SOAK_WS --workload $WORKLOAD"
if [ "$RELEASE" = "1" ]; then
	profile="release"
else
	profile="dev (debug)"
fi

write_schedule_json() {
	dest="$1"
	jq -n \
		--arg task "CTX-0642" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg workload "$WORKLOAD" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson interval "$INTERVAL_SECS" \
		--argjson effective "$EFFECTIVE_INTERVAL" \
		--argjson planned "$CAPTURES" \
		'{
			schema_version: 1, task: $task, issues: [1063],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			soak: {
				status: "scheduled", duration_secs: $duration,
				interval_secs: $interval, interval_effective_secs: $effective,
				workspace: $ws, workload: $workload, planned_captures: $planned
			}
		}' >"$dest"
}

if [ "$DRY_RUN" = "1" ]; then
	echo "real-render-soak plan: duration=${DURATION_SECS}s interval=${INTERVAL_SECS}s effective=${EFFECTIVE_INTERVAL}s captures=${CAPTURES} workspace=${SOAK_WS} workload=${WORKLOAD}"
	if [ "$EFFECTIVE_INTERVAL" != "$INTERVAL_SECS" ]; then
		echo "real-render-soak plan: interval widened to ${EFFECTIVE_INTERVAL}s to respect MAX_CAPTURES=512"
	fi
	echo "real-render-soak plan: capture offsets: 0..${DURATION_SECS}s step ${EFFECTIVE_INTERVAL}s"
	if [ -n "$WRITE_SCHEDULE" ]; then
		mkdir -p "$(dirname "$WRITE_SCHEDULE")"
		write_schedule_json "$WRITE_SCHEDULE"
		echo "real-render-soak plan: schedule -> $WRITE_SCHEDULE"
	fi
	exit 0
fi

for tool in hyprctl grim jq cargo; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "real-render-soak: UNAVAILABLE (capture leg missing tool: $tool)" >&2
		exit 2
	fi
done
if [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
	echo "real-render-soak: UNAVAILABLE (no Hyprland session: HYPRLAND_INSTANCE_SIGNATURE unset)" >&2
	exit 2
fi
if [ -z "${BITTY_PERF_REAL_SOAK:-}" ] || [ "$BITTY_PERF_REAL_SOAK" != "1" ]; then
	echo "real-render-soak: UNAVAILABLE (opt-in gate closed: set BITTY_PERF_REAL_SOAK=1)" >&2
	exit 2
fi

[ -n "$OUT_DIR" ] || fail_usage "--out-dir must not be empty"
case "$OUT_DIR" in
/*) RUN_OUT="$OUT_DIR" ;;
*) RUN_OUT="$ROOT/$OUT_DIR" ;;
esac
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="$RUN_OUT/run-$STAMP"
mkdir -p "$RUN_DIR"

export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/1000}"

cd "$ROOT"
if [ "$RELEASE" = "1" ]; then
	BITTY_PROFILE_DIR="release"
	CARGO_PROFILE_FLAG="--release"
else
	BITTY_PROFILE_DIR="debug"
	CARGO_PROFILE_FLAG=""
fi
if [ -n "$BIN_OVERRIDE" ]; then
	BIN="$BIN_OVERRIDE"
else
	BIN="$ROOT/target/$BITTY_PROFILE_DIR/bitty"
fi
LOG="$RUN_DIR/bitty.log"
SOCK="$RUN_DIR/bitty-soak.sock"

CHILD_PID=""
WIN_ADDR=""
PREV_WS="$(hyprctl activeworkspace -j | jq -r .id)"
echo "real-render-soak: prev_ws=$PREV_WS soak_ws=$SOAK_WS run_dir=$RUN_DIR captures=$CAPTURES"

cleanup() {
	if [ -n "$WIN_ADDR" ]; then
		if hyprctl clients -j | jq -e --arg a "$WIN_ADDR" 'any(.[]; .address == $a)' >/dev/null 2>&1; then
			hyprctl dispatch "hl.dsp.window.close({ window = \"address:$WIN_ADDR\" })" >/dev/null 2>&1 || true
			for _ in $(seq 1 30); do
				if ! hyprctl clients -j | jq -e --arg a "$WIN_ADDR" 'any(.[]; .address == $a)' >/dev/null 2>&1; then
					break
				fi
				sleep 0.2
			done
			if hyprctl clients -j | jq -e --arg a "$WIN_ADDR" 'any(.[]; .address == $a)' >/dev/null 2>&1; then
				hyprctl dispatch closewindow "address:$WIN_ADDR" >/dev/null 2>&1 || true
			fi
		fi
	fi
	if [ -n "$CHILD_PID" ] && kill -0 "$CHILD_PID" 2>/dev/null; then
		kill "$CHILD_PID" 2>/dev/null || true
		for _ in $(seq 1 25); do
			kill -0 "$CHILD_PID" 2>/dev/null || break
			sleep 0.2
		done
		if kill -0 "$CHILD_PID" 2>/dev/null; then
			kill -9 "$CHILD_PID" 2>/dev/null || true
		fi
	fi
	if [ -n "$PREV_WS" ] && [ "$PREV_WS" != "null" ]; then
		hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null 2>&1 || true
	fi
}
trap cleanup EXIT

echo "real-render-soak: building bitty binary (crate bitty-app, profile $BITTY_PROFILE_DIR)"
# shellcheck disable=SC2086
cargo build -p bitty-app --locked --quiet $CARGO_PROFILE_FLAG
if [ ! -x "$BIN" ]; then
	echo "build produced no binary at $BIN" >&2
	exit 1
fi

echo "real-render-soak: launching $BIN"
BITTY_PERF_STARTUP_MARKER=1 BITTY_SOCKET="$SOCK" BITTY_INSTANCE_ID="real-soak-$STAMP" "$BIN" >"$LOG" 2>&1 &
CHILD_PID=$!
echo "real-render-soak: pid=$CHILD_PID log=$LOG"

echo "real-render-soak: waiting for window (class bitty, pid $CHILD_PID)"
WIN_ADDR=""
for _ in $(seq 1 $((WAIT_SECS * 5))); do
	if ! kill -0 "$CHILD_PID" 2>/dev/null; then
		echo "bitty exited early; log tail:" >&2
		tail -n 30 "$LOG" >&2 || true
		exit 1
	fi
	WIN_ADDR="$(hyprctl clients -j | jq -r --argjson pid "$CHILD_PID" '[.[] | select(.class == "bitty" and .pid == $pid)][0].address // empty')"
	if [ -n "$WIN_ADDR" ]; then
		break
	fi
	sleep 0.2
done
if [ -z "$WIN_ADDR" ]; then
	echo "timed out waiting for bitty window" >&2
	tail -n 30 "$LOG" >&2 || true
	exit 1
fi
echo "real-render-soak: window address=$WIN_ADDR"

hyprctl dispatch "hl.dsp.window.move({ workspace = \"$SOAK_WS\", window = \"address:$WIN_ADDR\" })"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$SOAK_WS\" })"

echo "real-render-soak: waiting for first frame"
FRAMES=0
for _ in $(seq 1 $((WAIT_SECS * 5))); do
	if grep -qE "bitty perf: first-frame|tick: frame=" "$LOG" 2>/dev/null; then
		FRAMES=1
		break
	fi
	if ! kill -0 "$CHILD_PID" 2>/dev/null; then
		echo "bitty exited before first frame; log tail:" >&2
		tail -n 30 "$LOG" >&2 || true
		exit 1
	fi
	sleep 0.2
done
if [ "$FRAMES" != "1" ]; then
	echo "timed out waiting for first frame" >&2
	tail -n 30 "$LOG" >&2 || true
	exit 1
fi
sleep "$SETTLE_SECS"

# DevTools-preferred introspection leg: resolve one terminal id for the
# workload driver and the grid-text snapshots. Best-effort: when the socket
# or scope is unavailable the pixel+RSS legs still produce evidence and
# every capture records driver_ok=false.
TERM_ID=""
if TERM_JSON="$("$BIN" ctl --socket "$SOCK" terminal list --format json 2>/dev/null)"; then
	TERM_ID="$(printf '%s' "$TERM_JSON" | jq -r '.. | objects | .id? // empty' 2>/dev/null | grep -E '^(t:)?[0-9]+$' | head -n 1 || true)"
	[ -n "$TERM_ID" ] || TERM_ID="$(printf '%s' "$TERM_JSON" | jq -r '[.. | strings] | map(select(test("^t:[0-9]+$"))) | first // empty' 2>/dev/null || true)"
fi
echo "real-render-soak: terminal leg id=${TERM_ID:-unavailable}"

read_rss_mb() {
	status_file="/proc/$1/status"
	if [ -r "$status_file" ]; then
		awk '/^VmRSS:/ { printf "%.3f", $2 / 1024 }' "$status_file" 2>/dev/null || true
	else
		kb="$(ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' || true)"
		case "$kb" in
		'' | *[!0-9]*) printf '' ;;
		*) awk -v kb="$kb" 'BEGIN { printf "%.3f", kb / 1024 }' ;;
		esac
	fi
}

drive_workload() {
	idx="$1"
	case "$WORKLOAD" in
	idle)
		return 0
		;;
	mixed)
		[ -n "$TERM_ID" ] || return 1
		case $((idx % 4)) in
		0) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "printf 'bitty-soak line $idx ok\n'" >/dev/null 2>&1 ;;
		1) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "echo bitty-soak-mixed-$idx" >/dev/null 2>&1 ;;
		2) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "printf 'cells %s\\n' '0123456789abcdef'" >/dev/null 2>&1 ;;
		*) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "true" >/dev/null 2>&1 ;;
		esac
		;;
	input-spam)
		[ -n "$TERM_ID" ] || return 1
		"$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "soakspam-$idx-abcdefghijklmnopqrstuvwxyz0123456789" >/dev/null 2>&1
		;;
	esac
}

CSV="$RUN_DIR/captures.csv"
printf 'index,at_secs,screenshot,rss_mb,grid_text_bytes,driver_ok\n' >"$CSV"
i=0
while [ "$i" -lt "$CAPTURES" ]; do
	at_secs=$((i * EFFECTIVE_INTERVAL))
	if [ "$i" -gt 0 ]; then
		sleep "$EFFECTIVE_INTERVAL"
		if ! kill -0 "$CHILD_PID" 2>/dev/null; then
			echo "bitty exited mid-soak at capture $i; log tail:" >&2
			tail -n 30 "$LOG" >&2 || true
			break
		fi
	else
		if ! kill -0 "$CHILD_PID" 2>/dev/null; then
			echo "bitty exited before first capture; log tail:" >&2
			tail -n 30 "$LOG" >&2 || true
			break
		fi
	fi
	shot="capture-$(printf '%04d' "$i").png"
	if grim "$RUN_DIR/$shot" 2>/dev/null; then
		shot_ok=1
	else
		echo "real-render-soak: grim failed at capture $i" >&2
		shot="missed-$(printf '%04d' "$i").png"
		shot_ok=0
	fi
	rss="$(read_rss_mb "$CHILD_PID")"
	grid_bytes=""
	if [ -n "$TERM_ID" ]; then
		grid_file="$RUN_DIR/capture-$(printf '%04d' "$i").grid.json"
		if "$BIN" ctl --socket "$SOCK" terminal text "$TERM_ID" --format json >"$grid_file" 2>/dev/null; then
			grid_bytes="$(wc -c <"$grid_file" | tr -d ' ')"
		else
			rm -f "$grid_file"
		fi
	fi
	if [ "$i" -gt 0 ] || [ "$WORKLOAD" != "idle" ]; then
		if drive_workload "$i" 2>/dev/null; then
			driver_ok=1
		else
			driver_ok=0
		fi
	else
		driver_ok=1
	fi
	[ "$shot_ok" = "1" ] || driver_ok=0
	printf '%s,%s,%s,%s,%s,%s\n' "$i" "$at_secs" "$shot" "$rss" "$grid_bytes" "$driver_ok" >>"$CSV"
	echo "real-render-soak: capture $i/$((CAPTURES - 1)) at ${at_secs}s rss=${rss:-n/a}MB driver_ok=$driver_ok"
	i=$((i + 1))
done
completed="$(($(wc -l <"$CSV") - 1))"

# Evidence JSON: same shape as bitty_perf::real_soak::evidence_json —
# screenshot file names only, no output dir or binary path recorded.
EVIDENCE="$RUN_DIR/soak-evidence.json"
if [ "$completed" -gt 0 ]; then
	rss_series="$(awk -F, 'NR>1 && $4 != "" { print $4 }' "$CSV")"
	rss_first="$(printf '%s' "$rss_series" | head -n 1)"
	rss_last="$(printf '%s' "$rss_series" | tail -n 1)"
	rss_max="$(printf '%s' "$rss_series" | sort -n | tail -n 1)"
	jq -Rn \
		--arg task "CTX-0642" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg workload "$WORKLOAD" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson interval "$INTERVAL_SECS" \
		--argjson effective "$EFFECTIVE_INTERVAL" \
		--argjson completed "$completed" \
		--arg rss_first "$rss_first" \
		--arg rss_last "$rss_last" \
		--arg rss_max "$rss_max" \
		--rawfile csv "$CSV" \
		'($csv | split("\n") | map(select(length > 0)) | .[1:] | map(split(",")) | map({
			index: (.[0] | tonumber), at_secs: (.[1] | tonumber), screenshot: .[2],
			rss_mb: (if .[3] == "" then null else (.[3] | tonumber) end),
			grid_text_bytes: (if .[4] == "" then null else (.[4] | tonumber) end),
			driver_ok: (.[5] == "1")
		})) as $captures
		| {
			schema_version: 1, task: $task, issues: [1063],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			soak: {
				status: "measured", duration_secs: $duration,
				interval_secs: $interval, interval_effective_secs: $effective,
				workspace: $ws, workload: $workload, completed_captures: $completed,
				rss_first_mb: ($rss_first | tonumber), rss_last_mb: ($rss_last | tonumber),
				rss_max_mb: ($rss_max | tonumber),
				rss_growth_pct: ((($rss_last | tonumber) - ($rss_first | tonumber)) / ($rss_first | tonumber) * 100),
				budget_mb: 250, captures: $captures
			}
		}' >"$EVIDENCE"
else
	jq -n \
		--arg task "CTX-0642" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg workload "$WORKLOAD" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson interval "$INTERVAL_SECS" \
		--argjson effective "$EFFECTIVE_INTERVAL" \
		'{
			schema_version: 1, task: $task, issues: [1063],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			soak: {
				status: "unavailable", duration_secs: $duration,
				interval_secs: $interval, interval_effective_secs: $effective,
				workspace: $ws, workload: $workload, completed_captures: 0,
				rss_first_mb: null, rss_last_mb: null, budget_mb: 250,
				reason: "soak window exited before the first capture completed"
			}
		}' >"$EVIDENCE"
fi

echo "real-render-soak: closing window"
hyprctl dispatch "hl.dsp.window.close({ window = \"address:$WIN_ADDR\" })" || true
for _ in $(seq 1 25); do
	kill -0 "$CHILD_PID" 2>/dev/null || break
	sleep 0.2
done
WIN_ADDR=""
CHILD_PID=""

echo "real-render-soak: restoring workspace $PREV_WS"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })"
echo "real-render-soak: done completed=$completed evidence=$EVIDENCE"
