#!/usr/bin/env bash
# check-scratch-paths.sh — CTX-0280 repo lint (CTX-0379: docs/metadata coverage).
#
# Usage: scripts/check-scratch-paths.sh [--root <dir>]
#   --root scans <dir> instead of the repository root. Used by the fixture
#   test `scripts/tests/check-scratch-paths.test.sh`.
#
# Fails on *new* stale scratch-path references. The workspace convention is
# the singular `recording/` directory for durable evidence (umbrella
# `recording/references/`, never `/tmp`), but the tree still carries
# intentional legacy spellings that must keep working.
#
# Rule 1 — no NEW `recordings/` (plural) refs (whole tree, text files):
#   Every `recordings/` line must match the explicit allowlist below.
#   Allowed today (each reviewed per-hit, none may grow silently):
#     - `crates/bitty-compat-lab/src/compare.rs`: intentional legacy-plural
#       fallback candidates (CTX-0206 design; singular is probed first and
#       ordering tests pin singular-before-legacy).
#     - lines containing `recordings/compat-matrix-2026-09-01.json`:
#       historical revision-history/CHANGELOG entries that name the v1
#       artifact as it existed then (the artifact itself is generated into
#       the git-ignored workspace `recording/` since CTX-0379).
#
# Rule 2 — no NEW hardcoded `/tmp/` or `/var/tmp/` evidence writes
# (code + scripts only):
#   Scope is `crates/*/src/**.rs`, `scripts/*`, `tools/**`. Integration
#   tests (`crates/*/tests/**`), fixtures, and docs examples are out of scope
#   (reviewed-legit: socket dirs, `temp_dir()` fixtures, manual commands).
#   Comment-only lines never count (comments cannot write files; this also
#   covers `///`/`//!` doc examples such as doctest `/tmp/x` fixtures).
#   A hit is allowed when the line (or one of the 3 lines above it) carries
#   `scratch-paths-exempt: <reason>`, or the line mentions `temp_dir`,
#   `mktemp`, or `sock` (socket paths), or — for `.rs` files — the hit sits
#   at/after the file's first `#[cfg(test)]` line (unit-test-only region).
#   The rule covers `/var/tmp/` too (the alternate system temp dir; the
#   inner `/tmp/` of `/var/tmp/` is lookbehind-excluded, so it needs its
#   own alternative). The match requires the leading slash NOT to follow `[A-Za-z0-9_/-]`.
#
# Rule 3 — no NEW host-absolute path literals in production code (CTX-0296):
#   Same file scope and exemptions as Rule 2. Fails on `/mnt/`,
#   `/home/<user>`, `/Users/<user>`, and Windows user paths (`C:\Users\...`
#   in raw and escaped spellings), which are machine-specific and must come
#   from config, parameters, or environment instead. System paths (`/usr`,
#   `/bin`, `/proc`, `/dev`, `/run/user/<uid>`, `/sys`, `C:\Windows`,
#   `C:\Program Files`) are deliberately out of scope: they are portable
#   contract paths, not host leftovers. Comment-only lines and unit-test
#   regions never count; `scratch-paths-exempt: <reason>` applies.
#
# Rule 4 — no NEW host-absolute paths in tracked repo-owned docs/metadata
# (CTX-0379; docs submodule scope CTX-0412):
#   Scans every repo-owned non-code, non-test file (root `*.md`, `.github/**`,
#   `justfile`, dotfiles, packaging metadata) for the same `/mnt/`,
#   `/home/<user>`, `/Users/<user>`, and `C:\Users` patterns. Docs are
#   user-follow instructions, so a host path there is drift even when the
#   file is prose; `scratch-paths-exempt: <reason>` on the hit line (or one of
#   the 3 lines above) is the auditable escape hatch.
#
#   The `docs/` Git submodule (bitty-terminal-docs) is external content owned
#   and linted by its own repository, so Rules 1 and 4 skip it. Scanning a
#   checked-out submodule here would fail on upstream examples the parent repo
#   cannot fix and would make the gate depend on whether `git submodule
#   update --init` ran.
#
#   This script and its fixture tree `scripts/tests/fixtures/scratch-paths/`
#   are exempt from all rules (they must spell the forbidden patterns to
#   define and test them).
#
# Escape hatch (auditable, grep-able, mirrors pty-gate):
#   - `// scratch-paths-exempt: <reason>` (or `# ...` in shell) on the hit
#     line or one of the 3 lines above it exempts that hit.
#
# Limitations (documented, cheap gate):
#   - Rule 1 scans file contents, not paths: a brand-new `recordings/`
#     directory with no `recordings/` string inside passes (reviewers must
#     still route new durable evidence to `recording/`).
#   - Rules 2/3/4 do not scan `crates/*/tests/**` or `tests/**` fixtures:
#     integration tests and `.bin` capture corpora embed neutral placeholders
#     (`/home/user`) or host bytes by design.
#   - The `#[cfg(test)]`-region rule uses the FIRST `#[cfg(test)]` line per
#     file; prod code placed after a trailing test module would be wrongly
#     exempt (no such layout exists today; keep test modules trailing).
#     Extracted unit-test modules named `tests.rs` under `src/` are treated
#     as test-only regions by name, because their `#[cfg(test)]` lives on the
#     parent's `mod tests;` declaration (CTX-0304).
set -euo pipefail

