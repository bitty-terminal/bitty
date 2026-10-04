#!/usr/bin/env bash
# check-compat-fixture-freshness.test.sh — fixture test for the W-105
# vendored-fixture gate (bitty CTX-0931).
#
# Exercises scripts/check-compat-fixture-freshness.sh against synthetic lab
# dirs (`--lab-dir`): the product-file side always reads this workspace, so
# the positive case pins one known-good real pair plus the real clipboard
# scenario, and every negative case crafts a drifting fixture.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/check-compat-fixture-freshness.sh
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

# 1. Positive: a lab dir whose fixtures mirror reality passes.
mkdir -p "$TMP/good/fixtures"
printf 'crates/bitty-platform/tests/clipboard_sync.rs\theadless_set_syncs_clipboard_and_primary\n' >"$TMP/good/fixtures/product-test-presence.txt"
printf '# comment\n\nclipboard\n' >"$TMP/good/fixtures/clipboard-live-scenarios.txt"
expect_exit 0 'good fixtures' --lab-dir "$TMP/good"

# 2. A renamed product test fails the gate.
mkdir -p "$TMP/renamed/fixtures"
printf 'crates/bitty-platform/tests/clipboard_sync.rs\tno_such_test_fn\n' >"$TMP/renamed/fixtures/product-test-presence.txt"
printf 'clipboard\n' >"$TMP/renamed/fixtures/clipboard-live-scenarios.txt"
expect_exit 1 'renamed product test' --lab-dir "$TMP/renamed"

# 3. A deleted product file fails the gate.
mkdir -p "$TMP/gone/fixtures"
printf 'crates/bitty-platform/tests/no_such_file.rs\tsome_test\n' >"$TMP/gone/fixtures/product-test-presence.txt"
printf 'clipboard\n' >"$TMP/gone/fixtures/clipboard-live-scenarios.txt"
expect_exit 1 'deleted product file' --lab-dir "$TMP/gone"

# 4. A malformed fixture line fails the gate.
mkdir -p "$TMP/malformed/fixtures"
printf 'no-tab-separator-here\n' >"$TMP/malformed/fixtures/product-test-presence.txt"
printf 'clipboard\n' >"$TMP/malformed/fixtures/clipboard-live-scenarios.txt"
expect_exit 1 'malformed fixture line' --lab-dir "$TMP/malformed"

# 5. A renamed live scenario fails the gate.
mkdir -p "$TMP/scenario/fixtures"
printf 'crates/bitty-platform/tests/clipboard_sync.rs\theadless_set_syncs_clipboard_and_primary\n' >"$TMP/scenario/fixtures/product-test-presence.txt"
printf 'no_such_scenario\n' >"$TMP/scenario/fixtures/clipboard-live-scenarios.txt"
expect_exit 1 'renamed live scenario' --lab-dir "$TMP/scenario"

# 6. An empty scenario list fails the gate (anti-silent-omission).
mkdir -p "$TMP/empty/fixtures"
printf 'crates/bitty-platform/tests/clipboard_sync.rs\theadless_set_syncs_clipboard_and_primary\n' >"$TMP/empty/fixtures/product-test-presence.txt"
printf '# only a comment\n' >"$TMP/empty/fixtures/clipboard-live-scenarios.txt"
expect_exit 1 'empty scenario list' --lab-dir "$TMP/empty"

# 7. A missing lab checkout fails closed with exit 2.
expect_exit 2 'missing lab checkout' --lab-dir "$TMP/absent"

# 8. Usage errors exit 2.
expect_exit 2 'unknown option' --bogus

# 9. Declaration-anchored matching: a `fn name(` inside a comment or string
# must not satisfy the gate (CodeRabbit: match declarations, not text).
if printf '// fn decoy_fn(\nlet s = "fn decoy_fn("; \n' | grep -qE '^[[:space:]]*fn decoy_fn\('; then
  echo 'FAIL: comment/string decoy satisfied the declaration pattern' >&2
  FAIL=1
fi
if ! printf 'fn decoy_fn(\n    fn decoy_fn(\n' | grep -qE '^[[:space:]]*fn decoy_fn\('; then
  echo 'FAIL: top-level and indented declarations not matched' >&2
  FAIL=1
fi

if ((FAIL)); then
  echo "check-compat-fixture-freshness-test: FAIL" >&2
  exit 1
fi
echo "check-compat-fixture-freshness-test: OK"
