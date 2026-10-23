#!/usr/bin/env bash
# perf-disk.test.sh — #2561 regression guard for the pinned perf-suite disk use.
#
# Two layers, no network and no build:
#
#   1. Fixture checks for scripts/ci-disk-budget.sh (the breadcrumb command the
#      workflow runs around the bench step) against a synthetic repository
#      layout: usage errors, report shape, CARGO_TARGET_DIR resolution, and the
#      --min-free-mib failure contract.
#   2. Static checks on .github/workflows/ci.yml: every job that runs the pinned
#      bitty-perf suite must compile it into the workspace target dir (one
#      target tree per runner), bound its build jobs, keep the defensive
#      reclaim step, and bracket the bench step with disk breadcrumbs. A job
#      that regresses to a second `external/bitty-perf/target` tree fails here
#      instead of on a full hosted runner (`ld` SIGBUS, then ENOSPC).
set -euo pipefail

cd "$(dirname "$0")/../.."
scratch="${TMPDIR:-/tmp}/bitty"
mkdir -p "$scratch"
TMP="$(mktemp -d "$scratch/perf-disk-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
FAIL=0

fail() {
  echo "FAIL: $*" >&2
  FAIL=1
}

expect_exit() { # <exit> <label> <args...>
  local want="$1" label="$2"
  shift 2
  local status=0 out
  out="$("$@" 2>&1)" || status=$?
  if ((status != want)); then
    echo "FAIL: $label: expected exit $want, got $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
  LAST_OUT="$out"
}

# --- 1. ci-disk-budget.sh fixture checks ------------------------------------
fixture="$TMP/repo"
mkdir -p "$fixture/scripts" "$fixture/target/debug/incremental"
printf 'unit\n' >"$fixture/target/debug/incremental/unit"
cp scripts/ci-disk-budget.sh "$fixture/scripts/ci-disk-budget.sh"
chmod +x "$fixture/scripts/ci-disk-budget.sh"
budget="$fixture/scripts/ci-disk-budget.sh"

expect_exit 2 'missing --label' "$budget"
expect_exit 2 'unknown argument' "$budget" --label ok --bogus
expect_exit 2 'non-integer minimum' "$budget" --label ok --min-free-mib abc

expect_exit 0 'report' "$budget" --label 'fixture report'
grep -Fq '== disk budget: fixture report ==' <<<"$LAST_OUT" ||
  fail 'report: missing labelled breadcrumb'
grep -Fq 'Filesystem' <<<"$LAST_OUT" || fail 'report: missing df output'
grep -Eq "^  $fixture/target +[0-9]" <<<"$LAST_OUT" ||
  fail 'report: workspace target dir must be reported with a size'
grep -Eq "^  $fixture/external/bitty-perf/target +absent$" <<<"$LAST_OUT" ||
  fail 'report: a missing second tree must be reported as absent'
grep -Eq "^  $fixture/external/bitty-compat-lab/target +absent$" <<<"$LAST_OUT" ||
  fail 'report: a missing compat-lab tree must be reported as absent'

# CARGO_TARGET_DIR wins over the repository default, mirroring the workflow.
expect_exit 0 'target dir override' \
  env CARGO_TARGET_DIR="$TMP/elsewhere" "$budget" --label 'override'
grep -Fq "$TMP/elsewhere" <<<"$LAST_OUT" ||
  fail 'report: CARGO_TARGET_DIR override must be reported'

expect_exit 0 'sufficient budget' "$budget" --label ok --min-free-mib 1
expect_exit 1 'insufficient budget' "$budget" --label 'guard probe' --min-free-mib 1000000000
grep -Fq '::error::runner disk budget too small for guard probe' <<<"$LAST_OUT" ||
  fail 'guard: failure must carry a labelled ::error:: annotation'
grep -Fq '#2561' <<<"$LAST_OUT" || fail 'guard: failure must reference #2561'

# --- 2. Workflow invariants -------------------------------------------------
python3 - .github/workflows/ci.yml <<'PY' || FAIL=1
import re
import sys

path = sys.argv[1]
lines = open(path, encoding="utf-8").read().splitlines()

