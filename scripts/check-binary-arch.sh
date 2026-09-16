#!/usr/bin/env bash
# check-binary-arch.sh — fail when a built binary is not the expected
# architecture (CTX-0502).
#
# The aarch64 Linux release cross-compiles on an x86_64 runner, so a broken
# cross path could produce a host-arch artifact that still renames, packages,
# and uploads. This gate mirrors the musl readelf check in the release
# `build-alpine` job (CTX-0447 / #824): it reads the ELF machine and program
# interpreter with `readelf` and fails on a mismatch or a foreign interpreter.
#
# What it does:
#   1. reads the ELF machine from `readelf -h` and compares it with the
#      expected architecture (`aarch64` or `x86_64`);
#   2. asserts the program interpreter from `readelf -l` is the expected
#      glibc loader and rejects a foreign interpreter;
#   3. prints the observed machine and interpreter as evidence.
#
# Usage:
#   scripts/check-binary-arch.sh --binary PATH --arch aarch64
#
# Options:
#   --binary PATH  ELF binary to inspect (required).
#   --arch NAME    Expected architecture: aarch64 or x86_64 (required).
#   -h, --help     Show this help.
#
# Exit codes: 0 = the binary matches, 1 = mismatch or inspection failure,
# 2 = usage error.
set -euo pipefail

export LC_ALL=C

BINARY=""
ARCH=""

usage() {
	cat <<'EOF'
Usage: scripts/check-binary-arch.sh --binary PATH --arch aarch64

Options:
  --binary PATH  ELF binary to inspect (required).
  --arch NAME    Expected architecture: aarch64 or x86_64 (required).
  -h, --help     Show this help.

Reads the ELF machine and program interpreter with readelf and fails on a
mismatch, so a cross-build regression cannot ship a host-arch artifact (see
.github/workflows/release.yml and CTX-0502).
EOF
}

usage_error() {
	printf 'check-binary-arch: %s\n' "$*" >&2
	printf 'run with --help\n' >&2
	exit 2
}

die() {
	printf 'check-binary-arch[error]: %s\n' "$*" >&2
	exit 1
}

while (($#)); do
	case "$1" in
	--binary | --arch)
		[[ $# -ge 2 ]] || usage_error "$1 needs a value"
		case "$1" in
		--binary) BINARY="$2" ;;
		--arch) ARCH="$2" ;;
		esac
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage_error "unknown argument: $1"
		;;
	esac
done

[[ -n "$BINARY" ]] || usage_error "--binary is required"
[[ -n "$ARCH" ]] || usage_error "--arch is required"
[[ -f "$BINARY" ]] || die "binary not found: $BINARY"
command -v readelf >/dev/null 2>&1 || die "readelf not found (install binutils)"

case "$ARCH" in
aarch64)
	MACHINE="AArch64"
	INTERPRETER="ld-linux-aarch64.so.1"
	;;
x86_64)
	MACHINE="X86-64"
	INTERPRETER="ld-linux-x86-64.so.2"
	;;
*)
	usage_error "unknown --arch '$ARCH' (expected aarch64 or x86_64)"
	;;
esac

head_out="$(readelf -h "$BINARY" 2>&1)" || die "readelf -h failed on $BINARY: $head_out"
actual_machine="$(printf '%s\n' "$head_out" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p' | head -n1)"
[[ -n "$actual_machine" ]] || die "no ELF Machine field in $BINARY (not an ELF file?)"
case "$actual_machine" in
*"$MACHINE"*) ;;
*) die "machine mismatch: expected $MACHINE, got '$actual_machine' ($BINARY)" ;;
esac

program_out="$(readelf -l "$BINARY" 2>&1)" || die "readelf -l failed on $BINARY: $program_out"
actual_interpreter="$(printf '%s\n' "$program_out" | sed -n 's/.*\[Requesting program interpreter: \(.*\)\]$/\1/p' | head -n1)"
[[ -n "$actual_interpreter" ]] || die "no program interpreter in $BINARY (static binary?)"
case "$actual_interpreter" in
*"$INTERPRETER"*) ;;
*) die "interpreter mismatch: expected *$INTERPRETER, got '$actual_interpreter' ($BINARY)" ;;
esac

printf 'check-binary-arch: PASS arch=%s machine=%s interpreter=%s\n' \
	"$ARCH" "$actual_machine" "$actual_interpreter"
