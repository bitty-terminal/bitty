#!/usr/bin/env bash
# publish-ctxpack.sh — publish a ctxpack snapshot to the workflow-mirror repo.
#
# On every PR merge the commander runs `just workflow-publish` from a
# bitty checkout (the trigger is the merge closeout, NOT a git hook:
# GitHub squash-merges never fire local git hooks, and `carryctx hooks
# install` behavior is intentionally left untouched).
#
# Repo-specific bits are env defaults, not hardcodes: override
# WORKFLOW_SOURCE_REPO / WORKFLOW_MIRROR_URL / WORKFLOW_MIRROR_DIR to reuse
# this script verbatim in another repo. Defaults target
# bitty-terminal/bitty-workflow.
#
# Publish-from-main (provenance): source.json records the branch of the
# checkout this script runs in (`git branch --show-current`), so a detached
# worktree records `repo_branch: detached`. For the cleanest provenance run
# the closeout publish from the primary checkout on main:
#   cd "$BITTY_WORKSPACE/bitty" && just workflow-publish
# Detached publishes still work; they warn and record `detached`.
#
# What it does:
#   1. Exports `carryctx export --pack-format dir` from this repo into a
#      staging clone of bitty-terminal/bitty-workflow as
#      <UTC-date>-<bitty-main-sha>/.
#   2. Redacts secret-shaped values inside the STAGING copy only (export-time
#      redaction pass, scripts/publish-ctxpack-redact.py): values of
#      secret-named fields (*_KEY/*_TOKEN/*_SECRET/*_PASSWORD, GH_PAT,
#      CLOUDFLARE_*, AWS_*) and 40+-char token-like runs become
#      `***REDACTED***`. JSONL-aware (parse per line, redact, re-serialize),
#      row counts unchanged, re-runnable no-op, covers every table file
#      including format-v2 `tombstones.jsonl`; stamps `redacted: true` into
#      manifest.json (counts untouched). The local carryctx DB is
#      never modified -- it keeps the originals and the next export redacts
#      them again.
#   3. Validates (round-trip self-test for format v1 and v2 snapshots, incl.
#      a planted-fake-secret fixture proving staging is redacted while the
#      source copy is untouched, and a validator accept/reject fixture
#      matrix).
#   4. Refreshes the mirror README.md pointer + LATEST file.
#   5. Commits and pushes to the mirror repo main branch.
#
# Privacy: the pack intentionally contains agent display names and absolute
# workspace paths, but secret-shaped values can NEVER reach the mirror: the
# redaction pass (step 2) runs on every export, dry-run or publish, before
# validation and commit, so even a live key accidentally pasted into a note
# or session payload is replaced with `***REDACTED***` (names kept for
# debuggability). A leaked-then-rotated secret still needs rotation at the
# source -- redaction limits mirror exposure, it does not un-leak anything.
#
# Usage:
#   scripts/publish-ctxpack.sh [--dry-run] [--mirror DIR] [--url URL]
#                              [--git-timeout SECS] [--keep-tmp]
#   --dry-run      export + validate (incl. round-trip self-test) only;
#                  no clone, no commit, no push.
#   --mirror DIR   staging clone location
#                  (default: $WORKFLOW_MIRROR_DIR or
#                  <repo>/../bitty-workflow).
#   --url URL      mirror remote for a fresh clone
#                  (default: $WORKFLOW_MIRROR_URL or
#                  https://github.com/bitty-terminal/bitty-workflow.git).
#   --git-timeout SECS  timeout for every git/carryctx network or heavy op
#                  (default: $GIT_TIMEOUT or 120).
#   --keep-tmp     keep temp dirs on exit (debug aid; prints paths).
#
# Guarantees: idempotent (re-running for the same source SHA reuses the
# existing snapshot dir and no-ops when the mirror is already current);
# fail-closed with clear errors; never writes to the source repo's own git
# state (only read-only rev-parse/branch queries there).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_REPO="${WORKFLOW_SOURCE_REPO:-bitty}"
MIRROR_NAME="${SOURCE_REPO}-workflow"
MIRROR_DEFAULT="$REPO_ROOT/../$MIRROR_NAME"
MIRROR_URL_DEFAULT="https://github.com/bitty-terminal/$MIRROR_NAME.git"

