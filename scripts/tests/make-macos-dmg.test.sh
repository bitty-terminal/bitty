#!/usr/bin/env bash
# make-macos-dmg retry + guard tests (CTX-0457).
#
# The DMG step failed on a macOS runner with `hdiutil: create failed - Resource
# busy` (orphaned diskimages-helper). The script now retries a bounded number
# of times. This exercises the loop without macOS: a stub `hdiutil` fails N
# times then succeeds, and the exit-1 path is checked too.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$REPO/scripts/make-macos-dmg.sh"

if [[ ! -x "$SCRIPT" ]]; then
	echo "make-macos-dmg.test: FAIL (script not executable: $SCRIPT)" >&2
	exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/bitty-dmgtest.XXXXXX")"
trap 'rm -rf "$work"' EXIT

fail_count_file="$work/fail_count"
attempts_file="$work/attempts"

# Stub lipo + hdiutil. `hdiutil` fails while fail_count > 0, decrementing it,
# and always records the attempt. `lipo -create` writes a plausible Mach-O.
mkdir -p "$work/bin"
cat >"$work/bin/lipo" <<EOF
#!/usr/bin/env bash
set -euo pipefail
out=""
want_out=0
mode=""
for a in "\$@"; do
	if [[ "\$want_out" -eq 1 ]]; then out="\$a"; want_out=0; continue; fi
	if [[ "\$a" == "-output" ]]; then want_out=1; fi
	if [[ "\$a" == "-archs" ]]; then mode="archs"; fi
done
# \`lipo -archs FILE\` only reports the slices; the real script then checks them.
if [[ "\$mode" == "archs" ]]; then
	echo "x86_64 arm64"
	exit 0
fi
[[ -n "\$out" ]] || { echo "lipo stub: no -output" >&2; exit 2; }
printf '\xcf\xfa\xed\xfe' > "\$out"
exit 0
EOF

cat >"$work/bin/hdiutil" <<EOF
#!/usr/bin/env bash
set -euo pipefail
echo x >> "$attempts_file"
remaining=0
if [[ -f "$fail_count_file" ]]; then
	remaining="\$(cat "$fail_count_file")"
fi
if [[ "\$remaining" -gt 0 ]]; then
	echo "\$((remaining - 1))" > "$fail_count_file"
	echo "hdiutil: create failed - Resource busy" >&2
	exit 1
fi
# Simulate a successful create producing a file: the output path is the last
# positional argument of \`hdiutil create ... "\$OUTPUT"\`.
for a in "\$@"; do out="\$a"; done
: > "\$out"
exit 0
EOF
chmod +x "$work/bin/lipo" "$work/bin/hdiutil"

printf '#!/bin/sh\necho bitty\n' >"$work/arm64"
cp "$work/arm64" "$work/x86"
chmod +x "$work/arm64" "$work/x86"

run_with_stub() {
	local out_dmg="$1"
	PATH="$work/bin:$PATH" bash "$SCRIPT" \
		--version 0.0.21 \
		--arm64 "$work/arm64" \
		--x86_64 "$work/x86" \
		--output "$out_dmg" 2>&1
}

# 1) hdiutil fails twice then succeeds -> the script retries and passes.
echo 2 >"$fail_count_file"
: >"$attempts_file"
out="$(run_with_stub "$work/ok.dmg")"
rc=$?
attempts="$(wc -l <"$attempts_file" | tr -d ' ')"
if [[ "$rc" -ne 0 ]]; then
	echo "make-macos-dmg.test: FAIL (transient failures should be retried)" >&2
	echo "$out" | tail -n 5 >&2
	exit 1
fi
if [[ "$attempts" -ne 3 ]]; then
	echo "make-macos-dmg.test: FAIL (expected 3 hdiutil attempts, got $attempts)" >&2
	exit 1
fi
if [[ ! -f "$work/ok.dmg" ]]; then
	echo "make-macos-dmg.test: FAIL (output missing after successful retry)" >&2
	exit 1
fi

# 2) hdiutil always fails -> bounded exit 1, no infinite loop.
echo 99 >"$fail_count_file"
: >"$attempts_file"
out="$(run_with_stub "$work/bad.dmg")"
rc=$?
attempts="$(wc -l <"$attempts_file" | tr -d ' ')"
if [[ "$rc" -eq 0 ]]; then
	echo "make-macos-dmg.test: FAIL (persistent hdiutil failure must exit nonzero)" >&2
	exit 1
fi
if [[ "$attempts" -ne 3 ]]; then
	echo "make-macos-dmg.test: FAIL (expected exactly 3 attempts then give up, got $attempts)" >&2
	exit 1
fi

# 3) dry-run still works and does not need the stubs' behavior.
dry="$(bash "$SCRIPT" --version 0.0.21 --arm64 "$work/arm64" --x86_64 "$work/x86" \
	--output "$work/dry.dmg" --dry-run 2>&1)"
if ! grep -q "dry-run PASS" <<<"$dry"; then
	echo "make-macos-dmg.test: FAIL (dry-run broken)" >&2
	echo "$dry" | tail -n 5 >&2
	exit 1
fi

echo "make-macos-dmg.test: OK"
