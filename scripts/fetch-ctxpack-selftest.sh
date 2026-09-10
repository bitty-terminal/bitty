#!/usr/bin/env bash
# fetch-ctxpack-selftest.sh — self-test for scripts/fetch-ctxpack.sh.
#
# Builds an isolated lab (temp HOME/XDG/TMPDIR + a local fixture mirror repo
# holding one redacted v2 ctxpack snapshot) and exercises, against throwaway
# target repos only:
#   1. empty-DB import: a fresh target with no state DB imports the LATEST
#      snapshot; restored task count and provenance match the fixture; the
#      pre-existing .carryctx/config.toml is left byte-identical.
#   2. non-empty refusal: a target with local rows is refused without
#      --force and its DB is unchanged.
#   3. --force path: the same target is replaced with --force; the local-only
#      task disappears and the fixture counts are restored.
#   4. --dry-run no-write: fetch + validate only; no state DB is created.
#   5. validation refusal: a tampered mirror (counts <-> *.jsonl skew) is
#      rejected before any DB write.
#
# Isolation guarantee: every case runs with HOME/XDG_*/TMPDIR redirected into
# the scratch dir and with --project pointing at a throwaway repository, so
# the real bitty CarryCtx DB is never opened, created, or mutated.
#
# Usage: scripts/fetch-ctxpack-selftest.sh
#   SELFTEST_KEEP_SCRATCH=1  keep the scratch dir and print its path.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FETCH="$REPO_ROOT/scripts/fetch-ctxpack.sh"
REDACT="$REPO_ROOT/scripts/publish-ctxpack-redact.py"
TIMEOUT_SECS="${SELFTEST_TIMEOUT:-180}"
KEEP_SCRATCH="${SELFTEST_KEEP_SCRATCH:-0}"

fail() {
	echo "fetch-ctxpack-selftest: FAIL: $1" >&2
	exit 1
}

pass() {
	echo "fetch-ctxpack-selftest: PASS: $1"
}

log() {
	echo "fetch-ctxpack-selftest: $1"
}

have() { command -v "$1" >/dev/null 2>&1; }

have git || fail "git not on PATH"
have carryctx || fail "carryctx not on PATH"
have python3 || fail "python3 not on PATH"
have timeout || fail "timeout not on PATH"
[[ -x "$FETCH" || -f "$FETCH" ]] || fail "fetch script $FETCH not found"
[[ -f "$REDACT" ]] || fail "redaction helper $REDACT not found"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/fetch-ctxpack-selftest.XXXXXX")"
cleanup() {
	if [[ "$KEEP_SCRATCH" == 0 && -d "$SCRATCH" ]]; then
		rm -rf "$SCRATCH"
	elif [[ -n "$SCRATCH" ]]; then
		log "keeping scratch dir $SCRATCH (SELFTEST_KEEP_SCRATCH=1)"
	fi
}
trap cleanup EXIT

# Isolate every carryctx/git invocation: temp HOME, XDG dirs, and TMPDIR so
# no global registry, config, or temp artifact can touch the real user state.
export HOME="$SCRATCH/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_STATE_HOME="$HOME/.local/state"
export TMPDIR="$SCRATCH/tmp"
mkdir -p "$HOME" "$TMPDIR"
unset CARRYCTX_AGENT CARRYCTX_SESSION CARRYCTX_TASK 2>/dev/null || true

gitq() { timeout "$TIMEOUT_SECS" git -C "$1" "${@:2}"; }
cctx() { timeout "$TIMEOUT_SECS" carryctx --project "$1" "${@:2}"; }

tasks_total() {
	cctx "$1" stats --json | python3 -c \
		'import json,sys; print(json.load(sys.stdin)["data"]["tasks_total"])'
}

has_task_title() {
	cctx "$1" task list --json | python3 -c '
import json, sys
title = sys.argv[1]
data = json.load(sys.stdin)["data"]
tasks = data if isinstance(data, list) else data.get("tasks", [])
print("yes" if any(t.get("title") == title for t in tasks) else "no")
' "$2"
}

git_init() {
	timeout "$TIMEOUT_SECS" git init -q -b main "$1"
	gitq "$1" config user.email "selftest@example.invalid"
	gitq "$1" config user.name "fetch-ctxpack-selftest"
}

# --- fixture: seed project + redacted v2 snapshot + local mirror repo -------
log "building fixture project + mirror in $SCRATCH"
SEED="$SCRATCH/seed"
mkdir -p "$SEED"
git_init "$SEED"
cctx "$SEED" init --name fetch-ctxpack-selftest --minimal --agent fetch-ctxpack-selftest >/dev/null ||
	fail "fixture carryctx init failed"
