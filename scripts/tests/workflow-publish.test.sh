#!/usr/bin/env bash
# workflow-publish.test.sh — CTX-0434 fixture test for the snapshot closeout.
#
# Proves each of the four root-caused defects stays fixed, using ephemeral
# fixture git repositories (no network, no CarryCtx database):
#   defect 1  the helper resolves scripts/workflow-publish.sh from the fresh
#             origin/main worktree, never from the primary checkout.
#   defect 2  the helper exports from a fresh origin/main worktree and
#             workflow-publish refuses a stale (behind) source.
#   defect 3  the repository name comes from the origin remote URL basename;
#             an undeterminable name fails loudly.
#   defect 4  a snapshot ref that does not advance after export fails loudly.
#
# The workspace helper is located via the common git dir so this also runs
# from a task worktree. In checkouts without a workspace parent carrying the
# (untracked) helper, the helper section reports SKIP while the per-repo
# checks still run.
set -euo pipefail

cd "$(dirname "$0")/../.."

PUBLISH=./scripts/workflow-publish.sh
FAIL=0

COMMON_DIR="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
PRIMARY_ROOT="$(dirname "$COMMON_DIR")"
WORKSPACE_ROOT="$(dirname "$PRIMARY_ROOT")"
HELPER="$WORKSPACE_ROOT/script/publish-snapshots.sh"

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/workflow-publish-test.XXXXXX")"
cleanup() {
	rm -rf "$TMP_BASE"
}
trap cleanup EXIT

export GIT_AUTHOR_NAME="workflow-publish-test"
export GIT_AUTHOR_EMAIL="workflow-publish-test@local"
export GIT_COMMITTER_NAME="workflow-publish-test"
export GIT_COMMITTER_EMAIL="workflow-publish-test@local"

pass() {
	echo "PASS: $1"
}

fail() {
	echo "FAIL: $1" >&2
	FAIL=1
}

# A stub carryctx that always succeeds; the tests that need it fail or pass
# before a real export would matter (branch/name checks, dry-run, no-op).
TEST_BIN="$TMP_BASE/bin"
mkdir -p "$TEST_BIN"
cat >"$TEST_BIN/carryctx" <<'STUB_EOF'
#!/usr/bin/env bash
exit 0
STUB_EOF
chmod +x "$TEST_BIN/carryctx"

# --- defect 1: helper resolves the script from the worktree ---
if [[ ! -x "$HELPER" ]]; then
	echo "SKIP: helper tests (no workspace helper at $HELPER in this checkout)"
else
	if grep -qF 'publish_script="$repo/scripts/workflow-publish.sh"' "$HELPER"; then
		fail "defect-1 helper still gates on the primary checkout"
	else
		pass "defect-1 helper no longer gates on the primary checkout"
	fi
	if grep -qF 'publish_script="$target/scripts/workflow-publish.sh"' "$HELPER"; then
		pass "defect-1 helper resolves the script from the worktree"
	else
		fail "defect-1 helper does not resolve the script from the worktree"
	fi
	if grep -qF 'is not current with origin/main' "$HELPER"; then
		pass "defect-2 helper verifies the worktree is current"
	else
		fail "defect-2 helper has no current-source verification"
	fi

	# Functional fixture: origin carries the script at C2, the primary
	# checkout is parked at C1 without it. The old helper skipped silently;
	# the fixed helper publishes through the fresh worktree.
	FIX_WS="$TMP_BASE/ws1"
	SEED="$TMP_BASE/seed1"
	BARE_ORIGIN="$TMP_BASE/myrepo.git"
	STUB_MARKER="$TMP_BASE/ws1-marker"
	STUB_LOG="$TMP_BASE/ws1-log"
	mkdir -p "$FIX_WS"
	git init -q -b main "$SEED"
	git -C "$SEED" commit -q --allow-empty -m c1
	C1="$(git -C "$SEED" rev-parse HEAD)"
	mkdir -p "$SEED/scripts"
	cat >"$SEED/scripts/workflow-publish.sh" <<'STUB_EOF'