DRY_RUN=0
MIRROR="${WORKFLOW_MIRROR_DIR:-$MIRROR_DEFAULT}"
MIRROR_URL="${WORKFLOW_MIRROR_URL:-$MIRROR_URL_DEFAULT}"
GIT_TIMEOUT="${GIT_TIMEOUT:-120}"
KEEP_TMP=0
REDACTIONS=0

while [[ $# -gt 0 ]]; do
	case "$1" in
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--mirror)
		MIRROR="${2:?--mirror requires a directory}"
		shift 2
		;;
	--mirror=*)
		MIRROR="${1#--mirror=}"
		shift
		;;
	--url)
		MIRROR_URL="${2:?--url requires a remote URL}"
		shift 2
		;;
	--url=*)
		MIRROR_URL="${1#--url=}"
		shift
		;;
	--git-timeout)
		GIT_TIMEOUT="${2:?--git-timeout requires seconds}"
		shift 2
		;;
	--git-timeout=*)
		GIT_TIMEOUT="${1#--git-timeout=}"
		shift
		;;
	--keep-tmp)
		KEEP_TMP=1
		shift
		;;
	--help | -h)
		sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}"
		exit 0
		;;
	*)
		echo "publish-ctxpack: FAIL: unknown flag $1 (see --help)" >&2
		exit 2
		;;
	esac
done

fail() {
	echo "publish-ctxpack: FAIL: $1" >&2
	exit 1
}

log() {
	echo "publish-ctxpack: $1"
}

have() { command -v "$1" >/dev/null 2>&1; }

have git || fail "git not on PATH"
have carryctx || fail "carryctx not on PATH"
have timeout || fail "timeout not on PATH"
have python3 || fail "python3 not on PATH (needed by the redaction pass)"

# Read-only introspection of the source checkout. Never a write op here:
# no checkout, reset, commit, stash, or index/worktree mutation.
REPO_SHA="$(timeout "$GIT_TIMEOUT" git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null)" ||
	fail "cannot read $SOURCE_REPO HEAD (not a git checkout?)"
REPO_SHORT="$(timeout "$GIT_TIMEOUT" git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null)" ||
	fail "cannot read $SOURCE_REPO short SHA"
REPO_BRANCH="$(timeout "$GIT_TIMEOUT" git -C "$REPO_ROOT" branch --show-current 2>/dev/null || true)"
if [[ "${REPO_BRANCH:-}" != "main" ]]; then
	log "WARN: $SOURCE_REPO checkout is on branch '${REPO_BRANCH:-detached}', snapshotting HEAD anyway"
fi
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SNAPSHOT="${STAMP}-${REPO_SHORT}"

TMP_ROOT=""
cleanup() {
	if [[ "$KEEP_TMP" == 0 && -n "$TMP_ROOT" && -d "$TMP_ROOT" ]]; then
		rm -rf "$TMP_ROOT"
	elif [[ -n "$TMP_ROOT" ]]; then
		log "keeping tmp dir $TMP_ROOT (--keep-tmp)"
	fi
}
trap cleanup EXIT

# Mirror git ops always go through this wrapper: timeout-wrapped and
# confined to the staging clone. $REPO_ROOT is never a write target.
mirror_git() {
	timeout "$GIT_TIMEOUT" git -C "$MIRROR" "$@"
}

