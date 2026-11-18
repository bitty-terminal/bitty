#!/usr/bin/env bash
# fuzz-campaign.test.sh — CTX-1877 campaign driver regression tests.
#
# The driver's only heavyweight dependency is a nightly cargo-fuzz run, so the
# tests stub `cargo` and exercise everything the evidence depends on: the plan
# and budget clamp, libFuzzer stat parsing, artifact classification (crash /
# leak / slow unit / OOM / timeout / pre-seeded / unclassified), the execution
# floor, the signal trap, the evidence schema, the cap `-max_len` derivation,
# and the wiring that keeps the campaign itself out of the PR-required checks
# while the driver test runs there.
set -euo pipefail

cd "$(dirname "$0")/../.."
scratch="${TMPDIR:-/tmp}/bitty"
mkdir -p "$scratch"
TMP="$(mktemp -d "$scratch/fuzz-campaign-test.XXXXXX")"
# The sanitizer probe looks for the target binary in CARGO_TARGET_DIR (cargo's
# own fallback is `fuzz/target`). Pin it at an empty scratch tree so a developer
# build elsewhere on the host cannot leak into a test expectation; the probe
# cases below swap in trees they populate themselves.
export CARGO_TARGET_DIR="$TMP/cargo-target"
mkdir -p "$CARGO_TARGET_DIR"
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
#1234	pulse  cov: 321 ft: 456 corp: 12/345b lim: 4096 exec/s: 176 rss: 321Mb
Done 1234 runs in 7 second(s)
stat::number_of_executed_units: 1234
stat::average_exec_per_sec: 176
stat::new_units_added: 12
stat::slowest_unit_time_sec: 0
stat::peak_rss_mb: 321
LOG
}

# A libFuzzer build whose log carries no coverage counters: the run executed
# but cannot prove what it covered, so it must never read as `clean`.
stats_without_coverage() {
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
  nocoverage)
    stats_without_coverage
    exit 0
    ;;
  leaknocov)
    write_artifact leak-deadbeef
    stats_without_coverage
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
*"-vV"*)
  echo 'host: x86_64-unknown-linux-gnu'
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

# --- provenance the acceptance needs: sanitizer, corpus digest, coverage
# (CTX-1880). Each is recorded per target, and an absent one cannot read clean.
expect_eq 'schema carries the sanitizer name' 'address' "$(jq -r '.sanitizer.name' "$TMP/out-clean/summary.json")"
expect_eq 'schema carries the forwarded sanitizer flag' '--sanitizer=address' "$(jq -r '.sanitizer.cargo_fuzz_flag' "$TMP/out-clean/summary.json")"
expect_eq 'schema carries the instrumentation flag' '-Zsanitizer=address' "$(jq -r '.sanitizer.instrumentation' "$TMP/out-clean/summary.json")"
expect_eq 'schema carries leak detection' 'lsan' "$(jq -r '.sanitizer.leak_detection' "$TMP/out-clean/summary.json")"
expect_eq 'schema records effective leak detection' 'true' \
  "$(jq -r '.sanitizer.leak_detection_effective' "$TMP/out-clean/summary.json")"
expect_eq 'schema records no disabling option' '' \
  "$(jq -r '.sanitizer.leak_detection_disabled_by' "$TMP/out-clean/summary.json")"
expect_eq 'schema records the leak-detection source' 'asan-runtime' "$(jq -r '.sanitizer.leak_detection_source' "$TMP/out-clean/summary.json")"
expect_eq 'schema records -detect_leaks' '1' "$(jq -r '.libfuzzer.detect_leaks' "$TMP/out-clean/summary.json")"
expect_eq 'target records its sanitizer' 'address' "$(jq -r '.targets[0].sanitizer' "$TMP/out-clean/summary.json")"
expect_eq 'target records the coverage counter' '321' "$(jq -r '.targets[0].coverage_edges' "$TMP/out-clean/summary.json")"
expect_eq 'target records the feature counter' '456' "$(jq -r '.targets[0].coverage_features' "$TMP/out-clean/summary.json")"
expect_eq 'target records no missing evidence' '[]' "$(jq -c '.targets[0].evidence_missing' "$TMP/out-clean/summary.json")"
grep -Fq -- '--sanitizer=address' "$TMP/cargo-calls" || fail 'the driver must forward --sanitizer to cargo-fuzz'
grep -Fq -- '-detect_leaks=1' "$TMP/cargo-calls" || fail 'the driver must forward -detect_leaks for an ASan build'
digest="$(jq -r '.targets[0].corpus_digest' "$TMP/out-clean/summary.json")"
case "$digest" in
'' | *[!0-9a-f]*)
  fail "corpus digest must be lowercase hex, got '$digest'"
  ;;
