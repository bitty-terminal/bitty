#!/usr/bin/env bash
# publish-ctxpack-selftest.sh — round-trip self-test for a ctxpack snapshot.
#
# Usage: scripts/publish-ctxpack-selftest.sh <export-dir> [--no-round-trip]
#
# Checks (fail-closed, exit 1):
#   1. manifest.json exists with format=carryctx-pack-dir and a supported
#      format_version (1 or 2), a non-empty counts map and a project_id;
#      project.json exists. v2 shape is enforced too: counts.tombstones,
#      non-empty unique parents that never name the manifest's own export_id,
#      watermark rows agreeing with counts, sequences/source shape, and the
#      redacted:true publication stamp. v1 keeps its legacy shape (no
#      watermarks, no tombstone counts) and needs no redacted stamp.
#   2. Every counts entry has a matching <table>.jsonl whose line count
#      equals the manifest count; a table with rows but no counts entry fails
#      closed (undeclared row set), matching carryctx check_counts.
#   3. Every line of every *.jsonl parses as JSON.
#   4. Validator fixture matrix: synthetic v1/v2 packs prove the checks above
#      accept valid packs and refuse newer format versions, tombstone/count
#      skew, malformed parents/watermarks, unredacted v2 packs, and unknown
#      or undeclared tables.
#   5. Round-trip (unless --no-round-trip): fresh git repo + fresh
#      `carryctx init --minimal` scratch project, `carryctx import
#      --mode replace`, then the imported table counts must match the
#      manifest (worktrees rows may be pruned by re-anchoring, so
#      worktrees <= manifest is accepted and reported).
#   6. Redaction fixture: synthetic v1/v2 packs holding FAKE secret values
#      (never real credentials) are copied to a staging dir and run through
#      scripts/publish-ctxpack-redact.py; the staging copy must come out
#      redacted (names kept, JSON valid, row counts unchanged, tombstones
#      rows handled, manifest.redacted=true stamped, counts untouched) while
#      the source copy still holds the fakes byte-identical (local DB
#      untouched).
#
# Known carryctx import limitation (loud WARN, still exit 0): import loads
# sessions while worktree rows missing at the import target are pruned, so
# a pack containing worktree-bound sessions can fail with
# "Failed to load pack table 'sessions': FOREIGN KEY constraint failed" on
# carryctx versions predating the nulling of pruned worktree references.
# The export itself is intact (checks 1-3 prove it); only the re-import
# probe cannot pass until the import target handles those refs. Do not
# "fix" this by editing the snapshot.
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
FIXT_ROOT=""
cleanup() {
	if [[ "$KEEP_SCRATCH" == 0 && -n "$SCRATCH" && -d "$SCRATCH" ]]; then
		rm -rf "$SCRATCH"
	elif [[ -n "$SCRATCH" ]]; then
		log "keeping scratch dir $SCRATCH (SELFTEST_KEEP_SCRATCH=1)"
	fi
	if [[ "$KEEP_SCRATCH" == 0 && -n "$FIXT_ROOT" && -d "$FIXT_ROOT" ]]; then
		rm -rf "$FIXT_ROOT"
	elif [[ -n "$FIXT_ROOT" ]]; then
		log "keeping fixture dir $FIXT_ROOT (SELFTEST_KEEP_SCRATCH=1)"
	fi
}
trap cleanup EXIT

log "checking manifest + per-table counts in $PACK"
timeout "$TIMEOUT_SECS" python3 - "$PACK" <<'PYEOF'
import copy
import json
import os
import re
import shutil
import sys
import tempfile

pack = sys.argv[1]

PACK_FORMAT = "carryctx-pack-dir"
V1_TABLES = (
    "agents",
    "tasks",
    "task_dependencies",
    "progress_items",
    "sessions",
    "worktrees",
    "checkpoints",
    "checkpoint_corrections",
    "scopes",
    "decisions",
    "handoffs",
    "teams",
    "team_members",
    "graph_nodes",
    "graph_edges",
    "events",
    "sequences",
)
V2_TABLES = V1_TABLES + ("tombstones",)
CURRENT_FORMAT_VERSION = 2
_RFC3339_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})\Z")


