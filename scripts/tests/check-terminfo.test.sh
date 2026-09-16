#!/usr/bin/env bash
# check-terminfo.test.sh — CTX-0451 fixture test for the terminfo gate.
#
# Drives scripts/check-terminfo.sh against synthetic repository roots so the
# retire-the-placeholder decision is exercised without touching the checkout.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-terminfo.sh
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# make_root <dir>: minimal tree with the paths the gate inspects.
make_root() {
	local root="$1"
	mkdir -p "$root/terminfo" "$root/packaging" "$root/.github/workflows"
	for f in \
		"$root/terminfo/README.md" \
		"$root/packaging/PKGBUILD" \
		"$root/packaging/PKGBUILD.bin" \
		"$root/nfpm.yaml" \
		"$root/.github/workflows/release.yml"; do
		: >"$f"
	done
}

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
		echo "FAIL: $label: gate passed but exit $tag was expected" >&2
		FAIL=1
	elif ! grep -qF "$tag" <<<"$out"; then
		echo "FAIL: $label: missing tag '$tag'" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# 1. The real repository passes.
expect_exit 0 'real repository' --root .

# 2. Clean synthetic root passes.
root="$TMP/clean"
make_root "$root"
expect_exit 0 'clean root' --root "$root"

# 3. The retired placeholder file fails.
root="$TMP/placeholder"
make_root "$root"
printf '# dummy terminfo for nfpm validation\n' >"$root/terminfo/bitty.terminfo"
expect_fail_tag 'placeholder is retired' 'retired placeholder file' --root "$root"

# 4. A non-markdown file carrying the marker fails.
root="$TMP/marker"
make_root "$root"
printf '# dummy terminfo\n' >"$root/terminfo/bitty.ti"
expect_fail_tag 'retired placeholder marker' 'placeholder marker in source' --root "$root"

# 5. A recipe installing a raw terminfo source fails.
root="$TMP/recipe"
make_root "$root"
printf 'install -Dm644 terminfo/bitty.ti "$pkgdir/usr/share/terminfo/b/bitty"\n' \
	>"$root/packaging/PKGBUILD"
expect_fail_tag 'usr/share/terminfo' 'recipe installs terminfo' --root "$root"

# 6. A workflow recreating the retired path fails.
root="$TMP/workflow"
make_root "$root"
printf 'echo "# dummy" > terminfo/bitty.terminfo\n' >"$root/.github/workflows/release.yml"
expect_fail_tag 'retired placeholder path' 'workflow recreates dummy' --root "$root"

# 7. Markdown may name the marker to document the decision.
root="$TMP/readme"
make_root "$root"
printf 'The retired "dummy terminfo" placeholder was removed.\n' >"$root/terminfo/README.md"
expect_exit 0 'markdown may document the marker' --root "$root"

if ((FAIL)); then
	echo "check-terminfo-test: FAIL" >&2
	exit 1
fi
echo "check-terminfo-test: OK"
