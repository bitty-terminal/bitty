#!/usr/bin/env bash
# check-test-support-gate.sh — #1519 / CTX-0855 production feature gate.
#
# Usage: scripts/check-test-support-gate.sh
#
# A *test-only* feature compiles hermetic entry points, denial branches, or
# fixture seams that production code must never link:
#
#   - `test-support` (external `bitty-ipc`): the authority-less `ServeContext`
#     constructors and the unbound automation-bearer minters;
#   - `io-denied-harness` (`bitty-config`, bitty#2869): denial branches inside
#     `read_config_file` and the configuration module-loader install;
#   - `input-vm-test-support` (`bitty-platform`, `bitty-runtime`,
#     `bitty-terminal`): the input-VM boundary regression seam;
#   - `publication-control-test-support` (`bitty-runtime`): the fixture TLS 1.2
#     server and the publication-control seam;
#   - `test-seams` (`bitty-daemon`) and `gui-tests` (`bitty-platform`).
#
# Each may be enabled only from `[dev-dependencies]`, or by a test invocation.
# This gate resolves the workspace feature graph over normal and build edges
# for every target and fails when any test-only feature is reachable there.
#
# Three checks keep the gate from passing vacuously:
#
#   1. every name in `test_only_features` must be matched by `pattern`, so a
#      typo in the list cannot silently unguard that feature;
#   2. any workspace `[features]` entry whose name follows the test-only naming
#      convention (`...test...` or `...harness...`) must be listed, so a new
#      test-only feature fails the gate until it is guarded rather than
#      escaping it; and
#   3. the pattern must match at least one feature in the full graph (dev edges
#      included), which proves the gate still recognizes `cargo tree` output
#      after a format change.
set -euo pipefail

cd "$(dirname "$0")/.."

# Every test-only feature in this workspace, and every test-only feature of a
# direct dependency. Add a name here when a crate gains one; check 2 fails
# until you do.
readonly -a test_only_features=(
  "test-support"
  "io-denied-harness"
  "input-vm-test-support"
  "publication-control-test-support"
  "test-seams"
  "gui-tests"
)

alternation="$(printf '%s\n' "${test_only_features[@]}" | paste -sd '|' -)"
readonly pattern="feature \"(${alternation})\""

# Check 1: the pattern must match each listed name.
for feature in "${test_only_features[@]}"; do
  if ! grep -qE "$pattern" <<<"feature \"$feature\""; then
    echo "check-test-support-gate: self-check failed: pattern misses 'feature \"$feature\"'" >&2
    exit 1
  fi
done

# Check 2: no conventionally test-only workspace feature may be unlisted.
discovered="$(
  for manifest in Cargo.toml crates/*/Cargo.toml; do
    awk '
      /^\[/ { in_features = ($0 == "[features]"); next }
      in_features && /^[A-Za-z0-9_-]+[[:space:]]*=/ {
        name = $1
        sub(/=.*$/, "", name)
        print name
      }
    ' "$manifest"
  done | sort -u | grep -E '(^|[-_])(test|tests|harness)([-_]|$)' || true
)"
unlisted=""
while IFS= read -r name; do
  [ -n "$name" ] || continue
  listed=0
  for feature in "${test_only_features[@]}"; do
    if [ "$feature" = "$name" ]; then
      listed=1
      break
    fi
  done
  if [ "$listed" -eq 0 ]; then
    unlisted="${unlisted}${name}"$'\n'
  fi
done <<<"$discovered"
if [ -n "$unlisted" ]; then
  echo "check-test-support-gate: test-only features missing from test_only_features:" >&2
  printf '%s' "$unlisted" >&2
  echo "  (add each with the reason it is test-only, then re-run)" >&2
  exit 1
fi

# Check 3: the pattern must still see the dev graph.
full="$(cargo tree --workspace --locked --target all -e features)"
if ! grep -qE "$pattern" <<<"$full"; then
  echo "check-test-support-gate: self-check failed: no test-only feature in the dev graph" >&2
  echo "  (cargo tree output format changed, or no crate uses one any more)" >&2
  exit 1
fi

production="$(cargo tree --workspace --locked --target all -e normal,build,features)"
if grep -qE "$pattern" <<<"$production"; then
  echo "check-test-support-gate: a test-only feature is enabled through a normal/build edge:" >&2
  grep -nE -B2 "$pattern" <<<"$production" >&2
  exit 1
fi

echo "check-test-support-gate: no test-only feature on normal/build edges (${#test_only_features[@]} guarded)"
