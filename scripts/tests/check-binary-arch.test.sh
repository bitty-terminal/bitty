#!/usr/bin/env bash
# check-binary-arch.test.sh — CTX-0502 fixture test for the architecture gate.
#
# Drives scripts/check-binary-arch.sh against recorded readelf output
# (scripts/tests/fixtures/binary-arch/) with a readelf stub on PATH, so the
# gate logic runs identically in every environment and no cross toolchain or
# ELF binary is needed.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-binary-arch.sh
FIX=scripts/tests/fixtures/binary-arch
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin"
cat >"$TMP/bin/readelf" <<'STUB'
#!/bin/sh
case "$1" in
-h) cat "$BINARY_ARCH_READELF_HEAD" ;;
-l) cat "$BINARY_ARCH_READELF_PROGRAM_HEADERS" ;;
*)
	echo "readelf stub: unsupported arguments: $*" >&2
	exit 1
	;;
esac
STUB
chmod +x "$TMP/bin/readelf"
export PATH="$TMP/bin:$PATH"
printf 'binary fixture\n' >"$TMP/binary"

# expect_exit <exit> <label> <gate args...>
expect_exit() {
	local want="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status != want)); then
		echo "FAIL: $label: expected exit $want, got $status" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# expect_fail_text <text> <label> <gate args...>
expect_fail_text() {
	local text="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status == 0)); then
		echo "FAIL: $label: gate passed but failure was expected" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	elif ! grep -qF "$text" <<<"$out"; then
		echo "FAIL: $label: missing '$text'" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# 1. aarch64 machine and interpreter pass.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-aarch64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-aarch64.txt" \
	expect_exit 0 'aarch64 binary' \
	--binary "$TMP/binary" --arch aarch64

# 2. A host x86_64 artifact fails the aarch64 gate on the machine.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-x86_64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-x86_64.txt" \
	expect_fail_text 'machine mismatch' 'x86_64 artifact under --arch aarch64' \
	--binary "$TMP/binary" --arch aarch64

# 3. An aarch64 machine with the host loader fails on the interpreter.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-aarch64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-x86_64.txt" \
	expect_fail_text 'interpreter mismatch' 'host interpreter under --arch aarch64' \
	--binary "$TMP/binary" --arch aarch64

# 4. A static binary without an interpreter cannot prove the target loader.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-aarch64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-static.txt" \
	expect_fail_text 'no program interpreter' 'static aarch64 binary' \
	--binary "$TMP/binary" --arch aarch64

# 5. The x86_64 mapping passes its own fixtures.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-x86_64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-x86_64.txt" \
	expect_exit 0 'x86_64 binary' \
	--binary "$TMP/binary" --arch x86_64

# 6. Usage errors: missing arguments and unknown arch.
expect_exit 2 'missing arguments' --binary "$TMP/binary"
expect_exit 2 'unknown arch' --binary "$TMP/binary" --arch riscv64

# 7. A missing binary is an inspection failure, not a usage error.
BINARY_ARCH_READELF_HEAD="$FIX/readelf-head-aarch64.txt" \
	BINARY_ARCH_READELF_PROGRAM_HEADERS="$FIX/readelf-program-headers-aarch64.txt" \
	expect_fail_text 'binary not found' 'missing binary' \
	--binary "$TMP/absent" --arch aarch64

if ((FAIL)); then
	echo "binary-arch-test: FAIL" >&2
	exit 1
fi
echo "binary-arch-test: OK"
