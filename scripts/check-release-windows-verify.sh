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
#     `Join-Path '<root>' $arch` -> `<root>/<arch>`. The lookup is parsed from
#     PowerShell code, not from prose: comments and here-string bodies are
#     ignored, and the job must contain exactly one such lookup (zero is a
#     failure, and so is more than one — the gate is not a first-match search).
#
#   Rule 2 — a version assertion tolerates a trimmed capture.
#     The payload is checked variously as a raw multi-line string and as a
#     `.Trim()`-ed console capture. Every `\Abitty ` assertion is parsed as a
#     PowerShell string expression (quote-aware, comment-free), and the
#     trailing expression after the last `\)` inside the pattern itself must
#     be exactly `\s*\z`. A mandatory newline anywhere before the optional
#     whitespace (for example `(?:\r\n|\n)\s*\z`) is rejected. A pattern
#     variable compared by a `-match` operator that the scan cannot resolve is
#     a failure, never a silent skip.
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
    sed -n '2,34p' "$0"
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


# --- PowerShell surface parsing ---------------------------------------------
# The workflow embeds PowerShell inside YAML block scalars, so a textual search
# also sees text PowerShell never executes: line comments and here-string data.
# The helpers below model just enough PowerShell to tell code from prose.


def strip_comment(line):
    """Drop a PowerShell line comment, keeping quoted literals intact."""
    quote = None
    out = []
    index = 0
    while index < len(line):
        char = line[index]
        if quote == "'":
            if char == "'":
                if line[index + 1 : index + 2] == "'":
                    out.append("''")
                    index += 2
                    continue
                quote = None
            out.append(char)
            index += 1
            continue
        if quote == '"':
            if char == "`" and index + 1 < len(line):
                out.append(line[index : index + 2])
                index += 2
                continue
            if char == '"':
                if line[index + 1 : index + 2] == '"':
                    out.append('""')
                    index += 2
                    continue
                quote = None
            out.append(char)
            index += 1
            continue
        if char in ("'", '"'):
            quote = char
            out.append(char)
            index += 1
            continue
        if char == "#":
            break
        out.append(char)
        index += 1
    return "".join(out)


def code_lines(source):
    """Yield (line_number, code) with comments dropped and here-strings blanked.

    A here-string opens with `@"` or `@'` as the last token of a line and closes
    with `"@` or `'@` as the first token of a later line. Its body is data, so
    a lookup quoted inside one is not an executed lookup.
    """
    closing = None
    for number, raw in enumerate(source.splitlines(), start=1):
        if closing is not None:
            if raw.lstrip().startswith(closing):
                closing = None
            yield number, ""
            continue
        trimmed = raw.rstrip()
        if trimmed.endswith('@"') or trimmed.endswith("@'"):
            closing = trimmed[-1] + "@"
            yield number, strip_comment(trimmed)
            continue
        yield number, strip_comment(raw)


def string_literals(expression):
    """Return ([(value, end_offset)], terminated) for a PowerShell expression.

    Single-quoted literals escape a quote by doubling it; double-quoted literals
    escape with a backtick (or a doubled quote). `terminated` is False when a
    literal is left open, which makes the expression unparseable.
    """
    literals = []
    index = 0
    while index < len(expression):
        char = expression[index]
        if char not in ("'", '"'):
            index += 1
            continue
        quote = char
        value = []
        index += 1
        terminated = False
        while index < len(expression):
            char = expression[index]
            if quote == "'":
                if char == "'":
                    if expression[index + 1 : index + 2] == "'":
                        value.append("'")
                        index += 2
                        continue
                    index += 1
                    terminated = True
                    break
            else:
                if char == "`" and index + 1 < len(expression):
                    value.append(expression[index + 1])
                    index += 2
                    continue
                if char == '"':
                    if expression[index + 1 : index + 2] == '"':
                        value.append('"')
                        index += 2
                        continue
                    index += 1
                    terminated = True
                    break
            value.append(char)
            index += 1
        if not terminated:
            return literals, False
        literals.append(("".join(value), index))
    return literals, True


# --- Rule 1: download path matches the directory the loop reads --------------
def job_block(name):
    """Return (job_text, first_line_number); job_text is '' when absent."""
    start = text.find(f"  {name}:")
    if start < 0:
        return "", 0
    match = re.search(r"\n  [a-z][a-z0-9-]*:\n", text[start + 1 :])
    end = start + 1 + match.start() if match else len(text)
    return text[start:end], text[:start].count("\n") + 1