cctx "$SEED" agent register --name fetch-ctxpack-selftest --kind subagent \
	--role implementer --provider selftest >/dev/null || fail "fixture agent register failed"
cctx "$SEED" task create --title "fixture task one" --priority high \
	--agent fetch-ctxpack-selftest >/dev/null || fail "fixture task create failed"
cctx "$SEED" task create --title "fixture task two" --priority low \
	--agent fetch-ctxpack-selftest >/dev/null || fail "fixture task create failed"

PACK="$SCRATCH/pack"
timeout "$TIMEOUT_SECS" carryctx export --project "$SEED" --pack-format dir -o "$PACK" >/dev/null ||
	fail "fixture export failed"
timeout "$TIMEOUT_SECS" python3 "$REDACT" "$PACK" >/dev/null ||
	fail "fixture redaction failed"

# Drop [worktree.cleanup] from the fixture config so `carryctx init` at the
# import target rewrites it (current defaults add the section); case 1 then
# proves the script restores the committed config file byte-identically.
timeout "$TIMEOUT_SECS" python3 - "$SEED/.carryctx/config.toml" <<'PYEOF' ||
import re
import sys

path = sys.argv[1]
with open(path) as fh:
    text = fh.read()
stripped = re.sub(r"\n\[worktree\.cleanup\]\n(?:[a-z_]+ = .*\n)+", "\n", text)
if stripped == text or "[worktree.cleanup]" in stripped:
    sys.exit("fixture config strip failed")
with open(path, "w") as fh:
    fh.write(stripped)
PYEOF
	fail "fixture config strip failed"

SNAP_NAME="20260101T000000Z-fixture"
FIXTURE_COMMIT="cafebabecafebabecafebabecafebabecafebabe"
MIRROR_SRC="$SCRATCH/mirror-src"
mkdir -p "$MIRROR_SRC/$SNAP_NAME"
cp -a "$PACK/." "$MIRROR_SRC/$SNAP_NAME/"
printf '{"snapshot":"%s","bitty_commit":"%s","bitty_branch":"fixture","exported_at":"20260101T000000Z","redactions":0,"tool":"fetch-ctxpack-selftest"}\n' \
	"$SNAP_NAME" "$FIXTURE_COMMIT" >"$MIRROR_SRC/$SNAP_NAME/source.json"
printf '%s\n' "$SNAP_NAME" >"$MIRROR_SRC/LATEST"
git_init "$MIRROR_SRC"
gitq "$MIRROR_SRC" add -A
gitq "$MIRROR_SRC" commit -qm "fixture snapshot"

# --- helpers for per-case throwaway targets ---------------------------------
make_target() {
	local dir="$1"
	mkdir -p "$dir"
	git_init "$dir"
	cp -a "$SEED/.carryctx" "$dir/.carryctx"
	gitq "$dir" add -A
	gitq "$dir" commit -qm "base"
}

run_fetch() {
	local out="$1"
	shift
	set +e
	timeout "$TIMEOUT_SECS" "$FETCH" "$@" >"$out" 2>&1
	local rc=$?
	set -e
	echo "$rc"
}

MIRROR_CLONE="$SCRATCH/mirror-clone"
MIRROR_BAD_CLONE="$SCRATCH/mirror-bad-clone"

# --- case 1: empty-DB import ------------------------------------------------
log "case 1: empty-DB import"
TARGET1="$SCRATCH/target1"
make_target "$TARGET1"
RC="$(run_fetch "$SCRATCH/case1.log" --project "$TARGET1" --url "$MIRROR_SRC" --mirror "$MIRROR_CLONE")"
[[ "$RC" == 0 ]] || {
	cat "$SCRATCH/case1.log" >&2
	fail "case 1: fetch-ctxpack exit $RC (expected 0)"
}
[[ "$(tasks_total "$TARGET1")" == 2 ]] || fail "case 1: expected 2 restored tasks"
grep -q "snapshot=$SNAP_NAME" "$SCRATCH/case1.log" ||
	fail "case 1: provenance snapshot id missing"
grep -q "source_commit=$FIXTURE_COMMIT" "$SCRATCH/case1.log" ||
	fail "case 1: provenance source commit missing"
cmp -s "$SEED/.carryctx/config.toml" "$TARGET1/.carryctx/config.toml" ||
	fail "case 1: .carryctx/config.toml was not preserved"
pass "empty-DB import restored 2 tasks with provenance and preserved config"

# --- case 2: non-empty refusal ----------------------------------------------
log "case 2: non-empty refusal"
cctx "$TARGET1" task create --title "local-only task" --priority medium \
	--agent fetch-ctxpack-selftest >/dev/null || fail "case 2: local task create failed"
