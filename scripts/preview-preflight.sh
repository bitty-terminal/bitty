#!/usr/bin/env bash
# preview-preflight.sh — decide whether a run is a nightly preview cadence slot.
#
# The accepted contract is
# bitty-terminal-docs/specifications/preview-release-contract.md (P-01, P-02,
# P-08, P-11). The two UTC crons in .github/workflows/preview.yml are triggers,
# not guarantees: this script is the only authority on whether a run belongs to
# a cadence slot. A run outside the accepted local window, or one that would
# duplicate a cadence date or re-preview the same commit, is a successful no-op
# (`run=false`), never a failure and never a backdated preview.
#
# Usage:
#   scripts/preview-preflight.sh [options]
#
# Options:
#   --now "YYYY-MM-DD HH:MM"  Europe/Berlin wall clock (tests); default: now.
#   --base-version X.Y.Z      workspace base version; default:
#                             scripts/check-release-version.sh --print.
#   --commit SHA              commit being previewed; default: git HEAD.
#   --tags-file FILE          output of `git ls-remote --tags origin 'refs/tags/v*'`
#                             (tests pass a fixture); default: query origin.
#   --releases-file FILE      preview release tag names, oldest first (the
#                             workflow writes `gh release list ... --jq
#                             'sort_by(.createdAt) | .[].tagName'`). When
#                             supplied, the same-commit no-op uses the most
#                             recent *successful preview release*; without it
#                             the script falls back to the newest preview tag.
#   --output FILE             write run/cadence_date/tag/base_version/sha/reason
#                             as `key=value` lines; default: $GITHUB_OUTPUT when
#                             set, otherwise stdout.
#   --summary FILE            append a Markdown result summary; default:
#                             $GITHUB_STEP_SUMMARY when set.
#   -h, --help                show this help.
#
# Stable-tag semantics (exact, not nearest):
#
#   * An **exact stable tag** matches `^v[0-9]+\.[0-9]+\.[0-9]+$`. Nothing else
#     is a stable tag: preview tags, suffixed tags (`-rc1`, `-beta`, ...),
#     prefixed tags, and malformed spellings are never used as a version bound.
#   * The bound is the numerically greatest exact stable tag — not the nearest
#     lexical match, not the most recently created tag, and not a tag prefix.
#     `v0.0.100` sorts above `v0.0.99` because major/minor/patch are compared as
#     integers.
#   * Two exact spellings that normalize to the same numeric version (for
#     example `v0.0.29` and `v0.0.029`) are **ambiguous**; the script fails
#     closed instead of choosing one. No exact stable tag at all also fails
#     closed: a preview cannot be ordered without a lower bound.
#
# Preview-tag semantics:
#
#   * A preview tag matches `^vX\.Y\.Z-preview\.YYYYMMDD$` and is unique per
#     cadence date. An existing tag for the computed cadence date is a no-op:
#     it is never moved, re-pointed, or reused.
#   * The same-commit no-op uses the most recent successful preview release
#     (`--releases-file`, newest by release creation time) when the caller
#     supplies release state. Without release state it falls back to the newest
#     preview tag (by date stamp, then numeric base version). When the marker's
#     target commit equals the commit under test the run is a no-op; a marker
#     that cannot be resolved to a commit fails closed.
#
# Exit codes: 0 = run or no-op decision written, 1 = fail-closed input error,
# 2 = usage error.
set -euo pipefail

export LC_ALL=C

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# P-01: the accepted schedule zone. A future change is a new owner decision; it
# is not an environment knob.
readonly PREVIEW_TZ="Europe/Berlin"
# P-02: accepted lateness window (inclusive), local wall clock.
readonly PREVIEW_WINDOW_START="23:25"
readonly PREVIEW_WINDOW_END="23:59"
readonly STABLE_TAG_RE='^v([0-9]+)\.([0-9]+)\.([0-9]+)$'
readonly PREVIEW_TAG_RE='^v([0-9]+)\.([0-9]+)\.([0-9]+)-preview\.([0-9]{8})$'

NOW=""
BASE_VERSION=""
COMMIT=""
TAGS_FILE=""
RELEASES_FILE=""
OUTPUT_FILE=""
SUMMARY_FILE=""