esac
expect_eq 'corpus digest is a sha256' '64' "$(printf '%s' "$digest" | wc -c | tr -d ' ')"
[ -f "$TMP/out-clean/corpus/vt_parser.sha256" ] ||
  fail 'every target must write a corpus digest artifact'
grep -Fq "$digest" "$TMP/out-clean/corpus/vt_parser.sha256" ||
  fail 'the corpus digest artifact must carry the recorded digest'
expect_eq 'targets.tsv carries every recorded column' '29' \
  "$(awk -F '\t' '{ print NF; exit }' "$TMP/out-clean/targets.tsv")"
grep -Fq '## Provenance per target' "$TMP/out-clean/summary.md" ||
  fail 'summary.md must carry the provenance table'
grep -Fq 'Cov' "$TMP/out-clean/summary.md" || fail 'summary.md must carry the coverage column'

# --- the digest is a function of the corpus, not of the walk order or the run
rm -rf "$TMP/out-digest-a" "$TMP/out-digest-b" "$TMP/out-digest-c"
run_fails 0 clean "$TMP/out-digest-a"
pristine="$(jq -r '.targets[0].corpus_digest' "$TMP/out-digest-a/summary.json")"
printf 'digest-probe-unit' >"$TMP/corpus/vt_parser/added.bin"
run_fails 0 clean "$TMP/out-digest-b"
if [ "$pristine" = "$(jq -r '.targets[0].corpus_digest' "$TMP/out-digest-b/summary.json")" ]; then
  fail 'a new corpus unit must change the digest'
fi
rm -f "$TMP/corpus/vt_parser/added.bin"
run_fails 0 clean "$TMP/out-digest-c"
expect_eq 'the digest is stable for one corpus' "$pristine" \
  "$(jq -r '.targets[0].corpus_digest' "$TMP/out-digest-c/summary.json")"

# --- an absent fact cannot read as clean: no coverage counters -> inconclusive
rm -rf "$TMP/out-nocov"
run_fails 4 nocoverage "$TMP/out-nocov"
expect_eq 'a run without coverage counters' 'inconclusive' "$(jq -r '.result' "$TMP/out-nocov/summary.json")"
expect_eq 'a run without coverage counters (target)' 'inconclusive' \
  "$(jq -r '.targets[0].result' "$TMP/out-nocov/summary.json")"
expect_eq 'absent coverage is null, never zero' 'null' \
  "$(jq -r '.targets[0].coverage_edges' "$TMP/out-nocov/summary.json")"
expect_eq 'the missing fact is named' 'coverage' \
  "$(jq -r '.targets[0].evidence_missing[0]' "$TMP/out-nocov/summary.json")"
grep -Fq 'missing evidence (coverage)' "$TMP/last.log" ||
  fail 'the driver must say why it refused clean'

# --- missing provenance never hides a finding
rm -rf "$TMP/out-leaknocov"
run_fails 1 leaknocov "$TMP/out-leaknocov"
expect_eq 'a leak without coverage counters is still a finding' 'finding' \
  "$(jq -r '.result' "$TMP/out-leaknocov/summary.json")"
expect_eq 'the leak is still counted' '1' "$(jq -r '.targets[0].leaks' "$TMP/out-leaknocov/summary.json")"
rm -f "$TMP/artifacts/vt_parser/leak-deadbeef"

# --- a discovered unit is saved under its content hash, without a `.bin` name
printf 'content-hash unit' >"$TMP/corpus/vt_parser/0123456789abcdef"
rm -rf "$TMP/out-digest-hash"
run_fails 0 clean "$TMP/out-digest-hash"
if [ "$pristine" = "$(jq -r '.targets[0].corpus_digest' "$TMP/out-digest-hash/summary.json")" ]; then
  fail 'an extensionless corpus unit must change the digest'
fi
expect_eq 'corpus units count extensionless units' '1' \
  "$(( $(jq -r '.targets[0].corpus_units' "$TMP/out-digest-hash/summary.json") - $(jq -r '.targets[0].corpus_units' "$TMP/out-digest-c/summary.json") ))"
rm -f "$TMP/corpus/vt_parser/0123456789abcdef"

# --- the sanitizer record is checked against the build this run produced
triple=x86_64-unknown-linux-gnu
mkdir -p "$TMP/bare-target/$triple/release"
printf 'stub fuzz binary, no sanitizer runtime\n' >"$TMP/bare-target/$triple/release/vt_parser"
CARGO_TARGET_DIR="$TMP/bare-target"
rm -rf "$TMP/out-nosan"
run_fails 4 clean "$TMP/out-nosan"
expect_eq 'a build without the ASan marker cannot read clean' 'inconclusive' \
  "$(jq -r '.targets[0].result' "$TMP/out-nosan/summary.json")"
