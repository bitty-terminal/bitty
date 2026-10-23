#!/usr/bin/env bash
# check-release-smoke-fail-closed.test.sh — pin the version-check contract.
#
# CTX-2504 (#2504): the 0.0.26 post-release smoke asserted the installed version
# with `bitty --version | grep -F "0.0.26"` on Linux/macOS and with an
# unanchored PowerShell `-notmatch` on Windows. Neither shape fails closed:
#   - a grep whose pattern is missing exits non-zero, but the literal is a
#     duplicated version that goes stale the moment the next tag is cut;
#   - a substring `-notmatch` accepts a wrong version (0.0.27 matches 0.0.270)
#     and a bare `Select-String` prints nothing on a miss while still exiting 0;
#   - a leg that only prints `bitty --version` (the Nix publisher) never fails.
#
# This gate pins the fail-closed shape on every PR:
#   - the expected version is derived from the release tag (`RELEASE_VERSION`)
#     instead of a version literal in the smoke job;
#   - every post-release leg compares the whole `bitty <version> ...` line;
#   - the pre-publish gates (Windows ZIP, Windows installer, macOS DMG, Unix
#     bundle, Nix) anchor the same way instead of substring-matching.
set -euo pipefail

cd "$(dirname "$0")/../.."

WORKFLOW=./.github/workflows/release.yml
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

job_block() { # <job>
  sed -n "/^  $1:/,/^  [a-z0-9-]*:/p" "$WORKFLOW"
}

expect_contains() { # <haystack> <literal> <label>
  if grep -qF -- "$2" <<<"$1"; then
    pass "$3"
  else
    fail "$3"
  fi
}

expect_absent() { # <haystack> <literal> <label>
  if grep -qF -- "$2" <<<"$1"; then
    fail "$3"
  else
    pass "$3"
  fi
}

expect_count() { # <haystack> <literal> <count> <label>
  local actual
  actual="$(grep -cF -- "$2" <<<"$1" || true)"
  if [[ "$actual" -eq "$3" ]]; then
    pass "$4"
  else
    fail "$4 (found $actual, want $3)"
  fi
}

if [[ ! -f "$WORKFLOW" ]]; then
  echo "FAIL: $WORKFLOW is missing" >&2
  exit 1
fi

# --- post-release smoke ---------------------------------------------------
SMOKE="$(job_block post-release-smoke)"

expect_contains "$SMOKE" 'TAG: ${{ github.ref_name }}' \
  "post-release smoke derives the version from the release tag"
expect_contains "$SMOKE" 'echo "RELEASE_VERSION=${version}" >>"$GITHUB_ENV"' \
  "post-release smoke exports RELEASE_VERSION to every leg"
expect_absent "$SMOKE" '0.0.' \
  "post-release smoke carries no duplicated version literal"
expect_absent "$SMOKE" 'Select-String' \
  "post-release smoke has no print-only Select-String check"
expect_absent "$SMOKE" '--version | grep' \
  "post-release smoke never pipes bitty --version straight into grep"

# Two bash legs (install.sh + Homebrew) compare the exact `bitty <version> `
# prefix; two PowerShell legs (install.ps1 + Scoop) anchor the same regex.
expect_count "$SMOKE" '"bitty ${RELEASE_VERSION} "*' 2 \
  "bash post-release legs compare the exact version prefix"
expect_count "$SMOKE" '-cnotmatch' 2 \
  "PowerShell post-release legs throw on a non-matching version"
expect_count "$SMOKE" '[regex]::Escape($expected)' 2 \
  "PowerShell post-release legs escape the expected version"

# --- pre-publish version gates -------------------------------------------
ZIP="$(job_block verify-windows-zip)"
expect_contains "$ZIP" "-cnotmatch ('^bitty ' + [regex]::Escape(\$version) + ' ')" \
  "Windows ZIP gate anchors the binary version"

INSTALLER="$(job_block verify-windows-installer)"
expect_contains "$INSTALLER" "-cnotmatch ('^bitty ' + [regex]::Escape(\$version) + ' ')" \
  "Windows installer gate anchors the binary version"
expect_contains "$INSTALLER" "-cnotmatch ('^' + [regex]::Escape(\$version) + '(\.0+)*\$')" \
  "Windows installer gate anchors the uninstall DisplayVersion"

DMG="$(job_block macos-universal)"
expect_count "$DMG" '"bitty ${VERSION} "*' 2 \
  "macOS DMG gate anchors both architecture launches"

BUNDLE="$(job_block verify-unix-bundle)"
expect_contains "$BUNDLE" '"bitty ${version} "*' \
  "Unix bundle gate anchors the bundle binary version"

NIX="$(job_block nix)"
expect_contains "$NIX" '"bitty ${version} "*' \
  "Nix publisher asserts the built binary reports the tagged version"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-release-smoke-fail-closed.test: FAIL" >&2
  exit 1
fi
echo "check-release-smoke-fail-closed.test: PASS"
