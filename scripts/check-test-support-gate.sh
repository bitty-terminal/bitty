#!/usr/bin/env bash
# check-test-support-gate.sh — #1519 / CTX-0855 production feature gate.
#
# Usage: scripts/check-test-support-gate.sh
#
# A crate's `test-support` feature compiles hermetic entry points that
# production code must never link (for `bitty-ipc`: the authority-less
# `ServeContext` constructors and the unbound automation-bearer minters). The
# feature may be enabled only from `[dev-dependencies]`. This gate resolves the
# workspace feature graph over normal and build edges for every target and
# fails when any `test-support` feature is reachable there.
#
# Self-check: the same pattern must match the full graph (dev edges included),
# which proves the gate still recognizes `cargo tree` output and cannot pass
# vacuously after a format change.
set -euo pipefail

cd "$(dirname "$0")/.."

readonly pattern='feature "test-support"'

full="$(cargo tree --workspace --locked --target all -e features)"
if ! grep -qF "$pattern" <<<"$full"; then
  echo "check-test-support-gate: self-check failed: no $pattern in the dev graph" >&2
  echo "  (cargo tree output format changed, or no crate uses test-support any more)" >&2
  exit 1
fi

production="$(cargo tree --workspace --locked --target all -e normal,build,features)"
if grep -qF "$pattern" <<<"$production"; then
  echo "check-test-support-gate: a test-support feature is enabled through a normal/build edge:" >&2
  grep -nF -B2 "$pattern" <<<"$production" >&2
  exit 1
fi

echo "check-test-support-gate: no test-support feature on normal/build edges"
