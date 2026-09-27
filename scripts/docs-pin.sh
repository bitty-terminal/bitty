#!/usr/bin/env bash
# docs-pin.sh — CTX-0810: move the `docs/` submodule pin without a checkout.
#
# Usage: scripts/docs-pin.sh [<rev>]        (default <rev>: origin/main)
#
# In the Bitty workspace the `docs/` mount stays uninitialized
# (`submodule.docs.update=none`) so agents edit the workspace
# `bitty-terminal-docs` checkout, never a pinned copy. This helper resolves
# <rev> in that checkout and writes the gitlink straight into the index with
# `git update-index --cacheinfo`; the mount is never populated.
#
# The docs checkout is `$BITTY_TERMINAL_DOCS` when set, otherwise
# `<workspace>/<repo>` where <repo> is the basename of the `.gitmodules` URL
# and <workspace> is `$BITTY_WORKSPACE` or, when unset, the parent of the
# primary checkout (derived from the common git dir, so task worktrees work).
#
# Refuses (exit 1, index untouched) when:
#   - the docs checkout is missing or its `origin` is not the `.gitmodules` URL;
#   - <rev> does not resolve to a commit there;
#   - the commit is not reachable from that checkout's `origin/main` (this
#     script never fetches: run `git -C <docs> fetch origin` first);
#   - the index entry for `docs` is not a gitlink, or the mount is populated
#     (a populated mount would re-stage its own HEAD on the next `git add`).
#
# The new pin is staged only; commit it under the owning task.
set -euo pipefail

readonly MOUNT=docs
readonly MAINLINE=origin/main
readonly GITLINK_MODE=160000
readonly SHORT=12

die() {
	printf 'docs-pin: %s\n' "$*" >&2
	exit 1
}

# Reduce https/ssh/scp-style remote URLs to a comparable host/owner/repo form.
normalize_url() {
	local u="${1%/}"
	u="${u%.git}"
	u="${u#*://}"
	u="${u#*@}"
	u="${u/://}"
	printf '%s' "${u,,}"
}

(($# <= 1)) || die "usage: scripts/docs-pin.sh [<rev>]"
rev="${1:-$MAINLINE}"

root="$(git rev-parse --show-toplevel)"
cd "$root"
common="$(git rev-parse --path-format=absolute --git-common-dir)"
workspace="${BITTY_WORKSPACE:-$(dirname "$(dirname "$common")")}"

url="$(git config -f .gitmodules --get "submodule.$MOUNT.url")" ||
	die ".gitmodules has no submodule.$MOUNT.url"
docs="${BITTY_TERMINAL_DOCS:-$workspace/$(basename "${url%.git}")}"

[[ -d "$docs" ]] || die "no docs checkout at $docs (set BITTY_TERMINAL_DOCS)"
docs="$(cd "$docs" && pwd -P)"
docs_top="$(git -C "$docs" rev-parse --show-toplevel 2>/dev/null)" ||
	die "$docs is not a Git checkout"
[[ "$(cd "$docs_top" && pwd -P)" == "$docs" ]] ||
	die "$docs is not the top of a Git checkout"

origin_url="$(git -C "$docs" remote get-url origin 2>/dev/null)" ||
	die "$docs has no origin remote"
[[ "$(normalize_url "$origin_url")" == "$(normalize_url "$url")" ]] ||
	die "$docs origin ($origin_url) is not the .gitmodules URL ($url)"

sha="$(git -C "$docs" rev-parse --verify --quiet "$rev^{commit}")" ||
	die "cannot resolve '$rev' to a commit in $docs"
git -C "$docs" rev-parse --verify --quiet "$MAINLINE^{commit}" >/dev/null ||
	die "$docs has no $MAINLINE (run: git -C $docs fetch origin)"
git -C "$docs" merge-base --is-ancestor "$sha" "$MAINLINE" ||
	die "${sha:0:SHORT} is not reachable from $MAINLINE in $docs (fetch first, or pin a merged commit)"

entry="$(git ls-files --stage -- "$MOUNT")"
[[ "$entry" == "$GITLINK_MODE "* ]] || die "index entry for $MOUNT is not a gitlink"
[[ ! -e "$MOUNT/.git" ]] ||
	die "$MOUNT/ is populated; deinit it first (git submodule deinit -f $MOUNT)"
old="$(cut -d' ' -f2 <<<"$entry")"

if [[ "$old" == "$sha" ]]; then
	printf 'docs-pin: %s already at %s\n' "$MOUNT" "${sha:0:SHORT}"
	exit 0
fi

git update-index --cacheinfo "$GITLINK_MODE,$sha,$MOUNT"
note=""
if ! git -C "$docs" merge-base --is-ancestor "$old" "$sha" 2>/dev/null; then
	note=" (not a fast-forward of the previous pin)"
fi
printf 'docs-pin: %s %s -> %s staged%s\n' "$MOUNT" "${old:0:SHORT}" "${sha:0:SHORT}" "$note"
