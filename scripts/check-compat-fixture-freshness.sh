#!/usr/bin/env bash
# check-compat-fixture-freshness.sh — W-105 (bitty CTX-0931) vendored-fixture gate.
#
# The `bitty-compat-lab` validation repository vendors two snapshots of product
# state it cross-checks its report against:
#   - `fixtures/product-test-presence.txt` (`file<TAB>test-fn` pairs),
#   - `fixtures/clipboard-live-scenarios.txt` (live-display scenario names).
# This gate fails when a vendored entry no longer matches this workspace:
# a renamed or deleted product test, or a renamed live scenario, must fail
# loudly here instead of silently rotting the external snapshot. (The reverse
# direction — the suite adopting new product tests — is owned by the suite's
# own task; this gate only pins the snapshot to reality.)
#
# Usage: scripts/check-compat-fixture-freshness.sh [--lab-dir <dir>]
#   --lab-dir checks <dir> instead of the resolved compat-lab checkout.
#   Resolution without --lab-dir mirrors scripts/m1-matrix.sh
#   (BITTY_COMPAT_LAB_DIR, else $BITTY_WORKSPACE/bitty-compat-lab, else
#   ../bitty-compat-lab).
#
# Scope: reads tracked text only (`git ls-files`-free; fixed paths below),
# no network, no build.
set -euo pipefail

LAB_DIR=""
while (($# > 0)); do
  case "$1" in
  --lab-dir)
    LAB_DIR="${2:?--lab-dir requires a directory}"
    shift 2
    ;;
  -h | --help)
    echo "usage: $0 [--lab-dir <dir>]"
    exit 0
    ;;
  *)
    echo "usage: $0 [--lab-dir <dir>]" >&2
    exit 2
    ;;
  esac
done

if [[ -z "$LAB_DIR" ]]; then
  if [[ -n "${BITTY_COMPAT_LAB_DIR:-}" ]]; then
    LAB_DIR="$BITTY_COMPAT_LAB_DIR"
  elif [[ -n "${BITTY_WORKSPACE:-}" ]]; then
    LAB_DIR="$BITTY_WORKSPACE/bitty-compat-lab"
  else
    LAB_DIR="$(cd "$(dirname "$0")/.." && pwd)/../bitty-compat-lab"
  fi
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FAIL=0

# --- product-test-presence.txt: every pair must resolve in this tree ---
presence="$LAB_DIR/fixtures/product-test-presence.txt"
if [[ ! -f "$presence" ]]; then
  echo "compat-fixture-freshness: missing $presence" >&2
  echo "compat-fixture-freshness: set BITTY_COMPAT_LAB_DIR to the pinned bitty-compat-lab checkout (see validation-pins.env)" >&2
  exit 2
fi
while IFS=$'\t' read -r file testfn rest; do
  file="${file%$'\r'}"
  testfn="${testfn%$'\r'}"
  [[ -z "${file// /}" ]] && continue
  case "$file" in \#*) continue ;; esac
  if [[ -z "${testfn:-}" || -n "${rest:-}" ]]; then
    echo "compat-fixture-freshness: malformed line in fixtures/product-test-presence.txt: $file" >&2
    FAIL=1
    continue
  fi
  if [[ ! -f "$ROOT/$file" ]]; then
    echo "compat-fixture-freshness: product file gone: $file (test $testfn)" >&2
    FAIL=1
    continue
  fi
  if ! grep -qF "fn ${testfn}(" "$ROOT/$file"; then
    echo "compat-fixture-freshness: product test gone: $file::${testfn}" >&2
    FAIL=1
  fi
done <"$presence"

# --- clipboard-live-scenarios.txt: every scenario must own a live test ---
scenarios="$LAB_DIR/fixtures/clipboard-live-scenarios.txt"
if [[ ! -f "$scenarios" ]]; then
  echo "compat-fixture-freshness: missing $scenarios" >&2
  FAIL=1
else
  clipboard_live="$ROOT/crates/bitty-platform/tests/clipboard_live.rs"
  if [[ ! -f "$clipboard_live" ]]; then
    echo "compat-fixture-freshness: missing product file crates/bitty-platform/tests/clipboard_live.rs" >&2
    FAIL=1
  else
    seen=0
    while IFS= read -r line; do
      line="${line%$'\r'}"
      line="$(printf '%s' "$line" | tr -d '[:space:]')"
      [[ -z "$line" ]] && continue
      case "$line" in \#*) continue ;; esac
      seen=$((seen + 1))
      if ! grep -qF "fn live_${line}_" "$clipboard_live"; then
        echo "compat-fixture-freshness: live scenario gone: $line" >&2
        FAIL=1
      fi
    done <"$scenarios"
    if ((seen == 0)); then
      echo "compat-fixture-freshness: fixtures/clipboard-live-scenarios.txt names no scenario" >&2
      FAIL=1
    fi
  fi
fi

if ((FAIL)); then
  echo "compat-fixture-freshness: FAIL — vendored compat fixtures drifted from this workspace; refresh them in bitty-compat-lab (owned, reviewed pin/fixture bump)" >&2
  exit 1
fi
echo "compat-fixture-freshness: OK"