class PackError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise PackError(message)


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def read_json(path, what):
    try:
        with open(path) as handle:
            return json.load(handle)
    except FileNotFoundError as error:
        raise PackError("%s is missing" % what) from error
    except ValueError as error:
        raise PackError("%s is not valid JSON (%s)" % (what, error)) from error


def validate_pack(directory, verbose=True):
    """Validate a ctxpack directory (format v1 or v2), fail-closed.

    Mirrors the carryctx-pack manifest/check_counts contract; returns
    (format_version, counts, worktree_bound_sessions).
    """
    manifest = read_json(os.path.join(directory, "manifest.json"), "manifest.json")
    require(isinstance(manifest, dict), "manifest.json is not a JSON object")
    require(
        manifest.get("format") == PACK_FORMAT,
        "manifest format != %s: %r" % (PACK_FORMAT, manifest.get("format")),
    )
    version = manifest.get("format_version")
    require(is_int(version), "manifest format_version is not an integer: %r" % (version,))
    require(
        version <= CURRENT_FORMAT_VERSION,
        "manifest format_version %d is newer than supported %d" % (version, CURRENT_FORMAT_VERSION),
    )
    require(
        version >= 1,
        "manifest format_version %d is older than the oldest supported 1" % version,
    )
    for field in ("carryctx_version", "project_id", "export_id"):
        value = manifest.get(field)
        require(
            isinstance(value, str) and value.strip(),
            "manifest %s is missing or empty" % field,
        )
    require(is_int(manifest.get("schema_version")), "manifest schema_version is not an integer")
    created_at = manifest.get("created_at")
    require(
        isinstance(created_at, str) and _RFC3339_RE.match(created_at),
        "manifest created_at is not RFC3339: %r" % (created_at,),
    )
    require(isinstance(manifest.get("source"), dict), "manifest source is not an object")
    for key, value in manifest["source"].items():
        require(
            value is None or isinstance(value, str),
            "manifest source.%s is not a string" % key,
        )

    counts = manifest.get("counts")
    require(isinstance(counts, dict) and counts, "manifest counts missing or empty")
    for table, expected in counts.items():
        require(isinstance(table, str) and table, "manifest counts has an empty table name")
        require(
            is_int(expected) and expected >= 0,
            "manifest counts.%s is not a non-negative integer" % table,
        )

    redacted = manifest.get("redacted", False)
    require(isinstance(redacted, bool), "manifest redacted is not a boolean: %r" % (redacted,))

    allowed = V1_TABLES if version == 1 else V2_TABLES
    unknown = sorted(set(counts) - set(allowed))
    require(not unknown, "manifest counts unknown table(s): %s" % ", ".join(unknown))

    if version == 1:
        # Legacy shape: v2-only fields must not appear in a v1 manifest.
        require("tombstones" not in counts, "manifest v1 may not declare counts.tombstones")
        require(not manifest.get("watermarks"), "manifest v1 may not carry watermarks")
    else:
        # v2 shape (carryctx-pack manifest.rs): tombstone counts are
        # explicit, parents are unique non-empty ids that never self-reference,
        # watermarks must agree with counts, and the publication stamp must be
        # present -- this self-test runs after the redaction pass.
        require("tombstones" in counts, "manifest v2 omits counts.tombstones")
        require(
            redacted is True,
            "manifest v2 is not marked redacted: true (run the export-time redaction pass first)",
        )
        parents = manifest.get("parents", [])
        require(isinstance(parents, list), "manifest parents is not a list")
        seen = set()
        for parent in parents:
            require(
                isinstance(parent, str) and parent.strip(),
                "manifest v2 has an empty parents entry",
            )
            require(
                parent != manifest["export_id"],
                "manifest v2 lists its own export_id as a parent",
            )
            require(parent not in seen, "manifest v2 lists parent %s more than once" % parent)
            seen.add(parent)
        watermarks = manifest.get("watermarks", {})
        require(isinstance(watermarks, dict), "manifest watermarks is not an object")
        for table, watermark in sorted(watermarks.items()):
            require(table in V2_TABLES, "manifest watermarks unknown table %s" % table)
            require(isinstance(watermark, dict), "manifest watermark %s is not an object" % table)
            rows = watermark.get("rows")
            require(
                is_int(rows) and rows >= 0,
                "manifest watermark %s rows is not a non-negative integer" % table,
            )
            declared = counts.get(table, 0)
            require(
                rows == declared,
                "manifest watermark %s declares %d rows but counts declare %d"
                % (table, rows, declared),
            )
            max_updated = watermark.get("max_updated_at")
            require(
                max_updated is None or isinstance(max_updated, str),
                "manifest watermark %s max_updated_at is not a string" % table,
            )

    sequences = manifest.get("sequences", {})
    require(isinstance(sequences, dict), "manifest sequences is not an object")
    for kind, value in sequences.items():
        require(
            is_int(value) and value >= 0,
            "manifest sequences.%s is not a non-negative integer" % kind,
        )

    project = read_json(os.path.join(directory, "project.json"), "project.json")
    require(isinstance(project, dict), "project.json is not a JSON object")
    require(
        project.get("id") == manifest["project_id"],
        "project.json id != manifest project_id",
    )

    worktree_bound_sessions = 0
    for table in allowed:
        path = os.path.join(directory, "%s.jsonl" % table)
        try:
            with open(path) as handle:
                lines = handle.read().splitlines()
        except FileNotFoundError as error:
            raise PackError("missing table file %s.jsonl" % table) from error
        rows = 0
        for number, line in enumerate(lines, 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except ValueError as error:
                raise PackError(
                    "%s.jsonl line %d: invalid JSON (%s)" % (table, number, error)
                ) from error
            require(
                isinstance(row, dict),
                "%s.jsonl line %d is not a JSON object" % (table, number),
            )
            rows += 1
            if table == "sessions" and row.get("worktree_id"):
                worktree_bound_sessions += 1
        if table in counts:
            require(
                rows == counts[table],
                "%s.jsonl has %d rows, manifest wants %d" % (table, rows, counts[table]),
            )
        else:
            require(
                rows == 0,
                "%s.jsonl has %d undeclared rows (manifest omits counts)" % (table, rows),
            )
        if verbose:
            print("  table %-22s rows=%d ok" % (table, rows))
    return version, counts, worktree_bound_sessions


def fixture_manifest(version):
    return {
        "format": PACK_FORMAT,
        "format_version": version,
        "carryctx_version": "0.9.1",
        "schema_version": 18,
        "project_id": "01FAKEFIXTURE00000000000000",
        "export_id": "01FAKEFIXTURE00000000000001",
        "created_at": "2026-09-10T00:00:00Z",
        "parents": [],
        "sequences": {"display_id_CTX": 1},
        "source": {"git_commit": "0" * 40, "hostname": "fixture"},
        "counts": {"tasks": 1, "tombstones": 1},
        "redacted": True,
    }


def v1_manifest():
    manifest = fixture_manifest(1)
    manifest["counts"].pop("tombstones", None)
    manifest.pop("redacted", None)
    return manifest


TASK_ROW = {"id": "01FAKEFIXTURE00000000000002", "title": "fixture"}
TOMBSTONE_ROW = {
    "project_id": "01FAKEFIXTURE00000000000000",
    "table_name": "worktrees",
    "row_id": "01FAKEFIXTURE00000000000003",
    "deleted_at": "2026-09-10T00:00:00Z",
    "deleted_by": None,
    "reason": "fixture",
}


def write_fixture(root, name, manifest, tables, raw_files=None, drop_files=(), project_id=None):
    directory = os.path.join(root, name)
    os.makedirs(directory)
    with open(os.path.join(directory, "manifest.json"), "w") as handle:
        json.dump(manifest, handle, indent=2)
    with open(os.path.join(directory, "project.json"), "w") as handle:
        json.dump({"id": manifest.get("project_id") if project_id is None else project_id}, handle)
    table_set = set(V2_TABLES)
    table_set.update(manifest.get("counts", {}))
    table_set.update(tables)
    for table in sorted(table_set):
        with open(os.path.join(directory, "%s.jsonl" % table), "w") as handle:
            for row in tables.get(table, []):
                handle.write(json.dumps(row) + "\n")
    for table, text in (raw_files or {}).items():
        with open(os.path.join(directory, "%s.jsonl" % table), "w") as handle:
            handle.write(text)
    for table in drop_files:
        os.remove(os.path.join(directory, "%s.jsonl" % table))
    return directory


def build_cases():
    cases = []

    def add(name, manifest, tables, expect, raw_files=None, drop_files=(), project_id=None):
        cases.append(
            {
                "name": name,
                "manifest": manifest,
                "tables": tables,
                "expect": expect,
                "raw_files": raw_files,
                "drop_files": drop_files,
                "project_id": project_id,
            }
        )

    add("v2-minimal", fixture_manifest(2), {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, None)

    manifest = fixture_manifest(2)
    manifest["parents"] = ["01FAKEPARENT0000000000001"]
    manifest["watermarks"] = {"tasks": {"rows": 1, "max_updated_at": "2026-09-10T00:00:00Z"}}
    manifest["sequences"]["display_id_decision"] = 2
    add("v2-full", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, None)

    add("v1-legacy", v1_manifest(), {"tasks": [TASK_ROW]}, None)
    manifest = v1_manifest()
    manifest["redacted"] = True
    add("v1-redacted", manifest, {"tasks": [TASK_ROW]}, None)

    manifest = v1_manifest()
    manifest["format_version"] = 3
    add("v3-future", manifest, {"tasks": [TASK_ROW]}, "newer than supported")
    manifest = v1_manifest()
    manifest["format_version"] = 0
    add("v0-ancient", manifest, {"tasks": [TASK_ROW]}, "older than the oldest")

    manifest = fixture_manifest(2)
    manifest["counts"].pop("tombstones")
    add("v2-missing-tombstone-count", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "tombstones")
    manifest = fixture_manifest(2)
    manifest["counts"]["tombstones"] = 2
    add("v2-tombstone-count-skew", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "tombstones")
    manifest = fixture_manifest(2)
    manifest["parents"] = [""]
    add("v2-parent-empty", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "parents")
    manifest = fixture_manifest(2)
    manifest["parents"] = ["01FAKEPARENT0000000000001", "01FAKEPARENT0000000000001"]
    add("v2-parent-duplicate", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "more than once")
    manifest = fixture_manifest(2)
    manifest["parents"] = [manifest["export_id"]]
    add("v2-parent-self", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "own export_id")
    manifest = fixture_manifest(2)
    manifest["watermarks"] = {"bogus_table": {"rows": 0}}
    add("v2-watermark-unknown-table", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "unknown table")
    manifest = fixture_manifest(2)
    manifest["watermarks"] = {"tasks": {"rows": 2}}
    add("v2-watermark-skew", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "watermark")
    manifest = fixture_manifest(2)
    manifest["watermarks"] = {"tasks": {"rows": "1"}}
    add("v2-watermark-bad-shape", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "watermark")
    manifest = fixture_manifest(2)
    manifest["redacted"] = False
    add("v2-unredacted", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "redacted")
    manifest = fixture_manifest(2)
    manifest["redacted"] = "true"
    add("v2-redacted-string", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "redacted")

    manifest = v1_manifest()
    manifest["watermarks"] = {"tasks": {"rows": 1}}
    add("v1-watermarks", manifest, {"tasks": [TASK_ROW]}, "watermarks")
    manifest = v1_manifest()
    manifest["counts"]["tombstones"] = 0
    add("v1-tombstone-count", manifest, {"tasks": [TASK_ROW]}, "tombstones")

    manifest = fixture_manifest(2)
    manifest["counts"]["tasks"] = -1
    add("counts-negative", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "tasks")
    manifest = fixture_manifest(2)
    manifest["counts"]["bogus_table"] = 1
    add("counts-unknown-table", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "unknown table")
    manifest = fixture_manifest(2)
    add("declared-count-skew", manifest, {"tasks": [], "tombstones": [TOMBSTONE_ROW]}, "tasks.jsonl has 0 rows")
    manifest = fixture_manifest(2)
    add(
        "tombstone-file-missing",
        manifest,
        {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]},
        "missing table file tombstones.jsonl",
        drop_files=("tombstones",),
    )
    manifest = fixture_manifest(2)
    manifest["counts"].pop("tasks")
    add("undeclared-rows", manifest, {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]}, "undeclared")
    manifest = fixture_manifest(2)
    manifest["project_id"] = "01FAKEFIXTURE00000000000099"
    add(
        "project-id-mismatch",
        manifest,
        {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]},
        "project.json",
        project_id="01FAKEFIXTURE00000000000000",
    )
    manifest = fixture_manifest(2)
    add(
        "bad-jsonl",
        manifest,
        {"tasks": [TASK_ROW], "tombstones": [TOMBSTONE_ROW]},
        "invalid JSON",
        raw_files={"tasks": "{not json\n"},
    )
    return cases