ROOT=""
while (($# > 0)); do
	case "$1" in
	--root)
		ROOT="${2:?--root requires a directory}"
		shift 2
		;;
	-h | --help)
		echo "usage: $0 [--root <dir>]"
		exit 0
		;;
	*)
		echo "usage: $0 [--root <dir>]" >&2
		exit 2
		;;
	esac
done

if [[ -n "$ROOT" ]]; then
	cd "$ROOT"
else
	cd "$(dirname "$0")/.."
fi

FAIL=0
SELF=scripts/check-scratch-paths.sh

# --- Rule 1: recordings/ (plural) allowlist ---
while IFS= read -r hit; do
	file="${hit%%:*}"
	file="${file#./}"
	rest="${hit#*:}"
	line="${rest%%:*}"
	text="${rest#*:}"
	case "$file" in
	crates/bitty-compat-lab/src/compare.rs)
		continue
		;;
	esac
	case "$text" in
	*recordings/compat-matrix-2026-09-01.json* | *scratch-paths-exempt:*)
		continue
		;;
	esac
	if ((line > 1)); then
		from=$((line > 3 ? line - 3 : 1))
		if sed -n "${from},$((line - 1))p" "$file" | rg -q 'scratch-paths-exempt:' 2>/dev/null; then
			continue
		fi
	fi
	echo "scratch-paths[recordings]: $hit"
	FAIL=1
done < <(
	rg -n --no-heading -g '!target/**' -g '!.git' -g '!.git/**' -g '!.worktrees/**' -g '!*.bin' -g '!docs/**' \
		-g '!scripts/check-scratch-paths.sh' -g '!scripts/tests/fixtures/**' 'recordings/' . 2>/dev/null || true
)

# --- Rule 2: hardcoded /tmp/ writes in code + scripts ---
mapfile -d '' SRC_FILES < <(
	find crates scripts tools -type f \
		\( -path 'crates/*/src/*.rs' -o -path 'crates/*/src/**/*.rs' \
		-o -path 'scripts/*' -o -path 'tools/*' \) \
		! -path 'scripts/tests/fixtures/*' -print0 2>/dev/null | sort -z
)

