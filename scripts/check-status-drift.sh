#!/usr/bin/env bash
# check-status-drift.sh — CTX-0437 machine drift gate (research note 024 §4 + §17.2).
#
# Usage: scripts/check-status-drift.sh [--root <dir>]
#   --root scans <dir> instead of the repository root. Used by the fixture
#   test `scripts/tests/check-status-drift.test.sh`.
#
# Purpose: code-doc status contradictions must not re-accumulate. The docs
# corpus (bitty-docs decisions/open-questions plus owning RFC frontmatter) is
# canonical; in-repo `*.rs` / `*.md` claims that contradict it fail the gate.
#
# Expectations table (vendored, verified read-only against the canonical
# register before encoding; owning frontmatter re-read at gate time when the
# sibling checkout exists, otherwise the vendored value applies):
#   OQ-008 = accepted — bitty-docs open-questions: Accepted rich-presentation-rfc
#     (closed OQ-008 on 2026-08-28); owning frontmatter
#     bitty-terminal-docs/specifications/rich-presentation-rfc.md `status: accepted`.
#   OQ-011 = accepted — bitty-docs open-questions: Accepted plugin-platform-rfc
#     (Plugin API v1); owning frontmatter
#     bitty-plugins-docs/specifications/plugin-platform-rfc.md `status: accepted`.
#   OQ-012 = accepted — same register + owning RFC as OQ-011.
#   OQ-013 = accepted — same register + owning RFC as OQ-011
#     (DropOldest v1 default closed decision point).
#   OQ-014 = accepted — bitty-docs open-questions: Accepted isolation-resource-rfc
#     (closed OQ-014 on 2026-08-28); owning frontmatter
#     bitty-plugins-docs/specifications/isolation-resource-rfc.md `status: accepted`.
#   OQ-018 = accepted — bitty-docs open-questions: Accepted ipc-agent-rfc
#     (closed OQ-018 on 2026-08-29); owning frontmatter
#     bitty-ai-docs/specifications/ipc-agent-rfc.md `status: accepted`.
#   OQ-053 = accepted/closed — bitty-docs open-questions: Accepted
#     Bundled-Plugin Split Decision (closed OQ-053 on 2026-09-14).
#   REL-10 additions (vendored, verified read-only against the canonical
#   register plus owning `status: accepted` frontmatter before encoding):
#   OQ-007 = accepted — bitty-docs open-questions: Accepted terminal-state-rfc;
#     owning frontmatter
#     bitty-terminal-docs/specifications/terminal-state-rfc.md `status: accepted`.
#   OQ-009 = accepted — bitty-docs open-questions: Accepted lua-runtime-rfc
#     (closed OQ-009 on 2026-08-27); owning frontmatter
#     bitty-plugins-docs/runtime/lua-runtime-rfc.md `status: accepted`.
#     (OQ-030, OQ-031, OQ-032 are separate follow-ups outside this row.)
#   OQ-021 = accepted — bitty-docs open-questions: Accepted package-lifecycle-rfc;
#     owning frontmatter
#     bitty-plugins-docs/packaging/package-lifecycle-rfc.md `status: accepted`.
#   OQ-022 = accepted — bitty-docs open-questions: Accepted package-followup-rfc
#     (closed OQ-022 on 2026-08-28); owning frontmatter
#     bitty-plugins-docs/packaging/package-followup-rfc.md `status: accepted`.
#   OQ-023 = accepted — bitty-docs open-questions: Accepted website-delivery-rfc
#     (closed OQ-023 on 2026-08-29); owning frontmatter
#     bitty-terminal-docs/specifications/website-delivery-rfc.md `status: accepted`.
#   OQ-024 = accepted — bitty-docs open-questions: Accepted governance-rfc
#     (closed OQ-024 on 2026-08-29); owning frontmatter
#     bitty-terminal-docs/specifications/governance-rfc.md `status: accepted`.
#   OQ-025 = accepted — bitty-docs open-questions: Accepted risk-evidence-rfc
#     (closed OQ-025 on 2026-08-29); owning frontmatter
#     bitty-terminal-docs/specifications/risk-evidence-rfc.md `status: accepted`.
#   OQ-026 = accepted — same register + owning RFC as OQ-022
#     (closed OQ-026 on 2026-08-28).
#   OQ-027 = accepted — same register + owning RFC as OQ-022
#     (closed OQ-027 on 2026-08-28).
#   OQ-028 = accepted — same register + owning RFC as OQ-022
#     (closed OQ-028 on 2026-08-28).
#
# Rule 1 — OQ status claims (stale open language for an accepted/closed OQ):
#   Scan `*.rs` + in-repo `*.md` for lines mentioning a tabled OQ together
#   with stale language (`remain(s) open`, `unresolved`, `has/have not landed`,
#   `not landed`, `not yet implemented`, `will be decided when`, `future`).
#   Honest lines that state `accepted`/`closed` without stale language pass;
#   `candidate` tunables under an accepted OQ (e.g. OQ-014 queue depths) pass
#   because `candidate` is not stale language.
#
# Rule 2 — crate count:
#   ACTUAL = number of `crates/*` members in Cargo.toml `[workspace] members`.
#   Canonical in-repo docs locations: `CONTRIBUTING.md` and
#   `specifications/status-drift-gate.md`. Every `N crates` / `N-crate`
#   statement found there must equal ACTUAL, else fail with both numbers.
#   (CHANGELOG historical `9 crates at 0.0.1` is intentionally out of scope:
#   it records a past release, not the current workspace total.)
#
# Rule 3 — submodule wiring:
#   `bitty/.gitmodules` must contain the `docs` mount (`path = docs`,
#   `url` contains `bitty-terminal-docs`). In-repo wiring docs (`README.md`,
#   `CONTRIBUTING.md`, `AGENTS.md`) must not claim a `later phase`
#   (`later phase`, `later wiring`, `designed to be mounted`,
#   `wire this repository*submodule`, `mount*later*phase`,
#   `submodule*later phase`, case-insensitive).
#
# Rule 4 — canonical RFC status spot-checks:
#   EXPECTED (frontmatter when a sibling checkout exists, else vendored
#   `accepted`): plugin-platform, isolation-resource, ipc-agent,
#   rich-presentation, terminal-state, governance, risk-evidence,
#   package-followup (REL-10 expansion).
#   Owning candidates (read-only, first hit wins):
#     plugin-platform: ../bitty-plugins-docs/specifications/plugin-platform-rfc.md,
#       docs/specifications/plugin-platform-rfc.md
#     isolation-resource: ../bitty-plugins-docs/specifications/isolation-resource-rfc.md,
#       docs/specifications/isolation-resource-rfc.md
#     ipc-agent: ../bitty-ai-docs/specifications/ipc-agent-rfc.md,
#       docs/specifications/ipc-agent-rfc.md
#     rich-presentation: ../bitty-terminal-docs/specifications/rich-presentation-rfc.md,
#       docs/specifications/rich-presentation-rfc.md
#     terminal-state: ../bitty-terminal-docs/specifications/terminal-state-rfc.md,
#       docs/specifications/terminal-state-rfc.md
#     governance: ../bitty-terminal-docs/specifications/governance-rfc.md,
#       docs/specifications/governance-rfc.md
#     risk-evidence: ../bitty-terminal-docs/specifications/risk-evidence-rfc.md,
#       docs/specifications/risk-evidence-rfc.md
#     package-followup: ../bitty-plugins-docs/packaging/package-followup-rfc.md,
#       docs/packaging/package-followup-rfc.md
#   Deliberately not yet covered (in-repo `Proposed`/`draft` claims still
#   present; encoding them as `accepted`-expected would flip the gate red —
#   reconcile the claims in their owning scopes first): package-lifecycle
#   (`crates/bitty-package`, `crates/bitty-plugin-host` install seam),
#   lua-runtime (`crates/bitty-plugin-host` Lua dependency note),
#   configuration-model (`crates/bitty-config` status sections).
#   Any in-repo `*.rs` file mentioning the RFC identifier
#   (`plugin-platform-rfc`, `Plugin Platform RFC`, `isolation-resource-rfc`,
#   `Isolation*Resource RFC`, `ipc-agent-rfc`, `IPC*Agent RFC`,
#   `rich-presentation-rfc`, `terminal-state-rfc`, `governance-rfc`,
#   `risk-evidence-rfc`, `package-followup-rfc`, or the matching spaced
#   `X RFC` / `X Resource RFC` form) while claiming
#   `Proposed` / `draft` / `Draft` for that RFC fails when EXPECTED is
#   `accepted`, unless the file also carries an RFC-specific acceptance
#   phrase (`<rfc>.*accepted` / `accepted.*<rfc>`, case-insensitive).
#
# Scope (fast, deterministic, no network):
#   - Only tracked text: `*.rs`, `*.md` (+ `Cargo.toml`, `.gitmodules`,
#     `CONTRIBUTING.md`, `README.md`, `AGENTS.md` where stated).
#     Enumerated with `git ls-files -z` (tracked plus untracked-but-not-
#     ignored, mirroring the old `rg --files` set).
#   - Excluded: `target/**`, `.git/**`, `.worktrees/**`, `docs/**` (external
#     submodule owned by bitty-terminal-docs), `*.bin`,
#     `scripts/tests/fixtures/**`, this script itself.
#   - `docs/` is excluded so the gate behaves identically with and without
#     `git submodule update --init` (mirrors the scratch-path gate).
#   - Tooling is limited to `git`, `grep`, `sed`, `sort`, `tr`, `find`:
#     all present in a git checkout on `ubuntu-latest`. There is
#     deliberately no `rg` (ripgrep) dependency — `rg` is absent from
#     `ubuntu-latest` runners, where the first unguarded `rg` invocation
#     killed this gate with a silent exit 127 under `set -euo pipefail`.
#
# Escape hatch (auditable, grep-able, mirrors pty-gate/scratch-paths):
#   - `// status-drift-exempt: <reason>` (or `# ...` in shell/docs) on the
#     hit line or one of the 3 lines above it exempts that hit.
#
# Limitations (documented, cheap gate):
#   - Rule 1 matches an OQ line plus the next 3 lines as one window: a split
#     wider than 3 lines is missed (reviewers must still read the paragraph).
#   - Rule 2 only checks the two canonical locations, not every historical
#     count in CHANGELOG/releases (those are point-in-time records).
#   - Rule 4 is file-local for Proposed/draft claims: a file that discusses
#     the RFC history (`Draft -> ... -> Accepted`) while also quoting the old
#     `Proposed` label needs an RFC-specific acceptance phrase to pass.
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

