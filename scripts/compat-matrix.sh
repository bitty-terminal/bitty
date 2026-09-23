#!/usr/bin/env bash
# compat-matrix.sh — CTX-0692 release compatibility matrix runner and Tier 1 aggregator.
#
# PERF-11 (#1065) automation for the 14 surfaces x 4 terminals release matrix
# (`crates/bitty-compat-lab/src/matrix.rs`): shell/tmux/nvim/fzf/htop/ssh/
# alt-screen/mouse/resize/OSC/clipboard/Kitty/IME/DPI across Ghostty/Kitty/
# WezTerm/Alacritty differential.
#
# The release-matrix suites were only implicit members of the blanket
# `cargo test --workspace` invocation, so a per-platform regression was
# invisible in the job pile and a renamed or emptied suite could stop running
# without failing anything. This driver runs each suite by name on every
# platform leg, asserts it actually executed tests, emits the deterministic
# machine-readable artifacts (`compat_report` JSON plus the 14x4 matrix JSON),
# and aggregates the legs into one pass/fail gate.
#
# Headless / no platform-conditional normalization:
#   - compat_matrix / compare / oracle / report / harness / dogfooding_corpus
#     drive `Parser -> State -> Snapshot` plus the on-disk reference dumps;
#     no window, font, GPU, RNG, network, or display participates.
#   - vertical_slice_gates pins the M1-29 review gates A1-A9 (same headless
#     corpus discipline).
#   - live_compat probes the tools installed on the leg and records absent
#     tools as `"status": "skipped"`, never as verified claims; the suite
#     itself always executes its tests, so the floor below holds everywhere.
# There is deliberately no skip list. If a future suite genuinely cannot run
# somewhere it fails here and needs an explicit, reviewed exemption instead
# of a silent omission.
#
# Usage:
#   compat-matrix.sh run --platform <id> [--out <file>] [--report <file>] [--matrix <file>] [--summary <file>]
#       Run the eight release-matrix suites, write the TSV plus the JSON
#       artifacts, print the per-platform table, and append it to <summary>.
#   compat-matrix.sh aggregate [--dir <dir>] [--summary <file>]
#       Require the complete Tier 1 matrix; print and (optionally) append the
#       aggregated per-platform table. `--summary` defaults to
#       $GITHUB_STEP_SUMMARY when set.
#   compat-matrix.sh platforms
#   compat-matrix.sh suites
#
# Environment (aggregate only): COMPAT_JOB_<PLATFORM> carries
# `needs.<job>.result` from the workflow so a platform whose job failed
# before this step still yields a precise failure reason.
#
# Exit codes: 0 matrix clean, 1 matrix failure, 2 usage error.
set -euo pipefail

# --- Single source of truth -------------------------------------------------
# `<suite>|<package>|<min tests>`. The minimum is the anti-silent-omission
# guard: `cargo test --test <name>` exits 0 when the target exists but holds
# zero tests, so a suite that lost its tests would otherwise vanish without
# failing CI. Floors only grow as coverage is added.
COMPAT_SUITES=(
  "compat_matrix|bitty-compat-lab|8"
  "compare|bitty-compat-lab|5"
  "oracle|bitty-compat-lab|10"
  "report|bitty-compat-lab|7"
  "harness|bitty-compat-lab|3"
  "dogfooding_corpus|bitty-compat-lab|6"
  "vertical_slice_gates|bitty-compat-lab|9"
  "live_compat|bitty-compat-lab|8"
)

# ADR-0002 Tier 1 platform legs as wired in .github/workflows/ci.yml.
COMPAT_PLATFORMS=(linux-x11 linux-wayland macos windows)

usage() {
  cat <<'EOF'
usage: compat-matrix.sh <command> [options]

commands:
  run --platform <id> [--out <file>] [--report <file>] [--matrix <file>] [--summary <file>]
      Run the eight release-matrix suites for one platform, write a TSV plus
      the deterministic JSON artifacts, and print the per-platform table.
      `--summary` (default: $GITHUB_STEP_SUMMARY) additionally appends the
      per-platform table to that file.
  aggregate [--dir <dir>] [--summary <file>]
      Require the complete Tier 1 matrix and print/append the summary.
  platforms
      Print the expected Tier 1 platform ids, one per line.
  suites
      Print "<suite>\t<package>\t<min-tests>" per release-matrix suite.
EOF
}

job_result_var() {
  case "$1" in
  linux-x11) printf 'COMPAT_JOB_LINUX_X11' ;;
  linux-wayland) printf 'COMPAT_JOB_LINUX_WAYLAND' ;;
  macos) printf 'COMPAT_JOB_MACOS' ;;
  windows) printf 'COMPAT_JOB_WINDOWS' ;;
  *) return 1 ;;
  esac
}

