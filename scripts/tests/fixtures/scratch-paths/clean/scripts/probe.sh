#!/usr/bin/env bash
set -euo pipefail

out=recording/references/bitty
cp "$out/keep" "$out/keep.bak"

# `/tmp/bitty` is the workspace-sanctioned ephemeral scratch root.
tool=/tmp/bitty/appimagetool
scratch="/tmp/bitty"
printf '%s %s\n' "$tool" "$scratch" >/dev/null