# Minimal indentation-aware reader: this workflow uses two-space job keys, a
# four-space job body, and six-space `- ` step items, so no YAML library is
# needed for the invariants below.
jobs = {}
job = None
in_jobs = False
step = None
for line in lines:
    if not in_jobs:
        if line.rstrip() == "jobs:":
            in_jobs = True
        continue
    if line and not line.startswith(" "):
        break  # end of the jobs mapping
    m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
    if m:
        job = m.group(1)
        jobs[job] = {"env": {}, "runs_on": "", "steps": []}
        step = None
        continue
    if job is None:
        continue
    m = re.match(r"^    runs-on:\s*(\S+)\s*$", line)
    if m:
        jobs[job]["runs_on"] = m.group(1)
        continue
    if line.startswith("      - "):
        step = [line]
        jobs[job]["steps"].append(step)
        continue
    if step is not None:
        step.append(line)
        continue
    m = re.match(r"^      ([A-Za-z0-9_-]+):\s*(.*)$", line)
    if m:
        jobs[job]["env"][m.group(1)] = m.group(2).strip()

def text(lines_):
    return "\n".join(lines_)


def scalar(value):
    # The workflow quotes a few env values (`"0"`); compare the decoded form.
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


problems = []
perf_jobs = []
perf_workdir = re.compile(r"working-directory:\s*external/bitty-perf\b")


def runs_pinned_bench(step_lines):
    # The quality job also has a parser-throughput test step in the same
    # checkout; only the bench build is the disk-heavy one this guard covers.
    step_text = text(step_lines)
    return bool(perf_workdir.search(step_text)) and (
        "cargo test --benches" in step_text or "cargo bench" in step_text
    )


for name, data in jobs.items():
    bench = [i for i, s in enumerate(data["steps"]) if runs_pinned_bench(s)]
    if not bench:
        continue
    perf_jobs.append(name)

    target = scalar(data["env"].get("CARGO_TARGET_DIR", ""))
    if target != "${{ github.workspace }}/target":
        problems.append(
            f"{name}: job-level CARGO_TARGET_DIR must be "
            f"'${{{{ github.workspace }}}}/target' (found {target!r}); an unset or relative "
            "target dir gives external/bitty-perf its own full debug tree (#2561)"
        )
    if data["runs_on"].startswith("ubuntu"):
        if scalar(data["env"].get("CARGO_INCREMENTAL", "")) != "0":
            problems.append(f'{name}: ubuntu perf job must set CARGO_INCREMENTAL: "0"')

    for index in bench:
        step_text = text(data["steps"][index])
        if "CARGO_TARGET_DIR" in step_text:
            problems.append(f"{name}: bench step must inherit CARGO_TARGET_DIR, not override it")
        if "CARGO_BUILD_JOBS" not in step_text and "CARGO_BUILD_JOBS" not in data["env"]:
            problems.append(f"{name}: bench step must bound CARGO_BUILD_JOBS")
        before = [s for s in data["steps"][:index] if "ci-disk-budget.sh" in text(s)]
        after = [s for s in data["steps"][index + 1:] if "ci-disk-budget.sh" in text(s)]
        if not before:
            problems.append(f"{name}: bench step needs a ci-disk-budget.sh step before it")
        if not any("if: always()" in text(s) for s in after):
            problems.append(
                f"{name}: bench step needs an `if: always()` ci-disk-budget.sh step after it"
            )

    if not any("rm -rf external/bitty-perf/target" in text(s) for s in data["steps"]):
        problems.append(f"{name}: keep the defensive `rm -rf external/bitty-perf/target` step")

expected = {"quality", "linux-x11", "linux-wayland", "macos"}
missing = expected - set(perf_jobs)
if missing:
    problems.append(f"jobs running the pinned perf suite shrank, missing: {sorted(missing)}")

for problem in problems:
    print(f"FAIL: {problem}", file=sys.stderr)

if problems:
    sys.exit(1)
print(f"perf-disk: workflow ok for jobs: {', '.join(sorted(perf_jobs))}")
PY

# No job may point the build at the external checkout again, at any level.
if grep -Eq 'CARGO_TARGET_DIR:.*external/' .github/workflows/ci.yml; then
  fail 'workflow: CARGO_TARGET_DIR must never point inside external/ (#2561)'
fi

if ((FAIL != 0)); then
  exit 1
fi
echo "perf-disk: ok"
