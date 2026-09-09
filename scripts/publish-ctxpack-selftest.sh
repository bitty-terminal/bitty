#!/usr/bin/env bash
# publish-ctxpack-selftest.sh — round-trip self-test for a ctxpack snapshot.
#
# Usage: scripts/publish-ctxpack-selftest.sh <export-dir> [--no-round-trip]
#
# Checks (fail-closed, exit 1):
#   1. manifest.json exists with format=carryctx-pack-dir, format_version=1,
#      a non-empty counts map and a project_id; project.json exists.
#   2. Every counts entry has a matching <table>.jsonl whose line count
#      equals the manifest count.
#   3. Every line of every *.jsonl parses as JSON.
#   4. Round-trip (unless --no-round-trip): fresh git repo + fresh
#      `carryctx init --minimal` scratch project, `carryctx import
#      --mode replace`, then the imported table counts must match the
#      manifest (worktrees rows may be pruned by re-anchoring, so
#      worktrees <= manifest is accepted and reported).
#
# Known carryctx v1 limitation (loud WARN, still exit 0): import loads
# sessions while worktree rows missing at the import target are pruned, so
# a pack containing worktree-bound sessions fails with
# "Failed to load pack table 'sessions': FOREIGN KEY constraint failed".
# The export itself is intact (checks 1-3 prove it); only the re-import
# probe cannot pass until carryctx nulls or re-anchors those refs on
# import. Do not "fix" this by editing the snapshot.
set -euo pipefail

PACK="${1:?usage: publish-ctxpack-selftest.sh <export-dir> [--no-round-trip]}"
ROUND_TRIP=1
if [[ "${2:-}" == "--no-round-trip" ]]; then
	ROUND_TRIP=0
elif [[ -n "${2:-}" ]]; then
	echo "publish-ctxpack-selftest: FAIL: unknown flag $2" >&2
	exit 2
fi

TIMEOUT_SECS="${SELFTEST_TIMEOUT:-120}"
KEEP_SCRATCH=0
if [[ "${SELFTEST_KEEP_SCRATCH:-0}" == 1 ]]; then
	KEEP_SCRATCH=1
fi

fail() {
	echo "publish-ctxpack-selftest: FAIL: $1" >&2
	exit 1
}

warn() {
	echo "publish-ctxpack-selftest: WARN: $1" >&2
}

log() {
	echo "publish-ctxpack-selftest: $1"
}

command -v python3 >/dev/null 2>&1 || fail "python3 not on PATH"
command -v carryctx >/dev/null 2>&1 || fail "carryctx not on PATH"
command -v git >/dev/null 2>&1 || fail "git not on PATH"
command -v timeout >/dev/null 2>&1 || fail "timeout not on PATH"
[[ -d "$PACK" ]] || fail "export dir $PACK not found"

SCRATCH=""
cleanup() {
	if [[ "$KEEP_SCRATCH" == 0 && -n "$SCRATCH" && -d "$SCRATCH" ]]; then
		rm -rf "$SCRATCH"
	elif [[ -n "$SCRATCH" ]]; then
		log "keeping scratch dir $SCRATCH (SELFTEST_KEEP_SCRATCH=1)"
	fi
}
trap cleanup EXIT

log "checking manifest + per-table counts in $PACK"
timeout "$TIMEOUT_SECS" python3 - "$PACK" <<'PYEOF'
import json, sys

pack = sys.argv[1]

with open(pack + "/manifest.json") as f:
    manifest = json.load(f)
if manifest.get("format") != "carryctx-pack-dir":
    sys.exit("manifest format != carryctx-pack-dir: %r" % manifest.get("format"))
if manifest.get("format_version") != 1:
    sys.exit("manifest format_version != 1: %r" % manifest.get("format_version"))
counts = manifest.get("counts")
if not isinstance(counts, dict) or not counts:
    sys.exit("manifest counts missing or empty")
if not manifest.get("project_id"):
    sys.exit("manifest project_id missing")

with open(pack + "/project.json") as f:
    project = json.load(f)
if project.get("id") != manifest["project_id"]:
    sys.exit("project.json id != manifest project_id")

