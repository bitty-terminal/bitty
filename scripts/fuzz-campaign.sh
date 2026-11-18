#!/usr/bin/env bash
# fuzz-campaign.sh — bounded cargo-fuzz campaign driver and evidence record.
#
# Risk R-001 / acceptance criterion P0-AC-002 (bitty#3227, CTX-1877) requires a
# long-running fuzz campaign over the `fuzz/` cargo-fuzz targets with zero
# crashes, hangs, or memory-safety findings. The targets and the retained
# corpora existed; this driver is the repeatable form. The nightly
# `.github/workflows/fuzz-campaign.yml` workflow and the owner-facing
# `just fuzz-campaign <secs>` recipe both call it, so a nightly run and an
# hours-long owner campaign share one budget rule, one libFuzzer flag set, and
# one evidence schema.
#
# Per target the driver:
#   1. seeds the scratch corpus from the committed `fuzz/corpora/<target>`
#      (when present) without discarding units a previous run discovered;
#   2. runs libFuzzer for the budget with pinned `-timeout`, `-rss_limit_mb`,
#      `-workers`, and `-artifact_prefix` (`control_string_cap` also overrides
#      `-max_len`);
#   3. classifies every artifact a run adds under the artifact prefix
#      (`crash-*`, `leak-*`, `slow-unit-*`, `oom-*`, `timeout-*`, anything else)
#      and copies it into the evidence directory;
#   4. refuses to record `clean` for a target whose artifact directory already
#      held an artifact when the run started, because libFuzzer names an
#      artifact from its input (`leak-<sha1>`, `timeout-<sha1>`, ...): the same
#      input re-discovered by a later run reuses the same name and would
#      otherwise leave the name diff empty;
#   5. appends a row to `targets.tsv` and rewrites `summary.json`/`summary.md`
#      after every target, so a campaign killed mid-flight still leaves a
#      valid record of what ran.
#
# Every artifact class is a finding: a leak is exactly the "memory-safety
# finding" P0-AC-002 forbids, a slow unit is the hang class, and an
# unclassified artifact is a finding until someone classifies it. An artifact
# still present for the target after the run — whether newly written or left
# behind by an earlier campaign, a local `just fuzz`, or a shared cache — is a
# finding too, so an artifact-seeded run can never be called `clean`. That is
# why the campaign assumes the target's artifact directory is empty at start:
# an existing artifact is reported (and counted under `pre_existing_artifacts`)
# until it is triaged and the directory cleared, instead of being silently
# folded into a clean result. libFuzzer's own error exit (77 by default) is a
# finding even when it left no artifact, because that is how it reports an
# abort; any other non-zero exit is a runner/build `error`, and a run that
# exits 0 without executing anything is `inconclusive` — never `clean`.
#
# Evidence schema (`summary.json`): campaign, acceptance, revision,
# revision_dirty, toolchain, cargo_fuzz, sanitizer{name, cargo_fuzz_flag,
# instrumentation, leak_detection, leak_detection_source}, libfuzzer flags,
# started/finished UTC, requested_secs, targets[] {target, secs_requested,
# secs_actual, secs_fuzzed, runs, exec_per_sec, new_units, corpus_seeds,
# corpus_delta, corpus_units, corpus_digest, coverage_edges, coverage_features,
# peak_rss_mb, crashes, leaks, slow_units, oom, timeouts, other_artifacts,
# exit_code, result, sanitizer, sanitizer_verified, evidence_missing,
# artifacts[], pre_existing_artifacts, pre_existing_artifact_names[]},
# totals{...}, result. `secs_actual` is the wall clock of the whole per-target
# step (build plus run); `secs_fuzzed` is libFuzzer's own `Done <runs> runs in
# <secs> second(s)` budget. No host path is recorded — the corpus digest is a
# bare hex string — so a summary is safe to attach to a security-corpus record
# as-is.
#
# P0-AC-002 evidence completeness (CTX-1880, bitty#3235): a `clean` result is
# only recorded when the run also carries the three provenance facts the
# acceptance needs — which sanitizer build produced it and with which flags, a
# sha256 digest over the corpus the run ended with, and a coverage signal
# (libFuzzer's own `cov:`/`ft:` counters). Each is recorded per target. When one
# cannot be captured the target is downgraded to `inconclusive` with the missing
# fact named in `evidence_missing`; it is never recorded `clean`. A
# `SANITIZER=none` build counts as missing: it can report zero leaks without
# saying anything about memory safety. Findings and runner errors keep their
# precedence, so missing provenance can only ever remove a `clean`, never
# manufacture or hide a finding.
#
# Bounds: --secs 10..86400 (default 600); -timeout 1..600 (default 25);
# -rss_limit_mb 256..16384 (default 2048); coverage counters are accepted only
# as integers in 0..4294967295 (anything else is treated as absent, not as a
# zero); each target's process runs in its
# own session and is hard-killed at the budget plus HARD_LIMIT_SLACK_SECS, so a
# wedged build cannot hang the caller. The caller and its child are trapped on
# INT/TERM/HUP: the child session is terminated and the work directory removed.
# The scratch corpus is trimmed to MAX_CORPUS_FILES units per target after the
# run, which keeps a persisted CI cache bounded; the committed corpora are
# never modified. `corpus_delta` counts the units a run added before that trim,
# and each target's full libFuzzer log stays in `logs/` (only the tail reaches
# the caller's output).
#
# Portability: the driver needs bash and POSIX find. `timeout(1)`/`setsid(1)`
# and GNU `find -printf` are used when present (Linux, including CI); without
# them the hard kill and the corpus retention trim degrade with a warning
# instead of aborting. The `just fuzz-campaign` recipe is therefore
# Linux-first, and a macOS host needs coreutils' `gtimeout`.
#
# Usage:
#   scripts/fuzz-campaign.sh                     # 600 s per target, all targets
#   scripts/fuzz-campaign.sh --secs 1800         # nightly-length campaign
#   scripts/fuzz-campaign.sh --secs 3600 --targets vt_parser,osc_string
#   scripts/fuzz-campaign.sh --out-dir recording/fuzz-campaign/owner-run
#   scripts/fuzz-campaign.sh --generate-cap-seeds  # materialize cap seeds once
#   scripts/fuzz-campaign.sh --dry-run           # plan only, no toolchain
#
# Exit codes: 0 = every target clean, 1 = at least one finding (crash, leak,
# slow unit, OOM, timeout, unclassified artifact, or an artifact already in the
# target's artifact directory when the run started), 2 = usage error,
# 3 = environment or runner error (toolchain or cargo-fuzz missing, build
# failure, wrapper timeout) with no finding recorded, 4 = inconclusive record
# (a target executed zero iterations, so a clean result cannot be claimed).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FUZZ_DIR="$REPO_ROOT/fuzz"
MANIFEST="$FUZZ_DIR/Cargo.toml"
COMMITTED_CORPORA="$FUZZ_DIR/corpora"
SCRATCH_CORPORA="$FUZZ_DIR/corpus"
DEFAULT_ARTIFACTS_ROOT="$FUZZ_DIR/artifacts"
DEFAULT_OUT_DIR="$REPO_ROOT/recording/fuzz-campaign"

