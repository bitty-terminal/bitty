#!/usr/bin/env bash
# fuzz-campaign.test.sh — CTX-1877 campaign driver regression tests.
#
# The driver's only heavyweight dependency is a nightly cargo-fuzz run, so the
# tests stub `cargo` and exercise everything the evidence depends on: the plan
# and budget clamp, libFuzzer stat parsing, artifact classification (crash /
# leak / slow unit / OOM / timeout / stale / unclassified), the execution floor,
# the signal trap, the evidence schema, the cap `-max_len` derivation, and the
# wiring that keeps the campaign itself out of the PR-required checks while the
# driver test runs there.
set -euo pipefail

cd "$(dirname "$0")/../.."
scratch="${TMPDIR:-/tmp}/bitty"
mkdir -p "$scratch"
TMP="$(mktemp -d "$scratch/fuzz-campaign-test.XXXXXX")"
DRIVER_PID=""
CHILD_PID=""
cleanup() {
  if [ -n "$CHILD_PID" ] && kill -0 "$CHILD_PID" 2>/dev/null; then
    kill -KILL "$CHILD_PID" 2>/dev/null || true
  fi
  if [ -n "$DRIVER_PID" ] && kill -0 "$DRIVER_PID" 2>/dev/null; then
    kill -KILL "$DRIVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
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

# The stub mirrors cargo-fuzz 0.13.2: the artifact destination comes from the
# `-artifact_prefix=` the driver forwards, never from the test environment.
cat >"$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_FUZZ_CALLS:-/dev/null}"

artifact_prefix=""
for arg in "$@"; do
  case "$arg" in
  -artifact_prefix=*) artifact_prefix="${arg#-artifact_prefix=}" ;;
  esac
done

write_artifact() { # name
  if [[ -z "$artifact_prefix" ]]; then
    echo 'stub cargo: the driver did not forward -artifact_prefix' >&2
    exit 3
  fi
  mkdir -p "$artifact_prefix"
  : >"${artifact_prefix}$1"
}

stats() {
  cat <<'LOG'
Done 1234 runs in 7 second(s)
stat::number_of_executed_units: 1234
stat::average_exec_per_sec: 176
stat::new_units_added: 12
stat::slowest_unit_time_sec: 0
stat::peak_rss_mb: 321
LOG
}

case " $* " in
*" fuzz run "*)
  case "${FAKE_FUZZ_MODE:-clean}" in
  clean)
    stats
    exit 0
    ;;
  crash)
    write_artifact crash-deadbeef
    stats
    echo '==ERROR: libFuzzer: deadly signal' >&2
    exit 77
    ;;
  leak)
    write_artifact leak-deadbeef
    stats
    exit 0
    ;;
  slowunit)
    write_artifact slow-unit-deadbeef
    stats
    exit 0
    ;;
  oom)
    write_artifact oom-deadbeef
    stats
    exit 77
    ;;
  timeout)
    write_artifact timeout-deadbeef
    stats
    exit 77
    ;;
  unclassified)
    write_artifact mystery-deadbeef
    stats
    exit 0
    ;;
  silent)
    # Exit 0 with no stats and no artifact: zero executions.
    exit 0
    ;;
  abort)
    # libFuzzer's default error exit (77) with no artifact on disk.
    echo '==ERROR: libFuzzer: deadly signal' >&2
    exit 77
    ;;
  error)
    echo 'error: failed to build fuzz target (stub)' >&2
    exit 101
    ;;
  hang)
    printf '%s\n' "$$" >"${FAKE_FUZZ_CHILD_PID:?}"
    exec sleep 120
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

expect_eq 'pinned cargo-fuzz version' '0.13.2' "$(./scripts/fuzz-campaign.sh --pinned-cargo-fuzz)"

# --- budget bounds and usage errors
clamped="$(./scripts/fuzz-campaign.sh --dry-run --secs 1 2>&1 | awk '$1 == "secs:" { print $2 }')"
expect_eq 'sub-minimum budget clamps up' '10' "$clamped"
clamped="$(./scripts/fuzz-campaign.sh --dry-run --secs 999999 2>&1 | awk '$1 == "secs:" { print $2 }')"
expect_eq 'over-maximum budget clamps down' '86400' "$clamped"
run_fails 2 plan "$TMP/out" --targets not-a-target
grep -Fq 'unknown fuzz target' "$TMP/last.log" || fail 'unknown target must name the defect'
run_fails 2 plan "$TMP/out" --secs twelve
grep -Fq -- '--secs must be an integer' "$TMP/last.log" || fail 'non-numeric budget must name the defect'
run_fails 2 plan "$TMP/out" --secs
grep -Fq -- '--secs requires a value' "$TMP/last.log" || fail 'a valueless flag must be a usage error'
run_fails 2 plan "$TMP/out" --out-dir
grep -Fq -- '--out-dir requires a value' "$TMP/last.log" || fail 'a valueless --out-dir must be a usage error'