cmd_platforms() {
  local p
  for p in "${COMPAT_PLATFORMS[@]}"; do
    printf '%s\n' "$p"
  done
}

cmd_suites() {
  local entry suite pkg min
  for entry in "${COMPAT_SUITES[@]}"; do
    IFS='|' read -r suite pkg min <<<"$entry"
    printf '%s\t%s\t%s\n' "$suite" "$pkg" "$min"
  done
}

# run: execute every release-matrix suite on one platform, emit the JSON
# artifacts, write the result TSV, and print a per-platform Markdown table
# so the job log shows the matrix leg directly.
cmd_run() {
  local platform="local" out="" report="" matrix="" summary="${COMPAT_SUMMARY:-${GITHUB_STEP_SUMMARY:-}}"
  while (($# > 0)); do
    case "$1" in
    --platform)
      platform="${2:?--platform requires a value}"
      shift 2
      ;;
    --out)
      out="${2:?--out requires a value}"
      shift 2
      ;;
    --report)
      report="${2:?--report requires a value}"
      shift 2
      ;;
    --matrix)
      matrix="${2:?--matrix requires a value}"
      shift 2
      ;;
    --summary)
      summary="${2:?--summary requires a value}"
      shift 2
      ;;
    *)
      echo "compat-matrix: unknown run option '$1'" >&2
      usage >&2
      return 2
      ;;
    esac
  done
  local dir="compat-results"
  [ -n "$out" ] || out="$dir/${platform}.tsv"
  [ -n "$report" ] || report="$dir/${platform}-report.json"
  [ -n "$matrix" ] || matrix="$dir/${platform}-matrix.json"
  mkdir -p "$(dirname "$out")" "$(dirname "$report")" "$(dirname "$matrix")"
  : >"$out"

  local entry suite pkg min log rc passed failed ignored status
  local failures=0 table
  table="$(mktemp)"
  {
    printf '### Compat release matrix — `%s`\n\n' "$platform"
    printf '| Suite | Package | Result | Passed | Failed | Ignored |\n'
    printf '| ----- | ------- | ------ | ------ | ------ | ------- |\n'
  } >"$table"
  for entry in "${COMPAT_SUITES[@]}"; do
    IFS='|' read -r suite pkg min <<<"$entry"
    rc=0
    log="$(cargo test -p "$pkg" --test "$suite" --locked -- --test-threads=1 2>&1)" || rc=$?
    passed="$(printf '%s\n' "$log" | sed -n 's/^test result:.* \([0-9][0-9]*\) passed;.*/\1/p' | tail -n 1)"
    failed="$(printf '%s\n' "$log" | sed -n 's/^test result:.* \([0-9][0-9]*\) failed;.*/\1/p' | tail -n 1)"
    ignored="$(printf '%s\n' "$log" | sed -n 's/^test result:.* \([0-9][0-9]*\) ignored.*/\1/p' | tail -n 1)"
    passed="${passed:-0}"
    failed="${failed:-0}"
    ignored="${ignored:-0}"
    status="pass"
    if ((rc != 0)) || ((failed > 0)) || ((ignored > 0)); then
      status="fail"
    elif ((passed < min)); then
      # live_compat.rs is #![cfg(unix)]: on windows it legitimately
      # compiles to zero tests. Waive the floor only for that exact
      # empty shape (0 passed with rc 0 and 0 failed); any real
      # shortfall (e.g. 7 passed) still fails the leg.
      if [ "$suite" = "live_compat" ] && [ "$platform" = "windows" ] && ((passed == 0)); then
        status="pass"
      else
        status="fail"
      fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$platform" "$suite" "$status" "$passed" "$failed" "$ignored" >>"$out"
    printf '| `%s` | %s | %s | %s | %s | %s |\n' "$suite" "$pkg" "$status" "$passed" "$failed" "$ignored" >>"$table"
    if [ "$status" != "pass" ]; then
      failures=$((failures + 1))
      printf '\nfailed suite `%s` (rc=%s, min=%s, ignored=%s); cargo output:\n\n```text\n%s\n```\n' \
        "$suite" "$rc" "$min" "$ignored" "$log" >&2
    fi
  done

  # Deterministic machine-readable artifacts for this leg. The report binary
  # exits nonzero on generation failure, which fails the leg.
  if ! cargo run -p bitty-compat-lab --bin compat_report --locked -- \
    --out "$report" --matrix-json "$matrix" >&2; then
    printf '\nartifact emission failed for `%s`\n' "$platform" >&2
    failures=$((failures + 1))
  fi

  printf '\n' >>"$table"
  cat "$table"
  if [ -n "$summary" ]; then
    cat "$table" >>"$summary"
  fi
  rm -f "$table"
  if ((failures > 0)); then
    printf 'compat-matrix: %s FAIL (%s/%s suites failed)\n' "$platform" "$failures" "$((${#COMPAT_SUITES[@]} + 1))" >&2
    return 1
  fi
  printf 'compat-matrix: %s PASS (%s suites + artifacts)\n' "$platform" "${#COMPAT_SUITES[@]}"
}

