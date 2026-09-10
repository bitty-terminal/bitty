#!/usr/bin/env bash
# check-scratch-paths.sh — CTX-0280 repo lint.
#
# Fails on *new* stale scratch-path references. The workspace convention is
# the singular `recording/` directory for durable evidence (umbrella
# `recording/references/`, never `/tmp`), but the tree still carries
# intentional legacy spellings that must keep working.
#
# Rule 1 — no NEW `recordings/` (plural) refs (whole tree, text files):
#   Every `recordings/` line must match the explicit allowlist below.
#   Allowed today (each reviewed per-hit in CTX-0280, none may grow silently):
#     - `crates/bitty-compat-lab/src/compare.rs`: intentional legacy-plural
#       fallback candidates (CTX-0206 design; singular is probed first and
#       ordering tests pin singular-before-legacy).
#     - `.gitignore`: functional ignores of real project-local paths
#       (`recordings/manual-smoke/`, `recordings/references/bitty/`).
#     - `recordings/README.md`: self-description of the project-local dir,
#       which is really named `recordings/` (kept per CTX-0280 exception b).
#     - lines containing `recordings/compat-matrix-2026-09-01.json`: the kept
#       v1 artifact itself plus its v1 follow-up references (separate task
#       owns the rename, if any).
#     - lines containing `recordings/manual-smoke/`: project-local
#       git-ignored windowed-capture scratch (functional `mkdir`/`grim`
#       paths, never committed).
#     - lines containing `recordings/README.md`: pointer at the real local
#       file (e.g. compat-matrix CTX-0114 note).
#
# Rule 2 — no NEW hardcoded `/tmp/` evidence writes (code + scripts only):
#   Scope is `crates/*/src/**.rs`, `scripts/*`, `tools/**`. Integration
#   tests (`crates/*/tests/**`), fixtures, and docs examples are out of scope
#   (reviewed-legit: socket dirs, `temp_dir()` fixtures, manual commands).
#   Comment-only lines never count (comments cannot write files; this also
#   covers `///`/`//!` doc examples such as doctest `/tmp/x` fixtures).
#   A hit is allowed when the line (or one of the 3 lines above it) carries
#   `scratch-paths-exempt: <reason>`, or the line mentions `temp_dir`,
#   `mktemp`, or `sock` (socket paths), or — for `.rs` files — the hit sits
#   at/after the file's first `#[cfg(test)]` line (unit-test-only region).
#   The `/tmp/` match requires the slash NOT to follow `[A-Za-z0-9_/-]`.
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
#   This script exempts itself from all rules
#   (it must spell the forbidden patterns to define them).
#
# Escape hatch (auditable, grep-able, mirrors pty-gate):
#   - `// scratch-paths-exempt: <reason>` (or `# ...` in shell) on the hit
#     line or one of the 3 lines above it exempts that hit.
#
# Limitations (documented, cheap gate):
#   - Rule 1 scans file contents, not paths: a brand-new `recordings/`
#     directory with no `recordings/` string inside passes (reviewers must
#     still route new durable evidence to `recording/`).
#   - Rule 2 does not scan `docs/` or `crates/*/tests/`: a new doc telling
#     humans to dump evidence to `/tmp/` passes (docs are instructions, and
#     current manual commands such as `grim /tmp/*.png` stay legit).
#   - The `#[cfg(test)]`-region rule uses the FIRST `#[cfg(test)]` line per
#     file; prod code placed after a trailing test module would be wrongly
#     exempt (no such layout exists today; keep test modules trailing).
#     Extracted unit-test modules named `tests.rs` under `src/` are treated
#     as test-only regions by name, because their `#[cfg(test)]` lives on the
#     parent's `mod tests;` declaration (CTX-0304).
#   - Rule 3 does not scan `docs/`, `crates/*/tests/`, or fixtures: docs
#     record historical machine paths and `.bin` capture corpora embed
#     whatever host produced them (sanitizing fixtures is a separate task).
set -euo pipefail

cd "$(dirname "$0")/.."

FAIL=0

# --- Rule 1: recordings/ (plural) allowlist ---
SELF=scripts/check-scratch-paths.sh
while IFS= read -r hit; do
	file="${hit%%:*}"
	file="${file#./}"
	rest="${hit#*:}"
	line="${rest%%:*}"
	text="${rest#*:}"
	case "$file" in
	crates/bitty-compat-lab/src/compare.rs | .gitignore | recordings/README.md)
		continue
		;;
	esac
	case "$text" in
	*recordings/compat-matrix-2026-09-01.json* | *recordings/manual-smoke/* | *recordings/README.md* | *scratch-paths-exempt:*)
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
	rg -n --no-heading -g '!target/**' -g '!.git/**' -g '!.worktrees/**' -g '!*.bin' -g '!scripts/check-scratch-paths.sh' 'recordings/' . 2>/dev/null || true
)

# --- Rule 2: hardcoded /tmp/ writes in code + scripts ---
mapfile -d '' SRC_FILES < <(
	find crates scripts tools -type f \
		\( -path 'crates/*/src/*.rs' -o -path 'crates/*/src/**/*.rs' \
		-o -path 'scripts/*' -o -path 'tools/*' \) -print0 2>/dev/null | sort -z
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
		rg -n -P --no-heading '(?<![A-Za-z0-9_/\-])/tmp/' "$file" 2>/dev/null || true
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

if ((FAIL)); then
	echo "scratch-paths: FAIL — new stale scratch refs; fix to recording/ or extend the explicit allowlist with a reason" >&2
	exit 1
fi
echo "scratch-paths: OK"
