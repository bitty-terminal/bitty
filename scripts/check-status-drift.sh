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
#   `accepted`): plugin-platform, isolation-resource, ipc-agent.
#   Owning candidates (read-only, first hit wins):
#     plugin-platform: ../bitty-plugins-docs/specifications/plugin-platform-rfc.md,
#       docs/specifications/plugin-platform-rfc.md
#     isolation-resource: ../bitty-plugins-docs/specifications/isolation-resource-rfc.md,
#       docs/specifications/isolation-resource-rfc.md
#     ipc-agent: ../bitty-ai-docs/specifications/ipc-agent-rfc.md,
#       docs/specifications/ipc-agent-rfc.md
#   Any in-repo `*.rs` file mentioning the RFC identifier
#   (`plugin-platform-rfc`, `Plugin Platform RFC`, `isolation-resource-rfc`,
#   `Isolation*Resource RFC`, `ipc-agent-rfc`, `IPC*Agent RFC`) while claiming
#   `Proposed` / `draft` / `Draft` for that RFC fails when EXPECTED is
#   `accepted`, unless the file also carries an RFC-specific acceptance
#   phrase (`<rfc>.*accepted` / `accepted.*<rfc>`, case-insensitive).
#
# Scope (fast, deterministic, no network):
#   - Only tracked text: `*.rs`, `*.md` (+ `Cargo.toml`, `.gitmodules`,
#     `CONTRIBUTING.md`, `README.md`, `AGENTS.md` where stated).
#   - Excluded: `target/**`, `.git/**`, `.worktrees/**`, `docs/**` (external
#     submodule owned by bitty-terminal-docs), `*.bin`,
#     `scripts/tests/fixtures/**`, this script itself.
#   - `docs/` is excluded so the gate behaves identically with and without
#     `git submodule update --init` (mirrors the scratch-path gate).
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
	if sed -n "${lineno}p" "$file" 2>/dev/null | rg -q 'status-drift-exempt:'; then
		return 0
	fi
	if ((lineno > 1)); then
		local from=$((lineno > 3 ? lineno - 3 : 1))
		if sed -n "${from},$((lineno - 1))p" "$file" 2>/dev/null | rg -q 'status-drift-exempt:'; then
			return 0
		fi
	fi
	return 1
}

# --- Rule 1: OQ status claims ---
OQS=(OQ-008 OQ-011 OQ-012 OQ-013 OQ-014 OQ-018 OQ-053)
STALE_RE='remains?[^[:alnum:]]*open|unresolved|has not landed|have not landed|not landed|not yet implemented|will be decided when|future'

mapfile -d '' RULE1_FILES < <(
	rg --files -0 --hidden \
		-g '*.rs' -g '*.md' \
		-g '!target/**' -g '!.git' -g '!.git/**' -g '!.worktrees/**' \
		-g '!docs/**' -g '!*.bin' \
		-g '!scripts/tests/fixtures/**' \
		-g '!scripts/check-status-drift.sh' \
		. 2>/dev/null || true
)

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
			if ! printf '%s' "$window" | rg -q -i "$STALE_RE"; then
				continue
			fi
			# Honest closure statements name the OQ as closed in the same
			# window (e.g. OQ-013 closed decision point with RC ceilings
			# remaining open for a different subject). Those pass;
			# future-conditionals (`will be decided when ... accepted`)
			# stay stale.
			if printf '%s' "$window" | rg -q -w 'closed'; then
				continue
			fi
			if is_exempt "$clean" "$lineno"; then
				continue
			fi
			echo "status-drift[oq-status]: $clean:$lineno: $oq contradicts accepted/closed register: $text"
			FAIL=1
		done < <(
			rg -n --no-heading -i -e "$oq" "$clean" 2>/dev/null || true
		)
	done
done