try:
    version, counts, worktree_bound_sessions = validate_pack(pack)
except PackError as failure:
    sys.exit("publish-ctxpack-selftest: FAIL: %s" % failure)

print("manifest v%d + counts + JSON: PASS (%d tables)" % (version, len(counts)))
print("WORKTREE_BOUND_SESSIONS=%d" % worktree_bound_sessions)

# Validator fixture matrix (check 4): the same validate_pack used on the real
# snapshot must accept valid v1/v2 packs and refuse the documented failure
# modes.
matrix_root = tempfile.mkdtemp(prefix="ctxpack-selftest-matrix.")
failures = []
case_count = 0
try:
    for case in build_cases():
        directory = write_fixture(
            matrix_root,
            case["name"],
            copy.deepcopy(case["manifest"]),
            copy.deepcopy(case["tables"]),
            raw_files=case["raw_files"],
            drop_files=case["drop_files"],
            project_id=case["project_id"],
        )
        try:
            validate_pack(directory, verbose=False)
            error = None
        except PackError as failure:
            error = str(failure)
        case_count += 1
        name = case["name"]
        expect = case["expect"]
        if expect is None:
            if error is not None:
                failures.append("fixture %s: expected accept, got: %s" % (name, error))
        elif error is None:
            failures.append("fixture %s: expected reject (%s), got accept" % (name, expect))
        elif expect not in error:
            failures.append("fixture %s: expected error containing %r, got: %s" % (name, expect, error))
