#!/usr/bin/env bash
# workflow-publish.sh — publish a redacted CarryCtx snapshot inside this repo.
#
# In-repo publication (no mirror repository): `carryctx export --publication`
# redacts every table row and project.json, stamps manifest.redacted, and
# commits exactly one snapshot to the fixed public ref
# `refs/heads/carryctx-snapshots`. CarryCtx never touches the network, so this
# target pushes that local ref itself — and fails loudly when the ref does not
# advance after export. Native carryctx commits one snapshot per export, so a
# re-run publishes again rather than no-opping; a ref that did not advance
# means a stale source or a broken export, never a silent success.
#
# Trigger: the commander's merge closeout runs the workspace publish-snapshots
# helper, which publishes each repo from a fresh origin/main worktree whose
# basename matches the repository name (NOT a git hook: GitHub squash-merges
# never fire local hooks, and `carryctx hooks install` behavior is
# intentionally untouched). A manual `just workflow-publish` from the primary
# checkout on main is allowed only when that checkout is current with
# origin/main.
# The companion CI gate (`snapshot-source`) verifies the pushed snapshot's
# `CarryCtx-Source` trailer matches main HEAD.
#
# What it does:
#   1. `carryctx export --pack-format dir -o <tmp> --publication`, which
#      redacts the bundle and commits it to `refs/heads/carryctx-snapshots`.
#   2. Refuses to push unless the committed `manifest.json` is stamped
#      `redacted: true`.
#   3. Pushes `refs/heads/carryctx-snapshots` to the remote; fails loudly when
#      the local ref did not advance (CTX-0434 defect 4).
#
# The local CarryCtx DB is never modified. `--dry-run` validates the export and
# writes neither the ref nor the remote.
#
# Usage:
#   scripts/workflow-publish.sh [--dry-run] [--project DIR] [--remote NAME]
#                               [--git-timeout SECS] [--allow-non-main]
#                               [--allow-name-mismatch] [--help]
#   --dry-run          export + report only; no ref write, no push.
#   --project DIR      repository to publish (default: this checkout root).
#   --remote NAME      git remote to push to (default: origin).
#   --git-timeout SECS timeout for every carryctx/git op (default: 120).
#   --allow-non-main   publish even when HEAD is not reachable from main; the
#                      trailer records a source the CI gate rejects.
#   --allow-name-mismatch
#                      publish even when the checkout directory basename does
#                      not match the repository name; the trailer records the
#                      wrong project token.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUB_REF="refs/heads/carryctx-snapshots"
PROJECT="$REPO_ROOT"
REMOTE="${WORKFLOW_REMOTE:-origin}"
GIT_TIMEOUT="${GIT_TIMEOUT:-120}"
DRY_RUN=0
ALLOW_NON_MAIN=0
ALLOW_NAME_MISMATCH=0

usage() {
	sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d'
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--allow-non-main)
		ALLOW_NON_MAIN=1
		shift
		;;
	--allow-name-mismatch)
		ALLOW_NAME_MISMATCH=1
		shift
		;;
	--project)
		PROJECT="${2:?--project requires a directory}"
		shift 2
		;;
	--project=*)
		PROJECT="${1#--project=}"
		shift
		;;
	--remote)
		REMOTE="${2:?--remote requires a name}"
		shift 2
		;;
	--remote=*)
		REMOTE="${1#--remote=}"
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
	--help | -h)
		usage
		exit 0
		;;
	*)
		echo "workflow-publish: FAIL: unknown flag $1 (see --help)" >&2
		exit 2
		;;
	esac
done

fail() {
	echo "workflow-publish: FAIL: $1" >&2
	exit 1
}

log() {
	echo "workflow-publish: $1"
}

have() { command -v "$1" >/dev/null 2>&1; }

have git || fail "git not on PATH"
have carryctx || fail "carryctx not on PATH"
have timeout || fail "timeout not on PATH"

PROJECT="$(cd "$PROJECT" && pwd)" || fail "project directory $PROJECT not found"

# CarryCtx records the checkout directory basename as the project token in the
# `CarryCtx-Source` trailer, so a worktree or recovery clone with a different
# name publishes misleading provenance. Require the basename to match the
# repository name derived from the remote.
PROJECT_NAME="$(basename "$PROJECT")"
REPO_NAME="$(basename "$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" remote get-url "$REMOTE" 2>/dev/null || true)" .git)"
if [[ -z "$REPO_NAME" ]]; then
	if [[ "$ALLOW_NAME_MISMATCH" == 1 ]]; then
		log "WARN: cannot determine repository name from remote '$REMOTE' URL; --allow-name-mismatch set, CarryCtx-Source trailer may record the wrong project token"
	else
		fail "cannot determine repository name from remote '$REMOTE' URL; refusing to publish with a basename-derived CarryCtx-Source trailer. Fix the remote URL or pass --allow-name-mismatch."
	fi
fi
if [[ -n "$REPO_NAME" && "$PROJECT_NAME" != "$REPO_NAME" ]]; then
	if [[ "$ALLOW_NAME_MISMATCH" == 1 ]]; then
		log "WARN: checkout basename '$PROJECT_NAME' != repository name '$REPO_NAME'; --allow-name-mismatch set"
	else
		fail "checkout basename '$PROJECT_NAME' != repository name '$REPO_NAME'; CarryCtx records the basename in the CarryCtx-Source trailer. Publish from a checkout named '$REPO_NAME' or pass --allow-name-mismatch."
	fi
fi