# Single source of truth for the version the nightly workflow installs and the
# evidence record reports.
CARGO_FUZZ_VERSION="0.13.2"

DEFAULT_SECS=600
MIN_SECS=10
MAX_SECS=86400
DEFAULT_TIMEOUT_SECS=25
MIN_TIMEOUT_SECS=1
MAX_TIMEOUT_SECS=600
DEFAULT_RSS_LIMIT_MB=2048
MIN_RSS_LIMIT_MB=256
MAX_RSS_LIMIT_MB=16384

# The cap target feeds whole control strings, so it is the one target whose
# inputs exceed libFuzzer's 1 MiB default mutation cap. The value has exactly
# one owner in the tree: `CONTROL_STRING_MAX_BYTES` in
# `crates/bitty-vt/src/parser.rs` (the parser-wide cap) plus the generator
# headroom in `FUZZ_MAX_LEN` in `crates/bitty-vt/tests/control_string_cap.rs`.
# The driver carries no copy of the number: it reads the derived, checked-in
# `fuzz/control_string_cap.max_len`, which the generator test asserts equal to
# `FUZZ_MAX_LEN`, so a drift fails `cargo test -p bitty-vt` and this driver's
# test instead of silently fuzzing a different bound (CTX-1880 review gap F10).
CAP_TARGET="control_string_cap"
CAP_MAX_LEN_FILE="$FUZZ_DIR/$CAP_TARGET.max_len"

# libFuzzer counters are u32 in its own statistics; anything outside this range
# is a parse failure, not a large number.
MAX_COUNTER_VALUE=4294967295

# The sanitizer build the campaign records and runs. `cargo-fuzz 0.13.2`
# defaults to `address`, and `--sanitizer` pins it explicitly so the forwarded
# flag — not an unstated default — is what the record names. ASan on Linux
# carries LSan, which is why the leak class is memory-safety evidence.
DEFAULT_SANITIZER="address"
SANITIZER="${SANITIZER:-$DEFAULT_SANITIZER}"

# Corpus units retained per target after a run (see the header bounds).
MAX_CORPUS_FILES=2000

# Extra wall clock allowed for the per-target build before the hard kill.
HARD_LIMIT_SLACK_SECS=300
CAT_SEED_TIMEOUT_SECS=300

# Optional GNU/userland helpers. Missing ones degrade, they never abort a run.
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
SETSID_BIN="$(command -v setsid || true)"
FIND_PRINTF=0
if find "$FUZZ_DIR" -maxdepth 1 -printf '' >/dev/null 2>&1; then
  FIND_PRINTF=1
fi

# sha256 implementation for the corpus digest: GNU coreutils, or BSD `shasum`.
# Without either, the digest is an absent fact and a target cannot be recorded
# clean (see the completeness rule in the header) — the campaign still runs.
HASH_CMD=()
if command -v sha256sum >/dev/null 2>&1; then
  HASH_CMD=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  HASH_CMD=(shasum -a 256)
fi

hash_file() {
  "${HASH_CMD[@]}" -- "$1" | awk '{ print $1 }'
}

SECS="$DEFAULT_SECS"
TARGETS=""
OUT_DIR=""
CORPUS_DIR="$SCRATCH_CORPORA"
ARTIFACTS_ROOT="$DEFAULT_ARTIFACTS_ROOT"
TOOLCHAIN="${BITTY_FUZZ_TOOLCHAIN:-nightly}"
TIMEOUT_SECS="$DEFAULT_TIMEOUT_SECS"
RSS_LIMIT_MB="$DEFAULT_RSS_LIMIT_MB"
CARGO="${CARGO:-cargo}"
GENERATE_CAP_SEEDS=0
DRY_RUN=0
LIST_TARGETS=0
PRINT_CARGO_FUZZ_VERSION=0

# Launch state for the trap: the child session we started and the work dir.
CHILD_PID=""
WORK_DIR=""

usage() {
  awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0"
}

die() {
  echo "fuzz-campaign: $*" >&2
  exit 2
}

env_die() {
  echo "fuzz-campaign: $*" >&2
  exit 3
}

is_uint() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

clamp() {
  local value="$1" min="$2" max="$3"
  if [ "$value" -lt "$min" ]; then
    printf '%s' "$min"
  elif [ "$value" -gt "$max" ]; then
    printf '%s' "$max"
  else
    printf '%s' "$value"
  fi
}

# Kill only the process session this driver created at launch (`setsid` makes
# the wrapper a session leader, so the negative PID reaches timeout, cargo, and
# the fuzzer). Never a name or pattern match.
stop_child() {
  [ -n "$CHILD_PID" ] || return 0
  if [ -n "$SETSID_BIN" ]; then
    kill -TERM -- "-$CHILD_PID" 2>/dev/null || true
  else
    kill -TERM "$CHILD_PID" 2>/dev/null || true
  fi
  CHILD_PID=""
}

cleanup() {
  local status="${1:-$?}"
  trap - EXIT INT TERM HUP
  stop_child
  if [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ]; then
    rm -rf -- "$WORK_DIR"
  fi
  exit "$status"
}

