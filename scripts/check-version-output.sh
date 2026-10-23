#!/usr/bin/env bash
# check-version-output.sh — fail-closed native --version assertion.
#
# Usage: check-version-output.sh EXPECTED_VERSION COMMAND [ARG...]
#
# A temporary file preserves physical line count because command substitution
# strips trailing newlines. The command must exit zero and print exactly one
# nonempty `bitty X.Y.Z (channel commit)` line.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 EXPECTED_VERSION COMMAND [ARG...]" >&2
  exit 2
fi

expected="$1"
shift
output_file="$(mktemp)"
trap 'rm -f "$output_file"' EXIT

status=0
if "$@" --version >"$output_file" 2>&1; then
  status=0
else
  status=$?
fi

output="$(cat "$output_file")"
line_count="$(awk 'END { print NR }' "$output_file")"
expected_re="${expected//./\.}"

if [[ "$status" -ne 0 ]]; then
  echo "version check: command exited $status" >&2
  exit 1
fi
if [[ "$line_count" -ne 1 || -z "$output" ]]; then
  echo "version check: expected one nonempty output line, got $line_count" >&2
  exit 1
fi
if [[ ! "$output" =~ ^bitty\ ${expected_re}\ \([^[:space:]()]+\ [^[:space:]()]+\)$ ]]; then
  echo "version check: unexpected output: $output" >&2
  exit 1
fi

echo "bitty --version => $output"