usage() {
  cat <<'EOF'
Usage: scripts/preview-preflight.sh [options]

  --now "YYYY-MM-DD HH:MM"  Europe/Berlin wall clock (tests); default: now
  --base-version X.Y.Z      workspace version; default: check-release-version --print
  --commit SHA              previewed commit; default: git HEAD
  --tags-file FILE          `git ls-remote --tags origin 'refs/tags/v*'` output
  --releases-file FILE      preview release tags, oldest first
  --output FILE             output file (default: $GITHUB_OUTPUT, else stdout)
  --summary FILE            step summary file (default: $GITHUB_STEP_SUMMARY)
  -h, --help                show this help

Writes run=<true|false>, cadence_date, tag, base_version, sha, and reason.
A no-op writes run=false and exits 0; invalid or ambiguous inputs exit 1.
EOF
}

usage_error() {
  printf 'preview-preflight: %s\n' "$*" >&2
  printf 'run with --help\n' >&2
  exit 2
}

die() {
  printf 'preview-preflight[error]: %s\n' "$*" >&2
  exit 1
}

while (($#)); do
  case "$1" in
  --now | --base-version | --commit | --tags-file | --releases-file | --output | --summary)
    [[ $# -ge 2 ]] || usage_error "$1 needs a value"
    case "$1" in
    --now) NOW="$2" ;;
    --base-version) BASE_VERSION="$2" ;;
    --commit) COMMIT="$2" ;;
    --tags-file) TAGS_FILE="$2" ;;
    --releases-file) RELEASES_FILE="$2" ;;
    --output) OUTPUT_FILE="$2" ;;
    --summary) SUMMARY_FILE="$2" ;;
    esac
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage_error "unknown argument: $1"
    ;;
  esac
done

# The output sink is opened once so an invoked workflow never inherits stale
# keys from an earlier step. --output wins over $GITHUB_OUTPUT; with neither,
# print to stdout for a human or a test.
OUTPUT_TO_STDOUT=0
if [[ -z "$OUTPUT_FILE" ]]; then
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    OUTPUT_FILE="$GITHUB_OUTPUT"
  else
    OUTPUT_TO_STDOUT=1
  fi
fi
if ((OUTPUT_TO_STDOUT == 0)); then
  : >"$OUTPUT_FILE"
fi

emit() { # <key> <value>
  local key="$1" value="$2"
  if ((OUTPUT_TO_STDOUT)); then
    printf '%s=%s\n' "$key" "$value"
  else
    printf '%s=%s\n' "$key" "$value" >>"$OUTPUT_FILE"
  fi
}

summarize() { # <markdown line...>
  if [[ -n "$SUMMARY_FILE" ]]; then
    printf '%s\n' "$@" >>"$SUMMARY_FILE"
  fi
}

# A no-op is green: it records the reason for the step summary and writes the
# outputs a downstream job gates on. It never creates a tag or a release.
noop() { # <reason>
  local reason="$1"
  emit run false
  emit reason "$reason"
  emit cadence_date "$CADENCE_DATE"
  emit tag "$PREVIEW_TAG"
  emit base_version "$BASE_VERSION"
  emit sha "$COMMIT"
  summarize "### Preview skipped" "" "$reason" ""
  printf 'preview-preflight: no-op: %s\n' "$reason"
  exit 0
}

[[ -n "$NOW" ]] || NOW="$(TZ="$PREVIEW_TZ" date '+%Y-%m-%d %H:%M')"
if [[ ! "$NOW" =~ ^[0-9]{4}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])\ ([01][0-9]|2[0-3]):[0-5][0-9]$ ]]; then
  usage_error "--now must look like 'YYYY-MM-DD HH:MM' in local time, got: $NOW"
fi
CADENCE_DATE="${NOW:0:4}${NOW:5:2}${NOW:8:2}"
LOCAL_HM="${NOW:11:5}"

