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
#   - every post-release leg accepts one exact `bitty <version> (channel commit)`
#     line only, with a successful native process exit;
#   - the pre-publish gates (Windows ZIP, Windows installer, macOS DMG, Unix
#     bundle, Nix) anchor the same way instead of substring-matching.
#
# PowerShell is not available on the Linux development host. The PowerShell
# assertions below are therefore syntax/static contract checks; execution of
# the native Windows and Scoop paths is evidence supplied by Windows CI.
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

stable_release_tag() { # <tag>
  [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

expect_version_command() { # <expected> <path> <want-status> <label>
  local expected="$1" path="$2" want="$3" label="$4"
  local output status=0
  if output="$(./scripts/check-version-output.sh "$expected" "$path" 2>&1)"; then
    status=0
  else
    status=$?
  fi
  if [[ "$status" -eq 0 ]]; then
    if [[ "$want" == accept ]]; then
      pass "$label"
    else
      fail "$label accepted unexpected output"
    fi
  elif [[ "$want" == reject ]]; then
    pass "$label"
  else
    fail "$label rejected valid output (status=$status, output=$output)"
  fi
}

expect_scoop_path() { # <path> <want> <label>
  local path="$1" want="$2" label="$3"
  if [[ -f "$path" && -x "$path" ]]; then
    if [[ "$want" == accept ]]; then
      pass "$label"
    else
      fail "$label accepted an invalid path"
    fi
  elif [[ "$want" == reject ]]; then
    pass "$label"
  else
    fail "$label rejected an existing executable"
  fi
}

if [[ ! -f "$WORKFLOW" ]]; then
  echo "FAIL: $WORKFLOW is missing" >&2
  exit 1
fi

# --- post-release smoke ---------------------------------------------------
SMOKE="$(job_block post-release-smoke)"
VALIDATE="$(job_block validate)"
RELEASE="$(job_block release)"

expect_contains "$SMOKE" 'TAG: ${{ github.ref_name }}' \
  "post-release smoke derives the version from the release tag"
expect_contains "$SMOKE" 'echo "RELEASE_VERSION=${version}" >>"$GITHUB_ENV"' \
  "post-release smoke exports RELEASE_VERSION to every leg"
expect_contains "$VALIDATE" 'release tags must be stable vX.Y.Z tags' \
  "release validation rejects prerelease tags"
expect_contains "$SMOKE" 'unexpected stable release tag' \
  "post-release smoke rejects prerelease versions"
expect_absent "$SMOKE" '0.0.' \
  "post-release smoke carries no duplicated version literal"
expect_absent "$SMOKE" 'Select-String' \
  "post-release smoke has no print-only Select-String check"
expect_absent "$SMOKE" '--version | grep' \
  "post-release smoke never pipes bitty --version straight into grep"

# Two bash legs (install.sh + Homebrew) compare the exact table output; two
# PowerShell legs (install.ps1 + Scoop) anchor the same regex.
expect_count "$SMOKE" 'scripts/check-version-output.sh' 2 \
  "Bash post-release legs use the strict native version parser"
expect_count "$SMOKE" '-cnotmatch' 2 \
  "PowerShell post-release legs throw on a non-matching version"
expect_count "$SMOKE" '[regex]::Escape($expected)' 2 \
  "PowerShell post-release legs escape the expected version"
expect_count "$SMOKE" "'\\Abitty '" 2 \
  "PowerShell post-release legs anchor the complete version line"
expect_count "$SMOKE" '2>&1 | Out-String' 2 \
  "PowerShell post-release legs capture native stdout and stderr"
expect_contains "$SMOKE" 'Test-Path -LiteralPath $shim -PathType Leaf' \
  "Scoop smoke requires an existing shim file"
expect_contains "$SMOKE" "GetExtension(\$shim) -notin @('.exe', '.cmd', '.bat', '.ps1')" \
  "Scoop smoke requires an executable shim extension"
expect_contains "$SMOKE" 'Test-Path -LiteralPath $exe -PathType Leaf' \
  "native Windows smoke requires an existing executable file"
expect_contains "$RELEASE" 'prerelease: false' \
  "GitHub release is not marked as a prerelease"
if stable_release_tag v0.0.27; then
  pass "stable release tag is accepted"
else
  fail "stable release tag was rejected"
fi
if stable_release_tag v0.0.27-rc.1; then
  fail "prerelease tag was accepted"
else
  pass "prerelease tag is rejected"
fi

# --- pre-publish version gates -------------------------------------------
ZIP="$(job_block verify-windows-zip)"
expect_contains "$ZIP" "\$pattern = '\\Abitty ' + [regex]::Escape(\$version)" \
  "Windows ZIP gate anchors the binary version"

INSTALLER="$(job_block verify-windows-installer)"
expect_contains "$INSTALLER" "\$pattern = '\\Abitty ' + [regex]::Escape(\$version)" \
  "Windows installer gate anchors the binary version"
expect_contains "$INSTALLER" "-cnotmatch ('^' + [regex]::Escape(\$version) + '(\.0+)*\$')" \
  "Windows installer gate anchors the uninstall DisplayVersion"

DMG="$(job_block macos-universal)"
expect_count "$DMG" 'scripts/check-version-output.sh "$VERSION"' 2 \
  "macOS DMG gate anchors both architecture launches"

BUNDLE="$(job_block verify-unix-bundle)"
expect_contains "$BUNDLE" 'scripts/check-version-output.sh "$version"' \
  "Unix bundle gate anchors the bundle binary version"

NIX="$(job_block nix)"
expect_contains "$NIX" 'scripts/check-version-output.sh "$version" result/bin/bitty' \
  "Nix publisher asserts the built binary reports the tagged version"

# --- native output and Scoop shim fixtures -------------------------------
FIXTURE_DIR="$(mktemp -d)"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

make_fixture() { # <name> <body>
  local name="$1" body="$2"
  printf '%s\n' '#!/usr/bin/env bash' "$body" >"$FIXTURE_DIR/$name"
  chmod +x "$FIXTURE_DIR/$name"
}

make_fixture native-ok 'printf "%s\\n" "bitty 0.0.27 (stable abc1234)"'
make_fixture native-nonzero 'printf "%s\\n" "bitty 0.0.27 (stable abc1234)"; exit 7'
make_fixture native-empty 'exit 0'
make_fixture native-multiline 'printf "%s\\n%s\\n" "bitty 0.0.27 (stable abc1234)" "unexpected"'
make_fixture native-trailing 'printf "%s\\n" "bitty 0.0.27 (stable abc1234) trailing"'
make_fixture native-blank-line 'printf "%s\\n\\n" "bitty 0.0.27 (stable abc1234)"'
make_fixture scoop-shim.exe 'printf "%s\\n" "bitty 0.0.27 (stable abc1234)"'

expect_version_command 0.0.27 "$FIXTURE_DIR/native-ok" accept \
  "native version output is accepted"
expect_version_command 0.0.27 "$FIXTURE_DIR/native-nonzero" reject \
  "native nonzero exit is rejected"
expect_version_command 0.0.27 "$FIXTURE_DIR/native-empty" reject \
  "native empty output is rejected"
expect_version_command 0.0.27 "$FIXTURE_DIR/native-multiline" reject \
  "native multiline output is rejected"
expect_version_command 0.0.27 "$FIXTURE_DIR/native-trailing" reject \
  "native trailing junk is rejected"
expect_version_command 0.0.27 "$FIXTURE_DIR/native-blank-line" reject \
  "native extra blank line is rejected"
expect_scoop_path "$FIXTURE_DIR/scoop-shim.exe" accept \
  "Scoop shim resolves to an existing executable"
expect_version_command 0.0.27 "$FIXTURE_DIR/scoop-shim.exe" accept \
  "Scoop shim version output is validated"
expect_scoop_path "$FIXTURE_DIR/missing-shim.exe" reject \
  "Scoop nonexistent path is rejected"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-release-smoke-fail-closed.test: FAIL" >&2
  exit 1
fi
echo "check-release-smoke-fail-closed.test: PASS"
