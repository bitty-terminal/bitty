#!/usr/bin/env bash
# dogfood-session.sh — Automated continuous daily-driver session (CTX-0643, PERF-10).
# Builds on the PERF-09 real-render soak chain: launches the bitty binary on
# a chosen Hyprland workspace, waits for the first frame, then drives every
# selected daily-driver app (shell, cargo, git, nvim, tmux, ssh) once per
# cycle — the way a daily driver actually works — and collects per-cycle
# evidence through two legs per capture: the pixel leg (hyprctl+grim
# screenshot) and the DevTools-preferred introspection leg
# (`bitty ctl terminal text` grid snapshot), plus a per-cycle RSS sample
# for the PB-3 typical-session anchor. The window is never stranded: an EXIT
# trap closes it and restores the previously focused workspace.
#
# Evidence stays local and uncommitted under --out-dir (default
# recording/dogfood-session, gitignored): cycles.csv,
# session-evidence.json, per-cycle PNG + grid JSON, and the bitty log.
# Screenshots are file names only inside the JSON; no absolute host path is
# recorded. Promoting an artifact to crates/bitty-perf/baselines/ is an
# explicit reviewed copy.
#
# Usage:
#   bash scripts/dogfood-session.sh --out-dir DIR [--duration-secs N]
#       [--cycle-secs N] [--workspace ID] [--apps shell,cargo,git,nvim,tmux,ssh]
#       [--binary PATH] [--release] [--wait-secs N] [--settle-secs N]
#   bash scripts/dogfood-session.sh --dry-run [--duration-secs N] [--cycle-secs N] ...
#   bash scripts/dogfood-session.sh --print-systemd --repo-dir DIR --out-dir DIR [...]
#   bash scripts/dogfood-session.sh --print-cron --repo-dir DIR --out-dir DIR [...]

set -euo pipefail

OUT_DIR="recording/dogfood-session"
DURATION_SECS=14400
CYCLE_SECS=600
SOAK_WS="4"
APPS_CSV="shell,cargo,git,nvim,tmux,ssh"
BIN_OVERRIDE=""
RELEASE=0
WAIT_SECS=15
SETTLE_SECS=2
DRY_RUN=0
PRINT_SYSTEMD=0
PRINT_CRON=0
REPO_DIR_OVERRIDE=""
WRITE_SCHEDULE=""
CANON_APPS="shell cargo git nvim tmux ssh"
MAX_CYCLES=256

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
	--cycle-secs)
		CYCLE_SECS="${2:-}"
		shift 2
		;;
	--workspace)
		SOAK_WS="${2:-}"
		shift 2
		;;
	--apps)
		APPS_CSV="${2:-}"
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
		echo "  --out-dir DIR      evidence directory (default recording/dogfood-session)"
		echo "  --duration-secs N  session wall time 60..86400 (default 14400 = 4 h)"
		echo "  --cycle-secs N     app-rotation cadence 60..3600 (default 600 = 10 min)"
		echo "  --workspace ID     Hyprland workspace 1..10 (default 4)"
		echo "  --apps CSV         app subset of shell,cargo,git,nvim,tmux,ssh (default all)"
		echo "  --binary PATH      explicit bitty binary (default: target build)"
		echo "  --release          build and measure the release binary"
		echo "  --dry-run          print the cycle plan; launch nothing"
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
	echo "dogfood-session: $1" >&2
	exit 2
}

is_uint "$DURATION_SECS" || fail_usage "--duration-secs must be a positive integer"
is_uint "$CYCLE_SECS" || fail_usage "--cycle-secs must be a positive integer"
[ "$DURATION_SECS" -ge 60 ] && [ "$DURATION_SECS" -le 86400 ] || fail_usage "--duration-secs must be 60..86400"
[ "$CYCLE_SECS" -ge 60 ] && [ "$CYCLE_SECS" -le 3600 ] || fail_usage "--cycle-secs must be 60..3600"
case "$SOAK_WS" in
1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10) ;;
*) fail_usage "--workspace must be 1..10" ;;
esac

# Validate the app selection and canonicalize to drive order (unknown names
# fail closed; an empty selection means the full daily-driver set).
APPS_ORDERED=""
for app in $CANON_APPS; do
	case ",$APPS_CSV," in
	*",$app,"*) APPS_ORDERED="$APPS_ORDERED${APPS_ORDERED:+,}$app" ;;
	esac
