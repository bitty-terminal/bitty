#!/usr/bin/env bash
# fuzz-campaign.test.sh — CTX-1877 campaign driver regression tests.
#
# The driver's only heavyweight dependency is a nightly cargo-fuzz run, so the
# tests stub `cargo` and exercise everything the evidence depends on: the plan
# and budget clamp, libFuzzer stat parsing, artifact classification (crash /
# OOM / timeout / stale), the "clean" exit path, the environment-error path,
# and the wiring that keeps the campaign out of the PR-required checks.
set -euo pipefail

cd "$(dirname "$0")/../.."
scratch="${TMPDIR:-/tmp}/bitty"
mkdir -p "$scratch"
TMP="$(mktemp -d "$scratch/fuzz-campaign-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
FAIL=0

fail() {
  echo "FAIL: $*" >&2
  FAIL=1
}

expect_eq() { # description expected actual
  if [ "$2" != "$3" ]; then
    fail "$1: expected '$2', got '$3'"
  fi
}

mkdir -p "$TMP/bin" "$TMP/artifacts/vt_parser"

cat >"$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_FUZZ_CALLS:-/dev/null}"
case " $* " in
*" fuzz run "*)
  case "${FAKE_FUZZ_MODE:-clean}" in
  clean)
    cat <<'LOG'
Done 1234 runs in 7 second(s)
stat::number_of_executed_units: 1234
stat::average_exec_per_sec: 176
stat::new_units_added: 12
stat::slowest_unit_time_sec: 0
stat::peak_rss_mb: 321
LOG
    exit 0
    ;;
  crash | oom | timeout)
    case "$FAKE_FUZZ_MODE" in
    crash) : >"$FAKE_FUZZ_ARTIFACTS/crash-deadbeef" ;;
    oom) : >"$FAKE_FUZZ_ARTIFACTS/oom-deadbeef" ;;
    timeout) : >"$FAKE_FUZZ_ARTIFACTS/timeout-deadbeef" ;;
    esac
    printf 'Done 99 runs in 3 second(s)\nstat::number_of_executed_units: 99\nstat::peak_rss_mb: 200\n'
    echo "==ERROR: libFuzzer: $FAKE_FUZZ_MODE" >&2
    exit 77
    ;;
  error)
    echo 'error: failed to build fuzz target (stub)' >&2
    exit 101
    ;;
  esac
  ;;
*"fuzz --version"*)
  echo 'cargo-fuzz 0.13.2'
  exit 0
  ;;
*"--version"*)
  echo 'cargo 1.100.0-nightly (stub)'
  exit 0
  ;;
esac
echo "stub cargo: unexpected invocation: $*" >&2
exit 2
STUB
chmod +x "$TMP/bin/cargo"

run_campaign() { # mode out-dir [extra args]
  local mode="$1" out="$2"
  shift 2
  FAKE_FUZZ_MODE="$mode" \
    FAKE_FUZZ_ARTIFACTS="$TMP/artifacts/vt_parser" \
    FAKE_FUZZ_CALLS="$TMP/cargo-calls" \
    CARGO="$TMP/bin/cargo" \
    ./scripts/fuzz-campaign.sh --secs 10 --targets vt_parser \
    --out-dir "$out" --corpus-dir "$TMP/corpus" --artifacts-dir "$TMP/artifacts" \
    "$@" >"$TMP/last.log" 2>&1
}

run_rc=0
run_fails() { # expected-rc mode out-dir [extra args]
  local expected="$1"
  shift
  run_rc=0
  run_campaign "$@" || run_rc=$?
  expect_eq "exit code for ${*:-default}" "$expected" "$run_rc"
}

# --- plan: every manifest target is enumerated, defaults resolve, no toolchain.
rm -f "$TMP/cargo-calls"
if ! CARGO="$TMP/bin/cargo" FAKE_FUZZ_CALLS="$TMP/cargo-calls" \
  ./scripts/fuzz-campaign.sh --dry-run >"$TMP/plan.log" 2>&1; then
  fail 'dry-run must exit 0 without a toolchain'
fi
for target in $(./scripts/fuzz-campaign.sh --list-targets); do
  grep -Fq "$target" "$TMP/plan.log" || fail "dry-run plan must list target $target"
done
grep -Fq 'secs: 600' "$TMP/plan.log" || fail 'dry-run must show the default 600 s budget'
if [ -e "$TMP/cargo-calls" ]; then
  fail 'dry-run must not invoke cargo'
fi
if [ -e "$TMP/out" ]; then
  fail 'dry-run must not create the evidence directory'
fi

expect_eq 'pinned cargo-fuzz version' '0.13.2' "$(./scripts/fuzz-campaign.sh --pinned-cargo-fuzz)"

