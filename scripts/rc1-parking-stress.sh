#!/usr/bin/env bash
# RC-1 wall-budget parking stress harness (bitty#2781).
#
# Runs the ignored `rc1_wall_parking_stress` load harness with CPU pressure for
# the duration and prints the measured parking rate plus the clean/parked
# wall-time distributions.
#
# Two modes run back to back:
#   raw      - production budgets, one dispatch: fails on any park or host-call
#              timeout (the pre-bitty#2781 probe behaviour);
#   settled  - documented cold-tick resume plus the harness host-call deadline:
#              must pass under the same load (the fix).
#
# Environment:
#   RC1_STRESS_CPUS        CPU list to pin burners and the harness to, so a
#                          wide host can emulate a small CI runner (`0-3`);
#                          default: every core
#   RC1_STRESS_CPU_QUOTA   cgroup CPU quota for the harness (for example
#                          `20%`), which reproduces the period-long stalls a
#                          busy shared runner imposes; requires systemd-run
#   RC1_STRESS_BURNERS     CPU burners to run (default: nproc)
#   RC1_STRESS_WORKERS     concurrent probe runtimes (default: 4, the CI
#                          quality-shard test width on the standard runner)
#   RC1_STRESS_ITERATIONS  dispatches per worker (default: 50)
#   CARGO_TARGET_DIR       build cache (the caller owns cleanup)
#
# Usage: scripts/rc1-parking-stress.sh
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
burners=${RC1_STRESS_BURNERS:-$(nproc)}
workers=${RC1_STRESS_WORKERS:-4}
iterations=${RC1_STRESS_ITERATIONS:-50}

pin=()
if [[ -n "${RC1_STRESS_CPUS:-}" ]]; then
  pin=(taskset -c "$RC1_STRESS_CPUS")
fi

scope=()
if [[ -n "${RC1_STRESS_CPU_QUOTA:-}" ]]; then
  scope=(
    systemd-run --user --scope --quiet
    --property "CPUQuota=$RC1_STRESS_CPU_QUOTA"
    --property "AllowedCPUs=${RC1_STRESS_CPUS:-$(seq -s, 0 "$(($(nproc) - 1))")}"
  )
fi

burner_pids=()
stop_burners() {
  local pid
  for pid in "${burner_pids[@]:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
}
trap stop_burners EXIT

for _ in $(seq 1 "$burners"); do
  "${pin[@]}" yes >/dev/null &
  burner_pids+=("$!")
done
echo "rc1-parking-stress: cpus=${RC1_STRESS_CPUS:-all} quota=${RC1_STRESS_CPU_QUOTA:-none} burners=$burners workers=$workers iterations=$iterations"

cd "$repo_root"
"${scope[@]}" "${pin[@]}" env \
  RC1_STRESS_WORKERS="$workers" RC1_STRESS_ITERATIONS="$iterations" \
  cargo nextest run -p bitty-runtime --test rc1_wall_parking_stress \
  --run-ignored only -E 'test(load_harness_)' --nocapture
