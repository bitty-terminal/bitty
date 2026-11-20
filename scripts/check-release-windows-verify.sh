#!/usr/bin/env bash
# check-release-windows-verify.sh — CTX-1916 (bitty#3274) static gate for the
# release workflow's Windows verification jobs.
#
# Those jobs only run on a release tag, so a defect there is discovered after
# the tag is pushed and every build has already succeeded. The v0.0.46 run
# (44718678246) proved the cost: two verification scripts failed on trivial
# bookkeeping, so the R2 mirror, the GitHub Release and every package channel
# were skipped.
#
# This gate pins the two invariants those scripts broke:
#
#   Rule 1 — the portable-ZIP verifier reads the directories it downloaded.
#     Each `bitty-windows-zip-<arch>` artifact download path must equal the
#     directory the expand loop's own lookup builds for that same arch, i.e.
#     `Join-Path '<root>' $arch` -> `<root>/<arch>`. The lookup expression is
#     parsed, not assumed: a rename of the joined root is a rule-1 failure.
#
#   Rule 2 — a version assertion tolerates a trimmed capture.
#     The payload is checked variously as a raw multi-line string and as a
#     `.Trim()`-ed console capture. The trailing expression after `\)` must be
#     exactly `\s*\z`, so a mandatory newline anywhere before the optional
#     whitespace (for example `(?:\r\n|\n)\s*\z`) is rejected.
#
# Usage: scripts/check-release-windows-verify.sh [--root <dir>]
#   --root scans <dir> instead of the repository root (fixture tests).
#
# Read-only: it parses the workflow text and never executes PowerShell.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKFLOW_REL=".github/workflows/release.yml"

while [[ $# -gt 0 ]]; do
  case "$1" in
  --root)
    ROOT="$(cd "$2" && pwd)"
    shift 2
    ;;
  -h | --help)
    sed -n '2,30p' "$0"
    exit 0
    ;;
  *)
    echo "check-release-windows-verify: unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

WORKFLOW="$ROOT/$WORKFLOW_REL"
if [[ ! -f "$WORKFLOW" ]]; then
  echo "check-release-windows-verify: missing $WORKFLOW_REL under $ROOT" >&2
  exit 1
fi

python3 - "$WORKFLOW" <<'PY'
import re
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
failures = []


def fail(rule, message):
    failures.append(f"{rule}: {message}")


# --- Rule 1: download path matches the directory the loop reads --------------
def job_block(name):
    start = text.find(f"  {name}:")
    if start < 0:
        return ""
    match = re.search(r"\n  [a-z][a-z0-9-]*:\n", text[start + 1:])
    return text[start:start + 1 + match.start()] if match else text[start:]


zip_job = job_block("verify-windows-zip")
if not zip_job:
    fail("rule-1", "verify-windows-zip job not found")

arch_list = re.search(r"\$arch in @\(([^)]*)\)", zip_job)
if not arch_list:
    fail("rule-1", "verify-windows-zip does not declare its architecture loop")
    arches = []
else:
    arches = re.findall(r"'([^']+)'", arch_list.group(1))
    if not arches:
        fail("rule-1", "verify-windows-zip architecture loop is empty")

downloads = dict(
    (m.group(1), m.group(2))
    for m in re.finditer(
        r"name: bitty-windows-zip-(\w+)\n\s+path: (\S+)", zip_job
    )
)

# Rule 1 compares against the verifier's own lookup, not a constant: the loop
# builds its path from `Join-Path '<root>' $arch`, so parse that expression.
lookup = re.search(r"Join-Path\s+'([^']+)'\s+\$arch", zip_job)
if not lookup:
    fail("rule-1", "verify-windows-zip does not build its archive directory with Join-Path '<root>' $arch")
    lookup_root = None
else:
    lookup_root = lookup.group(1).rstrip("/")

if lookup_root is not None:
    for arch in arches:
        if arch not in downloads:
            fail("rule-1", f"no bitty-windows-zip-{arch} download in verify-windows-zip")
            continue
        expected = f"{lookup_root}/{arch}"
        if downloads[arch] != expected:
            fail(
                "rule-1",
                f"bitty-windows-zip-{arch} downloads to {downloads[arch]} but the loop reads {expected}",
            )

for extra, dest in downloads.items():
    if extra not in arches:
        fail(
            "rule-1",
            f"bitty-windows-zip-{extra} downloads to {dest} but no loop iteration reads it",
        )

# --- Rule 2: version assertions tolerate a trimmed capture -------------------
assertions = [
    line.strip()
    for line in text.splitlines()
    if "\\Abitty " in line and "$pattern" in line
]
if not assertions:
    fail("rule-2", "no version assertion found; the gate would be vacuous")

for line in assertions:
    # The pattern is the last single-quoted segment of the line; everything
    # after `\)` must be exactly the optional-whitespace terminator.
    tail_marker = line.rfind("\\)")
    if tail_marker < 0:
        fail("rule-2", f"assertion has no version-suffix terminator: {line}")
        continue
    tail = line[tail_marker + 2 :]
    if tail != "\\s*\\z'":
        fail(
            "rule-2",
            f"assertion suffix must be exactly \\s*\\z (no mandatory newline): {line}",
        )

if failures:
    for failure in failures:
        print(f"check-release-windows-verify: FAIL: {failure}", file=sys.stderr)
    sys.exit(1)

print(f"check-release-windows-verify: {len(assertions)} version assertion(s) tolerate trimmed output")
print(f"check-release-windows-verify: portable ZIP directories match the loop: {', '.join(sorted(downloads))}")
print("check-release-windows-verify: PASS")
PY
