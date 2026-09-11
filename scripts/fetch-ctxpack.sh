#!/usr/bin/env bash
# fetch-ctxpack.sh — restore this repo's local CarryCtx DB from the
# workflow-mirror LATEST snapshot (fresh-clone recipe).
#
# Fresh clones have no .git/carryctx/state.sqlite: CarryCtx runtime state is
# never cloned. The engineering history lives in the `<repo>-workflow` mirror
# as redacted ctxpack snapshots (format v1, current format v2). This script:
#   1. ensures a staging clone of the mirror (default
#      bitty-terminal/bitty-workflow, override with --url / --mirror),
#   2. reads LATEST (or --snapshot ID) and copies that snapshot directory to
#      a local temp staging path,
#   3. validates the staged copy with the publish validator's static checks
#      (scripts/publish-ctxpack-selftest.sh --no-round-trip: format v1/v2
#      shape, counts <-> *.jsonl agreement, JSON parse, v2 redacted stamp,
#      fixture matrix) BEFORE anything touches the local DB,
#   4. initializes the local CarryCtx state if it has no project row, then
#      imports the staged snapshot with `carryctx import --mode replace`,
#   5. prints provenance (snapshot id + source commit) and `carryctx stats`.
#
# Safety:
#   - a NON-EMPTY local state DB is never replaced without --force;
#   - --dry-run fetches + validates only; it never initializes or imports;
#   - the pre-existing .carryctx/config.toml is conserved: `carryctx init`
#     rewrites local config defaults, so the script restores the committed
#     file after init and leaves the checkout clean;
#   - mirror fetch is read-only (fetch/reset of the staging clone only);
#     nothing is ever pushed.
#
# Mirrors hold redacted publication artifacts (manifest.redacted=true for v2)
# and redaction is one-way; CarryCtx refuses redacted bundles as merge
# sources, so restoring always uses replace mode. A leaked-then-rotated
# secret is already in the mirror: rotate at the source, redaction only
# limits exposure.
#
# Usage:
#   scripts/fetch-ctxpack.sh [--dry-run] [--force] [--mirror DIR] [--url URL]
#                            [--snapshot ID] [--project DIR]
#                            [--git-timeout SECS] [--keep-tmp]
#   --dry-run      fetch + validate only; no init, no import, no DB writes.
#   --force        replace a non-empty local DB (default: refuse).
#   --mirror DIR   staging clone location
#                  (default: $WORKFLOW_MIRROR_DIR or <repo>/../bitty-workflow).
#   --url URL      mirror remote for a fresh clone
#                  (default: $WORKFLOW_MIRROR_URL or
#                  https://github.com/bitty-terminal/bitty-workflow.git).
#   --snapshot ID  restore this snapshot directory instead of LATEST.
#   --project DIR  target repo root (default: the repo containing this
#                  script; useful for throwaway clones and testing).
#   --git-timeout SECS  timeout for every git/carryctx/python op
#                  (default: $GIT_TIMEOUT or 120).
#   --keep-tmp     keep temp dirs on exit (debug aid; prints paths).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIRROR_DEFAULT="$REPO_ROOT/../bitty-workflow"
MIRROR_URL_DEFAULT="https://github.com/bitty-terminal/bitty-workflow.git"

DRY_RUN=0
FORCE=0
KEEP_TMP=0
MIRROR="${WORKFLOW_MIRROR_DIR:-$MIRROR_DEFAULT}"
MIRROR_URL="${WORKFLOW_MIRROR_URL:-$MIRROR_URL_DEFAULT}"
GIT_TIMEOUT="${GIT_TIMEOUT:-120}"
SNAPSHOT_OVERRIDE=""
PROJECT="${WORKFLOW_IMPORT_PROJECT:-$REPO_ROOT}"

while [[ $# -gt 0 ]]; do
	case "$1" in
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--force)
		FORCE=1
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
	--snapshot)
		SNAPSHOT_OVERRIDE="${2:?--snapshot requires a snapshot directory name}"
		shift 2
		;;
	--snapshot=*)
		SNAPSHOT_OVERRIDE="${1#--snapshot=}"
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
		echo "fetch-ctxpack: FAIL: unknown flag $1 (see --help)" >&2
		exit 2
		;;
	esac