# Every `[[bin]]` name in the fuzz manifest, in declaration order. Reading the
# manifest keeps a new target from silently escaping the campaign (the same
# "enumerate, do not assume" rule the soak campaign applies to its tests).
list_targets() {
  awk '
    /^\[\[bin\]\]/ { in_bin = 1; next }
    /^\[/ { in_bin = 0 }
    in_bin && /^name[[:space:]]*=/ {
      n = split($0, parts, "\"")
      if (n >= 2) print parts[2]
      in_bin = 0
    }
  ' "$MANIFEST"
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
    --secs | --targets | --out-dir | --corpus-dir | --artifacts-dir | --toolchain | --timeout | --rss-limit-mb)
      [ $# -ge 2 ] || die "$1 requires a value"
      case "$1" in
      --secs) SECS="$2" ;;
      --targets) TARGETS="$2" ;;
      --out-dir) OUT_DIR="$2" ;;
      --corpus-dir) CORPUS_DIR="$2" ;;
      --artifacts-dir) ARTIFACTS_ROOT="$2" ;;
      --toolchain) TOOLCHAIN="$2" ;;
      --timeout) TIMEOUT_SECS="$2" ;;
      --rss-limit-mb) RSS_LIMIT_MB="$2" ;;
      esac
      shift 2
      ;;
    --generate-cap-seeds)
      GENERATE_CAP_SEEDS=1
      shift
      ;;
    --list-targets)
      LIST_TARGETS=1
      shift
      ;;
    --pinned-cargo-fuzz)
      PRINT_CARGO_FUZZ_VERSION=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "fuzz-campaign: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
    esac
  done
}

count_files() {
  find "$1" -maxdepth 1 -type f | wc -l | tr -d ' '
}

# Committed seed corpus for a target. `parser_to_state` has no directory of its
# own: it replays the canonical R-001 corpus (`fuzz/corpora/vt`), which the
# `vt_parser` seeds are a byte-identical copy of.
seed_source() {
  case "$1" in
  parser_to_state) printf '%s' "$COMMITTED_CORPORA/vt" ;;
  *) printf '%s' "$COMMITTED_CORPORA/$1" ;;
  esac
}

count_seeds() {
  find "$1" -maxdepth 1 -type f -name '*.bin' | wc -l | tr -d ' '
}

# Basenames of the files directly inside a directory, one per line, without
# depending on GNU `find -printf`.
list_file_names() {
  if [ "$FIND_PRINTF" -eq 1 ]; then
    find "$1" -maxdepth 1 -type f -printf '%f\n'
  else
    find "$1" -maxdepth 1 -type f -exec basename {} \;
  fi
}

# Count the artifacts whose name starts with `$2` in a newline-separated list.
count_prefix() {
  printf '%s\n' "$1" | awk -v prefix="$2" 'index($0, prefix) == 1 { n++ } END { print n + 0 }'
}

# Last value of a libFuzzer `stat::name: value` line; 0 when absent.
stat_value() {
  awk -v key="$2" '$1 == key ":" { value = $2 } END { print value + 0 }' "$1"
}

# --- provenance helpers (CTX-1880) -----------------------------------------

# `--sanitizer` value cargo-fuzz accepts. An unknown name is rejected before
# the run, so the record can never carry a made-up sanitizer identity.
valid_sanitizer() {
  case "$1" in
  address | leak | memory | thread | none) return 0 ;;
  *) return 1 ;;
  esac
}

# Symbol each sanitizer runtime installs in the fuzz binary; `none` links no
# runtime, so it has no marker to probe for.
sanitizer_marker() {
  case "$1" in
  address) printf '__asan_init' ;;
  leak) printf '__lsan_init' ;;
  memory) printf '__msan_init' ;;
  thread) printf '__tsan_init' ;;
  *) printf '' ;;
  esac
}

# LeakSanitizer is not a separate build here: with `--sanitizer=address` the
# ASan runtime links LSan, and libFuzzer's `-detect_leaks` (on by default,
# passed explicitly below) drives it. That is why a `leak-*` artifact counts as
# a memory-safety finding for this campaign.
sanitizer_leak_detection() {
  case "$1" in
  address | leak) printf 'lsan' ;;
  *) printf 'none' ;;
  esac
}

# Best-effort probe of the instrumentation the run actually built: look for the
# runtime symbol in the target binary. A binary that is found and lacks the
# marker contradicts the recorded sanitizer and blocks `clean`; a binary that
# cannot be located (custom profile, exotic layout, a stubbed toolchain) leaves
# the record at "declared", which is all the forwarded `--sanitizer` flag
# claims. The probe never upgrades a claim into a finding, it only refuses to
# certify the opposite.
probe_sanitizer_build() { # target marker -> found | mismatch | absent | not-applicable
  local target="$1" marker="$2" root binary
  if [ -z "$marker" ]; then
    printf 'not-applicable'
    return 0
  fi
  # The binary this run just built lives in exactly one tree: the caller's
  # CARGO_TARGET_DIR when set, else cargo-fuzz's own `fuzz/target`.
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    root="$CARGO_TARGET_DIR"
  else
    root="$FUZZ_DIR/target"
  fi
  [ -d "$root" ] || { printf 'absent'; return 0; }
  binary="$(
    find "$root" -maxdepth 3 -type f -name "$target" -perm -u+x 2>/dev/null |
      { read -r found || true; printf '%s' "${found:-}"; }
  )" || true
  if [ -z "$binary" ]; then
    printf 'absent'
  elif grep -aqF -- "$marker" "$binary" 2>/dev/null; then
    printf 'found'
  else
    printf 'mismatch'
  fi
}

# Last integer that follows an exact token in the fuzz log (`cov:`, `ft:`,
# `stat::cov:`). Prints nothing when the token never carried a number, which
# keeps "absent" distinguishable from "zero" — the whole point of the
# completeness rule below.
last_number_after() {
  awk -v token="$2" '
    {
      for (i = 1; i < NF; i++) {
        if ($i == token && $(i + 1) ~ /^[0-9]+$/) value = $(i + 1)
      }
    }
    END { if (value != "") print value }
  ' "$1"
}

# One bounded coverage counter: libFuzzer's per-target `cov` (edges) or `ft`
# (frontier features) from its last pulse line, falling back to the final
# `stat::` block. Values outside libFuzzer's own u32 range are treated as a
# parse failure, never as a large number. Prints nothing when unavailable.
coverage_counter() { # log cov|ft
  local value
  value="$(last_number_after "$1" "$2:")"
  if [ -z "$value" ]; then
    value="$(last_number_after "$1" "stat::$2:")"
  fi
  case "$value" in
  '' | *[!0-9]*) return 0 ;;
  esac
  if [ "${#value}" -gt 10 ] || [ "$value" -gt "$MAX_COUNTER_VALUE" ]; then
    return 0
  fi
  printf '%s' "$value"
}