# --- budget bounds
clamped="$(./scripts/fuzz-campaign.sh --dry-run --secs 1 2>&1 | awk '$1 == "secs:" { print $2 }')"
expect_eq 'sub-minimum budget clamps up' '10' "$clamped"
clamped="$(./scripts/fuzz-campaign.sh --dry-run --secs 999999 2>&1 | awk '$1 == "secs:" { print $2 }')"
expect_eq 'over-maximum budget clamps down' '86400' "$clamped"
run_fails 2 plan "$TMP/out" --targets not-a-target
grep -Fq 'unknown fuzz target' "$TMP/last.log" || fail 'unknown target must name the defect'
run_fails 2 plan "$TMP/out" --secs twelve
grep -Fq -- '--secs must be an integer' "$TMP/last.log" || fail 'non-numeric budget must name the defect'

# --- clean campaign: stat parsing, evidence schema, exit 0
rm -rf "$TMP/out-clean"
run_fails 0 clean "$TMP/out-clean"
grep -Fq -- '-max_total_time=10' "$TMP/cargo-calls" || fail 'driver must pass the clamped budget to libFuzzer'
grep -Fq -- '-timeout=25' "$TMP/cargo-calls" || fail 'driver must pass the pinned libFuzzer timeout'
expect_eq 'clean campaign result' 'clean' "$(jq -r '.result' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign runs' '1234' "$(jq -r '.targets[0].runs' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign fuzzed seconds' '7' "$(jq -r '.targets[0].secs_fuzzed' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign peak RSS' '321' "$(jq -r '.targets[0].peak_rss_mb' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign crashes' '0' "$(jq -r '.totals.crashes' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign targets' '1' "$(jq -r '.totals.targets' "$TMP/out-clean/summary.json")"
grep -Fq '| **total** |' "$TMP/out-clean/summary.md" || fail 'markdown summary must carry a totals row'
expect_eq 'clean campaign records one row' '1' "$(wc -l <"$TMP/out-clean/targets.tsv" | tr -d ' ')"
if grep -Eq '/(home|Users|mnt|tmp)/' "$TMP/out-clean/summary.json"; then
  fail 'summary.json must not embed host paths'
fi

# --- stale artifacts from an earlier run are not counted as new findings
: >"$TMP/artifacts/vt_parser/crash-stale"
rm -rf "$TMP/out-stale"
run_fails 0 clean "$TMP/out-stale"
expect_eq 'stale artifact ignored' '0' "$(jq -r '.targets[0].crashes' "$TMP/out-stale/summary.json")"
rm -f "$TMP/artifacts/vt_parser/crash-stale"

# --- each finding class fails the campaign and is named in the evidence
for mode in crash oom timeout; do
  case "$mode" in
  crash) field=crashes ;;
  oom) field=oom ;;
  timeout) field=timeouts ;;
  esac
  rm -rf "$TMP/out-$mode"
  run_fails 1 "$mode" "$TMP/out-$mode"
  expect_eq "$mode campaign result" 'finding' "$(jq -r '.result' "$TMP/out-$mode/summary.json")"
  expect_eq "$mode campaign exit_code field" '77' "$(jq -r '.targets[0].exit_code' "$TMP/out-$mode/summary.json")"
  expect_eq "$mode artifact count" "1" "$(jq -r ".targets[0].$field" "$TMP/out-$mode/summary.json")"
  expect_eq "$mode artifact name" "$mode-deadbeef" "$(jq -r '.targets[0].artifacts[0]' "$TMP/out-$mode/summary.json")"
  if [ ! -f "$TMP/out-$mode/artifacts/vt_parser/$mode-deadbeef" ]; then
    fail "$mode artifact must be copied into the evidence directory"
  fi
  grep -Fq '## Findings' "$TMP/out-$mode/summary.md" || fail "$mode campaign must list findings in summary.md"
  rm -f "$TMP/artifacts/vt_parser/$mode-deadbeef"
done

# --- build/runner failure without an artifact is an environment error
rm -rf "$TMP/out-error"
run_fails 3 error "$TMP/out-error"
expect_eq 'error campaign result' 'error' "$(jq -r '.result' "$TMP/out-error/summary.json")"
expect_eq 'error campaign target result' 'error' "$(jq -r '.targets[0].result' "$TMP/out-error/summary.json")"

# --- wiring guards: nightly-only campaign, wired recipe, no PR-required gate
grep -Fq 'fuzz-campaign' justfile || fail 'justfile must expose the fuzz-campaign recipe'
grep -Fq 'fuzz-campaign-test' justfile || fail 'justfile must expose the driver test recipe'
if grep -Fq 'fuzz-campaign' .github/workflows/ci.yml; then
  fail 'ci.yml must not run the fuzz campaign (it is not a PR gate)'
fi
if [ ! -f .github/workflows/fuzz-campaign.yml ]; then
  fail 'the nightly fuzz-campaign workflow must exist'
fi

if [ "$FAIL" -ne 0 ]; then
  echo 'fuzz-campaign.test.sh: FAIL' >&2
  exit 1
fi
echo 'fuzz-campaign.test.sh: PASS'
