#!/usr/bin/env bash
# check-pty-gated-tests.sh — CTX-0267 repo lint.
#
# Fails when *test code* references live-spawn markers without a gate.
# Live-spawn tests spawn a real shell/PTY through `portable-pty` on Unix.
# The Windows ConPTY backend is unimplemented per ADR-0002, so an ungated
# live spawn fails on `windows-latest` CI instead of skipping (CTX-0227
# sh-fakes, CTX-0257 workspace live-close whack-a-mole).
#
# Every live-spawn test must either:
#   1. call `require_pty!()` first (preferred: compiles everywhere, skips
#      with a SKIP notice where no PTY backend exists), or
#   2. carry `#[cfg(unix)]` / `#![cfg(unix)]` (accepted legacy gate: the
#      test is compiled out on Windows).
#
# Markers are live-spawn *call sites* (fixed strings): spawn_shell_for_view,
# spawn_shell_with_args, spawn_shell(, PtyBuilder::new, fake_editor_script,
# #!/bin/sh. A bare `/bin/sh` string is deliberately NOT a marker: it is
# used as inert string data all over the shell-resolution unit tests and
# never spawns by itself.
#
# Scope (no false positives by construction):
#   - Only files under crates/*/src/** and crates/*/tests/** are scanned.
#   - crates/bitty-test-support/** (the helper itself) is excluded.
#   - Each hit is classified by its enclosing function (nearest `fn` item
#     walking back, skipping blanks/comments/attributes): only `#[test]`
#     functions are checked. Prod code, `#[cfg(test)]` helpers, and
#     module-level items are skipped, so prod `/bin/sh` fallbacks and
#     spawn API definitions never count.
#   - Comment-only and doc-comment lines never count (comments cannot spawn).
#   - `fn` definition lines never count (helper names contain markers).
#   - Whole-file `#![cfg(unix)]` integration tests pass wholesale.
#
# Escape hatch (auditable, grep-able):
#   - `// pty-gate-exempt: <reason>` on the hit line or one of the 3 lines
#     above it exempts that hit (e.g. spawn-rejection tests that assert
#     `is_err()` without ever creating a live PTY).
#   - `// pty-gate-exempt-file: <reason>` in the first 5 lines exempts the
#     whole file (use only when every spawn call in the file is
#     rejection-only; prefer per-hit exemptions otherwise).
#
# Limitations (documented, cheap gate):
#   - Indirect spawns (e.g. a `SpawnSpec{program:"/bin/sh"}` struct handed
#     to a chrome action pages later) carry no call-site marker in the test
#     body; reviewers must gate those by hand (today: exactly one site,
#     `new_split_spawns_private_shell_and_close_tears_it_down`, gated).
#   - The enclosing-fn walk caps at 300 lines back; a hit more than 300
#     lines below its `fn` line is skipped (no such function exists today;
#     keep test functions small).
set -euo pipefail

cd "$(dirname "$0")/.."

MARKERS=(spawn_shell_for_view spawn_shell_with_args 'spawn_shell(' PtyBuilder::new fake_editor_script '#!/bin/sh')
FAIL=0

mapfile -d '' FILES < <(
	find crates -path crates/bitty-test-support -prune -o \
		\( -path 'crates/*/src/*.rs' -o -path 'crates/*/src/**/*.rs' \
		-o -path 'crates/*/tests/*.rs' -o -path 'crates/*/tests/**/*.rs' \) \
		-type f -print0 2>/dev/null
)

rg_args=()
for m in "${MARKERS[@]}"; do rg_args+=(--fixed-strings -e "$m"); done

is_blank_or_comment() {
	local re='^[[:space:]]*(//|$)'
	[[ "$1" =~ $re ]]
}

is_attr() {
	local re='^[[:space:]]*#\['
	[[ "$1" =~ $re ]]
}

is_fn_def() {
	local re='^[[:space:]]*(pub([^()]*)?)?fn[[:space:]]'
	[[ "$1" =~ $re ]]
}

for f in "${FILES[@]}"; do
	[ -n "$f" ] || continue
	if head -n 5 "$f" | grep -q 'pty-gate-exempt-file:'; then
		continue
	fi
	if [[ "$f" == */tests/* ]] && grep -q '#!\[cfg(unix)\]' "$f"; then
		continue
	fi
	while IFS= read -r hit; do
		lineno=${hit%%:*}
		content=${hit#*:}
		is_blank_or_comment "$content" && continue
		is_fn_def "$content" && continue
		# Per-hit exemption directive on this or the 3 lines above.
		start=$((lineno - 3))
		[ "$start" -lt 1 ] && start=1
		if sed -n "${start},${lineno}p" "$f" | grep -q 'pty-gate-exempt:'; then
			continue
		fi
		# Walk back to the enclosing `fn` item. Body statements between the
		# hit and the `fn` line are normal: the first `fn` line met walking
		# back is always the enclosing one (Rust has no nested `fn` items),
		# so keep going through code lines; blanks/comments/attrs are
		# transparent.
		fn_line=0
		l=$((lineno - 1))
		steps=0
		while [ "$l" -ge 1 ] && [ "$steps" -lt 300 ]; do
			line=$(sed -n "${l}p" "$f")
			steps=$((steps + 1))
			l=$((l - 1))
			if is_fn_def "$line"; then
				fn_line=$((l + 1))
				break
			fi
		done
		# No enclosing fn (module-level item): cannot spawn, skip.
		[ "$fn_line" -eq 0 ] && continue
		# Classify via the attribute lines directly above the fn.
		is_test=0
		has_cfg_unix=0
		l=$((fn_line - 1))
		for _ in 1 2 3 4 5 6 7 8; do
			[ "$l" -ge 1 ] || break
			line=$(sed -n "${l}p" "$f")
			if is_attr "$line"; then
				[[ "$line" == *'[test]'* ]] && is_test=1
				[[ "$line" == *'cfg(unix)'* ]] && has_cfg_unix=1
				l=$((l - 1))
				continue
			fi
			is_blank_or_comment "$line" && {
				l=$((l - 1))
				continue
			}
			break
		done
		# Not a #[test] fn (prod code or test helper): skip.
		[ "$is_test" -eq 0 ] && continue
		# Legacy cfg gate on the test: pass.
		[ "$has_cfg_unix" -eq 1 ] && continue
		# Otherwise the body between the fn line and the hit must call
		# require_pty!() first.
		if sed -n "$((fn_line + 1)),$((lineno - 1))p" "$f" | grep -q 'require_pty'; then
			continue
		fi
		# Edge: require_pty on the hit line itself (single-line test).
		if [[ "$content" == *require_pty* ]]; then
			continue
		fi
		echo "UNGATED live-spawn marker in $f:$lineno: $content"
		FAIL=1
	done < <(rg -n "${rg_args[@]}" "$f" 2>/dev/null || true)
done

if [ "$FAIL" -ne 0 ]; then
	echo
	echo "FAIL: ungated live-spawn test code (see above). Gate each hit with"
	echo "require_pty!() (preferred), #[cfg(unix)], or an audited"
	echo "// pty-gate-exempt: <reason> directive. Helper: bitty-test-support."
	exit 1
fi
echo "OK: all live-spawn test markers are gated."