# `-max_len` for the cap target, read from the single-owner derived file.
# Missing or malformed is an environment error: fuzzing the cap with libFuzzer's
# 1 MiB default would silently test a different bound than the record claims.
cap_max_len() {
  local value
  [ -f "$CAP_MAX_LEN_FILE" ] ||
    env_die "$CAP_MAX_LEN_FILE is missing; it carries the cap target's -max_len (owner: CONTROL_STRING_MAX_BYTES in crates/bitty-vt/src/parser.rs)"
  value="$(awk -F= '
    /^[[:space:]]*#/ { next }
    $1 == "max_len" { gsub(/[^0-9]/, "", $2); value = $2 }
    END { print value }
  ' "$CAP_MAX_LEN_FILE")"
  is_uint "$value" && [ "$value" -gt 0 ] ||
    env_die "$CAP_MAX_LEN_FILE has no positive integer max_len"
  printf '%s' "$value"
}

# Deterministic digest of a corpus directory: sha256 over the name-sorted
# `<unit-sha256>  <unit-name>` manifest. Independent of directory-walk order and
# of the host path (no path is recorded), and it changes when a unit is added,
# removed, renamed, or rewritten. Empty output means no sha256 tool on this
# host, which makes the digest an absent fact rather than a guessed one.
corpus_digest() {
  local dir="$1"
  [ "${#HASH_CMD[@]}" -gt 0 ] || return 0
  (
    cd "$dir" 2>/dev/null || exit 1
    find . -maxdepth 1 -type f -name '*.bin' -print | LC_ALL=C sort |
      while IFS= read -r unit; do
        printf '%s  %s\n' "$(hash_file "$unit")" "${unit#./}"
      done
  ) | "${HASH_CMD[@]}" | awk '{ print $1 }'
}

# libFuzzer's `Done <runs> runs in <secs> second(s)` summary line.
done_runs() {
  awk '$1 == "Done" && $2 ~ /^[0-9]+$/ { runs = $2 } END { print runs + 0 }' "$1"
}

done_secs() {
  awk '$1 == "Done" && $2 ~ /^[0-9]+$/ && $5 ~ /^[0-9]+$/ { secs = $5 } END { print secs + 0 }' "$1"
}