# --- Rule 2: crate count ---
ACTUAL=0
if [[ -f Cargo.toml ]]; then
	ACTUAL="$(rg -o -N '"crates/[^"]+"' Cargo.toml 2>/dev/null | sort -u | wc -l | tr -d ' ')"
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
			nums="$(printf '%s\n' "$text" | rg -o -P -i -N '[0-9]+(?=[[:space:]]*-?crates?\b)|[0-9]+(?=-crate\b)' 2>/dev/null || true)"
			[[ -n "$nums" ]] || continue
			while IFS= read -r n; do
				[[ -n "$n" ]] || continue
				if ((10#$n != 10#$ACTUAL)); then
					echo "status-drift[crate-count]: $doc:$lineno states $n crates but Cargo.toml has $ACTUAL: $text"
					FAIL=1
				fi
			done <<<"$nums"
		done < <(
			rg -n --no-heading -i -e '[0-9]+[[:space:]]*-?crates?\b' -e '[0-9]+-crate\b' "$doc" 2>/dev/null || true
		)
	done
fi

# --- Rule 3: submodule wiring ---
if [[ -f .gitmodules ]]; then
	if ! rg -q '\[submodule "docs"\]' .gitmodules 2>/dev/null; then
		echo 'status-drift[submodule]: .gitmodules missing [submodule "docs"] mount'
		FAIL=1
	elif ! rg -q 'path = docs' .gitmodules 2>/dev/null; then
		echo 'status-drift[submodule]: .gitmodules docs mount missing `path = docs`'
		FAIL=1
	elif ! rg -q 'bitty-terminal-docs' .gitmodules 2>/dev/null; then
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
		rg -n --no-heading -i -e "$WIRING_RE" "$doc" 2>/dev/null || true
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
				status="$(rg -N -m1 -i -o --no-heading '(?<=^status:\s*)\S+' "$candidate" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
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
				status="$(rg -N -m1 -i -o --no-heading '(?<=^status:\s*)\S+' "$candidate" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
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
				status="$(rg -N -m1 -i -o --no-heading '(?<=^status:\s*)\S+' "$candidate" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
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
	mapfile -d '' files < <(
		rg --files -0 --hidden \
			-g '*.rs' \
			-g '!target/**' -g '!.git' -g '!.git/**' -g '!.worktrees/**' \
			-g '!docs/**' \
			-g '!scripts/tests/fixtures/**' \
			-g '!scripts/check-status-drift.sh' \
			. 2>/dev/null || true
	)
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
			if ! printf '%s' "$lower" | rg -q -i 'rfc|oq-|frontmatter'; then
				continue
			fi
			from=$((lineno > 3 ? lineno - 3 : 1))
			to=$((lineno + 3))
			window="$(sed -n "${from},${to}p" "$clean" 2>/dev/null || true)"
			if ! printf '%s' "$window" | rg -q -i -e "$id_pat"; then
				continue
			fi
			if printf '%s' "$window" | rg -q -i -e "$accept_pat"; then
				continue
			fi
			if is_exempt "$clean" "$lineno"; then
				continue
			fi
			echo "status-drift[rfc-status]: $clean:$lineno: $name RFC claims Proposed/draft but owning frontmatter is accepted: $text"
			FAIL=1
		done < <(
			rg -n --no-heading -e '\b[Pp]roposed\b' -e '\b[Dd]raft\b' "$clean" 2>/dev/null || true
		)
	done
}

check_rfc plugin-platform 'plugin-platform|plugin platform' 'plugin-platform.*accepted|accepted.*plugin-platform|plugin platform.*accepted|accepted.*plugin platform'
check_rfc isolation-resource 'isolation-resource|isolation resource|isolation/resource' 'isolation-resource.*accepted|accepted.*isolation-resource|isolation resource.*accepted|accepted.*isolation resource|isolation/resource.*accepted|accepted.*isolation/resource'
check_rfc ipc-agent 'ipc-agent|ipc[^[:alnum:]]+agent rfc|ipc and agent' 'ipc-agent.*accepted|accepted.*ipc-agent|ipc[^[:alnum:]]+agent.*accepted|accepted.*ipc[^[:alnum:]]+agent'

if ((FAIL)); then
	echo "status-drift: FAIL — code-doc status contradictions; fix claims to match the canonical register or extend the explicit expectations table with a reason" >&2
	exit 1
fi
echo "status-drift: OK"
