#!/usr/bin/env bash
set -euo pipefail

out=/tmp/scratch-path-gate-probe
cp "$out" /home/example-user/.cache/bitty

# Allowlist lookalikes: only the exact `/tmp/bitty` root and its children pass.
backup=/tmp/bitty-backup/state
log=/tmp/bitty.log
root2=/tmp/bitty2/q
trav=/tmp/bitty/../x
vartmp=/var/tmp/bitty/appimagetool
drive=C:/tmp/bitty/x
printf '%s %s %s %s %s %s\n' "$backup" "$log" "$root2" "$trav" "$vartmp" "$drive" >/dev/null