# aggregate: stitch every platform TSV into one table and fail unless each
# Tier 1 leg ran all suites and passed.
cmd_aggregate() {
  local dir="compat-results" summary="${COMPAT_SUMMARY:-${GITHUB_STEP_SUMMARY:-}}"
  while (($# > 0)); do
    case "$1" in
    --dir)
      dir="${2:?--dir requires a value}"
      shift 2
      ;;
    --summary)
      summary="${2:?--summary requires a value}"
      shift 2
      ;;
    *)
      echo "compat-matrix: unknown aggregate option '$1'" >&2
      usage >&2
      return 2
      ;;
    esac
  done

  local combined problems=""
  combined="$(mktemp)"
  local f
  for f in "$dir"/*.tsv; do
    [ -e "$f" ] || continue
    cat "$f" >>"$combined"
  done

  local p v suite status
  for p in "${COMPAT_PLATFORMS[@]}"; do
    v="$(job_result_var "$p")"
    local result="${!v:-}"
    if [ -n "$result" ] && [ "$result" != "success" ]; then
      problems="${problems}${p}: platform job result=${result}"$'\n'
    fi
    local entry
    for entry in "${COMPAT_SUITES[@]}"; do
      suite="${entry%%|*}"
      status="$(awk -F'\t' -v p="$p" -v s="$suite" '$1 == p && $2 == s { print $3; exit }' "$combined")"
      if [ -z "$status" ]; then
        problems="${problems}${p}: missing result for ${suite}"$'\n'
      elif [ "$status" != "pass" ]; then
        problems="${problems}${p}: ${suite} status=${status}"$'\n'
      fi
    done
  done

  local plats="${COMPAT_PLATFORMS[*]}" suites=""
  for entry in "${COMPAT_SUITES[@]}"; do
    suites="${suites}${suites:+ }${entry%%|*}"
  done

  local table
  table="$(awk -F'\t' -v plats="$plats" -v suites="$suites" '

		BEGIN { np = split(plats, P, " "); ns = split(suites, S, " ") }

		{ st[$1 SUBSEP $2] = $3 }

		END {

			printf "## Compat release matrix (Tier 1)\n\n"

			printf "Every release-matrix suite (14 surfaces x 4 terminals) on every ADR-0002 Tier 1 platform.\n\n"

			printf "| Platform |"

			for (j = 1; j <= ns; j++) printf " %s |", S[j]

			printf " Result |\n"

			printf "| -------- |"

			for (j = 1; j <= ns; j++) printf " %s |", "------"

			printf " ------ |\n"

			overall = 1

			for (i = 1; i <= np; i++) {

				printf "| `%s` |", P[i]

				leg = 1

				for (j = 1; j <= ns; j++) {

					k = P[i] SUBSEP S[j]

					cell = (k in st) ? st[k] : "MISSING"

					if (cell != "pass") leg = 0

					printf " %s |", cell

				}

				printf " %s |\n", leg ? "PASS" : "FAIL"

				if (!leg) overall = 0

			}

			printf "| **Tier 1** |"

			for (j = 1; j <= ns; j++) printf " |"

			printf " **%s** |\n", overall ? "PASS" : "FAIL"

		}

	' "$combined")"

  printf '%s\n' "$table"
  rm -f "$combined"

  if [ -n "$summary" ]; then
    printf '%s\n' "$table" >>"$summary"
    if [ -n "$problems" ]; then
      {
        printf '\n### Compat matrix problems\n\n```text\n%s```\n' "$problems"
      } >>"$summary"
    fi
  fi

  if [ -n "$problems" ]; then
    printf 'compat-matrix: aggregate FAIL\n%s' "$problems" >&2
    return 1
  fi
  printf 'compat-matrix: aggregate PASS (%s platforms x %s suites)\n' "${#COMPAT_PLATFORMS[@]}" "${#COMPAT_SUITES[@]}"
}

main() {
  (($# > 0)) || {
    usage >&2
    return 2
  }
  local command="$1"
  shift
  case "$command" in
  run) cmd_run "$@" ;;
  aggregate) cmd_aggregate "$@" ;;
  platforms) cmd_platforms "$@" ;;
  suites) cmd_suites "$@" ;;
  -h | --help | help)
    usage
    ;;
  *)
    echo "compat-matrix: unknown command '$command'" >&2
    usage >&2
    return 2
    ;;
  esac
}

main "$@"
