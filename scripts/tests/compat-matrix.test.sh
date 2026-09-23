#!/usr/bin/env bash
# compat-matrix.test.sh — CTX-0692 fixture test for the compat release-matrix driver.
#
# Runs the driver's pure/reporting paths against a fake `cargo` and synthetic
# TSVs: no workspace build, no real compat suite, and no network. The real
# suites are exercised by `cargo test --workspace` and by the CI platform legs.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/compat-matrix.sh
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
want_suites=$'compat_matrix\tbitty-compat-lab\t8\ncompare\tbitty-compat-lab\t5\noracle\tbitty-compat-lab\t10\nreport\tbitty-compat-lab\t7\nharness\tbitty-compat-lab\t3\ndogfooding_corpus\tbitty-compat-lab\t6\nvertical_slice_gates\tbitty-compat-lab\t9\nlive_compat\tbitty-compat-lab\t8'
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
#    the `--test <name>` argument and prints a `test result:` line; the
#    artifact step (`cargo run ... --bin compat_report`) writes the requested
#    JSON paths so the driver path is exercised end to end.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
suite=""
out_prev=""
report_path=""
matrix_path=""
while (($# > 0)); do
	case "$1" in
	--test)
		suite="$2"
		shift 2
		;;
	--out)
		report_path="$2"
		shift 2
		;;
	--matrix-json)
		matrix_path="$2"
		shift 2
		;;
	*)
		out_prev="$1"
		shift
		;;
	esac
done
if [[ -n "$report_path" || -n "$matrix_path" ]]; then
	[[ -n "$report_path" ]] && { mkdir -p "$(dirname "$report_path")"; printf '{"fake":"report"}\n' >"$report_path"; }
	[[ -n "$matrix_path" ]] && { mkdir -p "$(dirname "$matrix_path")"; printf '{"fake":"matrix"}\n' >"$matrix_path"; }
	echo "compat_report: wrote fake artifacts"
	exit 0
fi
case "$suite" in
compat_matrix) echo "test result: ok. 8 passed; 0 failed; 0 ignored" ;;
compare) echo "test result: ok. 5 passed; 0 failed; 0 ignored" ;;
oracle) echo "test result: ok. 10 passed; 0 failed; 0 ignored" ;;
report) echo "test result: ok. 7 passed; 0 failed; 0 ignored" ;;
harness) echo "test result: ok. 3 passed; 0 failed; 0 ignored" ;;
dogfooding_corpus) echo "test result: ok. 6 passed; 0 failed; 0 ignored" ;;
vertical_slice_gates) echo "test result: ok. 9 passed; 0 failed; 0 ignored" ;;
live_compat)
	if [[ "${FAKE_COMPAT_FAIL:-}" == "live" ]]; then
		echo "test result: FAILED. 7 passed; 1 failed; 0 ignored"
		exit 1
	fi
	if [[ "${FAKE_COMPAT_FAIL:-}" == "ignored" ]]; then
		echo "test result: ok. 7 passed; 0 failed; 1 ignored"
		exit 0
	fi
	echo "test result: ok. 8 passed; 0 failed; 0 ignored"
	;;
*) echo "no such test target: $suite" >&2; exit 101 ;;
esac
SH
chmod +x "$TMP/bin/cargo"
export PATH="$TMP/bin:$PATH"

# 3a. All suites green -> exit 0, a PASS table row per suite, and artifacts.
if ! out="$("$SCRIPT" run --platform linux-x11 --out "$TMP/green.tsv" --report "$TMP/green-report.json" --matrix "$TMP/green-matrix.json" 2>&1)"; then
  echo "FAIL: run green exited non-zero" >&2
  printf '%s\n' "$out" >&2
  FAIL=1
fi
for needle in 'compat_matrix' 'compare' 'oracle' 'report' 'harness' 'dogfooding_corpus' \
  'vertical_slice_gates' 'live_compat' '| pass |' 'linux-x11 PASS (8 suites + artifacts)'; do
  if ! grep -qF -- "$needle" <<<"$out"; then
    echo "FAIL: run green missing '$needle'" >&2
    FAIL=1
  fi