#!/usr/bin/env bash
echo "stub HEAD $(git rev-parse HEAD) ORIGIN $(git rev-parse origin/main 2>/dev/null || echo none)" >>"$STUB_LOG"
touch "$STUB_MARKER"
exit 0
STUB_EOF
	chmod +x "$SEED/scripts/workflow-publish.sh"
	git -C "$SEED" add scripts/workflow-publish.sh
	git -C "$SEED" commit -q -m c2
	C2="$(git -C "$SEED" rev-parse HEAD)"
	git init -q --bare "$BARE_ORIGIN"
	git -C "$SEED" remote add origin "$BARE_ORIGIN"
	git -C "$SEED" push -q origin main
	git clone -q "$BARE_ORIGIN" "$FIX_WS/myrepo"
	git -C "$FIX_WS/myrepo" checkout -q "$C1"
	if [[ -x "$FIX_WS/myrepo/scripts/workflow-publish.sh" ]]; then
		fail "defect-1 fixture setup broken (primary unexpectedly has the script)"
	fi
	export STUB_MARKER STUB_LOG
	helper_out="$(BITTY_WORKSPACE="$FIX_WS" bash "$HELPER" 2>&1)" || helper_status=$?
	helper_status="${helper_status:-0}"
	if ((helper_status != 0)); then
		fail "defect-1 helper exited $helper_status (expected 0): $helper_out"
	elif [[ ! -f "$STUB_MARKER" ]]; then
		fail "defect-1 helper skipped publication (silent-skip behavior): $helper_out"
	elif ! grep -q "ok   myrepo" <<<"$helper_out"; then
		fail "defect-1 helper did not report ok myrepo: $helper_out"
	else
		pass "defect-1 helper publishes via the origin/main worktree"
	fi
	WT_HEAD="$(git -C "$FIX_WS/.targets/closeout/myrepo" rev-parse HEAD 2>/dev/null || true)"
	ORIGIN_MAIN="$(git -C "$FIX_WS/myrepo" rev-parse origin/main 2>/dev/null || true)"
	if [[ -n "$WT_HEAD" && "$WT_HEAD" == "$ORIGIN_MAIN" && "$WT_HEAD" == "$C2" ]]; then
		pass "defect-2 helper exports from fresh origin/main"
	else
		fail "defect-2 worktree HEAD ${WT_HEAD:-unknown} != origin/main ${ORIGIN_MAIN:-unknown} (expected $C2)"
	fi
	if grep -q "stub HEAD $C2 ORIGIN $C2" "$STUB_LOG" 2>/dev/null; then
		pass "defect-2 stub observed a current export source"
	else
		fail "defect-2 stub did not observe a current source: $(cat "$STUB_LOG" 2>/dev/null || echo missing)"
	fi
	unset STUB_MARKER STUB_LOG
fi

# --- defect 2 (per-repo): a behind source fails loudly ---
SRC2="$TMP_BASE/src2"
BARE2="$TMP_BASE/remote2.git"
CLONE2="$TMP_BASE/clone2"
git init -q -b main "$SRC2"
git -C "$SRC2" commit -q --allow-empty -m a
git -C "$SRC2" commit -q --allow-empty -m b
git init -q --bare "$BARE2"
git -C "$SRC2" remote add origin "$BARE2"
git -C "$SRC2" push -q origin main
git clone -q "$BARE2" "$CLONE2"
git -C "$CLONE2" checkout -q --detach HEAD~1
behind_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --dry-run --allow-name-mismatch --project "$CLONE2" 2>&1)" || behind_status=$?
behind_status="${behind_status:-0}"
if ((behind_status == 0)); then
	fail "defect-2 behind source was accepted (expected loud failure): $behind_out"
elif grep -q "refusing stale publication" <<<"$behind_out"; then
	pass "defect-2 behind detached source fails loudly"
else
	fail "defect-2 behind source failed without the stale-source message: $behind_out"
fi
git -C "$CLONE2" checkout -q --detach origin/main
fresh_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --dry-run --allow-name-mismatch --project "$CLONE2" 2>&1)" || fresh_status=$?
fresh_status="${fresh_status:-0}"
if ((fresh_status != 0)); then
	fail "defect-2 fresh detached source was rejected: $fresh_out"
elif grep -q "dry-run PASS" <<<"$fresh_out"; then
	pass "defect-2 fresh detached source is accepted"
else
	fail "defect-2 fresh source gave no dry-run PASS: $fresh_out"
fi