expect_eq 'the contradiction is named' 'sanitizer-build' \
  "$(jq -r '.targets[0].evidence_missing[0]' "$TMP/out-nosan/summary.json")"
expect_eq 'the probe outcome is recorded' 'mismatch' \
  "$(jq -r '.targets[0].sanitizer_probe' "$TMP/out-nosan/summary.json")"
expect_eq 'an unverified sanitizer is not claimed verified' 'false' \
  "$(jq -r '.targets[0].sanitizer_verified' "$TMP/out-nosan/summary.json")"
printf 'stub fuzz binary with __asan_init linked\n' >"$TMP/bare-target/$triple/release/vt_parser"
rm -rf "$TMP/out-san"
run_fails 0 clean "$TMP/out-san"
expect_eq 'a probed ASan build restores clean' 'clean' "$(jq -r '.targets[0].result' "$TMP/out-san/summary.json")"
expect_eq 'a probed ASan build records the probe' 'found' \
  "$(jq -r '.targets[0].sanitizer_probe' "$TMP/out-san/summary.json")"
expect_eq 'a probed ASan build is verified' 'true' \
  "$(jq -r '.targets[0].sanitizer_verified' "$TMP/out-san/summary.json")"

# A stale build in a bare `release/` tree must not answer for this run.
mkdir -p "$TMP/bare-target/release"
printf 'stale binary, no sanitizer runtime\n' >"$TMP/bare-target/release/vt_parser"
rm -rf "$TMP/out-stale"
run_fails 0 clean "$TMP/out-stale"
expect_eq 'a stale bare-release build is ignored' 'found' \
  "$(jq -r '.targets[0].sanitizer_probe' "$TMP/out-stale/summary.json")"

# With no build under the host triple the probe says "unverified", not "found".
rm -rf "$TMP/bare-target/$triple"
rm -rf "$TMP/out-unprobed"
run_fails 0 clean "$TMP/out-unprobed"
expect_eq 'an unidentifiable build is not verified' 'absent' \
  "$(jq -r '.targets[0].sanitizer_probe' "$TMP/out-unprobed/summary.json")"
expect_eq 'an unidentifiable build is not claimed verified' 'false' \
  "$(jq -r '.targets[0].sanitizer_verified' "$TMP/out-unprobed/summary.json")"
CARGO_TARGET_DIR="$TMP/cargo-target"

# --- an inherited ASAN_OPTIONS=detect_leaks=0 means no leak check happened
rm -rf "$TMP/out-leaksoff"
LEAKSOFF_RC=0
ASAN_OPTIONS='detect_leaks=0' FAKE_FUZZ_MODE=clean FAKE_FUZZ_CALLS="$TMP/cargo-calls" \
  CARGO="$TMP/bin/cargo" \
  ./scripts/fuzz-campaign.sh --secs 10 --targets vt_parser --out-dir "$TMP/out-leaksoff" \
  --corpus-dir "$TMP/corpus" --artifacts-dir "$TMP/artifacts" >"$TMP/last.log" 2>&1 ||
  LEAKSOFF_RC=$?
expect_eq 'a run with leak detection disabled cannot read clean' '4' "$LEAKSOFF_RC"
expect_eq 'disabled leak detection is named' 'sanitizer-leaks-unchecked' \
  "$(jq -r '.targets[0].evidence_missing[0]' "$TMP/out-leaksoff/summary.json")"
expect_eq 'the effective leak state is recorded' 'false' \
  "$(jq -r '.sanitizer.leak_detection_effective' "$TMP/out-leaksoff/summary.json")"
expect_eq 'the disabling option is recorded' 'ASAN_OPTIONS=detect_leaks=0' \
  "$(jq -r '.sanitizer.leak_detection_disabled_by' "$TMP/out-leaksoff/summary.json")"
grep -Fq 'disables the leak check' "$TMP/last.log" || fail 'the driver must warn about disabled leak detection'

# --- an unreadable corpus unit fails the digest instead of shortening it
unreadable="$TMP/corpus/vt_parser/unreadable.bin"
printf 'cannot read me' >"$unreadable"
chmod 000 "$unreadable"
if [ -r "$unreadable" ]; then
  echo 'skip: this user can read a 0000 file, so the digest cannot be made to fail' >&2