done
for artifact in "$TMP/green-report.json" "$TMP/green-matrix.json"; do
  if [[ ! -s "$artifact" ]]; then
    echo "FAIL: run green did not emit artifact $artifact" >&2
    FAIL=1
  fi
done

# 3b. A failing suite -> exit 1 and a fail row.
if FAKE_COMPAT_FAIL=live "$SCRIPT" run --platform macos --out "$TMP/red.tsv" --report "$TMP/red-report.json" --matrix "$TMP/red-matrix.json" >/dev/null 2>&1; then
  echo "FAIL: run with a failing suite exited 0" >&2
  FAIL=1
fi
if ! awk -F'\t' '$1=="macos" && $2=="live_compat" && $3=="fail"' "$TMP/red.tsv" | grep -q .; then
  echo "FAIL: failing suite not recorded as fail" >&2
  cat "$TMP/red.tsv" >&2
  FAIL=1
fi

# 3c. The anti-silent-omission floor: a target that exits 0 with zero tests
#     must still fail the leg.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
for arg in "$@"; do
	if [[ "$arg" == "--out" || "$arg" == "--matrix-json" ]]; then
		exit 0
	fi
done
echo "test result: ok. 0 passed; 0 failed; 0 ignored"
SH
chmod +x "$TMP/bin/cargo"
if "$SCRIPT" run --platform windows --out "$TMP/empty.tsv" --report "$TMP/empty-report.json" --matrix "$TMP/empty-matrix.json" >/dev/null 2>&1; then
  echo "FAIL: zero-test suite was accepted" >&2
  FAIL=1
fi

# 3d. An ignored test is a silent skip and must fail the leg too.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
for arg in "$@"; do
	if [[ "$arg" == "--out" || "$arg" == "--matrix-json" ]]; then
		exit 0
	fi
done
echo "test result: ok. 20 passed; 0 failed; 2 ignored"
SH
chmod +x "$TMP/bin/cargo"
if "$SCRIPT" run --platform windows --out "$TMP/ignored.tsv" --report "$TMP/ignored-report.json" --matrix "$TMP/ignored-matrix.json" >/dev/null 2>&1; then
  echo "FAIL: ignored tests were accepted" >&2
  FAIL=1
fi

# 3e. Artifact emission failure fails the leg even when suites are green.
cat >"$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
for arg in "$@"; do
	if [[ "$arg" == "--out" || "$arg" == "--matrix-json" ]]; then
		echo "compat_report: fake write failure" >&2
		exit 1
	fi
done
echo "test result: ok. 20 passed; 0 failed; 0 ignored"
SH
chmod +x "$TMP/bin/cargo"
if "$SCRIPT" run --platform windows --out "$TMP/noartifact.tsv" --report "$TMP/noartifact-report.json" --matrix "$TMP/noartifact-matrix.json" >/dev/null 2>&1; then
  echo "FAIL: artifact failure was accepted" >&2
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
if ! grep -qF -- 'Compat release matrix (Tier 1)' "$TMP/summary.md"; then
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
awk -F'\t' 'BEGIN{OFS="\t"} $2=="oracle"{$3="fail"} {print}' "$agg/linux-wayland.tsv" >"$agg/linux-wayland.tsv.new"
mv "$agg/linux-wayland.tsv.new" "$agg/linux-wayland.tsv"
if "$SCRIPT" aggregate --dir "$agg" >/dev/null 2>&1; then
  echo "FAIL: aggregate accepted a fail row" >&2
  FAIL=1
fi
printf '%s\n' "$full" | awk -F'\t' -v p=linux-wayland 'BEGIN{OFS="\t"}{$1=p; print}' >"$agg/linux-wayland.tsv"

# 4c. A platform job that failed before the step is surfaced by env.
if COMPAT_JOB_MACOS=failure "$SCRIPT" aggregate --dir "$agg" >/dev/null 2>&1; then
  echo "FAIL: aggregate accepted a failed platform job result" >&2
  FAIL=1
fi

if ((FAIL)); then
  echo "compat-matrix-test: FAIL" >&2
  exit 1
fi
echo "compat-matrix-test: OK"
