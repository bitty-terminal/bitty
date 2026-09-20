#!/usr/bin/env bash
# m1-matrix.test.sh — CTX-0574 fixture test for the M1 matrix driver.
#
# Runs the driver's pure/reporting paths against a fake `cargo` and synthetic
# TSVs: no workspace build, no real M1 suite, and no network. The real suites
# are exercised by `cargo test --workspace` and by the CI platform legs.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/m1-matrix.sh
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

expect_exit() { # <exit> <label> <args...>
  local want="$1" label="$2"
  shift 2
  local status=0 out
  out="$("$SCRIPT" "$@" 2>&1)" || status=$?
  if ((status != want)); then
    echo "FAIL: $label: expected exit $want, got $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# 1. Discovery commands: the Tier 1 roster and the suite floor table.
platforms="$("$SCRIPT" platforms)"
want_platforms=$'linux-x11\nlinux-wayland\nmacos\nwindows'
if [[ "$platforms" != "$want_platforms" ]]; then
  echo "FAIL: platforms: unexpected roster" >&2
  printf '%s\n' "$platforms" >&2
  FAIL=1
fi

suites="$("$SCRIPT" suites)"
want_suites=$'m1_mode_golden\tbitty-compat-lab\t10\nm1_color_golden\tbitty-compat-lab\t7\nm1_mode_input\tbitty-runtime\t2\nm1_color_title\tbitty-runtime\t3\nm1_shell_coverage\tbitty-runtime\t15'
if [[ "$suites" != "$want_suites" ]]; then
  echo "FAIL: suites: unexpected table" >&2
  printf '%s\n' "$suites" >&2
  FAIL=1
fi

# 2. Usage errors exit 2.
expect_exit 2 'no command'
expect_exit 2 'unknown command' bogus
expect_exit 2 'unknown run option' run --platform linux-x11 --bogus
expect_exit 2 'unknown aggregate option' aggregate --bogus

# 3. `run` against a fake cargo, one case per suite outcome. The shim keys off
#    the `--test <name>` argument and prints a `test result:` line.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
suite=""
while (($# > 0)); do
	case "$1" in
	--test)
		suite="$2"
		shift 2
		;;
	*) shift ;;
	esac
done
case "$suite" in
m1_mode_golden) echo "test result: ok. 10 passed; 0 failed; 0 ignored" ;;
m1_color_golden) echo "test result: ok. 7 passed; 0 failed; 0 ignored" ;;
m1_mode_input) echo "test result: ok. 2 passed; 0 failed; 0 ignored" ;;
m1_shell_coverage) echo "test result: ok. 15 passed; 0 failed; 0 ignored" ;;
m1_color_title)
	if [[ "${FAKE_M1_FAIL:-}" == "color-title" ]]; then
		echo "test result: FAILED. 2 passed; 1 failed; 0 ignored"
		exit 1
	fi
	if [[ "${FAKE_M1_FAIL:-}" == "ignored" ]]; then
		echo "test result: ok. 2 passed; 0 failed; 1 ignored"
		exit 0
	fi
	echo "test result: ok. 3 passed; 0 failed; 0 ignored"
	;;
*) echo "no such test target: $suite" >&2; exit 101 ;;
esac
SH
chmod +x "$TMP/bin/cargo"
export PATH="$TMP/bin:$PATH"

# 3a. All suites green -> exit 0 and a PASS table row per suite.
if ! out="$("$SCRIPT" run --platform linux-x11 --out "$TMP/green.tsv" 2>&1)"; then
  echo "FAIL: run green exited non-zero" >&2
  printf '%s\n' "$out" >&2
  FAIL=1
fi
for needle in 'm1_mode_golden' 'm1_color_golden' 'm1_mode_input' 'm1_color_title' 'm1_shell_coverage' \
  '| pass |' 'linux-x11 PASS (5 suites)'; do
  if ! grep -qF -- "$needle" <<<"$out"; then
    echo "FAIL: run green missing '$needle'" >&2
    FAIL=1
  fi
done