# --- clean campaign: stat parsing, evidence schema, exit 0
rm -rf "$TMP/out-clean"
run_fails 0 clean "$TMP/out-clean"
grep -Fq -- '-max_total_time=10' "$TMP/cargo-calls" || fail 'driver must pass the clamped budget to libFuzzer'
grep -Fq -- '-timeout=25' "$TMP/cargo-calls" || fail 'driver must pass the pinned libFuzzer timeout'
grep -Fq -- "-artifact_prefix=$TMP/artifacts/vt_parser/" "$TMP/cargo-calls" ||
  fail 'driver must forward --artifacts-dir as -artifact_prefix'
expect_eq 'clean campaign result' 'clean' "$(jq -r '.result' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign runs' '1234' "$(jq -r '.targets[0].runs' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign fuzzed seconds' '7' "$(jq -r '.targets[0].secs_fuzzed' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign peak RSS' '321' "$(jq -r '.targets[0].peak_rss_mb' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign crashes' '0' "$(jq -r '.totals.crashes' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign leaks' '0' "$(jq -r '.totals.leaks' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign slow units' '0' "$(jq -r '.totals.slow_units' "$TMP/out-clean/summary.json")"
expect_eq 'clean campaign targets' '1' "$(jq -r '.totals.targets' "$TMP/out-clean/summary.json")"
case "$(jq -r '.revision_dirty' "$TMP/out-clean/summary.json")" in
true | false) ;;
*) fail 'schema revision_dirty must be a JSON boolean' ;;
esac
expect_eq 'schema carries toolchain' 'cargo 1.100.0-nightly (stub)' "$(jq -r '.toolchain' "$TMP/out-clean/summary.json")"
expect_eq 'schema carries libfuzzer flags' '25' "$(jq -r '.libfuzzer.timeout_secs' "$TMP/out-clean/summary.json")"
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

# --- every artifact class fails the campaign, including the exit-0 classes
for mode in crash leak slowunit unclassified oom timeout; do
  case "$mode" in
  crash) field=crashes artifact=crash-deadbeef expected_rc=1 ;;
  leak) field=leaks artifact=leak-deadbeef expected_rc=1 ;;
  slowunit) field=slow_units artifact=slow-unit-deadbeef expected_rc=1 ;;
  unclassified) field=other_artifacts artifact=mystery-deadbeef expected_rc=1 ;;
  oom) field=oom artifact=oom-deadbeef expected_rc=1 ;;
  timeout) field=timeouts artifact=timeout-deadbeef expected_rc=1 ;;
  esac
  rm -rf "$TMP/out-$mode"
  run_fails "$expected_rc" "$mode" "$TMP/out-$mode"
  expect_eq "$mode campaign result" 'finding' "$(jq -r '.result' "$TMP/out-$mode/summary.json")"
  expect_eq "$mode ${field} count" "1" "$(jq -r ".targets[0].$field" "$TMP/out-$mode/summary.json")"
  expect_eq "$mode artifact name" "$artifact" "$(jq -r '.targets[0].artifacts[0]' "$TMP/out-$mode/summary.json")"
  if [ ! -f "$TMP/out-$mode/artifacts/vt_parser/$artifact" ]; then
    fail "$mode artifact must be copied into the evidence directory"
  fi
  grep -Fq '## Findings' "$TMP/out-$mode/summary.md" || fail "$mode campaign must list findings in summary.md"
  rm -f "$TMP/artifacts/vt_parser/$artifact"
done

# --- a libFuzzer error exit with no artifact is still a finding
rm -rf "$TMP/out-abort"
run_fails 1 abort "$TMP/out-abort"
expect_eq 'abort campaign result' 'finding' "$(jq -r '.result' "$TMP/out-abort/summary.json")"
expect_eq 'abort campaign target result' 'finding' "$(jq -r '.targets[0].result' "$TMP/out-abort/summary.json")"

# --- zero executions is inconclusive, never clean (exit 4)
rm -rf "$TMP/out-silent"
run_fails 4 silent "$TMP/out-silent"
expect_eq 'zero-execution campaign result' 'inconclusive' "$(jq -r '.result' "$TMP/out-silent/summary.json")"
expect_eq 'zero-execution target result' 'inconclusive' "$(jq -r '.targets[0].result' "$TMP/out-silent/summary.json")"
expect_eq 'zero-execution runs' '0' "$(jq -r '.targets[0].runs' "$TMP/out-silent/summary.json")"

# --- build/runner failure without an artifact is an environment error
rm -rf "$TMP/out-error"
run_fails 3 error "$TMP/out-error"
expect_eq 'error campaign result' 'error' "$(jq -r '.result' "$TMP/out-error/summary.json")"
expect_eq 'error campaign target result' 'error' "$(jq -r '.targets[0].result' "$TMP/out-error/summary.json")"

# --- signal trap: a TERMed campaign kills its child session and removes .work
rm -rf "$TMP/out-hang"
CHILD_PID_FILE="$TMP/hang-child-pid"
FAKE_FUZZ_MODE=hang FAKE_FUZZ_CHILD_PID="$CHILD_PID_FILE" FAKE_FUZZ_CALLS="$TMP/cargo-calls" \
  CARGO="$TMP/bin/cargo" \
  ./scripts/fuzz-campaign.sh --secs 10 --targets vt_parser \
  --out-dir "$TMP/out-hang" --corpus-dir "$TMP/corpus" --artifacts-dir "$TMP/artifacts" \
  >"$TMP/hang.log" 2>&1 &