ensure_mirror() {
	if [[ -d "$MIRROR/.git" ]]; then
		local remote
		remote="$(mirror_git remote get-url origin 2>/dev/null)" ||
			fail "staging clone $MIRROR has no origin remote"
		case "$remote" in
		*"$MIRROR_NAME"*)
			;;
		*)
			fail "staging clone origin ($remote) does not look like $MIRROR_NAME; refusing to push (override with --mirror/--url)"
			;;
		esac
		if [[ -n "$(mirror_git status --porcelain 2>/dev/null)" ]]; then
			fail "staging clone $MIRROR has uncommitted changes; clean it up and re-run"
		fi
		mirror_git fetch origin --prune ||
			fail "git fetch failed in staging clone (network/auth?)"
		if mirror_git rev-parse --verify HEAD >/dev/null 2>&1; then
			mirror_git checkout -q main 2>/dev/null ||
				mirror_git checkout -q -b main --track origin/main ||
				fail "cannot check out main in staging clone"
			mirror_git reset -q --hard origin/main ||
				fail "cannot fast-forward staging clone to origin/main"
		else
			# Freshly cloned empty repo (or brand-new mirror): start main here.
			mirror_git checkout -q -b main ||
				fail "cannot create main branch in empty staging clone"
		fi
		return 0
	fi
	if [[ -e "$MIRROR" ]]; then
		fail "$MIRROR exists but is not a git clone; move it aside or pass --mirror"
	fi
	log "cloning mirror $MIRROR_URL -> $MIRROR"
	timeout "$GIT_TIMEOUT" git clone -q "$MIRROR_URL" "$MIRROR" ||
		fail "git clone failed (network/auth?); set --url or pre-clone --mirror yourself"
	ensure_mirror
}