# 3b. A failing suite -> exit 1 and a fail row.
if FAKE_M1_FAIL=color-title "$SCRIPT" run --platform macos --out "$TMP/red.tsv" >/dev/null 2>&1; then
  echo "FAIL: run with a failing suite exited 0" >&2
  FAIL=1
fi
if ! awk -F'\t' '$1=="macos" && $2=="m1_color_title" && $3=="fail"' "$TMP/red.tsv" | grep -q .; then
  echo "FAIL: failing suite not recorded as fail" >&2
  cat "$TMP/red.tsv" >&2
  FAIL=1
fi

# 3c. The anti-silent-omission floor: a target that exits 0 with zero tests
#     must still fail the leg.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
echo "test result: ok. 0 passed; 0 failed; 0 ignored"
SH
chmod +x "$TMP/bin/cargo"
if "$SCRIPT" run --platform windows --out "$TMP/empty.tsv" >/dev/null 2>&1; then
  echo "FAIL: zero-test suite was accepted" >&2
  FAIL=1
fi

# 3d. An ignored test is a silent skip and must fail the leg too.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
echo "test result: ok. 20 passed; 0 failed; 2 ignored"
SH
chmod +x "$TMP/bin/cargo"
if "$SCRIPT" run --platform windows --out "$TMP/ignored.tsv" >/dev/null 2>&1; then
  echo "FAIL: ignored tests were accepted" >&2
  FAIL=1
fi

# 4. Aggregation. Synthesize a complete matrix from the green TSV and check
#    PASS; then drop/alter rows and check each failure mode.
agg="$TMP/agg"
mkdir -p "$agg"
full="$(cat "$TMP/green.tsv")"
for p in linux-x11 linux-wayland macos windows; do
  printf '%s\n' "$full" | awk -F'\t' -v p="$p" 'BEGIN{OFS="\t"}{$1=p; print}' >"$agg/$p.tsv"
done

if ! out="$("$SCRIPT" aggregate --dir "$agg" --summary "$TMP/summary.md" 2>&1)"; then
  echo "FAIL: aggregate complete matrix exited non-zero" >&2
  printf '%s\n' "$out" >&2
  FAIL=1
fi
if ! grep -qF -- '**PASS**' <<<"$out"; then
  echo "FAIL: aggregate did not report overall PASS" >&2
  printf '%s\n' "$out" >&2
  FAIL=1
fi
if ! grep -qF -- 'M1 compatibility matrix (Tier 1)' "$TMP/summary.md"; then
  echo "FAIL: aggregate did not write the step summary" >&2
  FAIL=1
fi

# 4a. A missing platform leg fails the aggregate.
rm -f "$agg/windows.tsv"
if "$SCRIPT" aggregate --dir "$agg" >/dev/null 2>&1; then
  echo "FAIL: aggregate accepted a missing platform" >&2
  FAIL=1
fi
printf '%s\n' "$full" | awk -F'\t' -v p=windows 'BEGIN{OFS="\t"}{$1=p; print}' >"$agg/windows.tsv"

# 4b. A fail row fails the aggregate.
awk -F'\t' 'BEGIN{OFS="\t"} $2=="m1_mode_input"{$3="fail"} {print}' "$agg/linux-wayland.tsv" >"$agg/linux-wayland.tsv.new"
mv "$agg/linux-wayland.tsv.new" "$agg/linux-wayland.tsv"
if "$SCRIPT" aggregate --dir "$agg" >/dev/null 2>&1; then
  echo "FAIL: aggregate accepted a fail row" >&2
  FAIL=1
fi
printf '%s\n' "$full" | awk -F'\t' -v p=linux-wayland 'BEGIN{OFS="\t"}{$1=p; print}' >"$agg/linux-wayland.tsv"

# 4c. A platform job that failed before the step is surfaced by env.
if M1_JOB_MACOS=failure "$SCRIPT" aggregate --dir "$agg" >/dev/null 2>&1; then
  echo "FAIL: aggregate accepted a failed platform job result" >&2
  FAIL=1
fi

if ((FAIL)); then
  echo "m1-matrix-test: FAIL" >&2
  exit 1
fi
echo "m1-matrix-test: OK"
