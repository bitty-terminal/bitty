#!/usr/bin/env bash
# visual-smoke.sh — Hyprland workspace-4 visual smoke for render/UI slices.
# Codifies the DEC-0005 loop so every render task shares one workflow instead
# of re-deriving hyprctl Lua syntax: build bitty-app, launch on workspace 4,
# wait for first frames, grim screenshot to a given path, print the window
# class/title, close the window, restore the previously focused workspace.
# The window is never stranded: an EXIT trap closes it and restores focus.
# Usage: bash scripts/visual-smoke.sh --out PATH [--wait-secs N] [--settle-secs N]

set -euo pipefail

OUT=""
WAIT_SECS=15
SETTLE_SECS=2
TARGET_WS="4"

while [ $# -gt 0 ]; do
	case "$1" in
	--out)
		OUT="${2:-}"
		shift 2
		;;
	--wait-secs)
		WAIT_SECS="${2:-15}"
		shift 2
		;;
	--settle-secs)
		SETTLE_SECS="${2:-2}"
		shift 2
		;;
	--help | -h)
		echo "Usage: $0 --out PATH [--wait-secs N] [--settle-secs N]"
		echo "  --out PATH       grim screenshot destination (required)"
		echo "  --wait-secs N    max wait for window + first frame (default 15)"
		echo "  --settle-secs N  extra settle after first frame (default 2)"
		exit 0
		;;
	*)
		echo "unknown flag $1" >&2
		exit 2
		;;
	esac
done

if [ -z "$OUT" ]; then
	echo "missing required --out PATH" >&2
	exit 2
fi

for tool in hyprctl grim jq cargo; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "missing required tool: $tool" >&2
		exit 2
	fi
done

export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/1000}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
BIN="$ROOT/target/debug/bitty-app"
LOG="$(mktemp /tmp/bitty-visual-smoke.XXXXXX.log)"

CHILD_PID=""
WIN_ADDR=""
PREV_WS="$(hyprctl activeworkspace -j | jq -r .id)"
echo "visual-smoke: prev_ws=$PREV_WS target_ws=$TARGET_WS out=$OUT"

cleanup() {
	# Never strand a window; always restore the previously focused workspace.
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

echo "visual-smoke: building bitty-app"
cargo build -p bitty-app --locked --quiet
if [ ! -x "$BIN" ]; then
	echo "build produced no binary at $BIN" >&2
	exit 1
fi

echo "visual-smoke: launching $BIN"
"$BIN" >"$LOG" 2>&1 &
CHILD_PID=$!
echo "visual-smoke: pid=$CHILD_PID log=$LOG"

echo "visual-smoke: waiting for window (class bitty, pid $CHILD_PID)"
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
echo "visual-smoke: window address=$WIN_ADDR"

hyprctl dispatch "hl.dsp.window.move({ workspace = \"$TARGET_WS\", window = \"address:$WIN_ADDR\" })"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$TARGET_WS\" })"

echo "visual-smoke: waiting for first frame"
FRAMES=0
for _ in $(seq 1 $((WAIT_SECS * 5))); do
	if grep -q "tick: frame=" "$LOG" 2>/dev/null; then
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

mkdir -p "$(dirname "$OUT")"
grim "$OUT"
echo "visual-smoke: screenshot -> $OUT"

echo "visual-smoke: window identity"
hyprctl clients -j | jq -r --arg a "$WIN_ADDR" '.[] | select(.address == $a) | "class: \(.class)\ntitle: \(.title)\nsize: \(.size[0])x\(.size[1]) ws: \(.workspace.id)"'

echo "visual-smoke: gpu/surface lines from log"
grep -E "gpu attached|window created|tick: frame=1 " "$LOG" | head -n 5 || true

echo "visual-smoke: closing window"
hyprctl dispatch "hl.dsp.window.close({ window = \"address:$WIN_ADDR\" })" || true
for _ in $(seq 1 25); do
	kill -0 "$CHILD_PID" 2>/dev/null || break
	sleep 0.2
done
WIN_ADDR=""
CHILD_PID=""

echo "visual-smoke: restoring workspace $PREV_WS"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })"
echo "visual-smoke: done"
