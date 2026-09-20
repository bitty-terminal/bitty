#!/usr/bin/env bash
# m1-matrix.sh — CTX-0574 M1 compatibility matrix runner and Tier 1 aggregator.
#
# The accepted compatibility-milestone-rfc.md requires every M1 Required item
# verified on each ADR-0002 Tier 1 platform in CI. The M1 evidence suites were
# only implicit members of the blanket `cargo test --workspace` invocation, so
# a per-platform regression was invisible in the job pile and a renamed or
# emptied suite could stop running without failing anything. This driver runs
# each suite by name on every platform leg, asserts it actually executed
# tests, and aggregates the legs into one pass/fail gate.
#
# Headless / no platform-conditional normalization:
#   - m1_mode_golden / m1_color_golden drive `Parser -> State` and pin
#     `State::state_hash`; no window, font, GPU, RNG, or display participates.
#   - m1_mode_input drives runtime input encoding only (no frame).
#   - m1_color_title uses `Surface::headless` plus the deterministic headless
#     rasterizer, whose frames are byte-identical on Linux/macOS/Windows.
#   - m1_shell_coverage spawns each installed M1 roster shell (bash/zsh/fish/
#     PowerShell/cmd/nushell) through the real PTY with zero integration and
#     injects OSC 7/133; a shell absent from a leg skips with a recorded
#     reason (the suite still executes every test, so the floor holds), and
#     the suite fails a leg whose runner resolves no roster shell at all.
#   No Tier 1 platform therefore skips a suite and there is deliberately no
#   skip list. If a future suite genuinely cannot run somewhere it fails here
#   and needs an explicit, reviewed exemption instead of a silent omission.
#
# The corpus `.bin` fixtures rely on the repository `.gitattributes` `eol=lf`
# policy; they contain no CR byte, so they are byte-stable on every platform's
# checkout. A future fixture that embeds a literal CR must be marked binary.
#
# Usage:
#   m1-matrix.sh run --platform <id> [--out <file>] [--summary <file>]
#       Run the four M1 suites, write the TSV, print the per-platform table,
#       and append it to <summary>.
#   m1-matrix.sh aggregate [--dir <dir>] [--summary <file>]
#       Require the complete Tier 1 matrix; print and (optionally) append the
#       aggregated per-platform table. `--summary` defaults to
#       $GITHUB_STEP_SUMMARY when set.
#   m1-matrix.sh platforms
#   m1-matrix.sh suites
#
# Environment (aggregate only): M1_JOB_<PLATFORM> carries `needs.<job>.result`
# from the workflow so a platform whose job failed before this step still
# yields a precise failure reason.
#
# Exit codes: 0 matrix clean, 1 matrix failure, 2 usage error.
set -euo pipefail

# --- Single source of truth -------------------------------------------------
# `<suite>|<package>|<min tests>`. The minimum is the anti-silent-omission
# guard: `cargo test --test <name>` exits 0 when the target exists but holds
# zero tests, so a suite that lost its tests would otherwise vanish without
# failing CI. Floors only grow as coverage is added.
M1_SUITES=(
  "m1_mode_golden|bitty-compat-lab|10"
  "m1_color_golden|bitty-compat-lab|7"
  "m1_mode_input|bitty-runtime|2"
  "m1_color_title|bitty-runtime|3"
  "m1_shell_coverage|bitty-runtime|15"
)

# ADR-0002 Tier 1 platform legs as wired in .github/workflows/ci.yml.
M1_PLATFORMS=(linux-x11 linux-wayland macos windows)

usage() {
  cat <<'EOF'
usage: m1-matrix.sh <command> [options]

commands:
  run --platform <id> [--out <file>] [--summary <file>]
      Run the five M1 evidence suites for one platform and write a TSV.
      `--summary` (default: $GITHUB_STEP_SUMMARY) additionally appends the
      per-platform table to that file.
  aggregate [--dir <dir>] [--summary <file>]
      Require the complete Tier 1 matrix and print/append the summary.
  platforms
      Print the expected Tier 1 platform ids, one per line.
  suites
      Print "<suite>\t<package>\t<min-tests>" per M1 suite.
EOF
}

job_result_var() {
  case "$1" in
  linux-x11) printf 'M1_JOB_LINUX_X11' ;;
  linux-wayland) printf 'M1_JOB_LINUX_WAYLAND' ;;
  macos) printf 'M1_JOB_MACOS' ;;
  windows) printf 'M1_JOB_WINDOWS' ;;
  *) return 1 ;;
  esac
}

cmd_platforms() {
  local p
  for p in "${M1_PLATFORMS[@]}"; do
    printf '%s\n' "$p"
  done
}

cmd_suites() {
  local entry suite pkg min
  for entry in "${M1_SUITES[@]}"; do
    IFS='|' read -r suite pkg min <<<"$entry"
    printf '%s\t%s\t%s\n' "$suite" "$pkg" "$min"
  done
}