else
  rm -rf "$TMP/out-noread"
  run_fails 4 clean "$TMP/out-noread"
  expect_eq 'a failed digest cannot read clean' 'inconclusive' \
    "$(jq -r '.targets[0].result' "$TMP/out-noread/summary.json")"
  expect_eq 'a failed digest is recorded as null' 'null' \
    "$(jq -r '.targets[0].corpus_digest' "$TMP/out-noread/summary.json")"
  expect_eq 'a failed digest is named' 'corpus-digest' \
    "$(jq -r '.targets[0].evidence_missing[0]' "$TMP/out-noread/summary.json")"
fi
chmod 644 "$unreadable"
rm -f "$unreadable"

# --- an unknown sanitizer is a usage error, not a recorded identity
SANITIZER_BAD_RC=0
SANITIZER=bogus FAKE_FUZZ_MODE=plan FAKE_FUZZ_CALLS="$TMP/cargo-calls" CARGO="$TMP/bin/cargo" \
  ./scripts/fuzz-campaign.sh --secs 10 --targets vt_parser --out-dir "$TMP/out-badsan" \
  --corpus-dir "$TMP/corpus" --artifacts-dir "$TMP/artifacts" >"$TMP/last.log" 2>&1 ||
  SANITIZER_BAD_RC=$?
expect_eq 'an unknown sanitizer is rejected' '2' "$SANITIZER_BAD_RC"
grep -Fq 'unknown sanitizer' "$TMP/last.log" || fail 'an unknown sanitizer must name the defect'

# --- a build whose sanitizer cannot evidence memory safety cannot read clean
for entry in none:sanitizer-none:none memory:sanitizer-leaks-unchecked:none; do
  name="${entry%%:*}"
  rest="${entry#*:}"
  expected_gap="${rest%%:*}"
  expected_leak_detection="${rest#*:}"
  SANITIZER_RC=0
  SANITIZER="$name" FAKE_FUZZ_MODE=clean FAKE_FUZZ_CALLS="$TMP/cargo-calls" CARGO="$TMP/bin/cargo" \
    ./scripts/fuzz-campaign.sh --secs 10 --targets vt_parser --out-dir "$TMP/out-san-$name" \
    --corpus-dir "$TMP/corpus" --artifacts-dir "$TMP/artifacts" >"$TMP/last.log" 2>&1 ||
    SANITIZER_RC=$?
  expect_eq "a $name build cannot read clean" '4' "$SANITIZER_RC"
  expect_eq "a $name build records its sanitizer" "$name" \
    "$(jq -r '.targets[0].sanitizer' "$TMP/out-san-$name/summary.json")"
  expect_eq "a $name build names the gap" "$expected_gap" \
    "$(jq -r '.targets[0].evidence_missing[0]' "$TMP/out-san-$name/summary.json")"
  expect_eq "a $name build records effective leak detection" "$expected_leak_detection" \
    "$(jq -r '.sanitizer.leak_detection' "$TMP/out-san-$name/summary.json")"
  expect_eq "a $name build records -detect_leaks" '0' \
    "$(jq -r '.libfuzzer.detect_leaks' "$TMP/out-san-$name/summary.json")"
  expect_eq "a $name build records effective leak detection" 'false' \
    "$(jq -r '.sanitizer.leak_detection_effective' "$TMP/out-san-$name/summary.json")"
done

# --- a pre-seeded artifact is never `clean`, even when the run adds no new name.
# libFuzzer derives an artifact name from the input that produced it
# (`leak-<sha1>`, `timeout-<sha1>`, ...), so a run that re-discovers an input an
# earlier campaign, a local `just fuzz`, or a shared cache left in
# `fuzz/artifacts/<target>/` rewrites the same file and the name diff stays
# empty. P0-AC-002 counts findings, not new file names (bitty#3232), so the
# driver must refuse `clean` here and record what it saw.
for preseed in leak-deadbeef crash-stale timeout-stale; do
  : >"$TMP/artifacts/vt_parser/$preseed"
  rm -rf "$TMP/out-preseed"
  run_fails 1 clean "$TMP/out-preseed"
  expect_eq "pre-seeded $preseed campaign result" 'finding' "$(jq -r '.result' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed target result" 'finding' "$(jq -r '.targets[0].result' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed is not a new finding" '0' \
    "$(jq -r '.targets[0] | .crashes + .leaks + .slow_units + .oom + .timeouts + .other_artifacts' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed adds no name" '0' "$(jq -r '.targets[0].artifacts | length' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed is recorded" '1' "$(jq -r '.targets[0].pre_existing_artifacts' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed is named" "$preseed" "$(jq -r '.targets[0].pre_existing_artifact_names[0]' "$TMP/out-preseed/summary.json")"
  expect_eq "pre-seeded $preseed reaches the totals" '1' "$(jq -r '.totals.pre_existing_artifacts' "$TMP/out-preseed/summary.json")"
  grep -Fq "| Pre-existing |" "$TMP/out-preseed/summary.md" || fail 'summary.md must carry the pre-existing column'
  grep -Fq "$preseed (pre-existing artifact" "$TMP/out-preseed/summary.md" ||
    fail "pre-seeded $preseed must be named in summary.md findings"
  rm -f "$TMP/artifacts/vt_parser/$preseed"
