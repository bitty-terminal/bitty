#!/usr/bin/env bash
# Refusal regressions only: mocks never establish native display evidence.
set -euo pipefail
readonly RUNNER=scripts/clipboard-isolated.sh
BASH_BIN=$(command -v bash)
TRUE_BIN=$(type -P true)
readonly BASH_BIN TRUE_BIN
scratch=$(mktemp -d "${TMPDIR:?set TMPDIR to an approved scratch directory}/clipboard-runner.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"
for tool in bwrap timeout; do
  printf '#!/bin/sh\nexit 99\n' >"$scratch/bin/$tool"
  chmod +x "$scratch/bin/$tool"
done
status=0
PATH="$scratch/bin" "$BASH_BIN" "$RUNNER" "$TRUE_BIN" >"$scratch/result" 2>&1 || status=$?
[[ $status == 77 ]]
grep -q 'NOT EXECUTED (missing Xvfb)' "$scratch/result"
if grep -q 'verified-local' "$scratch/result"; then
  echo 'clipboard-runner-test: refusal incorrectly claimed native proof' >&2
  exit 1
fi
status=0
"$BASH_BIN" "$RUNNER" >"$scratch/usage" 2>&1 || status=$?
[[ $status == 2 ]]
echo 'clipboard-runner-test: PASS (refusal only; no native display executed)'