done
if [ -z "$APPS_CSV" ]; then
	APPS_ORDERED="shell,cargo,git,nvim,tmux,ssh"
elif [ -z "$APPS_ORDERED" ]; then
	# Distinguish "empty means all" from "no known app": re-check whether
	# the caller named anything at all.
	trimmed="$(printf '%s' "$APPS_CSV" | tr -d ' ,')"
	[ -n "$trimmed" ] && fail_usage "--apps must name at least one of shell,cargo,git,nvim,tmux,ssh"
	APPS_ORDERED="shell,cargo,git,nvim,tmux,ssh"
fi
APPS_COUNT="$(printf '%s' "$APPS_ORDERED" | awk -F, '{ print NF }')"

# Bounded cycle plan (mirrors bitty_perf::dogfood_session::plan_cycles):
# cycle 0 fires at session start, then every effective cadence while
# offset <= duration; the count never exceeds 256 (cadence widens instead).
EFFECTIVE_CYCLE="$CYCLE_SECS"
CYCLES=$((DURATION_SECS / EFFECTIVE_CYCLE + 1))
if [ "$CYCLES" -gt "$MAX_CYCLES" ]; then
	EFFECTIVE_CYCLE=$(((DURATION_SECS + MAX_CYCLES - 1) / MAX_CYCLES))
	[ "$EFFECTIVE_CYCLE" -lt 1 ] && EFFECTIVE_CYCLE=1
	CYCLES=$((DURATION_SECS / EFFECTIVE_CYCLE + 1))
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [ "$PRINT_SYSTEMD" = "1" ] || [ "$PRINT_CRON" = "1" ]; then
	[ -n "$REPO_DIR_OVERRIDE" ] || fail_usage "--print-systemd/--print-cron need --repo-dir DIR"
	[ -n "$OUT_DIR" ] || fail_usage "--print-systemd/--print-cron need --out-dir DIR"
	RUN_LINE="$REPO_DIR_OVERRIDE/scripts/dogfood-session.sh --out-dir $OUT_DIR --duration-secs $DURATION_SECS --cycle-secs $CYCLE_SECS --workspace $SOAK_WS --apps $APPS_ORDERED"
	if [ "$PRINT_SYSTEMD" = "1" ]; then
		cat <<EOF
# bitty dogfood session (CTX-0643, PERF-10) — install with:
#   mkdir -p ~/.config/systemd/user
#   bash scripts/dogfood-session.sh --print-systemd --repo-dir <repo> --out-dir <dir> > ~/.config/systemd/user/bitty-dogfood-session.service
#   (split the service/timer blocks below into bitty-dogfood-session.service and bitty-dogfood-session.timer)
#   systemctl --user daemon-reload && systemctl --user enable --now bitty-dogfood-session.timer
[Unit]
Description=bitty daily-driver dogfood session evidence capture
After=graphical-session.target

[Service]
Type=oneshot
ExecStart=$RUN_LINE

[Install]
WantedBy=default.target
---
[Unit]
Description=bitty dogfood session schedule (daily)

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
EOF

	else
		cat <<EOF
# bitty dogfood session (CTX-0643, PERF-10) — install with: crontab -e
# Daily 03:00 session; output stays under the given directory.
0 3 * * * $RUN_LINE
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
record_command="scripts/dogfood-session.sh --duration-secs $DURATION_SECS --cycle-secs $CYCLE_SECS --workspace $SOAK_WS --apps $APPS_ORDERED"
if [ "$RELEASE" = "1" ]; then
	profile="release"
	CARGO_PROFILE_FLAG="--release"
	BITTY_PROFILE_DIR="release"
else
	profile="dev (debug)"
	CARGO_PROFILE_FLAG=""
	BITTY_PROFILE_DIR="debug"
fi

write_schedule_json() {
	dest="$1"
	jq -n \
		--arg task "CTX-0643" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg apps "$APPS_ORDERED" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson cycle "$CYCLE_SECS" \
		--argjson effective "$EFFECTIVE_CYCLE" \
		--argjson planned "$CYCLES" \
		'{
			schema_version: 1, task: $task, issues: [1064],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			session: {
				status: "scheduled", duration_secs: $duration,
				cycle_secs: $cycle, cycle_effective_secs: $effective,
				workspace: $ws, apps: ($apps | split(",")), planned_cycles: $planned
			}
		}' >"$dest"
}