[[ "$(tasks_total "$TARGET1")" == 3 ]] || fail "case 2: expected 3 local tasks before refusal"
RC="$(run_fetch "$SCRATCH/case2.log" --project "$TARGET1" --url "$MIRROR_SRC" --mirror "$MIRROR_CLONE")"
[[ "$RC" != 0 ]] || fail "case 2: expected non-zero exit without --force"
grep -q "refusing to overwrite non-empty state" "$SCRATCH/case2.log" ||
	fail "case 2: refusal message missing"
[[ "$(tasks_total "$TARGET1")" == 3 ]] || fail "case 2: local DB changed during refusal"
[[ "$(has_task_title "$TARGET1" "local-only task")" == "yes" ]] ||
	fail "case 2: local-only task vanished during refusal"
pass "non-empty DB refused without --force and left unchanged"

# --- case 3: --force replacement --------------------------------------------
log "case 3: --force replacement"
RC="$(run_fetch "$SCRATCH/case3.log" --project "$TARGET1" --url "$MIRROR_SRC" --mirror "$MIRROR_CLONE" --force)"
[[ "$RC" == 0 ]] || {
	cat "$SCRATCH/case3.log" >&2
	fail "case 3: fetch-ctxpack exit $RC (expected 0 with --force)"
}
[[ "$(tasks_total "$TARGET1")" == 2 ]] || fail "case 3: expected fixture count 2 after --force"
[[ "$(has_task_title "$TARGET1" "local-only task")" == "no" ]] ||
	fail "case 3: local-only task survived --force replacement"
pass "--force replaced non-empty DB with the snapshot"

# --- case 4: --dry-run writes nothing ---------------------------------------
log "case 4: --dry-run no-write"
TARGET2="$SCRATCH/target2"
make_target "$TARGET2"
RC="$(run_fetch "$SCRATCH/case4.log" --project "$TARGET2" --url "$MIRROR_SRC" --mirror "$MIRROR_CLONE" --dry-run)"
[[ "$RC" == 0 ]] || {
	cat "$SCRATCH/case4.log" >&2
	fail "case 4: dry-run exit $RC (expected 0)"
}
grep -q "dry-run PASS" "$SCRATCH/case4.log" || fail "case 4: dry-run PASS line missing"
[[ ! -e "$TARGET2/.git/carryctx/state.sqlite" ]] ||
	fail "case 4: dry-run created a state DB"
pass "dry-run validated without creating or writing the local DB"

# --- case 5: tampered snapshot refused before any write ---------------------
log "case 5: tampered snapshot refusal"
MIRROR_BAD="$SCRATCH/mirror-bad"
cp -a "$MIRROR_SRC" "$MIRROR_BAD"
printf '%s\n' '{"display_id":"CTX-9999","row":"tampered"}' >>"$MIRROR_BAD/$SNAP_NAME/tasks.jsonl"
gitq "$MIRROR_BAD" add -A
gitq "$MIRROR_BAD" commit -qm "tamper: counts <-> jsonl skew"
TARGET3="$SCRATCH/target3"
make_target "$TARGET3"
RC="$(run_fetch "$SCRATCH/case5.log" --project "$TARGET3" --url "$MIRROR_BAD" --mirror "$MIRROR_BAD_CLONE" --dry-run)"
[[ "$RC" != 0 ]] || fail "case 5: tampered snapshot was not refused"
grep -q "snapshot validation failed" "$SCRATCH/case5.log" ||
	fail "case 5: validation failure message missing"
[[ ! -e "$TARGET3/.git/carryctx/state.sqlite" ]] ||
	fail "case 5: tampered snapshot wrote a state DB"
pass "tampered snapshot refused before any local DB write"

# --- case 6: empty DB file (read-command side effect) still restores -------
log "case 6: empty DB file with no project row"
TARGET4="$SCRATCH/target4"
make_target "$TARGET4"
cctx "$TARGET4" project show >/dev/null 2>&1 ||
	fail "case 6: project show failed"
[[ -e "$TARGET4/.git/carryctx/state.sqlite" ]] ||
	fail "case 6: project show did not create the expected empty DB"
RC="$(run_fetch "$SCRATCH/case6.log" --project "$TARGET4" --url "$MIRROR_SRC" --mirror "$MIRROR_CLONE")"
[[ "$RC" == 0 ]] || {
	cat "$SCRATCH/case6.log" >&2
	fail "case 6: fetch-ctxpack exit $RC (expected 0)"
}
grep -q "no project row in local DB" "$SCRATCH/case6.log" ||
	fail "case 6: init path was not taken"
[[ "$(tasks_total "$TARGET4")" == 2 ]] || fail "case 6: expected 2 restored tasks"
pass "empty DB with no project row was initialized and restored"

log "all cases PASS (empty import, refusal, --force, dry-run, tamper refusal, empty-DB init)"