is_exempt() {
	local file="$1" lineno="$2"
	if sed -n "${lineno}p" "$file" 2>/dev/null | grep -qF 'status-drift-exempt:'; then
		return 0
	fi
	if ((lineno > 1)); then
		local from=$((lineno > 3 ? lineno - 3 : 1))
		if sed -n "${from},$((lineno - 1))p" "$file" 2>/dev/null | grep -qF 'status-drift-exempt:'; then
			return 0
		fi
	fi
	return 1
}

# Portable file enumeration (no `rg --files`): print NUL-separated scan
# paths, one per file, relative to the scan root (leading `./` stripped,
# as the old code already tolerated via `${file#./}`).
#   $1 = rs_md (Rules 1: `*.rs` + `*.md`) or rs (Rule 4: `*.rs` only).
# `git ls-files --cached --others --exclude-standard` reproduces the old
# `rg --files` set (tracked plus untracked-but-not-ignored, `.gitignore`
# respected; the repo carries no `.ignore`/`.rgignore` that could skew
# `rg`). Globs below mirror the old `-g` filters. Outside a git work tree
# (e.g. `--root` pointing at an extracted copy) fall back to `find`.
# Always succeeds (prints nothing on enumeration failure) so callers under
# `set -euo pipefail` stay safe.
list_scan_files() {
	local mode="$1" f clean
	local -a cands=()
	if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
		mapfile -d '' cands < <(git ls-files -z --cached --others --exclude-standard -- . 2>/dev/null || true)
	else
		mapfile -d '' cands < <(find . -type f -print0 2>/dev/null || true)
	fi
	for f in "${cands[@]}"; do
		[[ -n "$f" ]] || continue
		clean="${f#./}"
		case "$mode" in
		rs_md)
			case "$clean" in
			*.rs | *.md) ;;
			*) continue ;;
			esac
			;;
		rs)
			case "$clean" in
			*.rs) ;;
			*) continue ;;
			esac
			;;
		*)
			echo "list_scan_files: unknown mode '$mode'" >&2
			return 0
			;;
		esac
		case "$clean" in
		target/* | .git | .git/* | .worktrees/* | docs | docs/* | *.bin | scripts/tests/fixtures/* | scripts/check-status-drift.sh)
			continue
			;;
		esac
		printf '%s\0' "$clean"
	done
	return 0
}

# --- Rule 1: OQ status claims ---
OQS=(OQ-008 OQ-011 OQ-012 OQ-013 OQ-014 OQ-018 OQ-053 OQ-007 OQ-009 OQ-021 OQ-022 OQ-023 OQ-024 OQ-025 OQ-026 OQ-027 OQ-028)
STALE_RE='remains?[^[:alnum:]]*open|unresolved|has not landed|have not landed|not landed|not yet implemented|will be decided when|future'

mapfile -d '' RULE1_FILES < <(list_scan_files rs_md || true)

for oq in "${OQS[@]}"; do
	for file in "${RULE1_FILES[@]}"; do
		[[ -n "$file" ]] || continue
		clean="${file#./}"
		while IFS= read -r hit; do
			lineno="${hit%%:*}"
			text="${hit#*:}"
			# OQ mentions and stale language may split across adjacent
			# lines of one paragraph (e.g. agent lib.rs OQ-018 on one line,
			# `remain open` three lines below), so match the hit line plus
			# the next 3 lines as one window.
			window="$(sed -n "${lineno},$((lineno + 3))p" "$clean" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
			if ! printf '%s' "$window" | grep -q -i -E -e "$STALE_RE"; then
				continue
			fi
			# Honest closure statements name the OQ as closed in the same
			# window (e.g. OQ-013 closed decision point with RC ceilings
			# remaining open for a different subject). Those pass;
			# future-conditionals (`will be decided when ... accepted`)
			# stay stale.
			if printf '%s' "$window" | grep -q -w 'closed'; then
				continue
			fi
			if is_exempt "$clean" "$lineno"; then
				continue
			fi
			echo "status-drift[oq-status]: $clean:$lineno: $oq contradicts accepted/closed register: $text"
			FAIL=1
		done < <(
			grep -n -i -e "$oq" -- "$clean" 2>/dev/null || true
		)
	done
done

# --- Rule 2: crate count ---
ACTUAL=0
if [[ -f Cargo.toml ]]; then
	# Braces keep the `|| true` bound to `grep` alone: without them the
	# fallback would swallow the `sort | wc` half of the pipeline on a
	# match. Emits 0 when no member is found so the check below reports
	# a clean error instead of tripping `set -e` (the old unguarded
	# `rg` here is what killed CI with exit 127 when `rg` was absent).
	ACTUAL="$({ grep -o -E '"crates/[^"]+"' Cargo.toml 2>/dev/null || true; } | sort -u | wc -l | tr -d ' ')"
	ACTUAL="${ACTUAL:-0}"
fi
if ! [[ "$ACTUAL" =~ ^[0-9]+$ ]] || ((ACTUAL == 0)); then
	echo "status-drift[crate-count]: unable to determine workspace crate count from Cargo.toml" >&2
	FAIL=1
else
	for doc in CONTRIBUTING.md specifications/status-drift-gate.md; do
		[[ -f "$doc" ]] || continue
		while IFS= read -r hit; do
			lineno="${hit%%:*}"
			text="${hit#*:}"
			if is_exempt "$doc" "$lineno"; then
				continue
			fi
			# Two-stage `grep -o`: first isolate each `N crates` / `N-crate`
			# phrase (ERE, no PCRE lookahead needed), then the leading
			# number. Output set matches the old
			# `rg -o -P '[0-9]+(?=...)'` extraction.
			nums="$(printf '%s\n' "$text" | grep -o -i -E -e '[0-9]+[[:space:]]*-?crates?\b' -e '[0-9]+-crate\b' 2>/dev/null | grep -o -E -e '[0-9]+' || true)"
			[[ -n "$nums" ]] || continue
			while IFS= read -r n; do
				[[ -n "$n" ]] || continue
				if ((10#$n != 10#$ACTUAL)); then
					echo "status-drift[crate-count]: $doc:$lineno states $n crates but Cargo.toml has $ACTUAL: $text"
					FAIL=1
				fi
			done <<<"$nums"
		done < <(
			grep -n -i -E -e '[0-9]+[[:space:]]*-?crates?\b' -e '[0-9]+-crate\b' -- "$doc" 2>/dev/null || true
		)
	done
fi

# --- Rule 3: submodule wiring ---
if [[ -f .gitmodules ]]; then
	if ! grep -qF '[submodule "docs"]' .gitmodules 2>/dev/null; then
		echo 'status-drift[submodule]: .gitmodules missing [submodule "docs"] mount'
		FAIL=1
	elif ! grep -qF 'path = docs' .gitmodules 2>/dev/null; then
		echo 'status-drift[submodule]: .gitmodules docs mount missing `path = docs`'
		FAIL=1
	elif ! grep -qF 'bitty-terminal-docs' .gitmodules 2>/dev/null; then
		echo 'status-drift[submodule]: .gitmodules docs mount url does not reference bitty-terminal-docs'
		FAIL=1
	fi
else
	echo 'status-drift[submodule]: missing .gitmodules (docs mount present required)'
	FAIL=1
fi

WIRING_DOCS=(README.md CONTRIBUTING.md AGENTS.md)
WIRING_RE='later phase|later wiring|designed to be mounted|wire this repository.*submodule|mount.*later.*phase|submodule.*later phase'
for doc in "${WIRING_DOCS[@]}"; do
	[[ -f "$doc" ]] || continue
	while IFS= read -r hit; do
		lineno="${hit%%:*}"
		text="${hit#*:}"
		if is_exempt "$doc" "$lineno"; then
			continue
		fi
		echo "status-drift[submodule]: $doc:$lineno claims later-phase wiring: $text"
		FAIL=1
	done < <(
		grep -n -i -E -e "$WIRING_RE" -- "$doc" 2>/dev/null || true
	)
done

# --- Rule 4: canonical RFC status spot-checks ---
rfc_expected() {
	local name="$1"
	local candidate status
	case "$name" in
	plugin-platform)
		for candidate in ../bitty-plugins-docs/specifications/plugin-platform-rfc.md docs/specifications/plugin-platform-rfc.md; do
			if [[ -f "$candidate" ]]; then
				# Portable `status:` frontmatter read (the old
				# `rg '(?<=^status:\s*)\S+'` never matched: look-around
				# needs `rg -P`, which was not passed, so the old code
				# always fell through to the vendored value; without a
				# sibling checkout both old and new yield `accepted`).
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	isolation-resource)
		for candidate in ../bitty-plugins-docs/specifications/isolation-resource-rfc.md docs/specifications/isolation-resource-rfc.md; do
			if [[ -f "$candidate" ]]; then
				# Portable `status:` frontmatter read (the old
				# `rg '(?<=^status:\s*)\S+'` never matched: look-around
				# needs `rg -P`, which was not passed, so the old code
				# always fell through to the vendored value; without a
				# sibling checkout both old and new yield `accepted`).
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	ipc-agent)
		for candidate in ../bitty-ai-docs/specifications/ipc-agent-rfc.md docs/specifications/ipc-agent-rfc.md; do
			if [[ -f "$candidate" ]]; then
				# Portable `status:` frontmatter read (the old
				# `rg '(?<=^status:\s*)\S+'` never matched: look-around
				# needs `rg -P`, which was not passed, so the old code
				# always fell through to the vendored value; without a
				# sibling checkout both old and new yield `accepted`).
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	rich-presentation)
		for candidate in ../bitty-terminal-docs/specifications/rich-presentation-rfc.md docs/specifications/rich-presentation-rfc.md; do
			if [[ -f "$candidate" ]]; then
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	terminal-state)
		for candidate in ../bitty-terminal-docs/specifications/terminal-state-rfc.md docs/specifications/terminal-state-rfc.md; do
			if [[ -f "$candidate" ]]; then
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	governance)
		for candidate in ../bitty-terminal-docs/specifications/governance-rfc.md docs/specifications/governance-rfc.md; do
			if [[ -f "$candidate" ]]; then
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	risk-evidence)
		for candidate in ../bitty-terminal-docs/specifications/risk-evidence-rfc.md docs/specifications/risk-evidence-rfc.md; do
			if [[ -f "$candidate" ]]; then
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
	package-followup)
		for candidate in ../bitty-plugins-docs/packaging/package-followup-rfc.md docs/packaging/package-followup-rfc.md; do
			if [[ -f "$candidate" ]]; then
				status="$(grep -i -m1 -E -e '^status:[[:space:]]*[^[:space:]]+' "$candidate" 2>/dev/null | sed -E -e 's/^[^:]*:[[:space:]]*([^[:space:]]+).*/\1/' | tr '[:upper:]' '[:lower:]' || true)"
				if [[ -n "$status" ]]; then
					printf '%s' "$status"
					return 0
				fi
			fi
		done
		printf 'accepted'
		;;