zip_job, zip_job_first_line = job_block("verify-windows-zip")
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
    for m in re.finditer(r"name: bitty-windows-zip-(\w+)\n\s+path: (\S+)", zip_job)
)

# Rule 1 compares against the verifier's own lookup, not a constant: the loop
# builds its path from `Join-Path '<root>' $arch`, so parse that expression.
# Exactly one executable occurrence is required — a stale comment or a second
# lookup (for example after a partial rename) is a failure, not a first-match
# guess.
lookup_pattern = re.compile(r"Join-Path\s+'([^']+)'\s+\$arch\b")
lookups = []
for number, code in code_lines(zip_job):
    for match in lookup_pattern.finditer(code):
        lookups.append((zip_job_first_line + number - 1, match.group(1).rstrip("/")))

if len(lookups) != 1:
    located = ", ".join(f"line {line}" for line, _ in lookups) if lookups else "none"
    fail(
        "rule-1",
        "verify-windows-zip must contain exactly one executable "
        f"Join-Path '<root>' $arch lookup, found {len(lookups)} ({located})",
    )
    lookup_root = None
else:
    lookup_root = lookups[0][1]

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
ASSIGNMENT = re.compile(r"^\s*\$(?P<var>[A-Za-z_]\w*)\s*=\s*(?P<rhs>.*)$")
MATCH_COMPARISON = re.compile(
    r"\$[A-Za-z_]\w*\s+-(?:cnotmatch|notmatch|cmatch|match)\s+\$(?P<var>[A-Za-z_]\w*)"
)

definitions = {}
for number, code in code_lines(text):
    assignment = ASSIGNMENT.match(code)
    if assignment:
        definitions.setdefault(assignment.group("var"), []).append(
            (number, assignment.group("rhs"))
        )

assertions = 0
assertion_vars = set()
for number, code in code_lines(text):
    if "\\Abitty " not in code:
        continue
    assertions += 1
    assignment = ASSIGNMENT.match(code)
    if not assignment:
        fail(
            "rule-2",
            f"line {number}: version assertion is not a '$name = <pattern>' assignment: {code.strip()}",
        )
        continue
    variable = assignment.group("var")
    expression = assignment.group("rhs")
    assertion_vars.add(variable)
    literals, terminated = string_literals(expression)
    if not terminated or not literals:
        fail(
            "rule-2",
            f"line {number}: cannot parse the pattern assigned to ${variable} "
            "(unterminated or literal-free expression)",
        )
        continue
    trailing = expression[literals[-1][1] :].strip(" \t;")
    if trailing:
        fail(
            "rule-2",
            f"line {number}: ${variable} pattern is not terminated by a string literal: {trailing}",
        )
        continue
    pattern = literals[-1][0]
    marker = pattern.rfind("\\)")
    if marker < 0:
        fail(
            "rule-2",
            f"line {number}: ${variable} has no version-suffix '\\)' terminator: {pattern}",
        )
        continue
    if pattern[marker + 2 :] != "\\s*\\z":
        fail(
            "rule-2",
            f"line {number}: ${variable} pattern suffix must be exactly "
            f"\\s*\\z (no mandatory newline): {pattern}",
        )

if not assertions:
    fail("rule-2", "no version assertion found; the gate would be vacuous")

# Coverage: every match comparison whose operand could be a version pattern is
# either one of the assertions checked above or provably something else. An
# unresolvable operand fails closed — a renamed pattern variable must never be
# silently unexamined.
for number, code in code_lines(text):
    for comparison in MATCH_COMPARISON.finditer(code):
        variable = comparison.group("var")
        if variable in assertion_vars:
            continue
        operand_definitions = definitions.get(variable, [])
        if any("\\Abitty " in rhs for _, rhs in operand_definitions):
            fail(
                "rule-2",
                f"line {number}: ${variable} is a version assertion the scan did not check",
            )
            continue
        if any(rhs.rstrip().endswith(('@"', "@'")) for _, rhs in operand_definitions):
            fail(
                "rule-2",
                f"line {number}: ${variable} pattern is a here-string; the gate cannot parse it",
            )
            continue
        if operand_definitions:
            continue
        fail(
            "rule-2",
            f"line {number}: match comparison uses ${variable}, which no assignment in the workflow defines",
        )

if failures:
    for failure in failures:
        print(f"check-release-windows-verify: FAIL: {failure}", file=sys.stderr)
    sys.exit(1)

print(f"check-release-windows-verify: {assertions} version assertion(s) tolerate trimmed output")
print(f"check-release-windows-verify: portable ZIP directories match the loop: {', '.join(sorted(downloads))}")
print("check-release-windows-verify: PASS")
PY