for file in "${SRC_FILES[@]}"; do
	[[ "$file" == "$SELF" ]] && continue
	test_from=0
	if [[ "$file" == *.rs ]]; then
		if [[ "$(basename "$file")" == "tests.rs" ]]; then
			# Extracted unit-test module (CTX-0304): the `#[cfg(test)]`
			# attribute lives on the parent's `mod tests;` declaration, so
			# the whole file is a test-only region.
			test_from=1
		else
			test_from="$(rg -n --max-count 1 '^[[:space:]]*#\[cfg\(test\)\]' "$file" 2>/dev/null | cut -d: -f1 || true)"
			test_from="${test_from:-0}"
		fi
	fi
	while IFS= read -r hit; do
		lineno="${hit%%:*}"
		text="${hit#*:}"
		if [[ "$text" =~ ^[[:space:]]*(//|#) ]]; then
			continue
		fi
		case "$text" in
		*scratch-paths-exempt:* | *temp_dir* | *mktemp* | *sock*)
			continue
			;;
		esac
		if [[ "$file" == *.rs ]] && ((test_from > 0)) && ((lineno >= test_from)); then
			continue
		fi
		if ((lineno > 1)); then
			from=$((lineno > 3 ? lineno - 3 : 1))
			if sed -n "${from},$((lineno - 1))p" "$file" | rg -q 'scratch-paths-exempt:' 2>/dev/null; then
				continue
			fi
		fi
		echo "scratch-paths[tmp-write]: $file:$hit"
		FAIL=1
	done < <(
		rg -n -P --no-heading '(?<![A-Za-z0-9_/\-])(/tmp/|/var/tmp/)' "$file" 2>/dev/null || true
	)
done

# --- Rule 3: hardcoded host-absolute paths in production code (CTX-0296) ---
for file in "${SRC_FILES[@]}"; do
	[[ "$file" == "$SELF" ]] && continue
	test_from=0
	if [[ "$file" == *.rs ]]; then
		if [[ "$(basename "$file")" == "tests.rs" ]]; then
			# Extracted unit-test module (CTX-0304): the `#[cfg(test)]`
			# attribute lives on the parent's `mod tests;` declaration, so
			# the whole file is a test-only region.
			test_from=1
		else
			test_from="$(rg -n --max-count 1 '^[[:space:]]*#\[cfg\(test\)\]' "$file" 2>/dev/null | cut -d: -f1 || true)"
			test_from="${test_from:-0}"
		fi
	fi
	while IFS= read -r hit; do
		lineno="${hit%%:*}"
		text="${hit#*:}"
		if [[ "$text" =~ ^[[:space:]]*(//|#) ]]; then
			continue
		fi
		case "$text" in
		*scratch-paths-exempt:*)
			continue
			;;
		esac
		if [[ "$file" == *.rs ]] && ((test_from > 0)) && ((lineno >= test_from)); then
			continue
		fi
		if ((lineno > 1)); then
			from=$((lineno > 3 ? lineno - 3 : 1))
			if sed -n "${from},$((lineno - 1))p" "$file" | rg -q 'scratch-paths-exempt:' 2>/dev/null; then
				continue
			fi
		fi
		echo "scratch-paths[abs-path]: $file:$hit"
		FAIL=1
	done < <(
		rg -n -e '/mnt/' -e '/home/[A-Za-z0-9]' -e '/Users/[A-Za-z0-9]' \
			-e 'C:\\Users' -e 'C:\\\\Users' "$file" 2>/dev/null || true
	)
done

# --- Rule 4: hardcoded host-absolute paths in tracked docs/metadata (CTX-0379) ---
mapfile -d '' DOC_FILES < <(
	rg --files -0 --hidden \
		-g '!crates/**' -g '!tests/**' -g '!scripts/**' -g '!tools/**' -g '!docs/**' \
		-g '!target/**' -g '!.git' -g '!.git/**' -g '!.worktrees/**' -g '!*.bin' \
		. 2>/dev/null || true
)

for file in "${DOC_FILES[@]}"; do
	file="${file#./}"
	while IFS= read -r hit; do
		lineno="${hit%%:*}"
		text="${hit#*:}"
		case "$text" in
		*scratch-paths-exempt:*)
			continue
			;;
		esac
		if ((lineno > 1)); then
			from=$((lineno > 3 ? lineno - 3 : 1))
			if sed -n "${from},$((lineno - 1))p" "$file" | rg -q 'scratch-paths-exempt:' 2>/dev/null; then
				continue
			fi
		fi
		echo "scratch-paths[doc-abs-path]: $file:$hit"
		FAIL=1
	done < <(
		rg -n -e '/mnt/' -e '/home/[A-Za-z0-9]' -e '/Users/[A-Za-z0-9]' \
			-e 'C:\\Users' -e 'C:\\\\Users' "$file" 2>/dev/null || true
	)
done

if ((FAIL)); then
	echo "scratch-paths: FAIL — new stale scratch refs; fix to recording/ or extend the explicit allowlist with a reason" >&2
	exit 1
fi
echo "scratch-paths: OK"