esac
}

check_rfc() {
	local name="$1" id_pat="$2" accept_pat="$3"
	local expected
	expected="$(rfc_expected "$name")"
	[[ "$expected" == "accepted" ]] || return 0
	mapfile -d '' files < <(list_scan_files rs || true)
	for file in "${files[@]}"; do
		[[ -n "$file" ]] || continue
		clean="${file#./}"
		# Status lines must name the RFC discussion (RFC/rfc, OQ-, or
		# frontmatter) so crate-level `draft` notes and unrelated
		# `Proposed default ...` tunables do not count. The RFC identifier
		# must appear on the same line or within 3 lines above/below, so a
		# `draft` note about a different RFC (e.g. provider-ecology) in the
		# same file does not misattribute.
		while IFS= read -r hit; do
			lineno="${hit%%:*}"
			text="${hit#*:}"
			lower="$(printf '%s' "$text" | tr '[:upper:]' '[:lower:]')"
			if ! printf '%s' "$lower" | grep -q -i -E -e 'rfc|oq-|frontmatter'; then
				continue
			fi
			from=$((lineno > 3 ? lineno - 3 : 1))
			to=$((lineno + 3))
			window="$(sed -n "${from},${to}p" "$clean" 2>/dev/null || true)"
			if ! printf '%s' "$window" | grep -q -i -E -e "$id_pat"; then
				continue
			fi
			if printf '%s' "$window" | grep -q -i -E -e "$accept_pat"; then
				continue
			fi
			if is_exempt "$clean" "$lineno"; then
				continue
			fi
			echo "status-drift[rfc-status]: $clean:$lineno: $name RFC claims Proposed/draft but owning frontmatter is accepted: $text"
			FAIL=1
		done < <(
			grep -n -E -e '\b[Pp]roposed\b' -e '\b[Dd]raft\b' -- "$clean" 2>/dev/null || true
		)
	done
}