finally:
    shutil.rmtree(matrix_root, ignore_errors=True)
if failures:
    sys.exit("publish-ctxpack-selftest: FAIL: validator fixture matrix: " + "; ".join(failures))
print("validator fixture matrix: PASS (%d cases)" % case_count)
PYEOF

# Check 6 — planted-fake-secret redaction fixture (CTX-0281, extended for
# ctxpack v2 in CTX-0314). Synthetic v1 and v2 packs only; FAKE marker
# values, never real credentials. Models the pipeline guarantee: the staging
# copy is redacted and stamped redacted=true (counts untouched, tombstones
# rows redacted like any other table), the source ("local DB") copy stays
# byte-identical.
REDACT_PY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/publish-ctxpack-redact.py"
[[ -f "$REDACT_PY" ]] || fail "redactor $REDACT_PY not found"
log "checking export-time redaction (planted-fake-secret v1+v2 fixtures)"
FIXT_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ctxpack-redact-fixture.XXXXXX")"
FIXT_ROOT="$FIXT_ROOT" timeout "$TIMEOUT_SECS" python3 - <<'PYEOF'
import json, os, shutil

root = os.environ["FIXT_ROOT"]
# Fake-but-shaped secret values: each carries a FAKE marker so a hit can
# never be mistaken for a real credential, while still tripping every
# redactor rule (secret field names, NAME=value env-dump lines, 40+ runs).
fakes = [
    "sk-FAKE-0123456789abcdef0123456789abcdef01",  # OPENAI_API_KEY / API_KEY value
    "FAKECLOUDFLARETOKENFAKECLOUDFLARE01",  # CLOUDFLARE_API_TOKEN value
    "ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE00",  # GH_PAT-shaped value
]
sha = "da39a3ee5e6b4b0d3255bfef95601890afd80709"  # must SURVIVE (SHA-1 exempt)
progress_row = {
    "id": "01FAKEFIXTURE00000000000001",
    "task_id": "01FAKEFIXTURE00000000000002",
    "type": "note",
    "content": "shell env:\nOPENAI_API_KEY=%s\nCLOUDFLARE_API_TOKEN=%s\nplain prose stays" % (fakes[0], fakes[1]),
}
event_row = {
    "id": "01FAKEFIXTURE00000000000003",
    "type": "progress.created",
    "payload_json": json.dumps(
        {"gh_pat": fakes[2], "detail": "session done", "sha": sha},
        separators=(",", ":"),
    ),
}
tombstone_row = {
    "project_id": "01FAKEFIXTURE00000000000000",
    "table_name": "worktrees",
    "row_id": "01FAKEFIXTURE00000000000004",
    "deleted_at": "2026-09-10T00:00:00Z",
    "deleted_by": "ctxpack-selftest",
    "reason": "cleanup API_KEY=%s" % fakes[0],
}
v2_manifest = {
    "format": "carryctx-pack-dir",
    "format_version": 2,
    "carryctx_version": "0.9.1",
    "schema_version": 18,
    "project_id": "01FAKEFIXTURE00000000000000",
    "export_id": "01FAKEFIXTURE00000000000005",
    "created_at": "2026-09-10T00:00:00Z",
    "parents": [],
    "sequences": {"display_id_CTX": 1},
    "source": {"git_commit": "0" * 40, "hostname": "fixture"},
    "counts": {"progress_items": 1, "events": 1, "tombstones": 1},
    "watermarks": {"tombstones": {"rows": 1}},
}
v1_manifest = {
    "format": "carryctx-pack-dir",
    "format_version": 1,
    "project_id": "01FAKEFIXTURE00000000000000",
    "counts": {"progress_items": 1, "events": 1},
}
for name, manifest, tables in (
    ("local-v1", v1_manifest, {"progress_items": [progress_row], "events": [event_row]}),
    (
        "local-v2",
        v2_manifest,
        {"progress_items": [progress_row], "events": [event_row], "tombstones": [tombstone_row]},
    ),
):
    directory = os.path.join(root, name)
    os.makedirs(directory)
    with open(os.path.join(directory, "manifest.json"), "w") as handle:
        json.dump(manifest, handle, indent=2)
        handle.write("\n")
    shutil.copyfile(
        os.path.join(directory, "manifest.json"),
        os.path.join(directory, "manifest.json.orig"),
    )
    with open(os.path.join(directory, "project.json"), "w") as handle:
        json.dump({"id": manifest["project_id"]}, handle)
    for table, rows in tables.items():
        with open(os.path.join(directory, "%s.jsonl" % table), "w") as handle:
            for row in rows:
                handle.write(json.dumps(row) + "\n")
