#!/usr/bin/env bash
# CTX-0404 local compatibility evidence runner (W-105 relocation, bitty CTX-0931).
#
# The compatibility suites live in the `bitty-compat-lab` validation
# repository at the pinned suite revision, not in this workspace. This script
# runs them from that checkout and writes the evidence under this checkout's
# `recording/compat-local/` (gitignored, durable on the contributor machine):
#
#   1. `compat_report` — deterministic matrix report: corpus replay (bounded,
#      deterministic, invariant-checked), named-test presence, and a PATH
#      probe of local tools.
#   2. `live_compat` — env-gated PTY scenarios for the tools that are actually
#      installed; absent tools are recorded as `"status": "skipped"`, never as
#      verified claims.
#
# Usage:
#   scripts/compat-local.sh [output-dir]
#
# Environment:
#   BITTY_COMPAT_LAB_DIR   `bitty-compat-lab` checkout at the pinned suite
#                          revision (see validation-pins.env). Defaults to
#                          `$BITTY_WORKSPACE/bitty-compat-lab` when
#                          `BITTY_WORKSPACE` is set, else `../bitty-compat-lab`.
#   CARGO_TARGET_DIR       optional; set it to keep build artifacts out of the
#                          checkouts (for example $BITTY_WORKSPACE/.targets/ctx-0404).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${1:-${repo_root}/recording/compat-local}"
revision="$(git -C "${repo_root}" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"

lab_dir="${BITTY_COMPAT_LAB_DIR:-}"
if [ -z "$lab_dir" ]; then
  if [ -n "${BITTY_WORKSPACE:-}" ]; then
    lab_dir="$BITTY_WORKSPACE/bitty-compat-lab"
  else
    lab_dir="$repo_root/../bitty-compat-lab"
  fi
fi
if [ ! -f "$lab_dir/Cargo.toml" ]; then
  echo "compat-local: compat-lab checkout missing (BITTY_COMPAT_LAB_DIR=$lab_dir)" >&2
  echo "compat-local: set BITTY_COMPAT_LAB_DIR to the pinned bitty-compat-lab checkout (see validation-pins.env)" >&2
  exit 2
fi

mkdir -p "${out_dir}"
export BITTY_COMPAT_REVISION="${revision}"

cargo run --manifest-path "$lab_dir/Cargo.toml" -p bitty-compat-lab --bin compat_report --locked -- \
	--out "${out_dir}/compat-report-${revision}.json"

BITTY_COMPAT_LIVE=1 cargo test --manifest-path "$lab_dir/Cargo.toml" -p bitty-compat-lab --test live_compat --locked -- \
	--nocapture --test-threads=1 | tee "${out_dir}/live-${revision}.log"

printf 'compat evidence written under %s (revision %s)\n' "${out_dir}" "${revision}"
