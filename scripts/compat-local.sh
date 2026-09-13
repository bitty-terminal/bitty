#!/usr/bin/env bash
# CTX-0404 local compatibility evidence runner.
#
# Produces the M1/M2 compatibility evidence for this checkout under
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
#   CARGO_TARGET_DIR   optional; set it to keep build artifacts out of the
#                      checkout (for example $BITTY_WORKSPACE/.targets/ctx-0404).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${1:-${repo_root}/recording/compat-local}"
revision="$(git -C "${repo_root}" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"

mkdir -p "${out_dir}"
export BITTY_COMPAT_REVISION="${revision}"

cargo run -p bitty-compat-lab --bin compat_report --locked -- \
	--out "${out_dir}/compat-report-${revision}.json"

BITTY_COMPAT_LIVE=1 cargo test -p bitty-compat-lab --test live_compat --locked -- \
	--nocapture --test-threads=1 | tee "${out_dir}/live-${revision}.log"

printf 'compat evidence written under %s (revision %s)\n' "${out_dir}" "${revision}"
