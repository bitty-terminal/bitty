#!/usr/bin/env bash
# check-nix-flake.test.sh - Assert Nix Flake and Cachix binary cache configuration
# Part of: bitty-terminal/bitty#2515 | Milestone: v0.0.27
set -euo pipefail

FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

expect_contains() {
  local haystack="$1"
  local needle="$2"
  local label="$3"
  if grep -qF "$needle" <<<"$haystack"; then
    pass "$label"
  else
    fail "$label (missing: $needle)"
  fi
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FLAKE="$ROOT/flake.nix"
WORKFLOW="$ROOT/.github/workflows/release.yml"
NIX_DOC="$ROOT/packaging/nix.md"

# 1. Assert flake.nix
if [[ ! -f "$FLAKE" ]]; then
  fail "flake.nix exists"
else
  pass "flake.nix exists"
  FLAKE_CONTENT="$(cat "$FLAKE")"
  expect_contains "$FLAKE_CONTENT" "nixConfig = {" "flake.nix declares nixConfig"
  expect_contains "$FLAKE_CONTENT" "https://bitty.cachix.org" "flake.nix points to bitty.cachix.org substituter"
  expect_contains "$FLAKE_CONTENT" "bitty.cachix.org-1:yglUvUc8ehSxMCnU5t5p7Rer56pGE+TNI7/pWbWDdmc=" "flake.nix declares Cachix public key"
fi

# 2. Assert release.yml workflow integration
if [[ ! -f "$WORKFLOW" ]]; then
  fail "release.yml exists"
else
  WORKFLOW_CONTENT="$(cat "$WORKFLOW")"
  expect_contains "$WORKFLOW_CONTENT" "nix:" "release.yml defines nix job"
  expect_contains "$WORKFLOW_CONTENT" "cachix/install-nix-action" "release.yml uses cachix/install-nix-action"
  expect_contains "$WORKFLOW_CONTENT" "cachix/cachix-action" "release.yml uses cachix/cachix-action"
  expect_contains "$WORKFLOW_CONTENT" "name: bitty" "release.yml configures bitty cache name"
  expect_contains "$WORKFLOW_CONTENT" "CACHIX_AUTH_TOKEN" "release.yml binds CACHIX_AUTH_TOKEN secret"
  expect_contains "$WORKFLOW_CONTENT" "NIX_RESULT" "release.yml channel-summary handles NIX_RESULT"
  expect_contains "$WORKFLOW_CONTENT" "Nix Flake (Cachix)" "release.yml channel-summary labels Nix Flake (Cachix)"
fi

# 3. Assert packaging documentation
if [[ ! -f "$NIX_DOC" ]]; then
  fail "packaging/nix.md exists"
else
  pass "packaging/nix.md exists"
  DOC_CONTENT="$(cat "$NIX_DOC")"
  expect_contains "$DOC_CONTENT" "https://bitty.cachix.org" "nix.md documents Cachix URL"
  expect_contains "$DOC_CONTENT" "bitty.cachix.org-1:yglUvUc8ehSxMCnU5t5p7Rer56pGE+TNI7/pWbWDdmc=" "nix.md documents Cachix public key"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-nix-flake.test: FAIL" >&2
  exit 1
fi
echo "check-nix-flake.test: PASS"
