#!/usr/bin/env bash
# CTX-2413 expanded runtime matrix -- headed runner wrapper.
#
# Usage:
#   BITTY_MATRIX_BINARY=/path/to/bitty ./headed-harness.sh run --plan smoke2413
#   BITTY_MATRIX_BINARY=/path/to/bitty ./headed-harness.sh run --plan expanded
#   ./headed-harness.sh matrix --plan expanded --binary /path/to/bitty
#   ./headed-harness.sh report --out <run-dir>
#
# Environment:
#   BITTY_MATRIX_BINARY  candidate binary (required for prepare/run)
#   BITTY_WORKSPACE      workspace root (default: discovered from the script)
#   BITTY_MATRIX_OUT     evidence directory (default: recording/.../rerun)
#   BITTY_MATRIX_XDG     isolated XDG root
#                        (default: $XDG_RUNTIME_DIR/ctx-2413/m26)
#
# The wrapper fails closed when the headed prerequisites are absent; it never
# falls back to the owner's config, clipboard, or display state.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

die() {
  printf 'headed-harness: %s\n' "$*" >&2
  exit 2
}

for tool in hyprctl grim wtype python3; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

hyprctl version >/dev/null 2>&1 || die "Hyprland is not reachable (hyprctl version failed)"

if [[ -z ${XDG_RUNTIME_DIR:-} ]]; then
  die "XDG_RUNTIME_DIR is unset; a Wayland session is required"
fi

args=("$@")
if ((${#args[@]} == 0)); then
  args=(matrix --plan smoke2413)
fi

exec python3 "${script_dir}/harness.py" "${args[@]}"
