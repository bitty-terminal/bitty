#!/usr/bin/env bash
# check-windows-installer.test.sh — fixture test for the Windows setup track.
#
# CTX-1064 / #1810. ISCC.exe and Windows never run here (Linux host); the
# release workflow's windows-installer + verify-windows-installer jobs are
# the live verifier. This pins the static contract: the validator passes on
# the committed script, rejects each drift class on a temp copy, and the
# workflow/README wiring carries the settled decisions (unsigned 0.1.0,
# shortcuts, registry uninstall entries, Scoop stays portable).
set -euo pipefail

cd "$(dirname "$0")/../.."

CHECK=./scripts/check-windows-installer.sh
ISS=./packaging/windows/bitty.iss
WORKFLOW=./.github/workflows/release.yml
README=./packaging/README.md
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

[[ -x "$CHECK" ]] || fail "validator not executable: $CHECK"
[[ -f "$ISS" ]] || fail "installer script missing: $ISS"

# 1. Validator passes on the committed script.
if "$CHECK" >/dev/null 2>&1; then
  pass "validator passes on the committed bitty.iss"
else
  fail "validator rejects the committed bitty.iss"
fi

# 2. Each drift class fails on a temp copy (validator --iss override).
TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/check-windows-installer-test.XXXXXX")"
trap 'rm -rf "$TMP_BASE"' EXIT

expect_reject() { # <label> <sed-expr>
  local label="$1" expr="$2"
  local copy="$TMP_BASE/reject.iss"
  sed -e "$expr" "$ISS" >"$copy"
  if "$CHECK" --iss "$copy" >/dev/null 2>&1; then
    fail "validator accepted drift ($label)"
  else
    pass "validator rejects drift ($label)"
  fi
}

expect_reject "hardcoded AppVersion" 's/AppVersion={#MyAppVersion}/AppVersion=0.0.1/'
expect_reject "missing desktopicon task" '/desktopicon/d'
expect_reject "active SignTool" 's/; SignTool=/SignTool=/'
expect_reject "PATH environment write" 's/\[Run\]/[Registry]\nRoot: HKCU; Subkey: "Environment"; ValueName: "Path"; ValueType: string; ValueData: "{app}"\n\n[Run]/'

# 3. Workflow wiring (static: the live compile + round-trip run on Windows CI).
for needle in 'windows-installer:' 'verify-windows-installer:' \
  'choco install innosetup' 'ISCC.exe' '/DMyAppVersion' '/DMySourceDir' \
  'bitty-*-windows-x86_64-setup.exe' 'Get-AuthenticodeSignature' 'NotSigned' \
  '/SILENT' '/TASKS="desktopicon"' 'Uninstall' 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'; do
  if grep -qF -- "$needle" "$WORKFLOW"; then
    pass "release.yml carries '$needle'"
  else
    fail "release.yml missing '$needle'"
  fi
done
if grep -q 'windows-installer' "$WORKFLOW" && grep -q 'verify-windows-installer' "$WORKFLOW"; then
  release_needs="$(sed -n '/^  release:/,/^  [a-z]/p' "$WORKFLOW")"
  if grep -q 'windows-installer' <<<"$release_needs" && grep -q 'verify-windows-installer' <<<"$release_needs"; then
    pass "release job gates on windows-installer + verify-windows-installer"
  else
    fail "release job needs: missing the installer legs"
  fi
fi

# 4. README documents the settled track (unsigned, shortcuts, registry, Scoop).
for needle in 'bitty.iss' 'Inno Setup 6' 'unsigned' 'SmartScreen' \
  'Start Menu' 'Desktop' 'Uninstall' 'Scoop' \
  'bitty-<VERSION>-windows-x86_64-setup.exe'; do
  if grep -qF -- "$needle" "$README"; then
    pass "packaging/README.md documents '$needle'"
  else
    fail "packaging/README.md missing '$needle'"
  fi
done

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-windows-installer.test: FAIL" >&2
  exit 1
fi
echo "check-windows-installer.test: PASS"