check_rfc plugin-platform 'plugin-platform|plugin platform' 'plugin-platform.*accepted|accepted.*plugin-platform|plugin platform.*accepted|accepted.*plugin platform'
check_rfc isolation-resource 'isolation-resource|isolation resource|isolation/resource' 'isolation-resource.*accepted|accepted.*isolation-resource|isolation resource.*accepted|accepted.*isolation resource|isolation/resource.*accepted|accepted.*isolation/resource'
check_rfc ipc-agent 'ipc-agent|ipc[^[:alnum:]]+agent rfc|ipc and agent' 'ipc-agent.*accepted|accepted.*ipc-agent|ipc[^[:alnum:]]+agent.*accepted|accepted.*ipc[^[:alnum:]]+agent'
check_rfc rich-presentation 'rich-presentation|rich presentation' 'rich-presentation.*accepted|accepted.*rich-presentation|rich presentation.*accepted|accepted.*rich presentation'
check_rfc terminal-state 'terminal-state|terminal state' 'terminal-state.*accepted|accepted.*terminal-state|terminal state.*accepted|accepted.*terminal state'
check_rfc governance 'governance-rfc|governance rfc' 'governance-rfc.*accepted|accepted.*governance-rfc|governance rfc.*accepted|accepted.*governance rfc'
check_rfc risk-evidence 'risk-evidence|risk evidence' 'risk-evidence.*accepted|accepted.*risk-evidence|risk evidence.*accepted|accepted.*risk evidence'
check_rfc package-followup 'package-followup|package followup|package follow-up' 'package-followup.*accepted|accepted.*package-followup|package followup.*accepted|accepted.*package followup|package follow-up.*accepted|accepted.*package follow-up'

if ((FAIL)); then
	echo "status-drift: FAIL — code-doc status contradictions; fix claims to match the canonical register or extend the explicit expectations table with a reason" >&2
	exit 1
fi
echo "status-drift: OK"
