#!/usr/bin/env bash
# docs-pin.test.sh — CTX-0810 fixture test for scripts/docs-pin.sh.
#
# Builds ephemeral fixture repositories (no network): an upstream docs repo,
# a workspace docs checkout cloned from it, and a parent repo whose `docs`
# gitlink is pinned to the first upstream commit with an empty mount. Proves:
#   - the default rev pins origin/main and the mount stays empty;
#   - an explicit merged rev pins; a re-pin to the same commit is a no-op;
#   - an unmerged (side-branch) commit, an unknown rev, a foreign origin, and
#     a populated mount are refused with the index untouched;
#   - an scp-style origin matches an https `.gitmodules` URL;
#   - without BITTY_TERMINAL_DOCS/BITTY_WORKSPACE the checkout is found next
#     to the primary checkout, also when run from a linked worktree.
set -euo pipefail

cd "$(dirname "$0")/../.."
SCRIPT="$PWD/scripts/docs-pin.sh"
FAIL=0

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/docs-pin-test.XXXXXX")"
cleanup() {
	rm -rf "$TMP_BASE"
}
trap cleanup EXIT

export GIT_AUTHOR_NAME="docs-pin-test"
export GIT_AUTHOR_EMAIL="docs-pin-test@local"
export GIT_COMMITTER_NAME="docs-pin-test"
export GIT_COMMITTER_EMAIL="docs-pin-test@local"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
unset BITTY_WORKSPACE BITTY_TERMINAL_DOCS

readonly URL="https://example.invalid/fixture-org/fixture-docs"
WS="$TMP_BASE/ws"
UPSTREAM="$TMP_BASE/upstream"
DOCS="$WS/fixture-docs"
PARENT="$WS/parent"

git init -q -b main "$UPSTREAM"
git -C "$UPSTREAM" commit -q --allow-empty -m c1
C1="$(git -C "$UPSTREAM" rev-parse HEAD)"
git -C "$UPSTREAM" commit -q --allow-empty -m c2
C2="$(git -C "$UPSTREAM" rev-parse HEAD)"
git -C "$UPSTREAM" checkout -q -b side "$C1"
git -C "$UPSTREAM" commit -q --allow-empty -m side
SIDE="$(git -C "$UPSTREAM" rev-parse HEAD)"
git -C "$UPSTREAM" checkout -q main

mkdir -p "$WS"
git clone -q --no-single-branch "$UPSTREAM" "$DOCS"
git -C "$DOCS" remote set-url origin "$URL.git"

git init -q -b main "$PARENT"
printf '[submodule "docs"]\n\tpath = docs\n\turl = %s\n' "$URL" >"$PARENT/.gitmodules"
mkdir "$PARENT/docs"
git -C "$PARENT" add .gitmodules
git -C "$PARENT" update-index --add --cacheinfo "160000,$C1,docs"
git -C "$PARENT" commit -q -m parent

pin_of() {
	git -C "${1:-$PARENT}" ls-files --stage -- docs | cut -d' ' -f2
}

# run <name> <expected-exit> <expected-pin> [args...] (env via caller).
run() {
	local name="$1" want_status="$2" want_pin="$3"
	shift 3
	local out status=0
	out="$(cd "$PARENT" && bash "$SCRIPT" "$@" 2>&1)" || status=$?
	if ((status != want_status)); then
		echo "FAIL: $name: exit $status, want $want_status: $out" >&2
		FAIL=1
	fi
	local pin
	pin="$(pin_of)"
	if [[ "$pin" != "$want_pin" ]]; then
		echo "FAIL: $name: pin ${pin:0:12}, want ${want_pin:0:12}" >&2
		FAIL=1
	fi
	if [[ -n "$(ls -A "$PARENT/docs")" ]]; then
		echo "FAIL: $name: docs/ mount was populated" >&2
		FAIL=1
	fi
}

# Default rev resolves origin/main of the checkout found next to the parent.
run "default rev" 0 "$C2"
run "same pin is a no-op" 0 "$C2"
run "explicit merged rev" 0 "$C1" "$C1"
run "unmerged side commit" 1 "$C1" "$SIDE"
run "unknown rev" 1 "$C1" no-such-rev
run "too many args" 1 "$C1" "$C1" extra

git -C "$DOCS" remote set-url origin "git@example.invalid:fixture-org/fixture-docs.git"
run "scp-style origin" 0 "$C2" origin/main
git -C "$PARENT" update-index --cacheinfo "160000,$C1,docs"

git -C "$DOCS" remote set-url origin "https://example.invalid/other-org/fixture-docs"
run "foreign origin" 1 "$C1"
git -C "$DOCS" remote set-url origin "$URL"

printf 'gitdir: elsewhere\n' >"$PARENT/docs/.git"
out="$(cd "$PARENT" && bash "$SCRIPT" 2>&1)" && {
	echo "FAIL: populated mount accepted: $out" >&2
	FAIL=1
}
[[ "$(pin_of)" == "$C1" ]] || {
	echo "FAIL: populated mount moved the pin" >&2
	FAIL=1
}
rm "$PARENT/docs/.git"

# Explicit override wins over the derived location.
mv "$DOCS" "$TMP_BASE/elsewhere"
run "missing checkout" 1 "$C1"
out="$(cd "$PARENT" && BITTY_TERMINAL_DOCS="$TMP_BASE/elsewhere" bash "$SCRIPT" 2>&1)" || {
	echo "FAIL: BITTY_TERMINAL_DOCS override: $out" >&2
	FAIL=1
}
[[ "$(pin_of)" == "$C2" ]] || {
	echo "FAIL: override did not pin origin/main" >&2
	FAIL=1
}
mv "$TMP_BASE/elsewhere" "$DOCS"

# A linked worktree derives the workspace from the common git dir.
WT="$PARENT/.worktrees/task"
git -C "$PARENT" worktree add -q -b task "$WT" main
out="$(cd "$WT" && bash "$SCRIPT" "$C2" 2>&1)" || {
	echo "FAIL: worktree run: $out" >&2
	FAIL=1
}
[[ "$(pin_of "$WT")" == "$C2" ]] || {
	echo "FAIL: worktree pin not updated" >&2
	FAIL=1
}

if ((FAIL)); then
	echo "docs-pin-test: FAIL" >&2
	exit 1
fi
echo "docs-pin-test: OK"
