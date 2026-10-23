#!/usr/bin/env bash
# ci-disk-budget.sh — report the runner disk budget around validation-suite builds.
#
# #2561: the pinned bitty-perf suite is a full product graph. While it built in
# its own `external/bitty-perf/target`, one runner held two complete debug
# trees; the hosted disk filled during the bench link and `ld` died with
# `signal 7 [Bus error]`, after which the job failed writing runner diagnostics
# with `No space left on device`. The suite now compiles into the workspace
# target dir, and this command prints the same budget fingerprint before and
# after the bench step, so a recurrence names its cause in the job log instead
# of surfacing as a linker crash.
#
# Usage:
#   ci-disk-budget.sh --label <text> [--min-free-mib <N>]
#
# Every path is derived, never configured per host: the repository root comes
# from this script's location, the build directory from CARGO_TARGET_DIR
# (default `<root>/target`), and the second-tree locations the fix removes are
# always reported so their reappearance is visible.
#
# Exit codes: 0 budget reported (and sufficient), 1 below --min-free-mib,
# 2 usage error.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
usage: ci-disk-budget.sh --label <text> [--min-free-mib <N>]

  --label <text>        describe where the budget is measured (shown in the
                        log and in the failure annotation)
  --min-free-mib <N>    fail with an ::error:: annotation when less than <N>
                        MiB is available on the build filesystem
EOF
}

label=""
min_free_mib=""
while (($# > 0)); do
  case "$1" in
  --label)
    (($# >= 2)) || {
      echo "ci-disk-budget: --label requires a value" >&2
      exit 2
    }
    label="$2"
    shift 2
    ;;
  --min-free-mib)
    (($# >= 2)) || {
      echo "ci-disk-budget: --min-free-mib requires a value" >&2
      exit 2
    }
    min_free_mib="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "ci-disk-budget: unknown argument: $1" >&2
    usage >&2
    exit 2
    ;;
  esac
done
[[ -n "$label" ]] || {
  echo "ci-disk-budget: --label is required" >&2
  exit 2
}
if [[ -n "$min_free_mib" && ! "$min_free_mib" =~ ^[0-9]+$ ]]; then
  echo "ci-disk-budget: --min-free-mib must be a non-negative integer, got '$min_free_mib'" >&2
  exit 2
fi

report_path() { # <path>
  local path="$1"
  if [[ -e "$path" ]]; then
    printf '  %-56s %s\n' "$path" "$(du -sh "$path" 2>/dev/null | awk '{print $1}')"
  else
    printf '  %-56s %s\n' "$path" "absent"
  fi
}

avail_kb="$(df -Pk "$root" | awk 'NR==2 {print $4}')"
avail_mib=$((avail_kb / 1024))

printf '== disk budget: %s ==\n' "$label"
printf '  workspace: %s\n' "$root"
df -h "$root"
printf '  build directories:\n'
report_path "$root/target"
target_dir="${CARGO_TARGET_DIR:-$root/target}"
[[ "$target_dir" == "$root/target" ]] || report_path "$target_dir"
report_path "$root/target/debug/incremental"
report_path "$root/external/bitty-perf/target"
report_path "$root/external/bitty-compat-lab/target"
printf '  available: %s MiB\n' "$avail_mib"

if [[ -n "$min_free_mib" ]] && ((avail_mib < min_free_mib)); then
  # Fail with a readable infrastructure signal instead of letting the linker
  # die on an mmap write, the same contract as the #2448 compat budget gate.
  printf '::error::runner disk budget too small for %s: %s MiB free, %s MiB required (#2561)\n' \
    "$label" "$avail_mib" "$min_free_mib"
  exit 1
fi
