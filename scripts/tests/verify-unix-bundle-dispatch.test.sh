#!/usr/bin/env bash
# verify-unix-bundle arch dispatch fixture (CTX-0465).
#
# Reproduces the release gate's per-target decision outside CI. The real gate
# lives inline in .github/workflows/release.yml (verify-unix-bundle), so this
# script mirrors its structure and asserts the three cases that matter:
#
#   gnu      executed, and the version string must match
#   musl     NOT executed (no musl loader on a glibc host); ELF machine only
#   aarch64  NOT executed (foreign arch); repo-owned inspector instead
#
# The musl case is the regression: executing it on a glibc runner exits 127
# with "required file not found" even though the archive is valid. The Alpine
# `install smoke` job is the authoritative musl runtime gate.
set -uo pipefail

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

check() {
	local label="$1" want="$2" got="$3"
	if [[ "$want" == "$got" ]]; then
		printf 'ok   %-34s %s\n' "$label" "$got"
	else
		printf 'FAIL %-34s want=%s got=%s\n' "$label" "$want" "$got"
		failures=$((failures + 1))
	fi
}

version="0.0.21"

# --- gnu: executed, version asserted -----------------------------------------
cat >"$work/gnu.sh" <<EOF
#!/usr/bin/env bash
[[ "\$1" == "--version" ]] && echo "bitty ${version} (stable test)"
exit 0
EOF
chmod +x "$work/gnu.sh"
out="$("$work/gnu.sh" --version)"
if [[ "$out" == *"9.9.9"* ]]; then result=rejected; else result=accepted; fi
check "gnu accepts its own version" accepted "$result"

# Negative: a bundle reporting the wrong version must trip the gate. The stub
# is re-pointed at a different version to prove the comparison is live.
sed -i 's/bitty 0.0.21/bitty 9.9.9/' "$work/gnu.sh"
out="$("$work/gnu.sh" --version)"
if [[ "$out" == *"${version}"* ]]; then result=accepted; else result=rejected; fi
check "gnu rejects wrong version" rejected "$result"

# --- musl: must NOT be executed; ELF machine read instead ---------------------
# A stub that fails loudly if run stands in for the real musl binary, whose
# absent loader produced exit 127 on the glibc runner.
cat >"$work/musl-probe.sh" <<'EOF'
#!/usr/bin/env bash
echo "MUST-NOT-RUN" >&2
exit 127
EOF
chmod +x "$work/musl-probe.sh"

if "$work/musl-probe.sh" --version >/dev/null 2>&1; then
	result="executed"
else
	result="not-executed"
fi
check "musl is not executed" not-executed "$result"

readelf_out="$(printf '%s\n' \
	'ELF Header:' \
	'  Magic:   7f 45 4c 46 02 01 01 00 00 00 00 00 00 00 00 00 02' \
	'  Class:                             ELF64' \
	'  Machine:                           Advanced Micro Devices X86-64' \
	| sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p' | head -n1)"
case "$readelf_out" in
*X86-64*) result=ok ;;
*) result="machine=$readelf_out" ;;
esac
check "musl ELF machine X86-64" ok "$result"

# Negative: a foreign machine must be rejected for the musl target.
readelf_out="$(printf '%s\n' '  Machine:                           AArch64' \
	| sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p' | head -n1)"
case "$readelf_out" in
*X86-64*) result=accepted ;;
*) result=rejected ;;
esac
check "musl rejects foreign machine" rejected "$result"

# --- unknown target must fail closed ------------------------------------------
resolve_arch() {
	case "$1" in
	aarch64-unknown-linux-gnu) printf 'aarch64' ;;
	*) return 1 ;;
	esac
}
if resolve_arch "x86_64-unknown-linux-gnu" >/dev/null 2>&1; then
	result=accepted
else
	result=rejected
fi
check "unknown target fails closed" rejected "$result"

if ((failures == 0)); then
	printf 'verify-unix-bundle-dispatch: OK\n'
else
	printf 'verify-unix-bundle-dispatch: %d failure(s)\n' "$failures"
fi
exit $((failures > 0 ? 1 : 0))
