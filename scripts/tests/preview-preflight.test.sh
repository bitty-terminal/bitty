#!/usr/bin/env bash
# preview-preflight.test.sh — hermetic matrix for scripts/preview-preflight.sh.
#
# Covers the accepted cadence decisions without network, git, or a release:
# the Europe/Berlin window and no-backdating rule (P-02/P-08), the exact stable
# bound and its ambiguity/missing failures (P-11), one preview per cadence date,
# the same-commit no-op, and the vX.Y.Z-preview.YYYYMMDD tag grammar.
# Follows the PASS:/FAIL: convention of the other scripts/tests/*.test.sh gates.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/preview-preflight.sh
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

[[ -x "$SCRIPT" ]] || {
  echo "FAIL: $SCRIPT is missing or not executable" >&2
  exit 1
}

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/preview-preflight-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

# Deterministic 40-character object ids; only equality is asserted.
sha_for() { printf '%040d' "$1"; }

HEAD_SHA="$(sha_for 100)"
OLD_SHA="$(sha_for 101)"
OTHER_SHA="$(sha_for 102)"

# write_tags <file> <tag>[:<sha>] ...
# A bare <tag> is a lightweight tag pointing at OLD_SHA. A <tag>^{} entry is
# written when the tag carries a peeled marker (annotated tag).
write_tags() {
  local file="$1"
  shift
  : >"$file"
  local spec tag sha
  for spec in "$@"; do
    tag="${spec%%:*}"
    if [[ "$spec" == *:* ]]; then
      sha="${spec#*:}"
    else
      sha="$OLD_SHA"
    fi
    if [[ "$tag" == *'^{}' ]]; then
      printf '%s refs/tags/%s\n' "$sha" "$tag" >>"$file"
    else
      printf '%s refs/tags/%s\n' "$sha" "$tag" >>"$file"
    fi
  done
}

OUT="$TMP_BASE/out"
SUMMARY="$TMP_BASE/summary"
STDOUT="$TMP_BASE/stdout"
STDERR="$TMP_BASE/stderr"
STATUS=0

run_case() { # <now> <base> <commit> <tags-file> [extra args...]
  local now="$1" base="$2" commit="$3" tags="$4"
  shift 4
  : >"$OUT"
  : >"$SUMMARY"
  set +e
  "$SCRIPT" --now "$now" --base-version "$base" --commit "$commit" \
    --tags-file "$tags" --output "$OUT" --summary "$SUMMARY" \
    "$@" >"$STDOUT" 2>"$STDERR"
  STATUS=$?
  set -e
}

field() { # <key>
  awk -F= -v key="$1" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$OUT"
}

expect_status() { # <expected> <label>
  if [[ "$STATUS" -eq "$1" ]]; then
    pass "$2 (exit $1)"
  else
    fail "$2 (expected exit $1, got $STATUS; stderr: $(tr '\n' ' ' <"$STDERR"))"
  fi
}

expect_field() { # <key> <expected> <label>
  local actual
  actual="$(field "$1")"
  if [[ "$actual" == "$2" ]]; then
    pass "$3 ($1=$2)"
  else
    fail "$3 (expected $1='$2', got '$actual')"
  fi
}

expect_reason_contains() { # <substring> <label>
  local actual
  actual="$(field reason)"
  if [[ "$actual" == *"$1"* ]]; then
    pass "$2"
  else
    fail "$2 (reason '$actual' does not contain '$1')"
  fi
}

TAGS="$TMP_BASE/tags"

# --- baseline: a cadence slot with an older preview and older stable tag ---
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.29-preview.20261026:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "baseline cadence slot"
expect_field run true "baseline publishes"
expect_field cadence_date 20261027 "baseline cadence date derives from Berlin wall clock"
expect_field tag "v0.0.30-preview.20261027" "baseline tag uses the bare base version and cadence date"
expect_field base_version 0.0.30 "baseline base version"
expect_field sha "$HEAD_SHA" "baseline commit"
if grep -q 'Preview cadence slot' "$SUMMARY"; then
  pass "baseline records the cadence slot in the step summary"
