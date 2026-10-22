#!/usr/bin/env bash
# check-spec.sh — validate packaging/bitty.spec metadata and structure.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPEC_PATH="$REPO_ROOT/packaging/bitty.spec"

fail() {
  echo "check-spec: FAIL: $1" >&2
  exit 1
}

[[ -f "$SPEC_PATH" ]] || fail "spec file not found: $SPEC_PATH"

assert_grep() {
  local pattern="$1" why="$2"
  grep -Eq "$pattern" "$SPEC_PATH" || fail "$why"
}

assert_grep "^Name:[[:space:]]+bitty$" "Name must be bitty"
assert_grep "^Version:[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+" "Version must be semver"
assert_grep "^Release:[[:space:]]+1%\{\?dist\}$" "Release must follow standard 1%{?dist}"
assert_grep "^Summary:[[:space:]]+" "Summary must be present"
assert_grep "^License:[[:space:]]+MIT OR Apache-2.0$" "License must be MIT OR Apache-2.0"
assert_grep "^URL:[[:space:]]+https://github\.com/bitty-terminal/bitty$" "URL must be repository URL"
assert_grep "^Source0:[[:space:]]+https://cdn\.bitty\.run/releases/v%\{version\}/bitty-%\{version\}-x86_64-unknown-linux-gnu\.tar\.zst$" \
  "Source0 must point to x86_64 R2 CDN bundle"
assert_grep "^Source1:[[:space:]]+https://cdn\.bitty\.run/releases/v%\{version\}/bitty-%\{version\}-aarch64-unknown-linux-gnu\.tar\.zst$" \
  "Source1 must point to aarch64 R2 CDN bundle"

assert_grep "^Provides:[[:space:]]+bitty-terminal[[:space:]]*=" "Provides must include bitty-terminal alias"
assert_grep "^Requires:[[:space:]]+fontconfig" "Requires must include fontconfig"
assert_grep "^Requires:[[:space:]]+freetype" "Requires must include freetype"
assert_grep "^Requires:[[:space:]]+libgcc" "Requires must include libgcc"

# Assert critical sections exist
for section in "%description" "%prep" "%build" "%install" "%files" "%changelog"; do
  grep -q "^$section" "$SPEC_PATH" || fail "spec file missing $section section"
done

# Assert install destinations
assert_grep "bin/bitty %\{buildroot\}%\{_bindir\}/bitty" "install must place /usr/bin/bitty"
assert_grep "run\.bitty\.Bitty\.desktop %\{buildroot\}%\{_datadir\}/applications/" "install must place desktop file"
assert_grep "run\.bitty\.Bitty\.metainfo\.xml %\{buildroot\}%\{_datadir\}/metainfo/" "install must place metainfo file"
assert_grep "tic -x -o %\{buildroot\}%\{_datadir\}/terminfo" "install must compile terminfo entry"

# Assert files manifest
assert_grep "%\{_bindir\}/bitty" "%files must contain %{_bindir}/bitty"
assert_grep "%\{_datadir\}/applications/run\.bitty\.Bitty\.desktop" "%files must contain desktop file"
assert_grep "%\{_datadir\}/metainfo/run\.bitty\.Bitty\.metainfo\.xml" "%files must contain metainfo file"
assert_grep "%\{_datadir\}/icons/hicolor/\*/apps/bitty\.\*" "%files must contain icons"
assert_grep "%\{_datadir\}/terminfo/b/bitty" "%files must contain compiled terminfo"

echo "check-spec: PASS"