done

# --- pre-existing and newly added artifacts are reported separately
: >"$TMP/artifacts/vt_parser/crash-stale"
rm -rf "$TMP/out-mixed"
run_fails 1 leak "$TMP/out-mixed"
expect_eq 'mixed campaign result' 'finding' "$(jq -r '.result' "$TMP/out-mixed/summary.json")"
expect_eq 'mixed campaign counts the new leak' '1' "$(jq -r '.targets[0].leaks' "$TMP/out-mixed/summary.json")"
expect_eq 'mixed campaign counts the pre-existing artifact' '1' "$(jq -r '.targets[0].pre_existing_artifacts' "$TMP/out-mixed/summary.json")"
expect_eq 'mixed campaign keeps the new name only in artifacts' 'leak-deadbeef' "$(jq -r '.targets[0].artifacts[0]' "$TMP/out-mixed/summary.json")"
expect_eq 'mixed campaign names the pre-existing artifact' 'crash-stale' "$(jq -r '.targets[0].pre_existing_artifact_names[0]' "$TMP/out-mixed/summary.json")"
rm -f "$TMP/artifacts/vt_parser/crash-stale" "$TMP/artifacts/vt_parser/leak-deadbeef"

# --- with the directory empty again the same run is clean
rm -rf "$TMP/out-reclean"
run_fails 0 clean "$TMP/out-reclean"
expect_eq 'emptied artifact dir restores a clean record' 'clean' "$(jq -r '.result' "$TMP/out-reclean/summary.json")"
expect_eq 'emptied artifact dir records no pre-existing artifact' '0' "$(jq -r '.totals.pre_existing_artifacts' "$TMP/out-reclean/summary.json")"

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

# --- the cap -max_len has one owner: the checked-in derived file the driver
# reads, kept equal to the Rust generator constant (CTX-1880 gap F10). The
# driver must not carry the number itself.
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
cap_file=fuzz/control_string_cap.max_len
wire="$(multiply_terms "$(extract_constant ITERM2_OSC_MAX_WIRE_BYTES "$iterm2")")"
attribute="$(multiply_terms "$(extract_constant ITERM2_OSC_MAX_ATTRIBUTE_BYTES "$iterm2")")"
prefix_len="$(awk -F'"' '/pub const ITERM2_OSC_PREFIX: &\[u8\] = b"/{ print length($2) }' "$iterm2")"
if [ -z "$wire" ] || [ -z "$attribute" ] || [ -z "$prefix_len" ]; then
  fail 'cap derivation: could not read the iTerm2 constants the generator uses'
else
  expected_max_len=$(((attribute + 1 + wire) + prefix_len + 2 + 19))
  [ -f "$cap_file" ] || fail "cap derivation: $cap_file must exist"
  recorded_max_len="$(awk -F= '/^max_len=/{ print $2 }' "$cap_file")"
  expect_eq 'cap -max_len matches the generator' "$expected_max_len" "$recorded_max_len"
  grep -Fq 'FUZZ_MAX_LEN: usize = CAP + 19' "$generator" ||
    fail 'cap derivation: the generator no longer defines FUZZ_MAX_LEN as CAP + 19'
  grep -Fq 'fuzz_max_len_file_matches_generator' "$generator" ||
    fail 'cap derivation: the generator test must keep the derived file equal to FUZZ_MAX_LEN'
  grep -Fq 'control_string_cap.max_len' scripts/fuzz-campaign.sh ||
    fail 'cap derivation: the driver must read the derived max_len file'
  if grep -Fq '4198432' scripts/fuzz-campaign.sh; then
    fail 'cap derivation: the driver must not carry its own copy of the cap literal'
  fi
  dry_run_plan="$(./scripts/fuzz-campaign.sh --dry-run 2>&1)"
  case "$dry_run_plan" in
  *"-max_len=$expected_max_len"*) ;;
  *) fail 'cap derivation: the dry-run plan must print the cap target -max_len' ;;
  esac
fi
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