with open(os.path.join(root, "fakes.txt"), "w") as handle:
    handle.write("\n".join(fakes + [sha]) + "\n")
print("fixture packs written (v1 + v2 source copies)")
PYEOF
for variant in v1 v2; do
	cp -a "$FIXT_ROOT/local-$variant" "$FIXT_ROOT/staging-$variant"
	timeout "$TIMEOUT_SECS" python3 "$REDACT_PY" "$FIXT_ROOT/staging-$variant" >/dev/null ||
		fail "redactor failed on the $variant planted-secret fixture"
done
FIXT_ROOT="$FIXT_ROOT" timeout "$TIMEOUT_SECS" python3 - <<'PYEOF'
import json, os, sys

root = os.environ["FIXT_ROOT"]
with open(os.path.join(root, "fakes.txt")) as handle:
    fakes_and_sha = handle.read().split()
fakes, sha = fakes_and_sha[:3], fakes_and_sha[3]

for variant, tables in (
    ("v1", ("progress_items", "events")),
    ("v2", ("progress_items", "events", "tombstones")),
):
    local = os.path.join(root, "local-" + variant)
    staging = os.path.join(root, "staging-" + variant)

    # The source copy is the local DB stand-in: byte-identical manifest and
    # the planted fakes still present.
    with open(os.path.join(local, "manifest.json"), "rb") as handle:
        source_manifest_bytes = handle.read()
    with open(os.path.join(local, "manifest.json.orig"), "rb") as handle:
        original_manifest_bytes = handle.read()
    if source_manifest_bytes != original_manifest_bytes:
        sys.exit("%s source manifest was rewritten (local DB must stay untouched)" % variant)

    with open(os.path.join(staging, "manifest.json")) as handle:
        staged_manifest = json.load(handle)
    with open(os.path.join(local, "manifest.json")) as handle:
        source_manifest = json.load(handle)
    if staged_manifest.get("redacted") is not True:
        sys.exit("%s staging manifest: redacted is not true (%r)" % (variant, staged_manifest.get("redacted")))
    if staged_manifest.get("counts") != source_manifest.get("counts"):
        sys.exit(
            "%s staging manifest: counts changed (%r -> %r)"
            % (variant, source_manifest.get("counts"), staged_manifest.get("counts"))
        )

    staged_text = ""
    local_text = ""
    for table in tables:
        with open(os.path.join(local, "%s.jsonl" % table)) as handle:
            local_lines = [line for line in handle.read().splitlines() if line.strip()]
        local_text += "\n".join(local_lines) + "\n"
        with open(os.path.join(staging, "%s.jsonl" % table)) as handle:
            staged_lines = [line for line in handle.read().splitlines() if line.strip()]
        if len(staged_lines) != len(local_lines):
            sys.exit(
                "%s staging %s.jsonl has %d rows, want %d"
                % (variant, table, len(staged_lines), len(local_lines))
            )
        for number, line in enumerate(staged_lines, 1):
            try:
                json.loads(line)
            except ValueError as error:
                sys.exit("%s staging %s.jsonl line %d: invalid JSON (%s)" % (variant, table, number, error))
        staged_text += "\n".join(staged_lines) + "\n"

    for fake in fakes:
        if fake in staged_text:
            sys.exit("%s staging copy still contains planted fake value %s..." % (variant, fake[:12]))
        if fake not in local_text:
            sys.exit("%s source copy lost planted fake value (local DB must stay untouched)" % variant)
    if "***REDACTED***" not in staged_text:
        sys.exit("%s staging copy has no ***REDACTED*** markers" % variant)
    for name in ("OPENAI_API_KEY", "CLOUDFLARE_API_TOKEN", "gh_pat"):
        if name not in staged_text:
            sys.exit("%s staging copy lost field/variable name %s (names must survive)" % (variant, name))
    if sha not in staged_text:
        sys.exit("%s staging copy lost the SHA-1 control string (over-redaction)" % variant)
    if variant == "v2":
        with open(os.path.join(staging, "tombstones.jsonl")) as handle:
            tombstone_rows = [json.loads(line) for line in handle if line.strip()]
        if len(tombstone_rows) != 1:
            sys.exit("v2 staging tombstones.jsonl has %d rows, want 1" % len(tombstone_rows))
        reason = tombstone_rows[0].get("reason") or ""
        if tombstone_rows[0].get("table_name") != "worktrees" or "cleanup" not in reason:
            sys.exit("v2 staging tombstone row content changed unexpectedly: %r" % tombstone_rows[0])
        if "API_KEY" not in reason:
            sys.exit("v2 staging copy lost the tombstone reason variable name")
    print("  redaction fixture %s: staging redacted + stamped, source untouched: PASS" % variant)
print("redaction fixture: PASS")
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