if [ "$DRY_RUN" = "1" ]; then
	echo "dogfood-session plan: duration=${DURATION_SECS}s cycle=${CYCLE_SECS}s effective=${EFFECTIVE_CYCLE}s cycles=${CYCLES} workspace=${SOAK_WS} apps=${APPS_ORDERED}"
	if [ "$EFFECTIVE_CYCLE" != "$CYCLE_SECS" ]; then
		echo "dogfood-session plan: cadence widened to ${EFFECTIVE_CYCLE}s to respect MAX_CYCLES=256"
	fi
	echo "dogfood-session plan: cycle offsets: 0..${DURATION_SECS}s step ${EFFECTIVE_CYCLE}s"
	if [ -n "$WRITE_SCHEDULE" ]; then
		mkdir -p "$(dirname "$WRITE_SCHEDULE")"
		write_schedule_json "$WRITE_SCHEDULE"
		echo "dogfood-session plan: schedule -> $WRITE_SCHEDULE"
	fi
	exit 0
fi

for tool in hyprctl grim jq cargo; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "dogfood-session: UNAVAILABLE (capture leg missing tool: $tool)" >&2
		exit 2
	fi
done
if [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
	echo "dogfood-session: UNAVAILABLE (no Hyprland session: HYPRLAND_INSTANCE_SIGNATURE unset)" >&2
	exit 2
fi
if [ -z "${BITTY_PERF_DOGFOOD_SESSION:-}" ] || [ "$BITTY_PERF_DOGFOOD_SESSION" != "1" ]; then
	echo "dogfood-session: UNAVAILABLE (opt-in gate closed: set BITTY_PERF_DOGFOOD_SESSION=1)" >&2
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
if [ -n "$BIN_OVERRIDE" ]; then
	BIN="$BIN_OVERRIDE"
else
	BIN="$ROOT/target/$BITTY_PROFILE_DIR/bitty"
fi
LOG="$RUN_DIR/bitty.log"
SOCK="$RUN_DIR/bitty-dogfood.sock"

CHILD_PID=""
WIN_ADDR=""
PREV_WS="$(hyprctl activeworkspace -j | jq -r .id)"
echo "dogfood-session: prev_ws=$PREV_WS soak_ws=$SOAK_WS run_dir=$RUN_DIR cycles=$CYCLES apps=$APPS_ORDERED"

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

echo "dogfood-session: building bitty binary (crate bitty-app, profile $BITTY_PROFILE_DIR)"
# shellcheck disable=SC2086
cargo build -p bitty-app --locked --quiet $CARGO_PROFILE_FLAG
if [ ! -x "$BIN" ]; then
	echo "build produced no binary at $BIN" >&2
	exit 1
fi

echo "dogfood-session: launching $BIN"
BITTY_PERF_STARTUP_MARKER=1 BITTY_SOCKET="$SOCK" BITTY_INSTANCE_ID="dogfood-session-$STAMP" "$BIN" >"$LOG" 2>&1 &
CHILD_PID=$!
echo "dogfood-session: pid=$CHILD_PID log=$LOG"

echo "dogfood-session: waiting for window (class bitty, pid $CHILD_PID)"
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
echo "dogfood-session: window address=$WIN_ADDR"

hyprctl dispatch "hl.dsp.window.move({ workspace = \"$SOAK_WS\", window = \"address:$WIN_ADDR\" })"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$SOAK_WS\" })"

echo "dogfood-session: waiting for first frame"
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
# app driver and the grid-text snapshots. Best-effort: when the socket
# or scope is unavailable the pixel+RSS legs still produce evidence and
# every cycle records driver_ok=false.
TERM_ID=""
if TERM_JSON="$("$BIN" ctl --socket "$SOCK" terminal list --format json 2>/dev/null)"; then
	TERM_ID="$(printf '%s' "$TERM_JSON" | jq -r '.. | objects | .id? // empty' 2>/dev/null | grep -E '^(t:)?[0-9]+$' | head -n 1 || true)"
	[ -n "$TERM_ID" ] || TERM_ID="$(printf '%s' "$TERM_JSON" | jq -r '[.. | strings] | map(select(test("^t:[0-9]+$"))) | first // empty' 2>/dev/null || true)"
fi
echo "dogfood-session: terminal leg id=${TERM_ID:-unavailable}"

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

# Drive one daily-driver app with a fixed synthetic probe (bounded,
# no network, no side effects beyond the scratch shell).
drive_app() {
	app="$1"
	idx="$2"
	[ -n "$TERM_ID" ] || return 1
	case "$app" in
	shell) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "printf 'bitty-dogfood shell cycle $idx ok\\n'" >/dev/null 2>&1 ;;
	cargo) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "cargo --version 2>&1 | head -n 3; echo cargo-ok\\n" >/dev/null 2>&1 ;;
	git) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "git status --porcelain 2>&1 | head -n 5; echo git-ok\\n" >/dev/null 2>&1 ;;
	nvim) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "nvim --version 2>&1 | head -n 3; echo nvim-ok\\n" >/dev/null 2>&1 ;;
	tmux) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "tmux -V 2>&1 | head -n 3; echo tmux-ok\\n" >/dev/null 2>&1 ;;
	ssh) "$BIN" ctl --socket "$SOCK" terminal send "$TERM_ID" "ssh -o ConnectTimeout=1 -o StrictHostKeyChecking=no localhost 'echo ssh-ok' 2>&1 | head -n 5; echo ssh-done\\n" >/dev/null 2>&1 ;;
	*) return 1 ;;
	esac
}

