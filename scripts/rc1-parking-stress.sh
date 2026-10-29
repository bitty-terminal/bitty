#!/usr/bin/env bash
# RC-1 wall-budget parking stress harness (bitty#2781, bitty#2800).
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
# It then replays the capability-suite command shapes from bitty#2800 through
# `rc1_suite_parking_stress`, and finally runs the real runtime suites named in
# `RC1_STRESS_SUITES` under the same burners. `rc1_resume` stays in the default
# list because it deliberately pins the production 50 ms budget: its park
# assertions must keep passing under load. `plugin_budgets` is audited on the
# same production budgets but is not in the default load pass: its tests drain
# every park to a terminal instruction/wall decision, so load pressure would
# only add activation-window exposure without exercising its assertions.
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
#   RC1_SUITE_WORKERS      concurrent suite probes (default: 4)
#   RC1_SUITE_ITERATIONS   suite probe rounds per worker (default: 25)
#   RC1_STRESS_SUITE_THREADS
#                          nextest test threads for the real suite pass
#                          (default: 4, the CI quality-shard width)
#   RC1_STRESS_SUITES      space-separated test targets for the final pass
#                          (default: every runtime suite audited by bitty#2800)
#   RC1_STRESS_SKIP_SUITES set to `1` to run only the two probe harnesses
#   CARGO_TARGET_DIR       build cache (the caller owns cleanup)
#
# Usage: scripts/rc1-parking-stress.sh
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
burners=${RC1_STRESS_BURNERS:-$(nproc)}
workers=${RC1_STRESS_WORKERS:-4}
iterations=${RC1_STRESS_ITERATIONS:-50}
suite_workers=${RC1_SUITE_WORKERS:-4}
suite_iterations=${RC1_SUITE_ITERATIONS:-25}
suite_threads=${RC1_STRESS_SUITE_THREADS:-4}
suite_targets=${RC1_STRESS_SUITES:-"plugin_colorscheme plugin_services plugin_workspace plugin_debug plugin_config_manage plugin_ui overlay_capture_api plugin_runtime_loop plugin_settings_file plugin_timers plugin_package command_invoke plugin_runtime plugin_panel_create component_request_surface rc1_resume plugin_keymaps plugin_panel_pin trace_kinds_1828"}

suite_args=()
for target in $suite_targets; do
  suite_args+=(--test "$target")
done

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

"${scope[@]}" "${pin[@]}" env \
  RC1_SUITE_WORKERS="$suite_workers" RC1_SUITE_ITERATIONS="$suite_iterations" \
  cargo nextest run -p bitty-runtime --test rc1_suite_parking_stress \
  --run-ignored only -E 'test(suite_probes_survive_the_load)' --nocapture

if [[ "${RC1_STRESS_SKIP_SUITES:-0}" == "1" ]]; then
  echo "rc1-parking-stress: skipping the real suite pass (RC1_STRESS_SKIP_SUITES=1)"
  exit 0
fi

"${scope[@]}" "${pin[@]}" cargo nextest run -p bitty-runtime "${suite_args[@]}" -j "$suite_threads" --locked