render_summary_json() {
  local output="$1" started="$2" revision="$3" dirty="$4" toolchain_version="$5" cargo_fuzz="$6"
  {
    cat <<JSON
{
  "campaign": "bitty-fuzz-campaign",
  "acceptance": "R-001 / P0-AC-002 long-running cargo-fuzz campaign",
  "issue": "bitty#3227",
  "task": "CTX-1877",
  "evidence_task": "CTX-1880",
  "revision": "$revision",
  "revision_dirty": $dirty,
  "toolchain": "$toolchain_version",
  "cargo_fuzz": "$cargo_fuzz",
  "sanitizer": {"name": "$SANITIZER", "cargo_fuzz_flag": "--sanitizer=$SANITIZER", "instrumentation": "$SANITIZER_INSTRUMENTATION", "leak_detection": "$SANITIZER_LEAK_DETECTION", "leak_detection_source": "$SANITIZER_LEAK_SOURCE"},
  "libfuzzer": {"max_total_time_secs": $SECS, "timeout_secs": $TIMEOUT_SECS, "rss_limit_mb": $RSS_LIMIT_MB, "workers": 1, "detect_leaks": $SANITIZER_DETECT_LEAKS},
  "started_utc": "$started",
  "finished_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "requested_secs": $SECS,
  "targets": [
JSON
    awk -F '\t' '
      function json_list(list,   n, i, parts, out) {
        if (list == "" || list == "-") return "[]"
        n = split(list, parts, " ")
        out = "["
        for (i = 1; i <= n; i++) {
          if (i > 1) out = out ", "
          out = out "\"" parts[i] "\""
        }
        return out "]"
      }
      function num(value) {
        return (value ~ /^[0-9]+$/) ? value + 0 : "null"
      }
      function json_string(value) {
        return (value == "" || value == "-") ? "null" : "\"" value "\""
      }
      {
        if (count++) printf ",\n"
        printf "    {\"target\": \"%s\", \"secs_requested\": %d, \"secs_actual\": %d, \"secs_fuzzed\": %d, \"runs\": %d, \"exec_per_sec\": %d, \"new_units\": %d, \"corpus_seeds\": %d, \"corpus_delta\": %d, \"peak_rss_mb\": %d, \"crashes\": %d, \"leaks\": %d, \"slow_units\": %d, \"oom\": %d, \"timeouts\": %d, \"other_artifacts\": %d, \"exit_code\": %d, \"result\": \"%s\", \"artifacts\": %s, \"pre_existing_artifacts\": %d, \"pre_existing_artifact_names\": %s, \"sanitizer\": \"%s\", \"sanitizer_probe\": \"%s\", \"sanitizer_verified\": %s, \"corpus_digest\": %s, \"corpus_units\": %s, \"coverage_edges\": %s, \"coverage_features\": %s, \"evidence_missing\": %s}", $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, json_list($19), $20, json_list($21), $22, $23, ($24 == "true" ? "true" : "false"), json_string($25), num($26), num($27), num($28), json_list($29)
        fuzzed += $4
        runs += $5
        new_units += $7
        if ($10 > peak) peak = $10
        crashes += $11
        leaks += $12
        slow += $13
        oom += $14
        timeouts += $15
        other += $16
        pre_existing += $20
        if ($18 == "finding") has_finding = 1
        if ($18 == "error") has_error = 1
        if ($18 == "inconclusive") has_inconclusive = 1
      }
      END {
        overall = has_finding ? "finding" : (has_error ? "error" : (has_inconclusive ? "inconclusive" : "clean"))
        printf "\n  ],\n"
        printf "  \"totals\": {\"targets\": %d, \"secs_fuzzed\": %d, \"runs\": %d, \"new_units\": %d, \"peak_rss_mb\": %d, \"crashes\": %d, \"leaks\": %d, \"slow_units\": %d, \"oom\": %d, \"timeouts\": %d, \"other_artifacts\": %d, \"pre_existing_artifacts\": %d},\n", count + 0, fuzzed + 0, runs + 0, new_units + 0, peak + 0, crashes + 0, leaks + 0, slow + 0, oom + 0, timeouts + 0, other + 0, pre_existing + 0
        printf "  \"result\": \"%s\"\n}\n", overall
      }
    ' "$WORK_TARGETS"
  } >"$output"
}

render_summary_md() {
  local output="$1" started="$2"
  {
    cat <<MD
# fuzz campaign summary

- Campaign: \`bitty-fuzz-campaign\` (R-001 / P0-AC-002 evidence; bitty#3227, CTX-1877; completeness CTX-1880)
- Revision: \`$REVISION\` (dirty: $REVISION_DIRTY), $TOOLCHAIN_VERSION, $CARGO_FUZZ_DETECTED
- Sanitizer: \`$SANITIZER\` (\`--sanitizer=$SANITIZER\`, \`$SANITIZER_INSTRUMENTATION\`, leak detection: $SANITIZER_LEAK_DETECTION)
- Started: $started
- Budget: $SECS s per target, \`-timeout=$TIMEOUT_SECS\`, \`-rss_limit_mb=$RSS_LIMIT_MB\`, \`-workers=1\`

| Target | Secs fuzzed | Runs | Exec/s | New units | Seeds | Peak RSS (MB) | Cov | FT | Crashes | Leaks | Slow units | OOM | Timeouts | Pre-existing | Result |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
MD
    awk -F '\t' '
      {
        printf "| `%s` | %d | %d | %d | %d | %d | %d | %s | %s | %d | %d | %d | %d | %d | %d | %s |\n", $1, $4, $5, $6, $7, $8, $10, $27, $28, $11, $12, $13, $14, $15, $20, $18
        fuzzed += $4
        runs += $5
        crashes += $11
        leaks += $12
        slow += $13
        oom += $14
        timeouts += $15
        pre_existing += $20
        if ($18 == "finding") has_finding = 1
        if ($18 == "error") has_error = 1
        if ($18 == "inconclusive") has_inconclusive = 1
      }
      END {
        overall = has_finding ? "finding" : (has_error ? "error" : (has_inconclusive ? "inconclusive" : "clean"))
        printf "| **total** | %d | %d | – | – | – | – | – | – | %d | %d | %d | %d | %d | %d | **%s** |\n", fuzzed + 0, runs + 0, crashes + 0, leaks + 0, slow + 0, oom + 0, timeouts + 0, pre_existing + 0, overall
      }
    ' "$WORK_TARGETS"
    printf '\n## Provenance per target\n\n| Target | Sanitizer | Build probe | Corpus units | Corpus sha256 | Coverage source | Missing evidence |\n| --- | --- | --- | --- | --- | --- | --- |\n'
    awk -F '\t' '
      {
        missing = ($29 == "-" || $29 == "") ? "none" : $29
        printf "| `%s` | %s | %s | %s | %s | %s | %s |\n", $1, $22, $23, $26, $25, "libFuzzer cov/ft", missing
      }
    ' "$WORK_TARGETS"
    if [ -s "$WORK_FINDINGS" ]; then
      printf '\n## Findings\n\n'
      sed 's/^/- /' "$WORK_FINDINGS"
    fi
  } >"$output"
}

write_summaries() {
  local started="$1"
  render_summary_json "$OUT_DIR/summary.json" "$started" "$REVISION" "$REVISION_DIRTY" \
    "$TOOLCHAIN_VERSION" "$CARGO_FUZZ_DETECTED"
  render_summary_md "$OUT_DIR/summary.md" "$started"
}

# Keep a persisted corpus bounded without touching the committed corpora. The
# oldest units by mtime go first; a unit libFuzzer still finds interesting gets
# re-added on the next run. Deliberately no `head` in the pipeline: a reader
# that exits early sends SIGPIPE to `sort`, which `pipefail` would turn into a
# campaign abort even though trimming is only hygiene. Retention needs GNU
# `find -printf`; without it the trim degrades to a warning.
trim_corpus() {
  local dir="$1" total excess
  total="$(count_files "$dir")"
  if [ "$total" -le "$MAX_CORPUS_FILES" ]; then
    return 0
  fi
  if [ "$FIND_PRINTF" -ne 1 ]; then
    echo "fuzz-campaign: skipping corpus trim (find -printf unavailable)" >&2
    return 0
  fi
  excess=$((total - MAX_CORPUS_FILES))
  find "$dir" -maxdepth 1 -type f -printf '%T@ %p\n' | sort -n | cut -d ' ' -f 2- >"$WORK_DIR/trim-$total.txt"
  awk -v limit="$excess" 'NR <= limit' "$WORK_DIR/trim-$total.txt" |
    while IFS= read -r stale; do rm -f -- "$stale"; done
  echo "fuzz-campaign: trimmed $excess stale corpus units from $(basename "$dir")"
}

generate_cap_seeds() {
  local log="$OUT_DIR/logs/$CAP_TARGET-seeds.log"
  local -a guard=()
  echo "fuzz-campaign: generating $CAP_TARGET seeds (parser cap boundaries)"
  if [ -n "$TIMEOUT_BIN" ]; then
    guard=("$TIMEOUT_BIN" --signal=TERM "${CAT_SEED_TIMEOUT_SECS}s")
  else
    guard=(env)
  fi
  if ! "${guard[@]}" "$CARGO" test --locked -p bitty-vt --test control_string_cap -- \
    --ignored --nocapture generate_cap_seeds >"$log" 2>&1; then
    cat -- "$log" >&2
    env_die "cap seed generation failed (see $log)"
  fi
  cat -- "$log"
}

record_target() {
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" "$9" "${10}" "${11}" "${12}" "${13}" \
    "${14}" "${15}" "${16}" "${17}" "${18}" "${19}" "${20}" "${21}" "${22}" \
    "${23}" "${24}" "${25}" "${26}" "${27}" "${28}" "${29}" \
    >>"$WORK_TARGETS"
}

run_target() {
  local target="$1"
  local corpus_dir="$CORPUS_DIR/$target"
  local artifacts_dir="$ARTIFACTS_ROOT/$target"
  local log="$OUT_DIR/logs/$target.log"
  mkdir -p "$corpus_dir" "$artifacts_dir" "$OUT_DIR/logs" "$WORK_DIR"

  if [ "$target" = "$CAP_TARGET" ] && [ "$GENERATE_CAP_SEEDS" -eq 1 ] && [ "$(count_files "$corpus_dir")" -eq 0 ]; then
    generate_cap_seeds
  fi

  # Seed from the retained corpus without dropping units from earlier runs.
  local seeds_dir
  seeds_dir="$(seed_source "$target")"
  if [ -d "$seeds_dir" ]; then
    while IFS= read -r -d '' seed; do
      local name
      name="$(basename "$seed")"
      if [ ! -e "$corpus_dir/$name" ]; then
        cp -- "$seed" "$corpus_dir/$name"
      fi
    done < <(find "$seeds_dir" -maxdepth 1 -type f -name '*.bin' -print0)
  fi

  local corpus_seeds corpus_before
  corpus_seeds="$(count_files "$corpus_dir")"
  corpus_before="$corpus_seeds"

  # `-artifact_prefix` is what makes `--artifacts-dir` real: libFuzzer (through
  # cargo-fuzz) writes its findings where this driver looks for them, instead
  # of always into `fuzz/artifacts/<target>`.
  local -a fuzz_args=(
    "-max_total_time=$SECS"
    "-timeout=$TIMEOUT_SECS"
    "-rss_limit_mb=$RSS_LIMIT_MB"
    "-workers=1"
    "-print_final_stats=1"
    "-detect_leaks=$SANITIZER_DETECT_LEAKS"
    "-artifact_prefix=$artifacts_dir/"
  )
  if [ "$target" = "$CAP_TARGET" ]; then
    fuzz_args+=("-max_len=$(cap_max_len)")
  fi

  list_file_names "$artifacts_dir" | sort >"$WORK_DIR/artifacts-pre-$target.txt"

  local started_at ended_at exit_code=0
  started_at="$(date -u +%s)"
  local -a runner=()
  if [ -n "$TIMEOUT_BIN" ]; then
    runner=("$TIMEOUT_BIN" --signal=TERM --kill-after=60 "$((SECS + HARD_LIMIT_SLACK_SECS))s")
  else
    echo "fuzz-campaign: warning: no timeout(1); $target runs without a hard kill" >&2
    runner=(env)
  fi
  if [ -n "$SETSID_BIN" ]; then
    runner=("$SETSID_BIN" "${runner[@]}")
  fi

  set +e
  "${runner[@]}" "$CARGO" "+$TOOLCHAIN" fuzz run --sanitizer="$SANITIZER" "$target" "$corpus_dir" -- "${fuzz_args[@]}" \
    >"$log" 2>&1 &
  CHILD_PID=$!
  wait "$CHILD_PID"
  exit_code=$?
  CHILD_PID=""
  set -e
  ended_at="$(date -u +%s)"
  # The step log is already in the evidence directory; print only the tail so a
  # long run does not push hundreds of thousands of pulse lines into CI output.
  tail -n 120 -- "$log"

  local secs_actual=$((ended_at - started_at))

  list_file_names "$artifacts_dir" | sort >"$WORK_DIR/artifacts-post-$target.txt"
  local new_artifacts
  new_artifacts="$(comm -13 "$WORK_DIR/artifacts-pre-$target.txt" "$WORK_DIR/artifacts-post-$target.txt")"

  # Artifacts the directory already held when the run started and still holds.
  # libFuzzer derives an artifact name from the input that produced it
  # (`leak-<sha1>`, `timeout-<sha1>`, `crash-<sha1>`), so a run that
  # re-discovers an input an earlier campaign left behind rewrites the very
  # same file: the name diff above is empty, yet the target has an artifact.
  # Presence after the run is therefore the test, which also covers the
  # `(name, mtime)` variant and needs no `find -printf`, so the guard holds on
  # a host where the retention trim already degrades.
  local pre_existing_artifacts pre_existing_count
  pre_existing_artifacts="$(comm -12 "$WORK_DIR/artifacts-pre-$target.txt" "$WORK_DIR/artifacts-post-$target.txt")"
  pre_existing_count="$(printf '%s\n' "$pre_existing_artifacts" | awk 'NF { n++ } END { print n + 0 }')"

  local crashes leaks slow_units oom timeouts other total_artifacts
  crashes="$(count_prefix "$new_artifacts" 'crash-')"
  leaks="$(count_prefix "$new_artifacts" 'leak-')"
  slow_units="$(count_prefix "$new_artifacts" 'slow-unit-')"
  oom="$(count_prefix "$new_artifacts" 'oom-')"
  timeouts="$(count_prefix "$new_artifacts" 'timeout-')"
  total_artifacts="$(printf '%s\n' "$new_artifacts" | awk 'NF { n++ } END { print n + 0 }')"
  other=$((total_artifacts - crashes - leaks - slow_units - oom - timeouts))

  if [ -n "$new_artifacts" ]; then
    mkdir -p "$OUT_DIR/artifacts/$target"
    while IFS= read -r artifact; do
      if [ -n "$artifact" ]; then
        cp -- "$artifacts_dir/$artifact" "$OUT_DIR/artifacts/$target/$artifact"
        printf '%s: %s/%s\n' "$target" "$target" "$artifact" >>"$WORK_FINDINGS"
      fi
    done <<<"$new_artifacts"
  fi

  # An artifact that survived the run from before it is named in the record
  # (and in the findings list) but not copied out: it is not this run's
  # evidence, only proof that the target could not start from a clean slate.
  if [ "$pre_existing_count" -gt 0 ]; then
    echo "fuzz-campaign: $target held $pre_existing_count artifact(s) before the run ($artifacts_dir); triage them and clear the directory to earn a clean record" >&2
    while IFS= read -r artifact; do
      if [ -n "$artifact" ]; then
        printf '%s: %s/%s (pre-existing artifact, not added by this run)\n' \
          "$target" "$target" "$artifact" >>"$WORK_FINDINGS"
      fi
    done <<<"$pre_existing_artifacts"
  fi

  local runs exec_per_sec new_units peak_rss result secs_fuzzed
  secs_fuzzed="$(done_secs "$log")"
  runs="$(stat_value "$log" 'stat::number_of_executed_units')"
  if [ "$runs" -eq 0 ]; then
    runs="$(done_runs "$log")"
  fi
  exec_per_sec="$(stat_value "$log" 'stat::average_exec_per_sec')"
  new_units="$(stat_value "$log" 'stat::new_units_added')"
  peak_rss="$(stat_value "$log" 'stat::peak_rss_mb')"

  # Coverage signal (CTX-1880): libFuzzer's own edge (`cov`) and frontier-feature
  # (`ft`) counters from the last pulse line. A read, not a target: the
  # criterion is zero findings, so no numeric coverage floor is asserted here —
  # but an unreadable counter is an absent fact and blocks `clean` below.
  local coverage_edges coverage_features
  coverage_edges="$(coverage_counter "$log" cov)"
  coverage_features="$(coverage_counter "$log" ft)"

  # Corpus digest (CTX-1880): taken over the corpus the run ended with, before
  # the retention trim, so it describes the units that produced the coverage
  # above. The digest is a bare hex string, never a host path.
  local corpus_units corpus_sha
  corpus_units="$(count_seeds "$corpus_dir")"
  corpus_sha="$(corpus_digest "$corpus_dir" || true)"
  if [ -n "$corpus_sha" ]; then
    mkdir -p "$OUT_DIR/corpus"
    printf '%s  %s.corpus  units=%s  algorithm=sha256\n' \
      "$corpus_sha" "$target" "$corpus_units" >"$OUT_DIR/corpus/$target.sha256"
  fi

  # Sanitizer identity (CTX-1880): the flag the driver forwarded is the claim;
  # the binary probe can only contradict it (a located binary without the
  # runtime symbol), never invent one.
  local sanitizer_probe sanitizer_verified=false
  sanitizer_probe="$(probe_sanitizer_build "$target" "$(sanitizer_marker "$SANITIZER")")"
  if [ "$sanitizer_probe" = "found" ]; then
    sanitizer_verified=true
  fi

  # Precedence: an artifact is a finding regardless of the exit status (a leak
  # can exit 0), and so is an artifact that was already there before the run —
  # P0-AC-002 counts findings, not new file names. A non-zero exit without an
  # artifact is a build/runner error; a zero-execution run is inconclusive,
  # never clean.
  if [ "$total_artifacts" -gt 0 ] || [ "$pre_existing_count" -gt 0 ]; then
    result="finding"
  elif [ "$exit_code" -ne 0 ] && [ "$exit_code" -ne 77 ]; then
    result="error"
  elif [ "$exit_code" -eq 77 ]; then
    # libFuzzer's default `-error_exitcode`: it aborted even though no artifact
    # survived. Never downgrade that to `error` ("no finding recorded").
    result="finding"
  elif [ "$runs" -eq 0 ] || [ "$secs_fuzzed" -eq 0 ]; then
    result="inconclusive"
  else
    result="clean"
  fi

  # Evidence completeness (CTX-1880): a run that cannot name its sanitizer
  # build, its corpus, or its coverage is not P0-AC-002 evidence even when it
  # found nothing, so it cannot read as `clean`. Only `clean` is downgraded:
  # findings and runner errors keep their precedence, so missing provenance can
  # never hide one.
  local -a missing=()
  if [ "$SANITIZER" = "none" ]; then
    # A build without a sanitizer can report zero leaks and still say nothing
    # about memory safety, which is the clause the campaign exists to evidence.
    missing+=("sanitizer-none")
  elif [ "$sanitizer_probe" = "mismatch" ]; then
    missing+=("sanitizer-build")
  fi
  if [ -z "$corpus_sha" ]; then
    missing+=("corpus-digest")
  fi
  if [ -z "$coverage_edges" ] || [ -z "$coverage_features" ]; then
    missing+=("coverage")
  fi
  local evidence_missing="-"
  if [ "${#missing[@]}" -gt 0 ]; then
    evidence_missing="$(printf '%s ' "${missing[@]}")"
    evidence_missing="${evidence_missing% }"
    if [ "$result" = "clean" ]; then
      echo "fuzz-campaign: $target recorded inconclusive, not clean: missing evidence ($evidence_missing)" >&2
      result="inconclusive"
    fi
  fi

  local corpus_delta=$(($(count_files "$corpus_dir") - corpus_before))
  local artifact_list="-" pre_existing_list="-"
  if [ -n "$new_artifacts" ]; then
    artifact_list="$(printf '%s\n' "$new_artifacts" | paste -sd ' ' -)"
  fi
  if [ -n "$pre_existing_artifacts" ]; then
    pre_existing_list="$(printf '%s\n' "$pre_existing_artifacts" | paste -sd ' ' -)"
  fi

  record_target "$target" "$SECS" "$secs_actual" "$secs_fuzzed" "$runs" "$exec_per_sec" \
    "$new_units" "$corpus_seeds" "$corpus_delta" "$peak_rss" "$crashes" "$leaks" \
    "$slow_units" "$oom" "$timeouts" "$other" "$exit_code" "$result" "$artifact_list" \
    "$pre_existing_count" "$pre_existing_list" "$SANITIZER" "$sanitizer_probe" \
    "$sanitizer_verified" "${corpus_sha:--}" "$corpus_units" "${coverage_edges:--}" \
    "${coverage_features:--}" "$evidence_missing"

  write_summaries "$STARTED_UTC"
  trim_corpus "$corpus_dir" || echo "fuzz-campaign: warning: corpus trim failed for $target" >&2

  echo "fuzz-campaign: $target -> $result (fuzzed=${secs_fuzzed}s runs=$runs cov=${coverage_edges:--} ft=${coverage_features:--} crashes=$crashes leaks=$leaks slow_units=$slow_units oom=$oom timeouts=$timeouts pre_existing=$pre_existing_count digest=${corpus_sha:0:12} sanitizer=$SANITIZER/$sanitizer_probe exit=$exit_code)"
}

main() {
  trap 'cleanup $?' EXIT
  trap 'cleanup 130' INT
  trap 'cleanup 143' TERM
  trap 'cleanup 129' HUP

  parse_args "$@"

  if [ "$PRINT_CARGO_FUZZ_VERSION" -eq 1 ]; then
    printf '%s\n' "$CARGO_FUZZ_VERSION"
    exit 0
  fi
  if [ "$LIST_TARGETS" -eq 1 ]; then
    list_targets
    exit 0
  fi

  is_uint "$SECS" || die "--secs must be an integer number of seconds, got: $SECS"
  is_uint "$TIMEOUT_SECS" || die "--timeout must be an integer number of seconds, got: $TIMEOUT_SECS"
  is_uint "$RSS_LIMIT_MB" || die "--rss-limit-mb must be an integer, got: $RSS_LIMIT_MB"

  SECS="$(clamp "$SECS" "$MIN_SECS" "$MAX_SECS")"
  TIMEOUT_SECS="$(clamp "$TIMEOUT_SECS" "$MIN_TIMEOUT_SECS" "$MAX_TIMEOUT_SECS")"
  RSS_LIMIT_MB="$(clamp "$RSS_LIMIT_MB" "$MIN_RSS_LIMIT_MB" "$MAX_RSS_LIMIT_MB")"

  # Sanitizer identity (CTX-1880): pinned, validated, and derived once so the
  # forwarded flag, the record, and the leak-detection claim cannot disagree.
  valid_sanitizer "$SANITIZER" ||
    die "unknown sanitizer '$SANITIZER' (known: address, leak, memory, thread, none)"
  if [ "$SANITIZER" = "none" ]; then
    SANITIZER_INSTRUMENTATION="none (no sanitizer runtime linked)"
  else
    SANITIZER_INSTRUMENTATION="-Zsanitizer=$SANITIZER"
  fi
  SANITIZER_LEAK_DETECTION="$(sanitizer_leak_detection "$SANITIZER")"
  SANITIZER_LEAK_SOURCE="none"
  SANITIZER_DETECT_LEAKS=0
  if [ "$SANITIZER_LEAK_DETECTION" = "lsan" ]; then
    SANITIZER_LEAK_SOURCE="asan-runtime"
    SANITIZER_DETECT_LEAKS=1
  fi

  local all_targets selected=""
  all_targets="$(list_targets)"
  [ -n "$all_targets" ] || die "no fuzz targets found in $MANIFEST"

  if [ -z "$TARGETS" ]; then
    selected="$all_targets"
  else
    local -a requested=()
    IFS=',' read -r -a requested <<<"$TARGETS"
    local candidate
    for candidate in "${requested[@]}"; do
      if ! printf '%s\n' "$all_targets" | grep -Fxq -- "$candidate"; then
        die "unknown fuzz target: $candidate (known: $(printf '%s' "$all_targets" | paste -sd, -))"
      fi
      selected="$selected$candidate"$'\n'
    done
    selected="${selected%$'\n'}"
  fi

  local selected_count
  selected_count="$(printf '%s\n' "$selected" | awk 'NF { n++ } END { print n + 0 }')"

  OUT_DIR="${OUT_DIR:-$DEFAULT_OUT_DIR/$(date -u +%Y%m%dT%H%M%SZ)}"
  STARTED_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "fuzz-campaign: dry-run"
    echo "  targets: $(printf '%s' "$selected" | paste -sd, -)"
    echo "  secs: $SECS"
    echo "  libfuzzer: -timeout=$TIMEOUT_SECS -rss_limit_mb=$RSS_LIMIT_MB -workers=1 -print_final_stats=1 -detect_leaks=$SANITIZER_DETECT_LEAKS"
    echo "  sanitizer: $SANITIZER (--sanitizer=$SANITIZER, $SANITIZER_INSTRUMENTATION, leak detection: $SANITIZER_LEAK_DETECTION)"
    echo "  evidence: sanitizer identity, corpus sha256, and cov/ft counters per target (a missing fact downgrades clean to inconclusive)"
    echo "  toolchain: $TOOLCHAIN"
    echo "  cargo-fuzz (CI pin): $CARGO_FUZZ_VERSION"
    echo "  corpus-dir: $CORPUS_DIR"
    echo "  artifacts-dir: $ARTIFACTS_ROOT"
    echo "  out-dir: $OUT_DIR"
    local seeds_dir
    while IFS= read -r target; do
      seeds_dir="$(seed_source "$target")"
      if [ -d "$seeds_dir" ]; then
        echo "  $target: seeds=$(count_seeds "$seeds_dir") from committed corpus"
      else
        echo "  $target: no committed corpus (scratch corpus only)"
      fi
      if [ "$target" = "$CAP_TARGET" ]; then
        echo "  $target: -max_len=$(cap_max_len) from $(basename "$CAP_MAX_LEN_FILE")"
      fi
    done <<<"$selected"
    exit 0
  fi

  mkdir -p "$OUT_DIR" "$OUT_DIR/logs"
  WORK_DIR="$OUT_DIR/.work"
  WORK_TARGETS="$OUT_DIR/targets.tsv"
  WORK_FINDINGS="$WORK_DIR/findings.txt"
  mkdir -p "$WORK_DIR"
  : >"$WORK_TARGETS"
  : >"$WORK_FINDINGS"

  REVISION="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
  REVISION_DIRTY=false
  if ! git -C "$REPO_ROOT" diff --quiet --ignore-submodules HEAD 2>/dev/null; then
    REVISION_DIRTY=true
  fi

  command -v "$CARGO" >/dev/null 2>&1 || env_die "cargo not found on PATH"
  TOOLCHAIN_VERSION="$("$CARGO" "+$TOOLCHAIN" --version 2>&1)" ||
    env_die "toolchain '$TOOLCHAIN' is not installed (rustup toolchain install $TOOLCHAIN)"
  CARGO_FUZZ_DETECTED="$("$CARGO" fuzz --version 2>&1)" ||
    env_die "cargo-fuzz is not installed (cargo install --locked --version $CARGO_FUZZ_VERSION cargo-fuzz)"

  echo "fuzz-campaign: $selected_count target(s), $SECS s each"
  echo "fuzz-campaign: $TOOLCHAIN_VERSION / $CARGO_FUZZ_DETECTED"
  echo "fuzz-campaign: evidence -> $OUT_DIR"

  local target
  while IFS= read -r target; do
    [ -n "$target" ] || continue
    run_target "$target"
  done <<<"$selected"

  rm -rf -- "$WORK_DIR"

  local result
  result="$(awk -F '\t' '
    $18 == "finding" { finding = 1 }
    $18 == "error" { error = 1 }
    $18 == "inconclusive" { inconclusive = 1 }
    END { print finding ? "finding" : (error ? "error" : (inconclusive ? "inconclusive" : "clean")) }
  ' "$WORK_TARGETS")"

  echo "fuzz-campaign: $result — evidence at $OUT_DIR/summary.json"
  case "$result" in
  clean) exit 0 ;;
  finding) exit 1 ;;
  inconclusive) exit 4 ;;
  *) exit 3 ;;
  esac
}

main "$@"