drive_cycle() {
	idx="$1"
	driven=0
	# shellcheck disable=SC2086
	for app in $(printf '%s' "$APPS_ORDERED" | tr ',' ' '); do
		if drive_app "$app" "$idx" 2>/dev/null; then
			driven=$((driven + 1))
		fi
	done
	printf '%s' "$driven"
}

CSV="$RUN_DIR/cycles.csv"
printf 'index,at_secs,screenshot,rss_mb,grid_text_bytes,apps_driven,driver_ok\n' >"$CSV"
i=0
while [ "$i" -lt "$CYCLES" ]; do
	at_secs=$((i * EFFECTIVE_CYCLE))
	if [ "$i" -gt 0 ]; then
		sleep "$EFFECTIVE_CYCLE"
		if ! kill -0 "$CHILD_PID" 2>/dev/null; then
			echo "bitty exited mid-session at cycle $i; log tail:" >&2
			tail -n 30 "$LOG" >&2 || true
			break
		fi
	else
		if ! kill -0 "$CHILD_PID" 2>/dev/null; then
			echo "bitty exited before first cycle; log tail:" >&2
			tail -n 30 "$LOG" >&2 || true
			break
		fi
	fi
	shot="cycle-$(printf '%04d' "$i").png"
	if grim "$RUN_DIR/$shot" 2>/dev/null; then
		shot_ok=1
	else
		echo "dogfood-session: grim failed at cycle $i" >&2
		shot="missed-$(printf '%04d' "$i").png"
		shot_ok=0
	fi
	rss="$(read_rss_mb "$CHILD_PID")"
	grid_bytes=""
	if [ -n "$TERM_ID" ]; then
		grid_file="$RUN_DIR/cycle-$(printf '%04d' "$i").grid.json"
		if "$BIN" ctl --socket "$SOCK" terminal text "$TERM_ID" --format json >"$grid_file" 2>/dev/null; then
			grid_bytes="$(wc -c <"$grid_file" | tr -d ' ')"
		else
			rm -f "$grid_file"
		fi
	fi
	driven="$(drive_cycle "$i")"
	if [ "$driven" = "$APPS_COUNT" ] && [ "$shot_ok" = "1" ]; then
		driver_ok=1
	else
		driver_ok=0
	fi
	[ "$shot_ok" = "1" ] || driver_ok=0
	printf '%s,%s,%s,%s,%s,%s,%s\n' "$i" "$at_secs" "$shot" "$rss" "$grid_bytes" "$driven" "$driver_ok" >>"$CSV"
	echo "dogfood-session: cycle $i/$((CYCLES - 1)) at ${at_secs}s rss=${rss:-n/a}MB apps=$driven/$APPS_COUNT driver_ok=$driver_ok"
	i=$((i + 1))
