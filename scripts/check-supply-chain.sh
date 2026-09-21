#!/usr/bin/env bash
# check-supply-chain.sh — CTX-0634 local supply-chain gate (SEC-16 / R-019).
#
# Usage: scripts/check-supply-chain.sh
#
# Mirrors the `Supply chain (deny/audit)` job in .github/workflows/ci.yml:
#   1. `cargo deny check` (advisories, bans, licenses, sources per deny.toml).
#   2. `cargo audit` with the same two advisory ignores as CI, using a fresh
#      advisory-db checkout under mktemp so the shared ~/.cargo/advisory-db
#      cache that `cargo deny` populates is left untouched.
# Fails closed: any finding fails the script.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> cargo deny check"
cargo deny check

echo "==> cargo audit"
tmpdb="$(mktemp -d)"
trap 'rm -rf "$tmpdb"' EXIT INT TERM
cargo audit --db "$tmpdb" --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0192
trap - EXIT INT TERM
rm -rf "$tmpdb"