# The publication source must be main (or a commit reachable from it), otherwise
# the snapshot-source gate can never match it. A detached checkout at
# origin/main is valid; a feature branch is not.
BRANCH="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" branch --show-current 2>/dev/null || true)"
if [[ "${BRANCH:-}" != "main" ]]; then
	if [[ "$ALLOW_NON_MAIN" == 1 ]]; then
		log "WARN: checkout is on branch '${BRANCH:-detached}', not main; --allow-non-main set, snapshot provenance will record that branch"
	else
		HEAD_SHA="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse HEAD 2>/dev/null || true)"
		MAIN_SHA="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse -q --verify "refs/remotes/$REMOTE/main" 2>/dev/null || true)"
		if [[ -n "$HEAD_SHA" && -n "$MAIN_SHA" ]] &&
			timeout "$GIT_TIMEOUT" git -C "$PROJECT" merge-base --is-ancestor "$HEAD_SHA" "$MAIN_SHA" 2>/dev/null; then
			if [[ "$HEAD_SHA" == "$MAIN_SHA" ]]; then
				log "checkout is detached at $REMOTE/main ($HEAD_SHA)"
			else
				fail "checkout is detached at $HEAD_SHA, behind $REMOTE/main ($MAIN_SHA); refusing stale publication. Re-run the closeout from a fresh $REMOTE/main worktree (or pass --allow-non-main to record the stale source explicitly)."
			fi
		else
			fail "checkout is on branch '${BRANCH:-detached}' at ${HEAD_SHA:-unknown}, which is not main and not reachable from $REMOTE/main; run the closeout from the primary checkout on main (or pass --allow-non-main). A feature-branch publication records a revision the snapshot-source gate can never match."
		fi
	fi
fi

# A primary checkout on branch main can also be stale (behind origin/main).
# The closeout helper always publishes from a fresh origin/main worktree; a
# manual run from a behind main must fail loudly rather than stamp a stale
# CarryCtx-Source (CTX-0434 defect 2).
if [[ "${BRANCH:-}" == "main" && "$ALLOW_NON_MAIN" == 0 ]]; then
	HEAD_ON_MAIN="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse HEAD 2>/dev/null || true)"
	MAIN_TIP="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse -q --verify "refs/remotes/$REMOTE/main" 2>/dev/null || true)"
	if [[ -n "$HEAD_ON_MAIN" && -n "$MAIN_TIP" && "$HEAD_ON_MAIN" != "$MAIN_TIP" ]] &&
		timeout "$GIT_TIMEOUT" git -C "$PROJECT" merge-base --is-ancestor "$HEAD_ON_MAIN" "$MAIN_TIP" 2>/dev/null; then
		fail "checkout on branch 'main' at $HEAD_ON_MAIN is behind $REMOTE/main ($MAIN_TIP); fetch/pull main before publishing (or pass --allow-non-main to record the stale source explicitly)."
	fi
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/workflow-publish.XXXXXX")"
cleanup() {
	rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

# Align the local publication ref with the remote tip before exporting, so the
# new snapshot fast-forwards the published branch even when this clone has never
# published or lags behind another clone. A divergent unpushed local ref is
# superseded by this export from the live database.
if [[ "$DRY_RUN" == 0 ]] &&
	timeout "$GIT_TIMEOUT" git -C "$PROJECT" fetch --no-tags "$REMOTE" \
		"+refs/heads/carryctx-snapshots:refs/remotes/$REMOTE/carryctx-snapshots" >/dev/null 2>&1; then
	REMOTE_TIP="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse -q --verify "refs/remotes/$REMOTE/carryctx-snapshots" 2>/dev/null || true)"
	if [[ -n "$REMOTE_TIP" ]]; then
		timeout "$GIT_TIMEOUT" git -C "$PROJECT" update-ref "$PUB_REF" "$REMOTE_TIP" ||
			fail "cannot align $PUB_REF to $REMOTE/$PUB_REF"
	fi
fi

BEFORE="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse -q --verify "$PUB_REF" 2>/dev/null || true)"

EXPORT_ARGS=(export --pack-format dir -o "$TMP_ROOT/pack" --publication --project "$PROJECT")
if [[ "$DRY_RUN" == 1 ]]; then
	EXPORT_ARGS+=(--dry-run)
fi

log "exporting redacted publication (ref $PUB_REF)"
if ! timeout "$GIT_TIMEOUT" carryctx "${EXPORT_ARGS[@]}" >"$TMP_ROOT/export.json" 2>"$TMP_ROOT/export.err"; then
	cat "$TMP_ROOT/export.err" >&2 2>/dev/null || true
	fail "carryctx export --publication failed"
fi

if [[ "$DRY_RUN" == 1 ]]; then
	log "dry-run PASS: export validated; no ref written, nothing pushed"
	exit 0
fi

AFTER="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" rev-parse "$PUB_REF" 2>/dev/null)" ||
	fail "publication ref $PUB_REF was not created by carryctx export --publication"

MANIFEST="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" show "$PUB_REF:manifest.json" 2>/dev/null)" ||
	fail "cannot read $PUB_REF:manifest.json"
if ! grep -Eq '"redacted"[[:space:]]*:[[:space:]]*true' <<<"$MANIFEST"; then
	fail "publication $PUB_REF is not stamped redacted:true; refusing to push"
fi

if [[ -n "$BEFORE" && "$BEFORE" == "$AFTER" ]]; then
	fail "snapshot ref $PUB_REF did not advance after export (still ${AFTER:0:12}); refusing silent success. The export source was stale or the export produced no new snapshot."
fi

log "pushing $PUB_REF (${BEFORE:0:12} -> ${AFTER:0:12}) to $REMOTE"
if ! timeout "$GIT_TIMEOUT" git -C "$PROJECT" push "$REMOTE" "refs/heads/carryctx-snapshots:refs/heads/carryctx-snapshots"; then
	fail "git push failed (network/auth/permissions?); the local publication commit is preserved"
fi
log "published $PUB_REF at ${AFTER:0:12}"