DRIVER_PID=$!
for _ in $(seq 1 100); do
  [ -s "$CHILD_PID_FILE" ] && break
  sleep 0.05
done
if [ ! -s "$CHILD_PID_FILE" ]; then
  fail 'signal-trap probe: the stub never started'
else
  CHILD_PID="$(cat "$CHILD_PID_FILE")"
  kill -TERM "$DRIVER_PID"
  hang_rc=0
  wait "$DRIVER_PID" || hang_rc=$?
  DRIVER_PID=""
  expect_eq 'TERMed campaign exit code' '143' "$hang_rc"
  gone=0
  for _ in $(seq 1 100); do
    if ! kill -0 "$CHILD_PID" 2>/dev/null; then
      gone=1
      break
    fi
    sleep 0.05
  done
  expect_eq 'TERMed campaign child is gone' '1' "$gone"
  CHILD_PID=""
  if [ -e "$TMP/out-hang/.work" ]; then
    fail 'TERMed campaign must remove its .work directory'
  fi
fi

# --- the cap -max_len is derived from the generator source, not duplicated
extract_constant() { # constant-name file
  awk -v name="$1" '
    $0 ~ "pub const " name ": usize[[:space:]]*=" {
      line = $0
      sub(/.*=[[:space:]]*/, "", line)
      sub(/;.*$/, "", line)
      gsub(/[^0-9* ]/, "", line)
      print line
      exit
    }
  ' "$2"
}
multiply_terms() { # "4 * 1024 * 1024" -> 4194304
  local value=1 term
  local -a terms=()
  read -r -a terms <<<"$1"
  for term in "${terms[@]}"; do
    case "$term" in
    '*' | '') continue ;;
    *[!0-9]*)
      fail "cap derivation: unexpected term '$term'"
      return 1
      ;;
    esac
    value=$((value * term))
  done
  printf '%s' "$value"
}
generator=crates/bitty-vt/tests/control_string_cap.rs
iterm2=crates/bitty-vt/src/iterm2_osc.rs
wire="$(multiply_terms "$(extract_constant ITERM2_OSC_MAX_WIRE_BYTES "$iterm2")")"
attribute="$(multiply_terms "$(extract_constant ITERM2_OSC_MAX_ATTRIBUTE_BYTES "$iterm2")")"
prefix_len="$(awk -F'"' '/pub const ITERM2_OSC_PREFIX: &\[u8\] = b"/{ print length($2) }' "$iterm2")"
if [ -z "$wire" ] || [ -z "$attribute" ] || [ -z "$prefix_len" ]; then
  fail 'cap derivation: could not read the iTerm2 constants the generator uses'
else
  expected_max_len=$(((attribute + 1 + wire) + prefix_len + 2 + 19))
  driver_max_len="$(awk -F= '/^CAP_MAX_LEN=/{ print $2 }' scripts/fuzz-campaign.sh)"
  expect_eq 'cap -max_len matches the generator' "$expected_max_len" "$driver_max_len"
  grep -Fq 'FUZZ_MAX_LEN: usize = CAP + 19' "$generator" ||
    fail 'cap derivation: the generator no longer defines FUZZ_MAX_LEN as CAP + 19'
fi

# --- wiring: the driver test runs in PR CI; the campaign itself never does
grep -Fq 'fuzz-campaign' justfile || fail 'justfile must expose the fuzz-campaign recipe'
grep -Fq 'fuzz-campaign-test' justfile || fail 'justfile must expose the driver test recipe'
workflow=.github/workflows/fuzz-campaign.yml
[ -f "$workflow" ] || fail 'the nightly fuzz-campaign workflow must exist'
grep -Fq 'schedule:' "$workflow" || fail 'the campaign workflow must be scheduled'
grep -Fq 'workflow_dispatch:' "$workflow" || fail 'the campaign workflow must be dispatchable'
if grep -Eq '^  (push|pull_request|pull_request_target):' "$workflow"; then
  fail 'the campaign workflow must not run on pushes or pull requests'
fi
grep -Fq 'expected_targets' "$workflow" || fail 'the merged record must carry the expected target set'
grep -Fq 'missing_targets' "$workflow" || fail 'the merged record must carry the missing target set'
grep -Fq 'bash scripts/tests/fuzz-campaign.test.sh' .github/workflows/ci.yml ||
  fail 'the campaign driver test must run in PR CI'
if grep -Fq 'scripts/fuzz-campaign.sh' .github/workflows/ci.yml; then
  fail 'ci.yml must not run the campaign itself (it is not a PR gate)'
fi

if [ "$FAIL" -ne 0 ]; then
  echo 'fuzz-campaign.test.sh: FAIL' >&2
  exit 1
fi
echo 'fuzz-campaign.test.sh: PASS'
