#!/usr/bin/env bash
# PX-6898: do not launch a display with an unproven authorization boundary.
set -euo pipefail

readonly BLOCKED=77
if [[ $# != 1 || ! -x $1 ]]; then
  echo 'usage: just --justfile scripts/clipboard.justfile isolated PATH_TO_CLIPBOARD_LIVE_TEST_BINARY' >&2
  exit 2
fi
echo 'clipboard-isolated: NOT EXECUTED (connection-bound disposable runner unavailable)' >&2
exit "$BLOCKED"