# --- defect 3: repository name comes from the remote URL ---
if grep -q "cannot determine repository name" "$PUBLISH"; then
	pass "defect-3 undeterminable name fails loudly"
else
	fail "defect-3 has no loud failure for an undeterminable name"
fi
MISMATCH_DIR="$TMP_BASE/wrong-name"
git init -q -b main "$MISMATCH_DIR"
git -C "$MISMATCH_DIR" commit -q --allow-empty -m init
git -C "$MISMATCH_DIR" remote add origin "https://example.invalid/org/realrepo.git"
mismatch_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --dry-run --project "$MISMATCH_DIR" 2>&1)" || mismatch_status=$?
mismatch_status="${mismatch_status:-0}"
if ((mismatch_status == 0)); then
	fail "defect-3 basename mismatch was accepted (expected failure): $mismatch_out"
elif grep -q "checkout basename 'wrong-name' != repository name 'realrepo'" <<<"$mismatch_out"; then
	pass "defect-3 basename mismatch fails loudly"
else
	fail "defect-3 mismatch failed without the basename message: $mismatch_out"
fi
allow_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --dry-run --allow-name-mismatch --project "$MISMATCH_DIR" 2>&1)" || allow_status=$?
allow_status="${allow_status:-0}"
if ((allow_status != 0)); then
	fail "defect-3 --allow-name-mismatch was rejected: $allow_out"
elif grep -q "WARN: checkout basename" <<<"$allow_out"; then
	pass "defect-3 --allow-name-mismatch warns and proceeds"
else
	fail "defect-3 allow flag gave no basename WARN: $allow_out"
fi
NOREMOTE_DIR="$TMP_BASE/noremote"
git init -q -b main "$NOREMOTE_DIR"
git -C "$NOREMOTE_DIR" commit -q --allow-empty -m init
noremote_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --dry-run --project "$NOREMOTE_DIR" 2>&1)" || noremote_status=$?
noremote_status="${noremote_status:-0}"
if ((noremote_status == 0)); then
	fail "defect-3 missing remote was accepted (expected loud failure): $noremote_out"
elif grep -q "cannot determine repository name" <<<"$noremote_out"; then
	pass "defect-3 missing remote fails loudly"
else
	fail "defect-3 missing remote failed without the name message: $noremote_out"
fi

# --- defect 4: no-advance after export fails loudly ---
if grep -q "did not advance after export" "$PUBLISH"; then
	pass "defect-4 no-advance fails loudly"
else
	fail "defect-4 has no loud no-advance failure"
fi
if grep -q "nothing to push" "$PUBLISH"; then
	fail "defect-4 silent no-op message is still present"
else
	pass "defect-4 silent no-op message is gone"
fi
NOADV_DIR="$TMP_BASE/noadv-repo"
NOADV_BARE="$TMP_BASE/noadv-origin.git"
git init -q -b main "$NOADV_DIR"
git -C "$NOADV_DIR" commit -q --allow-empty -m init
git init -q --bare "$NOADV_BARE"
git -C "$NOADV_DIR" remote add origin "$NOADV_BARE"
git -C "$NOADV_DIR" push -q -u origin main
git -C "$NOADV_DIR" checkout -q -b snap-src
printf '{"redacted": true}\n' >"$NOADV_DIR/manifest.json"
git -C "$NOADV_DIR" add manifest.json
git -C "$NOADV_DIR" commit -q -m snap
SNAP_SHA="$(git -C "$NOADV_DIR" rev-parse HEAD)"
git -C "$NOADV_DIR" checkout -q main
git -C "$NOADV_DIR" update-ref refs/heads/carryctx-snapshots "$SNAP_SHA"
noadv_out="$(PATH="$TEST_BIN:$PATH" bash "$PUBLISH" --allow-name-mismatch --project "$NOADV_DIR" 2>&1)" || noadv_status=$?
noadv_status="${noadv_status:-0}"
if ((noadv_status == 0)); then
	fail "defect-4 no-advance was accepted (expected loud failure): $noadv_out"
elif grep -q "did not advance after export" <<<"$noadv_out"; then
	pass "defect-4 no-advance fails loudly"
else
	fail "defect-4 no-advance failed without the advance message: $noadv_out"
fi

if ((FAIL)); then
	echo "workflow-publish-test: FAIL" >&2
	exit 1
fi
echo "workflow-publish-test: OK"