if [[ -z "$BASE_VERSION" ]]; then
  BASE_VERSION="$(bash "$REPO_ROOT/scripts/check-release-version.sh" --print)" ||
    die "could not read the workspace version"
  [[ "$BASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "workspace version in Cargo.toml is not X.Y.Z: $BASE_VERSION"
else
  [[ "$BASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    usage_error "--base-version must look like X.Y.Z, got: $BASE_VERSION"
fi

if [[ -z "$COMMIT" ]]; then
  COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"
fi
[[ "$COMMIT" =~ ^[0-9a-f]{7,64}$ ]] ||
  die "--commit must be a git object id, got: $COMMIT"

# Tag grammar guard before any output is emitted (contract: the tag is
# vX.Y.Z-preview.YYYYMMDD, and the date is the cadence date, never another).
PREVIEW_TAG="v${BASE_VERSION}-preview.${CADENCE_DATE}"
[[ "$PREVIEW_TAG" =~ $PREVIEW_TAG_RE ]] ||
  die "internal error: computed tag does not match the preview grammar: $PREVIEW_TAG"

# P-02: the lateness window is local wall clock. String comparison is safe
# because both sides are fixed-width zero-padded HH:MM.
if [[ "$LOCAL_HM" < "$PREVIEW_WINDOW_START" || "$LOCAL_HM" > "$PREVIEW_WINDOW_END" ]]; then
  noop "off-slot run at ${LOCAL_HM} ${PREVIEW_TZ}; the accepted window is ${PREVIEW_WINDOW_START}-${PREVIEW_WINDOW_END} local"
fi

# Resolve the remote tag list exactly once. The workflow checks out main and
# passes the peeled list, so a tag created while the run is in flight cannot
# change this decision.
TAG_LINES=""
if [[ -n "$TAGS_FILE" ]]; then
  [[ -f "$TAGS_FILE" ]] || die "tags file not found: $TAGS_FILE"
  TAG_LINES="$(cat "$TAGS_FILE")"
else
  TAG_LINES="$(git -C "$REPO_ROOT" ls-remote --tags origin 'refs/tags/v*')" ||
    die "could not list remote tags"
fi

declare -A TAG_COMMIT=()
declare -A STABLE_TAG_FOR_VERSION=()
NEWEST_PREVIEW_TAG=""
NEWEST_PREVIEW_DATE=""
NEWEST_PREVIEW_CORE=()
EXISTING_PREVIEW_FOR_DATE=""

while IFS= read -r line; do
  [[ -n "$line" ]] || continue
  [[ "$line" =~ ^([0-9a-fA-F]{7,64})[[:space:]]+(refs/tags/[^[:space:]]+)$ ]] ||
    die "unrecognized tags-file line (want '<sha> refs/tags/<tag>'): $line"
  sha="${BASH_REMATCH[1]}"
  ref="${BASH_REMATCH[2]}"
  name="${ref#refs/tags/}"
  peeled=0
  if [[ "$name" == *'^{}' ]]; then
    name="${name%'^{}'}"
    peeled=1
  fi

  # An annotated tag has both the tag object and the peeled commit; keep the
  # commit (the peeled entry can arrive after the tag entry).
  if [[ -z "${TAG_COMMIT[$name]:-}" || "$peeled" -eq 1 ]]; then
    TAG_COMMIT["$name"]="$sha"
  fi

  if [[ "$name" =~ $STABLE_TAG_RE ]]; then
    version="$((10#${BASH_REMATCH[1]})).$((10#${BASH_REMATCH[2]})).$((10#${BASH_REMATCH[3]}))"
    existing="${STABLE_TAG_FOR_VERSION[$version]:-}"
    if [[ -n "$existing" && "$existing" != "$name" ]]; then
      die "ambiguous stable tags for version $version: '$existing' and '$name'"
    fi
    STABLE_TAG_FOR_VERSION["$version"]="$name"
  fi

  if [[ "$name" =~ $PREVIEW_TAG_RE ]]; then
    date="${BASH_REMATCH[4]}"
    if [[ "$date" == "$CADENCE_DATE" ]]; then
      EXISTING_PREVIEW_FOR_DATE="$name"
    fi
    pmaj="$((10#${BASH_REMATCH[1]}))"
    pmin="$((10#${BASH_REMATCH[2]}))"
    ppat="$((10#${BASH_REMATCH[3]}))"
    replace=0
    if [[ -z "$NEWEST_PREVIEW_TAG" ]]; then
      replace=1
    elif [[ "$date" > "$NEWEST_PREVIEW_DATE" ]]; then
      replace=1
    elif [[ "$date" == "$NEWEST_PREVIEW_DATE" ]]; then
      if ((pmaj > NEWEST_PREVIEW_CORE[0] || (\
        pmaj == NEWEST_PREVIEW_CORE[0] && (pmin > NEWEST_PREVIEW_CORE[1] || (\
        pmin == NEWEST_PREVIEW_CORE[1] && ppat > NEWEST_PREVIEW_CORE[2]))))); then
        replace=1
      fi
    fi
    if ((replace)); then
      NEWEST_PREVIEW_TAG="$name"
      NEWEST_PREVIEW_DATE="$date"
      NEWEST_PREVIEW_CORE=("$pmaj" "$pmin" "$ppat")
    fi
  fi
done <<<"$TAG_LINES"

# P-11: the preview base must be strictly greater than the newest stable tag.
# A missing or ambiguous bound is not a no-op: it is an input error because the
# run cannot prove the ordering the contract requires.
if ((${#STABLE_TAG_FOR_VERSION[@]} == 0)); then
  die "no exact stable tag (vX.Y.Z) found; refusing to guess the newest stable bound"
fi

BEST_VERSION=""
for version in "${!STABLE_TAG_FOR_VERSION[@]}"; do
  if [[ -z "$BEST_VERSION" ]]; then
    BEST_VERSION="$version"
    continue
  fi
  IFS=. read -r maj min pat <<<"$version"
  IFS=. read -r bmaj bmin bpat <<<"$BEST_VERSION"
  if ((maj > bmaj || (maj == bmaj && (min > bmin || (min == bmin && pat > bpat))))); then
    BEST_VERSION="$version"
  fi
done

IFS=. read -r base_maj base_min base_pat <<<"$BASE_VERSION"
base_maj=$((10#$base_maj))
base_min=$((10#$base_min))
base_pat=$((10#$base_pat))
IFS=. read -r best_maj best_min best_pat <<<"$BEST_VERSION"
best_maj=$((10#$best_maj))
best_min=$((10#$best_min))
best_pat=$((10#$best_pat))
if ((base_maj < best_maj || (\
  base_maj == best_maj && (base_min < best_min || (\
  base_min == best_min && base_pat <= best_pat))))); then
  noop "base version ${BASE_VERSION} is not strictly greater than the newest stable tag ${STABLE_TAG_FOR_VERSION[$BEST_VERSION]}"
fi

# P-08: one preview per cadence date, independent of the base version. A bump
# that lands after a preview has already published must not admit a second
# preview on the same local date; the exact tag is the common case, but any
# preview tag carrying this date is the marker. A tag for another date never
# blocks and is only consulted for the same-commit no-op.
if [[ -n "$EXISTING_PREVIEW_FOR_DATE" ]]; then
  noop "preview tag ${EXISTING_PREVIEW_FOR_DATE} already exists for cadence date ${CADENCE_DATE}; a cadence date is never reused or moved"
fi

# Prefer the most recent successful preview release over a tag alone: a run
# that created a tag but failed before publishing leaves a tag marker that was
# never a successful preview. The caller supplies release state in production;
# the tag fallback keeps the script usable without API access.
MARKER_TAG=""
if [[ -n "$RELEASES_FILE" ]]; then
  [[ -f "$RELEASES_FILE" ]] || die "releases file not found: $RELEASES_FILE"
  while IFS= read -r tag; do
    [[ "$tag" =~ $PREVIEW_TAG_RE ]] || continue
    MARKER_TAG="$tag"
  done <"$RELEASES_FILE"
else
  MARKER_TAG="$NEWEST_PREVIEW_TAG"
fi

if [[ -n "$MARKER_TAG" ]]; then
  marker_commit="${TAG_COMMIT[$MARKER_TAG]:-}"
  [[ -n "$marker_commit" ]] ||
    die "cannot resolve the commit for the preview marker $MARKER_TAG"
  if [[ "$marker_commit" == "$COMMIT" ]]; then
    noop "commit ${COMMIT} was already previewed as ${MARKER_TAG}; main has not advanced"
  fi
fi

emit run true
emit reason ""
emit cadence_date "$CADENCE_DATE"
emit tag "$PREVIEW_TAG"
emit base_version "$BASE_VERSION"
emit sha "$COMMIT"
summarize "### Preview cadence slot" "" \
  "- Tag: \`${PREVIEW_TAG}\`" \
  "- Cadence date: \`${CADENCE_DATE}\` (${PREVIEW_TZ})" \
  "- Base version: \`${BASE_VERSION}\` (newest stable: \`${STABLE_TAG_FOR_VERSION[$BEST_VERSION]}\`)" \
  "- Commit: \`${COMMIT}\`" \
  ""
printf 'preview-preflight: run=true tag=%s commit=%s\n' "$PREVIEW_TAG" "$COMMIT"
