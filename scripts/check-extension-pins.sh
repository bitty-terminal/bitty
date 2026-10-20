#!/usr/bin/env bash
# check-extension-pins.sh — extracted-extension pin drift gate (CTX-1363, #2255).
#
# Usage: scripts/check-extension-pins.sh [--root <dir>] [--strict] [--require-network]
#   --root <dir>        scan <dir>/crates/*/Cargo.toml instead of this checkout.
#   --strict            treat over-threshold drift as a failure (default: warn).
#   --require-network   treat an unreachable upstream as a failure (default: skip).
#
# What it checks: every direct git dependency in `crates/*/Cargo.toml` whose
# URL is under https://github.com/bitty-terminal/bitty-* — the extracted
# extensions Core pins by exact revision. `phodopus` is a library dependency,
# not an extracted extension, and stays out of this policy.
#
# Declared policy:
#   FAIL  a pinned revision is not reachable from upstream `main` (force-push,
#         rewritten history, typo, or a pin to a non-main branch).
#   WARN  a pin is reachable but more than PIN_DRIFT_WARN commits behind
#         upstream `main` (default 10). `--strict` turns this into a failure.
#   SKIP  the upstream repository cannot be reached (offline or a transient
#         network failure); the gate never fails on missing network by default.
#   FAIL  a malformed `rev` (not 7-40 hex characters) — the workspace pin
#         convention is an exact abbreviated SHA, never a branch or tag.
#
# The upstream `main` history is mirrored into a local bare cache
# (BITTY_PIN_CACHE, default ${XDG_CACHE_HOME:-$HOME/.cache}/bitty/extension-pins)
# so repeat runs are cheap. BITTY_PIN_GIT_BASE rewrites the GitHub base URL for
# fixture tests and offline mirrors; production never sets it.
#
# Exit codes: 0 clean, drifted-within-policy, or skipped; 1 policy failure.
set -euo pipefail

readonly default_drift_warn=10
readonly github_base="https://github.com/bitty-terminal/"
readonly pin_url_prefix="${github_base}bitty-"

root=""
strict=0
require_network=0
while [ $# -gt 0 ]; do
  case "$1" in
  --root)
    [ $# -ge 2 ] || {
      echo "check-extension-pins: --root needs a directory" >&2
      exit 2
    }
    root="$2"
    shift 2
    ;;
  --strict)
    strict=1
    shift
    ;;
  --require-network)
    require_network=1
    shift
    ;;
  -h | --help)
    sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "check-extension-pins: unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

if [ -z "$root" ]; then
  root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi
if [ ! -d "$root/crates" ]; then
  echo "check-extension-pins: no crates/ directory under $root" >&2
  exit 2
fi

drift_warn="${PIN_DRIFT_WARN:-$default_drift_warn}"
case "$drift_warn" in
'' | *[!0-9]*)
  echo "check-extension-pins: PIN_DRIFT_WARN must be a non-negative integer" >&2
  exit 2
  ;;
esac

cache="${BITTY_PIN_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/bitty/extension-pins}"
git_base="${BITTY_PIN_GIT_BASE:-$github_base}"

# `note` emits a GitHub annotation in Actions and plain text elsewhere.
note() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    printf '::%s title=Extension pin drift::%s\n' "$1" "$2"
  else
    printf '%s\n' "$2" >&2
  fi
}