write_mirror_readme() {
	# Refresh the mirror-root pointer only when the template changes
	# (keeps re-publishes idempotent: no churn commit for identical text).
	local readme="$MIRROR/README.md" tmp
	tmp="$(mktemp)"
	cat >"$tmp" <<'EOF'
# __MIRROR_NAME__ — engineering workflow mirror

Transparent companion to [__SOURCE_REPO__](https://github.com/bitty-terminal/__SOURCE_REPO__):
every PR merge publishes a CarryCtx ctxpack snapshot here, so the whole
engineering workflow (tasks, sessions, decisions, checkpoints) is reviewable,
not just the code.

- Each `<UTC-timestamp>-<source-sha>/` directory is one
  `carryctx export --pack-format dir` snapshot (manifest.json + project.json
  + per-table `*.jsonl`; format v2 snapshots also carry `tombstones.jsonl`).
- `LATEST` names the newest snapshot directory.
- Each snapshot carries a `source.json` with the source commit/branch it was
  taken from (`repo`, `repo_commit`, `repo_branch` keys).
- Every snapshot is a redacted publication artifact (`redacted: true` in
  manifest.json); CarryCtx refuses redacted bundles as merge sources, so
  snapshots are never merged back. To restore a fresh clone's local CarryCtx
  DB from `LATEST`, run `just workflow-import` (`just workflow-import-dry`
  validates only); restore always uses replace mode and refuses to replace a
  non-empty local DB without `--force`.
- Trigger: the commander's merge closeout runs `just workflow-publish` in
  the __SOURCE_REPO__ repo. No git hook drives this (squash-merges never fire local
  hooks).

## Privacy notice

Snapshots intentionally contain agent display names and absolute workspace
paths. Secret-shaped values never reach the mirror: every export passes
through an automatic JSONL-aware redaction step (all table files, including
`tombstones.jsonl`) before validation and commit (secret-named fields such
as `*_KEY` / `*_TOKEN` / `*_SECRET` /
`*_PASSWORD`, `GH_PAT`, `CLOUDFLARE_*`, `AWS_*`, plus `NAME=value` pairs
and 40+-char token-like runs in free text, become `***REDACTED***`;
field/variable names are kept for debuggability; the local database is
never modified). Redaction limits mirror exposure -- it does not un-leak a
secret that was already pushed anywhere: rotate at the source and report
suspected leaks to the repository owner immediately.
EOF
	sed -i "s/__MIRROR_NAME__/$MIRROR_NAME/g; s/__SOURCE_REPO__/$SOURCE_REPO/g" "$tmp"
	if [[ -f "$readme" ]] && cmp -s "$tmp" "$readme"; then
		rm -f "$tmp"
	else
		mv "$tmp" "$readme"
	fi
}

run_export() {
	local dest="$1"
	log "exporting ctxpack -> $dest"
	timeout "$GIT_TIMEOUT" carryctx export --pack-format dir -o "$dest" --project "$REPO_ROOT" ||
		fail "carryctx export failed"
	[[ -f "$dest/manifest.json" && -f "$dest/project.json" ]] ||
		fail "export incomplete (manifest.json/project.json missing)"
	redact_snapshot "$dest"
}

# Export-time redaction seam (post-export, pre-validate/commit). Operates
# ONLY on the snapshot copy ($dest: the staging clone or a --dry-run tmp
# dir); the local carryctx DB is strictly read-only throughout this script.
# Row counts are unchanged, so manifest counts and the self-test still hold.
redact_snapshot() {
	local dest="$1" redact_out
	log "redacting secret-shaped values in staging copy $dest (local DB untouched)"
	redact_out="$(timeout "$GIT_TIMEOUT" python3 "$REPO_ROOT/scripts/publish-ctxpack-redact.py" "$dest")" ||
		fail "secret redaction failed; mirror left untouched"
	log "$redact_out"
	REDACTIONS="$(printf '%s\n' "$redact_out" | sed -n 's/^REDACTIONS=//p')"
	REDACTIONS="${REDACTIONS:-0}"
}

run_selftest() {
	local dest="$1"
	log "validating snapshot (self-test)"
	bash "$REPO_ROOT/scripts/publish-ctxpack-selftest.sh" "$dest" ||
		fail "snapshot self-test failed; mirror left untouched"
}

if [[ "$DRY_RUN" == 1 ]]; then
	TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ctxpack-dry.XXXXXX")"
	SNAP_DIR="$TMP_ROOT/$SNAPSHOT"
	run_export "$SNAP_DIR"
	run_selftest "$SNAP_DIR"
	log "dry-run PASS: snapshot $SNAPSHOT exports + validates; nothing pushed"
	exit 0
fi

ensure_mirror

# Idempotency: a snapshot for this source SHA already published? Reuse it so
# a re-run converges instead of duplicating history.
EXISTING=""
shopt -s nullglob
candidates=("$MIRROR"/*-"$REPO_SHORT")
if ((${#candidates[@]} > 0)) && [[ -d "${candidates[0]}" ]]; then
	EXISTING="$(basename "${candidates[0]}")"
fi
shopt -u nullglob

if [[ -n "$EXISTING" ]]; then
	log "snapshot for $REPO_SHORT already exists ($EXISTING); refreshing pointers"
	SNAP_DIR="$MIRROR/$EXISTING"
else
	SNAP_DIR="$MIRROR/$SNAPSHOT"
	run_export "$SNAP_DIR"
fi

run_selftest "$SNAP_DIR"

SOURCE_JSON="$SNAP_DIR/source.json"
if ! [[ -f "$SOURCE_JSON" ]] || ! grep -q "\"repo_commit\":\"$REPO_SHA\"" "$SOURCE_JSON"; then
	printf '{"snapshot":"%s","repo":"%s","repo_commit":"%s","repo_branch":"%s","exported_at":"%s","redactions":%d,"tool":"scripts/publish-ctxpack.sh"}\n' \
		"$(basename "$SNAP_DIR")" "$SOURCE_REPO" "$REPO_SHA" "${REPO_BRANCH:-detached}" "$STAMP" "$REDACTIONS" >"$SOURCE_JSON"
fi

write_mirror_readme
printf '%s\n' "$(basename "$SNAP_DIR")" >"$MIRROR/LATEST"

mirror_git add -A ||
	fail "git add failed in staging clone"
if mirror_git diff --cached --quiet; then
	log "mirror already current at $(cat "$MIRROR/LATEST"); nothing to push"
	exit 0
fi
mirror_git -c user.name="${GIT_AUTHOR_NAME:-$SOURCE_REPO-publisher}" \
	-c user.email="${GIT_AUTHOR_EMAIL:-$SOURCE_REPO-publisher@users.noreply.github.com}" \
	commit -q -m "chore(ctxpack): snapshot $(basename "$SNAP_DIR")" ||
	fail "git commit failed in staging clone"
mirror_git push origin main ||
	fail "git push failed (network/auth/permissions?)"
log "published $(basename "$SNAP_DIR") ($SOURCE_REPO $REPO_SHORT); LATEST updated"