done
completed="$(($(wc -l <"$CSV") - 1))"

# Evidence JSON: same shape as bitty_perf::dogfood_session::session_evidence_json —
# screenshot file names only, no output dir or binary path recorded.
EVIDENCE="$RUN_DIR/session-evidence.json"
if [ "$completed" -gt 0 ]; then
	rss_series="$(awk -F, 'NR>1 && $4 != "" { print $4 }' "$CSV")"
	rss_first="$(printf '%s' "$rss_series" | head -n 1)"
	rss_last="$(printf '%s' "$rss_series" | tail -n 1)"
	rss_max="$(printf '%s' "$rss_series" | sort -n | tail -n 1)"
	jq -Rn \
		--arg task "CTX-0643" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg apps "$APPS_ORDERED" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson cycle "$CYCLE_SECS" \
		--argjson effective "$EFFECTIVE_CYCLE" \
		--argjson completed "$completed" \
		--arg rss_first "$rss_first" \
		--arg rss_last "$rss_last" \
		--arg rss_max "$rss_max" \
		--rawfile csv "$CSV" \
		'($csv | split("\n") | map(select(length > 0)) | .[1:] | map(split(",")) | map({
			index: (.[0] | tonumber), at_secs: (.[1] | tonumber), screenshot: .[2],
			rss_mb: (if .[3] == "" then null else (.[3] | tonumber) end),
			grid_text_bytes: (if .[4] == "" then null else (.[4] | tonumber) end),
			apps_driven: (.[5] | tonumber), driver_ok: (.[6] == "1")
		})) as $cycles
		| {
			schema_version: 1, task: $task, issues: [1064],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			session: {
				status: "measured", duration_secs: $duration,
				cycle_secs: $cycle, cycle_effective_secs: $effective,
				workspace: $ws, apps: ($apps | split(",")), completed_cycles: $completed,
				rss_first_mb: ($rss_first | tonumber), rss_last_mb: ($rss_last | tonumber),
				rss_max_mb: ($rss_max | tonumber),
				rss_growth_pct: ((($rss_last | tonumber) - ($rss_first | tonumber)) / ($rss_first | tonumber) * 100),
				budget_mb: 250, cycles: $cycles
			}
		}' >"$EVIDENCE"
else
	jq -n \
		--arg task "CTX-0643" \
		--arg date "$capture_date" \
		--arg rev "$revision" \
		--arg cmd "$record_command" \
		--arg prof "$profile" \
		--arg os "$host_os" \
		--arg arch "$host_arch" \
		--arg toolchain "$host_toolchain" \
		--arg ws "$SOAK_WS" \
		--arg apps "$APPS_ORDERED" \
		--argjson cpus "$host_cpus" \
		--argjson mem "$host_mem_mb" \
		--argjson duration "$DURATION_SECS" \
		--argjson cycle "$CYCLE_SECS" \
		--argjson effective "$EFFECTIVE_CYCLE" \
		'{
			schema_version: 1, task: $task, issues: [1064],
			captured_at: $date, revision: $rev, command: $cmd, profile: $prof,
			budget_ref: "docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu",
			host_context: { os: $os, arch: $arch, toolchain: $toolchain, cpus: $cpus, total_memory_mb: $mem },
			session: {
				status: "unavailable", duration_secs: $duration,
				cycle_secs: $cycle, cycle_effective_secs: $effective,
				workspace: $ws, apps: ($apps | split(",")), completed_cycles: 0,
				rss_first_mb: null, rss_last_mb: null, rss_max_mb: null, budget_mb: 250,
				reason: "session window exited before the first cycle completed"
			}
		}' >"$EVIDENCE"
fi

echo "dogfood-session: closing window"
hyprctl dispatch "hl.dsp.window.close({ window = \"address:$WIN_ADDR\" })" || true
for _ in $(seq 1 25); do
	kill -0 "$CHILD_PID" 2>/dev/null || break
	sleep 0.2
done
WIN_ADDR=""
CHILD_PID=""

echo "dogfood-session: restoring workspace $PREV_WS"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })"
echo "dogfood-session: done completed=$completed evidence=$EVIDENCE"