# run: execute every M1 suite on one platform, write the result TSV, and print
# a per-platform Markdown table so the job log shows the matrix leg directly.
cmd_run() {
  local platform="local" out="" summary="${M1_SUMMARY:-${GITHUB_STEP_SUMMARY:-}}"
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
    --summary)
      summary="${2:?--summary requires a value}"
      shift 2
      ;;
    *)
      echo "m1-matrix: unknown run option '$1'" >&2
      usage >&2
      return 2
      ;;
    esac
  done
  [ -n "$out" ] || out="m1-results/${platform}.tsv"
  mkdir -p "$(dirname "$out")"
  : >"$out"

  # The `.bin` corpus paths are resolved from `CARGO_MANIFEST_DIR`, so `run`
  # works from any checkout; only the `cargo` invocation below is repo-root
  # relative and cargo resolves the manifest upward on its own.
  local entry suite pkg min log rc passed failed ignored status
  local failures=0 table
  table="$(mktemp)"
  {
    printf '### M1 compatibility matrix — `%s`\n\n' "$platform"
    printf '| Suite | Package | Result | Passed | Failed | Ignored |\n'
    printf '| ----- | ------- | ------ | ------ | ------ | ------- |\n'
  } >"$table"
  for entry in "${M1_SUITES[@]}"; do
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
      status="fail"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$platform" "$suite" "$status" "$passed" "$failed" "$ignored" >>"$out"
    printf '| `%s` | %s | %s | %s | %s | %s |\n' "$suite" "$pkg" "$status" "$passed" "$failed" "$ignored" >>"$table"
    if [ "$status" != "pass" ]; then
      failures=$((failures + 1))
      printf '\nfailed suite `%s` (rc=%s, min=%s, ignored=%s); cargo output:\n\n```text\n%s\n```\n' \
        "$suite" "$rc" "$min" "$ignored" "$log" >&2
    fi
  done
  printf '\n' >>"$table"
  cat "$table"
  if [ -n "$summary" ]; then
    cat "$table" >>"$summary"
  fi
  rm -f "$table"
  if ((failures > 0)); then
    printf 'm1-matrix: %s FAIL (%s/%s suites failed)\n' "$platform" "$failures" "${#M1_SUITES[@]}" >&2
    return 1
  fi
  printf 'm1-matrix: %s PASS (%s suites)\n' "$platform" "${#M1_SUITES[@]}"
}

# aggregate: stitch every platform TSV into one table and fail unless each
# Tier 1 leg ran all suites and passed.
cmd_aggregate() {
  local dir="m1-results" summary="${M1_SUMMARY:-${GITHUB_STEP_SUMMARY:-}}"
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
      echo "m1-matrix: unknown aggregate option '$1'" >&2
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
  for p in "${M1_PLATFORMS[@]}"; do
    v="$(job_result_var "$p")"
    local result="${!v:-}"
    if [ -n "$result" ] && [ "$result" != "success" ]; then
      problems="${problems}${p}: platform job result=${result}"$'\n'
    fi
    local entry
    for entry in "${M1_SUITES[@]}"; do
      suite="${entry%%|*}"
      status="$(awk -F'\t' -v p="$p" -v s="$suite" '$1 == p && $2 == s { print $3; exit }' "$combined")"
      if [ -z "$status" ]; then
        problems="${problems}${p}: missing result for ${suite}"$'\n'
      elif [ "$status" != "pass" ]; then
        problems="${problems}${p}: ${suite} status=${status}"$'\n'
      fi
    done
  done

  local plats="${M1_PLATFORMS[*]}" suites=""
  for entry in "${M1_SUITES[@]}"; do
    suites="${suites}${suites:+ }${entry%%|*}"
  done

  local table
  table="$(awk -F'\t' -v plats="$plats" -v suites="$suites" '
		BEGIN { np = split(plats, P, " "); ns = split(suites, S, " ") }
		{ st[$1 SUBSEP $2] = $3 }
		END {
			printf "## M1 compatibility matrix (Tier 1)\n\n"
			printf "Every M1 evidence suite on every ADR-0002 Tier 1 platform.\n\n"
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
        printf '\n### M1 matrix problems\n\n```text\n%s```\n' "$problems"
      } >>"$summary"
    fi
  fi

  if [ -n "$problems" ]; then
    printf 'm1-matrix: aggregate FAIL\n%s' "$problems" >&2
    return 1
  fi
  printf 'm1-matrix: aggregate PASS (%s platforms x %s suites)\n' "${#M1_PLATFORMS[@]}" "${#M1_SUITES[@]}"
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
    echo "m1-matrix: unknown command '$command'" >&2
    usage >&2
    return 2
    ;;
  esac
}

main "$@"
