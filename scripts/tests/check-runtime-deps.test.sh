#!/usr/bin/env bash
# check-runtime-deps.test.sh — CTX-0449 fixture test for the runtime-deps gate.
#
# Drives scripts/check-runtime-deps.sh against recorded readelf/ldd output
# (scripts/tests/fixtures/runtime-deps/clean/) with the real nfpm.yaml and
# packaging/linux/runtime-deps.toml, plus patched copies for divergence cases.
# No ELF binary is needed: readelf/ldd stubs on PATH replay the fixture output,
# so the gate logic runs identically in every environment.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-runtime-deps.sh
FIX=scripts/tests/fixtures/runtime-deps/clean
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin"
printf '#!/bin/sh\ncat "$RUNTIME_DEPS_READELF_OUT"\n' >"$TMP/bin/readelf"
printf '#!/bin/sh\ncat "$RUNTIME_DEPS_LDD_OUT"\n' >"$TMP/bin/ldd"
chmod +x "$TMP/bin/readelf" "$TMP/bin/ldd"
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

# expect_fail_tag <tag> <label> <gate args...>
expect_fail_tag() {
	local tag="$1" label="$2"
	shift 2
	local out status=0
	out="$("$GATE" "$@" 2>&1)" || status=$?
	if ((status == 0)); then
		echo "FAIL: $label: gate passed but $tag was expected" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	elif ! grep -qF "$tag" <<<"$out"; then
		echo "FAIL: $label: missing $tag" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# 1. Clean glibc binary against the real declarations (all three nfpm formats).
RUNTIME_DEPS_READELF_OUT="$FIX/readelf.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd.txt" \
	expect_exit 0 'clean glibc deb,rpm,archlinux' \
	--binary "$TMP/binary" --packagers deb,rpm,archlinux

# 2. Clean musl binary against the apk declarations.
RUNTIME_DEPS_READELF_OUT="$FIX/readelf-musl.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd-musl.txt" \
	expect_exit 0 'clean musl apk' \
	--binary "$TMP/binary" --packagers apk

# 3. Usage error without required arguments.
expect_exit 2 'missing --binary/--packagers'

# 4. Linked library that is not declared (deb loses libgcc-s1).
sed '/^      - libgcc-s1$/d' nfpm.yaml >"$TMP/nfpm-missing.yaml"
RUNTIME_DEPS_READELF_OUT="$FIX/readelf.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd.txt" \
	expect_fail_tag 'runtime-deps[declared-missing]' 'linked but not declared' \
	--binary "$TMP/binary" --packagers deb --nfpm-config "$TMP/nfpm-missing.yaml"

# 5. Declared package without link evidence (deb gains libwayland-client).
sed '/^      - libgcc-s1$/a\      - libwayland-client' nfpm.yaml >"$TMP/nfpm-extra.yaml"
RUNTIME_DEPS_READELF_OUT="$FIX/readelf.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd.txt" \
	expect_fail_tag 'runtime-deps[declared-extra]' 'declared without link evidence' \
	--binary "$TMP/binary" --packagers deb --nfpm-config "$TMP/nfpm-extra.yaml"

# 6. Linked soname without a mapping entry (libGL.so.1 appears in NEEDED).
{
	cat "$FIX/readelf.txt"
	printf ' 0x0000000000000001 (NEEDED)             Shared library: [libGL.so.1]\n'
} >"$TMP/readelf-unmapped.txt"
RUNTIME_DEPS_READELF_OUT="$TMP/readelf-unmapped.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd.txt" \
	expect_fail_tag 'runtime-deps[unmapped]' 'unmapped soname' \
	--binary "$TMP/binary" --packagers deb,rpm,archlinux

# 7. A format with no declarations at all (apk block stripped).
sed '/^  apk:$/,/^  archlinux:$/{/^  archlinux:$/!d}' nfpm.yaml >"$TMP/nfpm-no-apk.yaml"
RUNTIME_DEPS_READELF_OUT="$FIX/readelf-musl.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd-musl.txt" \
	expect_fail_tag 'runtime-deps[declared-absent]' 'apk declarations absent' \
	--binary "$TMP/binary" --packagers apk --nfpm-config "$TMP/nfpm-no-apk.yaml"

# 8. Unresolved shared library reported by ldd.
{
	cat "$FIX/ldd.txt"
	printf '\tlibmissing.so.1 => not found\n'
} >"$TMP/ldd-unresolved.txt"
RUNTIME_DEPS_READELF_OUT="$FIX/readelf.txt" RUNTIME_DEPS_LDD_OUT="$TMP/ldd-unresolved.txt" \
	expect_fail_tag 'runtime-deps[unresolved]' 'unresolved shared library' \
	--binary "$TMP/binary" --packagers deb

# 9. Musl sonames requested for a glibc format (deb must not ship the musl libc).
RUNTIME_DEPS_READELF_OUT="$FIX/readelf-musl.txt" RUNTIME_DEPS_LDD_OUT="$FIX/ldd-musl.txt" \
	expect_fail_tag 'runtime-deps[unmapped]' 'musl soname under deb' \
	--binary "$TMP/binary" --packagers deb

if ((FAIL)); then
	echo "runtime-deps-test: FAIL" >&2
	exit 1
fi
echo "runtime-deps-test: OK"