done

fail() {
	echo "fetch-ctxpack: FAIL: $1" >&2
	exit 1
}

log() {
	echo "fetch-ctxpack: $1"
}

warn() {
	echo "fetch-ctxpack: WARN: $1" >&2
}

have() { command -v "$1" >/dev/null 2>&1; }

have git || fail "git not on PATH"
have carryctx || fail "carryctx not on PATH"
have timeout || fail "timeout not on PATH"
have python3 || fail "python3 not on PATH (state probe + snapshot provenance)"

[[ -d "$PROJECT" ]] || fail "project dir $PROJECT not found"
[[ -f "$PROJECT/.carryctx/config.toml" ]] ||
	fail "$PROJECT has no .carryctx/config.toml (not a CarryCtx project?); refusing to guess a project identity"

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
# confined to the staging clone. $PROJECT is never a write target.
mirror_git() {
	timeout "$GIT_TIMEOUT" git -C "$MIRROR" "$@"
}

ensure_mirror() {
	if [[ -d "$MIRROR/.git" ]]; then
		if [[ -n "$(mirror_git status --porcelain 2>/dev/null)" ]]; then
			fail "staging clone $MIRROR has uncommitted changes; clean it up, pass --mirror, or remove it"
		fi
		mirror_git fetch -q origin --prune ||
			fail "git fetch failed in staging clone (network/auth?)"
		mirror_git checkout -q main 2>/dev/null ||
			mirror_git checkout -q -b main --track origin/main ||
			fail "cannot check out main in staging clone"
		mirror_git reset -q --hard origin/main ||
			fail "cannot fast-forward staging clone to origin/main"
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

# Resolve + stage the snapshot. LATEST is a single line; snapshot ids are
# validated before use so a hostile mirror cannot escape the mirror dir.
resolve_snapshot() {
	local latest
	if [[ -n "$SNAPSHOT_OVERRIDE" ]]; then
		SNAP="$SNAPSHOT_OVERRIDE"
	else
		[[ -f "$MIRROR/LATEST" ]] || fail "mirror $MIRROR has no LATEST file"
		latest="$(head -n1 "$MIRROR/LATEST" | tr -d '[:space:]')"
		[[ -n "$latest" ]] || fail "mirror $MIRROR LATEST is empty"
		SNAP="$latest"
	fi
	[[ "$SNAP" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
		fail "snapshot id '$SNAP' is not a plain directory name; refusing"
	[[ -d "$MIRROR/$SNAP" ]] || fail "snapshot dir $MIRROR/$SNAP not found"
}

stage_snapshot() {
	local dest="$TMP_ROOT/$SNAP"
	log "staging snapshot $SNAP -> $dest"
	cp -a "$MIRROR/$SNAP" "$dest" ||
		fail "cannot copy snapshot $SNAP to staging dir"
}

# Structural integrity + mirror honesty (v2 must carry the redacted stamp)
# via the publish validator's static checks. Import happens only after this
# passes; the check itself never touches the local CarryCtx DB.
validate_snapshot() {
	log "validating staged snapshot (publish-ctxpack-selftest --no-round-trip)"
	timeout "$GIT_TIMEOUT" bash "$REPO_ROOT/scripts/publish-ctxpack-selftest.sh" \
		"$STAGE" --no-round-trip || fail "snapshot validation failed; nothing imported"
}

# Legacy v1 mirror snapshots predate the export-time redaction pass (the v2
# validator already refuses unstamped v2 packs). Import is unaffected; the
# warning keeps the provenance honest.
warn_if_unstamped() {
	local stamped
	stamped="$(timeout "$GIT_TIMEOUT" python3 -c \
		'import json,sys; print("true" if json.load(open(sys.argv[1])).get("redacted") is True else "false")' \
		"$STAGE/manifest.json")" || fail "cannot read snapshot manifest"
	if [[ "$stamped" != "true" ]]; then
		warn "snapshot $SNAP has no redacted stamp (legacy v1 artifact): treat mirror contents as unredacted historical data"
	fi
}

# Read-only probe of the local CarryCtx state. Never creates or mutates the
# DB; exits 0 with "exists projects rows" or a clear failure. Data tables are
# the schema-17/18 row sets; unknown/missing tables are skipped.
probe_state() {
	timeout "$GIT_TIMEOUT" python3 - "$PROJECT" <<'PYEOF'
import os
import sqlite3
import subprocess
import sys

repo = sys.argv[1]
try:
    out = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--git-common-dir"],
        capture_output=True, text=True, check=True, timeout=60,
    ).stdout.strip()
except Exception as exc:  # noqa: BLE001 - report, never traceback
    sys.exit("fetch-ctxpack: FAIL: cannot resolve git common dir for %s (%s)" % (repo, exc))
if not out:
    sys.exit("fetch-ctxpack: FAIL: empty git common dir for %s" % repo)
common = out if os.path.isabs(out) else os.path.join(repo, out)
db = os.path.join(common, "carryctx", "state.sqlite")

exists = os.path.exists(db)
projects = 0
rows = 0
if exists:
    tables = (
        "agents", "tasks", "task_dependencies", "progress_items", "sessions",
        "worktrees", "checkpoints", "checkpoint_corrections", "scopes",
        "decisions", "handoffs", "events", "graph_nodes", "graph_edges",
        "teams", "team_members",
    )
    try:
        con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
        try:
            names = {r[0] for r in con.execute(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            if "projects" in names:
                projects = con.execute("SELECT COUNT(*) FROM projects").fetchone()[0]
            rows = sum(
                con.execute("SELECT COUNT(*) FROM %s" % t).fetchone()[0]
                for t in tables if t in names
            )
        finally:
            con.close()
    except sqlite3.Error as exc:
        sys.exit("fetch-ctxpack: FAIL: cannot read local state DB %s (%s)" % (db, exc))
print("%d %d %d" % (1 if exists else 0, projects, rows))
PYEOF
}

config_project_id() {
	timeout "$GIT_TIMEOUT" python3 - "$PROJECT/.carryctx/config.toml" <<'PYEOF'
import sys
import tomllib

try:
    with open(sys.argv[1], "rb") as fh:
        cfg = tomllib.load(fh)
except (OSError, tomllib.TOMLDecodeError):
    print("")
    sys.exit(0)
print(cfg.get("project", {}).get("id", ""))
PYEOF
}

bundle_project_id() {
	timeout "$GIT_TIMEOUT" python3 - "$STAGE/manifest.json" <<'PYEOF'
import json
import sys

with open(sys.argv[1]) as fh:
    print(json.load(fh).get("project_id", ""))
PYEOF
}

# `carryctx init` rewrites .carryctx/config.toml with current defaults (it
# adds new sections and resets e.g. [verification] commands). The committed
# config is project data, not runtime state: keep a copy and restore it so
# the checkout stays clean.
ensure_project_row() {
	local cfg="$PROJECT/.carryctx/config.toml" backup=""
	if [[ -f "$cfg" ]]; then
		backup="$TMP_ROOT/config.toml.before"
		cp -p "$cfg" "$backup" || fail "cannot back up $cfg"
	fi
	log "no project row in local DB: initializing CarryCtx state (carryctx init --non-interactive)"
	if ! timeout "$GIT_TIMEOUT" carryctx init --non-interactive --project "$PROJECT" \
		>"$TMP_ROOT/init.log" 2>&1; then
		cat "$TMP_ROOT/init.log" >&2 || true
		fail "carryctx init failed"
	fi
	if [[ -n "$backup" && -f "$cfg" ]] && ! cmp -s "$backup" "$cfg"; then
		cp -p "$backup" "$cfg" || fail "cannot restore $cfg after carryctx init"
		log "restored pre-existing .carryctx/config.toml (carryctx init rewrites local config defaults)"
	fi
}

print_provenance() {
	timeout "$GIT_TIMEOUT" python3 - "$STAGE" "$SNAP" <<'PYEOF'
import json
import os
import sys

stage, snap = sys.argv[1], sys.argv[2]
with open(os.path.join(stage, "manifest.json")) as fh:
    manifest = json.load(fh)
source = {}
source_path = os.path.join(stage, "source.json")
if os.path.exists(source_path):
    with open(source_path) as fh:
        source = json.load(fh)
print("fetch-ctxpack: provenance: snapshot=%s export_id=%s format_version=v%s carryctx=%s schema=%s redacted=%s" % (
    snap,
    manifest.get("export_id", "?"),
    manifest.get("format_version", "?"),
    manifest.get("carryctx_version", "?"),
    manifest.get("schema_version", "?"),
    "true" if manifest.get("redacted") is True else "false",
))
print("fetch-ctxpack: provenance: source_commit=%s exported_at=%s" % (
    source.get("bitty_commit") or source.get("repo_commit", "unknown"),
    source.get("exported_at", "unknown"),
))
PYEOF
}

# --- pre-flight: local state -------------------------------------------------
STATE_LINE="$(probe_state)" || fail "cannot inspect local CarryCtx state"
read -r STATE_EXISTS STATE_PROJECTS STATE_ROWS <<<"$STATE_LINE"
log "local state: db_exists=$STATE_EXISTS project_rows=$STATE_PROJECTS data_rows=$STATE_ROWS"
if [[ "$STATE_PROJECTS" -gt 1 ]]; then
	fail "local DB has $STATE_PROJECTS project rows; refusing to replace an ambiguous state DB"
fi
if [[ "$STATE_PROJECTS" -eq 1 && "$STATE_ROWS" -gt 0 && "$FORCE" == 0 && "$DRY_RUN" == 0 ]]; then
	fail "local DB already holds $STATE_ROWS row(s); refusing to overwrite non-empty state without --force (local DB untouched)"
fi

# --- fetch + validate --------------------------------------------------------
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ctxpack-import.XXXXXX")"
ensure_mirror
resolve_snapshot
STAGE="$TMP_ROOT/$SNAP"
stage_snapshot
validate_snapshot
warn_if_unstamped

BUNDLE_ID="$(bundle_project_id)" || fail "cannot read snapshot project id"
CONFIG_ID="$(config_project_id)" || fail "cannot read local .carryctx project id"
if [[ -n "$BUNDLE_ID" && -n "$CONFIG_ID" && "$BUNDLE_ID" != "$CONFIG_ID" ]]; then
	fail "snapshot project $BUNDLE_ID does not match local project $CONFIG_ID; refusing to fork project identity"
fi

if [[ "$DRY_RUN" == 1 ]]; then
	print_provenance
	if [[ "$STATE_PROJECTS" -eq 1 && "$STATE_ROWS" -gt 0 ]]; then
		log "dry-run PASS: snapshot $SNAP fetched + validated; local DB has $STATE_ROWS row(s), a real run needs --force"
	else
		log "dry-run PASS: snapshot $SNAP fetched + validated; nothing imported"
	fi
	exit 0
fi

# --- restore -----------------------------------------------------------------
if [[ "$STATE_PROJECTS" -eq 0 ]]; then
	ensure_project_row
elif [[ "$STATE_ROWS" -gt 0 ]]; then
	warn "--force: replacing $STATE_ROWS existing local row(s) in $PROJECT"
fi

log "importing snapshot $SNAP into $PROJECT (replace mode)"
if ! timeout "$GIT_TIMEOUT" carryctx import "$STAGE" --project "$PROJECT" --mode replace --yes; then
	fail "carryctx import failed; local DB is left in the state carryctx reports above"
fi

print_provenance
log "restore complete; local counts:"
timeout "$GIT_TIMEOUT" carryctx stats --project "$PROJECT" || fail "carryctx stats failed"
log "restored from mirror snapshot $SNAP; mirror artifacts are redacted, never merge them back"