# Collect unique (url, rev) pairs from the single-line git dependency entries.
# Both fields are extracted independently, so `rev` before `git` works too;
# a line without a 7-40 hex `rev` is left to the coverage guard below.
pairs="$(
  awk -v base="$pin_url_prefix" '
    /^[[:space:]]*#/ { next }
    {
      line = $0
      url = line
      sub(/.*git = "/, "", url)
      sub(/".*/, "", url)
      rev = line
      sub(/.*rev = "/, "", rev)
      sub(/".*/, "", rev)
      if (index(url, base) == 1 && rev ~ /^[0-9a-fA-F]{7,40}$/) print url "|" rev
    }
  ' "$root"/crates/*/Cargo.toml | sort -u
)"

# Guard: every extracted-extension git dependency must carry an inline exact
# rev. A multi-line entry, a branch, or a tag would otherwise slip past the
# table above and hide drift.
unparsed="$(
  grep -hE "^[^#]*git = \"${pin_url_prefix}" "$root"/crates/*/Cargo.toml |
    grep -vcE 'rev = "[0-9a-fA-F]{7,40}"' || true
)"
if [ "$unparsed" -gt 0 ]; then
  note error "$unparsed extracted-extension git dependency line(s) lack an inline exact 7-40 hex rev"
  exit 1
fi

if [ -z "$pairs" ]; then
  echo "check-extension-pins: no extracted-extension git pins found under $root/crates"
  exit 0
fi

checked=0
unreachable=0
behind_over=0
skipped=0

printf '%-28s %-10s %-10s %8s  %s\n' "repository" "pin" "upstream" "behind" "status"
while IFS='|' read -r url rev; do
  repo="${url##*/}"
  repo="${repo%.git}"
  repo_dir="$cache/$repo.git"
  fetch_url="$url"
  case "$url" in
  "$github_base"*) fetch_url="$git_base${url#"$github_base"}" ;;
  esac

  # The workspace pin convention is an exact abbreviated SHA (7-40 hex),
  # never a branch or tag: a name cannot be drift-checked and hides upgrades.
  case "$rev" in
  *[!0-9a-fA-F]*)
    printf '%-28s %-10s %-10s %8s  %s\n' "$repo" "$rev" "-" "-" "INVALID REV"
    note error "$repo: rev '$rev' is not an exact hex revision"
    unreachable=$((unreachable + 1))
    checked=$((checked + 1))
    continue
    ;;
  esac
  if [ "${#rev}" -lt 7 ] || [ "${#rev}" -gt 40 ]; then
    printf '%-28s %-10s %-10s %8s  %s\n' "$repo" "$rev" "-" "-" "INVALID REV"
    note error "$repo: rev '$rev' is not a 7-40 character abbreviated SHA"
    unreachable=$((unreachable + 1))
    checked=$((checked + 1))
    continue
  fi

  mkdir -p "$cache"
  offline=0
  if [ ! -d "$repo_dir" ]; then
    if ! git clone --quiet --bare "$fetch_url" "$repo_dir" 2>/dev/null; then
      offline=1
    fi
  fi
  if [ "$offline" -eq 0 ] && ! git -C "$repo_dir" fetch --quiet origin +refs/heads/main:refs/remotes/origin/main 2>/dev/null; then
    offline=1
  fi
  if [ "$offline" -eq 0 ] && ! git -C "$repo_dir" rev-parse --verify --quiet origin/main >/dev/null; then
    offline=1
  fi

  checked=$((checked + 1))
  if [ "$offline" -ne 0 ]; then
    skipped=$((skipped + 1))
    printf '%-28s %-10s %-10s %8s  %s\n' "$repo" "$rev" "-" "-" "SKIP (offline)"
    note warning "$repo: cannot reach $fetch_url; drift not evaluated"
    continue
  fi

  head="$(git -C "$repo_dir" rev-parse --short origin/main)"
  if ! git -C "$repo_dir" cat-file -e "$rev^{commit}" 2>/dev/null ||
    ! git -C "$repo_dir" merge-base --is-ancestor "$rev" origin/main 2>/dev/null; then
    unreachable=$((unreachable + 1))
    printf '%-28s %-10s %-10s %8s  %s\n' "$repo" "$rev" "$head" "-" "NOT REACHABLE"
    note error "$repo: pin $rev is not reachable from upstream main ($head)"
    continue
  fi

  distance="$(git -C "$repo_dir" rev-list --count "$rev..origin/main")"
  if [ "$distance" -gt "$drift_warn" ]; then
    behind_over=$((behind_over + 1))
    printf '%-28s %-10s %-10s %8d  %s\n' "$repo" "$rev" "$head" "$distance" "DRIFT"
    note warning "$repo: pinned $rev is $distance commits behind upstream main ($head); threshold $drift_warn"
  else
    printf '%-28s %-10s %-10s %8d  %s\n' "$repo" "$rev" "$head" "$distance" "ok"
  fi
done <<<"$pairs"

printf 'pins: %d checked, %d unreachable, %d over threshold (%d), %d skipped\n' \
  "$checked" "$unreachable" "$behind_over" "$drift_warn" "$skipped"

if [ "$unreachable" -gt 0 ]; then
  exit 1
fi
if [ "$require_network" -eq 1 ] && [ "$skipped" -gt 0 ]; then
  echo "check-extension-pins: --require-network set and $skipped repository/repositories skipped" >&2
  exit 1
fi
if [ "$strict" -eq 1 ] && [ "$behind_over" -gt 0 ]; then
  echo "check-extension-pins: --strict set and $behind_over pin(s) exceed the drift threshold" >&2
  exit 1
fi
exit 0