else
  fail "baseline step summary is missing the cadence slot"
fi

# --- P-02: the accepted window is inclusive at both ends ---
for hm in "23:25" "23:30" "23:59"; do
  run_case "2026-10-27 $hm" "0.0.30" "$HEAD_SHA" "$TAGS"
  expect_status 0 "window $hm"
  expect_field run true "window $hm publishes"
done
for hm in "23:24" "22:59" "00:00"; do
  run_case "2026-10-27 $hm" "0.0.30" "$HEAD_SHA" "$TAGS"
  expect_status 0 "outside window $hm"
  expect_field run false "outside window $hm is a green no-op"
  expect_reason_contains "off-slot" "outside window $hm names the off-slot reason"
done

# --- P-08: no backdating; a run after local midnight takes the new date ---
run_case "2026-10-28 00:05" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "post-midnight run"
expect_field run false "post-midnight run is a no-op"
expect_field cadence_date 20261028 "post-midnight run uses the new local date, never yesterday"
if grep -q 'Preview skipped' "$SUMMARY"; then
  pass "no-op records the skip in the step summary"
else
  fail "no-op step summary is missing the skip"
fi

# --- P-11: strict numeric ordering against the newest stable tag ---
write_tags "$TAGS" "v0.0.30" "v0.0.29"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "base equal to newest stable"
expect_field run false "base equal to newest stable is a no-op"
expect_reason_contains "not strictly greater" "equality names the P-11 reason"

write_tags "$TAGS" "v0.0.31"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "base older than newest stable"
expect_field run false "base older than newest stable is a no-op"

write_tags "$TAGS" "v0.0.99" "v0.0.100-preview.20261026:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.100" "$HEAD_SHA" "$TAGS"
expect_status 0 "numeric (not lexical) version ordering"
expect_field run true "0.0.100 is greater than 0.0.99 numerically"

write_tags "$TAGS" "v0.0.30-rc1" "v0.0.29"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "suffixed tag is not a stable bound"
expect_field run true "a suffixed tag never raises the stable bound"

# --- P-11 fail-closed: missing or ambiguous stable tags ---
write_tags "$TAGS" "v0.0.30-preview.20261026:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 1 "missing stable tag fails closed"
if [[ ! -s "$OUT" ]]; then
  pass "missing stable tag writes no run outputs"
else
  fail "missing stable tag wrote run outputs: $(tr '\n' ' ' <"$OUT")"
fi

write_tags "$TAGS" "v0.0.29" "v0.0.029"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 1 "ambiguous stable tags fail closed"
if grep -q 'ambiguous stable tags' "$STDERR"; then
  pass "ambiguous stable tags name the ambiguity"
else
  fail "ambiguous stable tags do not name the ambiguity: $(tr '\n' ' ' <"$STDERR")"
fi

# --- P-08: one preview per cadence date, exact tag only ---
write_tags "$TAGS" "v0.0.29" "v0.0.30-preview.20261027:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "existing cadence-date tag"
expect_field run false "existing cadence-date tag is a no-op"
expect_reason_contains "already exists" "existing tag never moves or gets reused"

write_tags "$TAGS" "v0.0.29" "v0.0.30-preview.20261025:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "tag for another date does not block"
expect_field run true "a preview on another date never blocks the cadence slot"

# A base-version bump after a preview must not admit a second preview on the
# same local date; the date marker is independent of the base version.
write_tags "$TAGS" "v0.0.28" "v0.0.29-preview.20261027:$OLD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "same-date preview with another base version"
expect_field run false "a cadence date admits only one preview, even after a bump"
expect_reason_contains "cadence date 20261027" "same-date no-op names the cadence date"

# --- same-commit no-op (nearest marker by date, then version) ---
write_tags "$TAGS" "v0.0.29" "v0.0.30-preview.20261026:$HEAD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "same commit re-preview"
expect_field run false "same commit is a no-op"
expect_reason_contains "already previewed" "same commit names the no-op"