worktree_bound_sessions = 0
for table, expected in sorted(counts.items()):
    path = "%s/%s.jsonl" % (pack, table)
    try:
        with open(path) as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        sys.exit("missing table file %s.jsonl (manifest wants %d rows)" % (table, expected))
    for i, line in enumerate(lines, 1):
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            sys.exit("%s.jsonl line %d: invalid JSON (%s)" % (table, i, e))
        if table == "sessions" and row.get("worktree_id"):
            worktree_bound_sessions += 1
    if len(lines) != expected:
        sys.exit("%s.jsonl has %d rows, manifest wants %d" % (table, len(lines), expected))
    print("  table %-22s rows=%d ok" % (table, expected))

print("manifest + counts + JSON: PASS (%d tables)" % len(counts))
print("WORKTREE_BOUND_SESSIONS=%d" % worktree_bound_sessions)
PYEOF

if [[ "$ROUND_TRIP" == 0 ]]; then
	log "round-trip skipped (--no-round-trip); static checks passed"
	exit 0
fi

# Round-trip probe: the export must re-import into a scratch project with
# matching table counts. Capture the worktree-bound session count first so
# the known-import-limitation WARN below can key off pack content, not
# import stderr text alone.
BOUND="$(timeout "$TIMEOUT_SECS" python3 -c "
import json
n = 0
with open('$PACK/sessions.jsonl') as f:
    for line in f:
        if json.loads(line).get('worktree_id'):
            n += 1
print(n)
")"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/ctxpack-roundtrip.XXXXXX")"
log "round-trip scratch: $SCRATCH"
timeout "$TIMEOUT_SECS" git -C "$SCRATCH" init -q ||
	fail "git init failed in scratch"
timeout "$TIMEOUT_SECS" carryctx init --project "$SCRATCH" --name ctxpack-roundtrip --minimal --agent ctxpack-selftest >/dev/null ||
	fail "carryctx init failed in scratch"

IMPORT_OUT="$SCRATCH/import.json"
set +e
timeout "$TIMEOUT_SECS" carryctx import "$PACK" --project "$SCRATCH" --mode replace --yes --agent ctxpack-selftest >"$IMPORT_OUT" 2>"$SCRATCH/import.err"
IMPORT_RC=$?
set -e

if [[ "$IMPORT_RC" -ne 0 ]]; then
	if grep -q "FOREIGN KEY constraint failed" "$SCRATCH/import.err" "$IMPORT_OUT" 2>/dev/null &&
		[[ "$BOUND" -gt 0 ]]; then
		warn "import probe hit the known carryctx v1 limitation: pack holds $BOUND worktree-bound session(s) whose worktree rows are pruned at the import target, so sessions fail FK (export itself validated above; mirror stays publish-only)"
		warn "round-trip: KNOWN-ISSUE (see script header); static checks passed"
		exit 0
	fi
	cat "$SCRATCH/import.err" >&2 2>/dev/null || true
	fail "carryctx import failed (rc=$IMPORT_RC); see above"
fi

log "comparing imported counts with manifest"
timeout "$TIMEOUT_SECS" python3 - "$PACK" "$IMPORT_OUT" <<'PYEOF'
import json, sys

with open(sys.argv[1] + "/manifest.json") as f:
    expected = json.load(f)["counts"]
with open(sys.argv[2]) as f:
    got = json.load(f)["counts"]

for table in sorted(expected):
    if table not in got:
        sys.exit("imported counts missing table %s" % table)
    if table == "worktrees":
        # Re-anchoring prunes worktree rows whose dirs are absent at the
        # import target; fewer-or-equal is the honest expectation here.
        if got[table] > expected[table]:
            sys.exit("worktrees grew on import (%d > %d)" % (got[table], expected[table]))
        print("  table %-22s manifest=%d imported=%d (prune-tolerant) ok" % (table, expected[table], got[table]))
    elif got[table] != expected[table]:
        sys.exit("table %s: manifest=%d imported=%d" % (table, expected[table], got[table]))
    else:
        print("  table %-22s rows=%d ok" % (table, got[table]))
print("round-trip counts: PASS")
PYEOF

log "self-test PASS"
