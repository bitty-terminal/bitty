#!/usr/bin/env bash
# RC-1 wall-budget parking stress harness (bitty#2781, bitty#2800).
#
# Runs the ignored `rc1_wall_parking_stress` load harness with CPU pressure for
# the duration and prints the measured parking rate plus the clean/parked
# wall-time distributions.
#
# Contract (bitty#2797): the two probe modes are separate nextest invocations
# with opposite expectations, because nextest runs whatever a filter selects
# concurrently and the raw probe is supposed to fail:
#   raw      - production budgets, one dispatch: expected failure. The
#              pre-bitty#2781 probe behaviour is to park (or hit a host-call
#              timeout) under the load, so the run must exit non-zero *and*
#              report a park; the script asserts that and keeps going instead
#              of letting the non-zero status abort it;
#   settled  - documented cold-tick resume plus the harness host-call deadline:
#              must pass under the same load (the fix).
# The script exits 0 only when the raw probe failed with a park AND the settled
# probe passed. A raw pass, a raw failure without a park, or a settled failure
# is a contract violation and exits non-zero.
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

# Contract 1 (bitty#2797): the raw probe is the reproduction, so it must fail
# with a park. Capture the status and output, assert both, and never let the
# expected non-zero status abort the script.
set +e
raw_output=$("${scope[@]}" "${pin[@]}" env \
  RC1_STRESS_WORKERS="$workers" RC1_STRESS_ITERATIONS="$iterations" \
  cargo nextest run -p bitty-runtime --test rc1_wall_parking_stress \
  --run-ignored only -E 'test(=load_harness_raw_dispatch_fails_on_any_park)' \
  --nocapture 2>&1)
raw_status=$?
set -e
printf '%s\n' "$raw_output"

if [[ "$raw_status" -eq 0 ]]; then
  echo "rc1-parking-stress: contract violation: raw probe passed; it must fail on a park" >&2
  exit 1
fi
if [[ "$raw_output" != *"Parked at the RC-1 wall budget"* ]]; then
  echo "rc1-parking-stress: contract violation: raw probe failed (status $raw_status) without a park" >&2
  exit 1
fi
echo "rc1-parking-stress: raw probe failed with a park as designed (status $raw_status)"

# Contract 2 (bitty#2797): the settled probe must pass under the same load.
if ! "${scope[@]}" "${pin[@]}" env \
  RC1_STRESS_WORKERS="$workers" RC1_STRESS_ITERATIONS="$iterations" \
  cargo nextest run -p bitty-runtime --test rc1_wall_parking_stress \
  --run-ignored only -E 'test(=load_harness_settled_dispatch_survives_the_load)' \
  --nocapture; then
  echo "rc1-parking-stress: contract violation: settled probe failed" >&2
  exit 1
fi

"${scope[@]}" "${pin[@]}" env \
  RC1_SUITE_WORKERS="$suite_workers" RC1_SUITE_ITERATIONS="$suite_iterations" \
  cargo nextest run -p bitty-runtime --test rc1_suite_parking_stress \
  --run-ignored only -E 'test(suite_probes_survive_the_load)' --nocapture

if [[ "${RC1_STRESS_SKIP_SUITES:-0}" == "1" ]]; then
  echo "rc1-parking-stress: skipping the real suite pass (RC1_STRESS_SKIP_SUITES=1)"
  exit 0
fi

"${scope[@]}" "${pin[@]}" cargo nextest run -p bitty-runtime "${suite_args[@]}" -j "$suite_threads" --locked