write_tags "$TAGS" "v0.0.29" "v0.0.30-preview.20261026:$OTHER_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "advanced main after a preview"
expect_field run true "a new commit publishes a new dated preview"

# Two previews share a date: the higher base version is the deterministic tag
# fallback marker when no release state is supplied.
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.30-preview.20261026:$OTHER_SHA" \
  "v0.0.31-preview.20261026:$HEAD_SHA"
run_case "2026-10-27 23:30" "0.0.32" "$HEAD_SHA" "$TAGS"
expect_status 0 "same-date tag fallback tie"
expect_field run false "the higher base version is the deterministic tie marker"

# Annotated tag: the peeled commit wins over the tag object id.
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.30-preview.20261026:$OTHER_SHA" \
  "v0.0.30-preview.20261026^{}:$HEAD_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "annotated preview tag resolves its commit"
expect_field run false "annotated preview tag no-ops on the peeled commit"

# The newest preview by date wins even when an older date points at HEAD.
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.30-preview.20261025:$HEAD_SHA" \
  "v0.0.30-preview.20261026:$OTHER_SHA"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 0 "newest preview marker selection"
expect_field run true "the newest preview, not any historical one, decides the no-op"

# --- release state outranks a tag that never published -----------------------
RELEASES="$TMP_BASE/releases.txt"

# A newer preview tag points at HEAD, but the newest successful preview
# release is older and points elsewhere: the commit is publishable.
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.30-preview.20261025:$OTHER_SHA" \
  "v0.0.30-preview.20261026:$HEAD_SHA"
printf '%s\n' "v0.0.30-preview.20261025" >"$RELEASES"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS" --releases-file "$RELEASES"
expect_status 0 "release state outranks an unpublished tag"
expect_field run true "a tag that never published is not a same-commit no-op"

# The newest successful preview release points at HEAD even though a newer tag
# points elsewhere: the commit is already previewed.
write_tags "$TAGS" \
  "v0.0.29" \
  "v0.0.30-preview.20261025:$OTHER_SHA" \
  "v0.0.30-preview.20261026:$HEAD_SHA"
printf '%s\n' "v0.0.29" "v0.0.30-preview.20261025" "v0.0.30-preview.20261026" >"$RELEASES"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS" --releases-file "$RELEASES"
expect_status 0 "newest successful release marker"
expect_field run false "the newest successful release at HEAD is a no-op"

# Release state exists but has no previews (for example a failed run left a
# tag): the tag fallback must not suppress the next legitimate preview.
write_tags "$TAGS" "v0.0.29" "v0.0.30-preview.20261026:$HEAD_SHA"
: >"$RELEASES"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS" --releases-file "$RELEASES"
expect_status 0 "empty release state"
expect_field run true "an unpublished tag does not block after a failed run"

# A release tag whose commit is absent from the tag snapshot fails closed.
write_tags "$TAGS" "v0.0.29"
printf '%s\n' "v0.0.30-preview.20261026" >"$RELEASES"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS" --releases-file "$RELEASES"
expect_status 1 "unresolvable release marker fails closed"

# --- input validation and fail-closed plumbing ---
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TMP_BASE/missing-tags"
expect_status 1 "missing tags file fails closed"

printf 'not-a-tag-line\n' >"$TMP_BASE/bad-tags"
run_case "2026-10-27 23:30" "0.0.30" "$HEAD_SHA" "$TMP_BASE/bad-tags"
expect_status 1 "malformed tags file fails closed"

run_case "2026-10-27 23:30" "0.0" "$HEAD_SHA" "$TAGS"
expect_status 2 "malformed base version is a usage error"

run_case "2026-13-27 23:30" "0.0.30" "$HEAD_SHA" "$TAGS"
expect_status 2 "malformed --now is a usage error"

if "$SCRIPT" --help >/dev/null 2>&1; then
  pass "--help exits zero"
else
  fail "--help exited non-zero"
fi

if ((FAIL)); then
  echo "preview-preflight-test: FAIL" >&2
  exit 1
fi
echo "preview-preflight-test: OK"
