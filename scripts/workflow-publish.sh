#!/usr/bin/env bash
# workflow-publish.sh — publish a redacted CarryCtx snapshot inside this repo.
#
# In-repo publication (no mirror repository): `carryctx export --publication`
# redacts every table row and project.json, stamps manifest.redacted, and
# commits exactly one snapshot to the fixed public ref
# `refs/heads/carryctx-snapshots`. CarryCtx never touches the network, so this
# target pushes that local ref itself — but only when the ref actually
# advanced. Native carryctx commits one snapshot per export, so a re-run
# publishes again rather than no-opping; the guard skips only a ref that did
# not advance.
#
# Trigger: the commander's merge closeout runs `just workflow-publish` from the
# primary checkout (NOT a git hook: GitHub squash-merges never fire local
# hooks, and `carryctx hooks install` behavior is intentionally untouched).
# The companion CI gate (`snapshot-source`) verifies the pushed snapshot's
# `CarryCtx-Source` trailer matches main HEAD.
#
# What it does:
#   1. `carryctx export --pack-format dir -o <tmp> --publication`, which
#      redacts the bundle and commits it to `refs/heads/carryctx-snapshots`.
#   2. Refuses to push unless the committed `manifest.json` is stamped
#      `redacted: true`.
#   3. Pushes `refs/heads/carryctx-snapshots` to the remote only when the local
#      ref changed; otherwise prints a no-op message.
#
# The local CarryCtx DB is never modified. `--dry-run` validates the export and
# writes neither the ref nor the remote.
#
# Usage:
#   scripts/workflow-publish.sh [--dry-run] [--project DIR] [--remote NAME]
#                               [--git-timeout SECS] [--help]
#   --dry-run          export + report only; no ref write, no push.
#   --project DIR      repository to publish (default: this checkout root).
#   --remote NAME      git remote to push to (default: origin).
#   --git-timeout SECS timeout for every carryctx/git op (default: 120).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUB_REF="refs/heads/carryctx-snapshots"
PROJECT="$REPO_ROOT"
REMOTE="${WORKFLOW_REMOTE:-origin}"
GIT_TIMEOUT="${GIT_TIMEOUT:-120}"
DRY_RUN=0

usage() {
	sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d'
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--dry-run)
		DRY_RUN=1
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

BRANCH="$(timeout "$GIT_TIMEOUT" git -C "$PROJECT" branch --show-current 2>/dev/null || true)"
if [[ "${BRANCH:-}" != "main" ]]; then
	log "WARN: checkout is on branch '${BRANCH:-detached}', not main; snapshot provenance will record that branch"
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/workflow-publish.XXXXXX")"
cleanup() {
	rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

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
	log "publication already current at ${AFTER:0:12}; nothing to push"
	exit 0
fi

log "pushing $PUB_REF (${BEFORE:0:12} -> ${AFTER:0:12}) to $REMOTE"
if ! timeout "$GIT_TIMEOUT" git -C "$PROJECT" push "$REMOTE" "refs/heads/carryctx-snapshots:refs/heads/carryctx-snapshots"; then
	fail "git push failed (network/auth/permissions?); the local publication commit is preserved"
fi
log "published $PUB_REF at ${AFTER:0:12}"
