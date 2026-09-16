#!/usr/bin/env bash
# check-terminfo.sh — guard the terminfo shipping decision (034 item 6 / CTX-0451).
#
# Bitty does not claim TERM=bitty: DEFAULT_TERM stays xterm-256color and no
# package format installs a terminfo entry. This gate fails when the retired
# placeholder or an unreviewed terminfo install comes back:
#   1. the retired placeholder path terminfo/bitty.terminfo must not exist;
#   2. no file under terminfo/ (markdown excepted) may carry the placeholder
#      marker;
#   3. no packaging input (PKGBUILDs, nfpm.yaml, release workflow) may install
#      into /usr/share/terminfo or reference the retired placeholder path.
#
# Shipping a real entry means updating this gate, terminfo/README.md, and the
# TERM contract in the same reviewed change.
#
# Usage: scripts/check-terminfo.sh [--root DIR]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
	echo "Usage: scripts/check-terminfo.sh [--root DIR]"
}

while (($#)); do
	case "$1" in
	--root)
		[[ $# -ge 2 ]] || {
			echo "check-terminfo: --root needs a value" >&2
			exit 2
		}
		ROOT="$2"
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "check-terminfo: unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

fail() {
	echo "check-terminfo: FAIL: $1" >&2
	exit 1
}

# 1. The placeholder path is retired (CTX-0451).
[[ -e "$ROOT/terminfo/bitty.terminfo" ]] &&
	fail "terminfo/bitty.terminfo exists: the placeholder is retired (CTX-0451); ship a real tic-compiled entry or nothing"

# 2. No placeholder marker in terminfo sources (markdown may document it).
while IFS= read -r file; do
	if [[ "$file" != *.md ]] && grep -q 'dummy terminfo' "$file"; then
		fail "$file carries the retired placeholder marker (dummy terminfo)"
	fi
done < <(find "$ROOT/terminfo" -type f 2>/dev/null)

# 3. Packaging inputs must not install a terminfo entry.
CONFIGS=(
	"$ROOT/packaging/PKGBUILD"
	"$ROOT/packaging/PKGBUILD.bin"
	"$ROOT/nfpm.yaml"
)
while IFS= read -r workflow; do
	CONFIGS+=("$workflow")
done < <(find "$ROOT/.github/workflows" -type f -name '*.yml' 2>/dev/null)

for config in "${CONFIGS[@]}"; do
	[[ -f "$config" ]] || continue
	rel="${config#"$ROOT"/}"
	if grep -q 'usr/share/terminfo' "$config"; then
		fail "$rel installs into /usr/share/terminfo; revisit the CTX-0451 decision (real tic-compiled entry) and update this gate together"
	fi
	if grep -q 'terminfo/bitty\.terminfo' "$config"; then
		fail "$rel references the retired placeholder path terminfo/bitty.terminfo"
	fi
done

echo "check-terminfo: PASS (no terminfo entry ships; decision recorded in terminfo/README.md)"
