#!/usr/bin/env bash
# check-supply-chain.test.sh — CTX-0634 controls for the supply-chain gate
# (P0-AC-034: a seeded banned dependency must be caught).
#
# Positive control: the committed deny.toml passes `cargo deny check bans`.
# Negative control: a seeded `bans.deny` entry for `paste` (a crate present
# in Cargo.lock) makes `cargo deny check bans` fail closed. The seed is
# applied to the workspace deny.toml with a mktemp backup restored under
# trap, so the committed config is never left modified.
set -euo pipefail

cd "$(dirname "$0")/../.."

FAIL=0

if ! deny_out="$(cargo deny check bans 2>&1)"; then
  echo "FAIL: committed deny.toml fails the bans check:" >&2
  printf '%s\n' "$deny_out" >&2
  FAIL=1
fi

bak="$(mktemp)"
cp deny.toml "$bak"
restore() {
  cp "$bak" deny.toml 2>/dev/null || true
  rm -f "$bak"
}
trap 'restore' EXIT INT TERM

sed -i 's/^deny = \[\]$/deny = [{ name = "paste" }]/' deny.toml
if seeded_out="$(cargo deny check bans 2>&1)"; then
  echo "FAIL: seeded bans.deny for 'paste' did not fail the gate" >&2
  FAIL=1
elif [[ "$seeded_out" != *paste* ]]; then
  echo "FAIL: gate failed, but not because of the seeded 'paste' ban:" >&2
  printf '%s\n' "$seeded_out" >&2
  FAIL=1
fi

restore
trap - EXIT INT TERM

if ((FAIL != 0)); then
  echo "supply-chain gate test: FAIL" >&2
  exit 1
fi
echo "supply-chain gate test: ok (positive passes, seeded ban caught)"
