#!/usr/bin/env bash
# check-windows-installer.sh — static gate for the Inno Setup installer.
#
# CTX-1064 / #1810. Linux-runnable: ISCC.exe and Windows never run here;
# Windows CI (release.yml windows-installer + verify-windows-installer) is
# the live verifier. This gate pins the settled owner decisions so they
# cannot drift silently: unsigned 0.1.0, Start Menu + opt-in Desktop
# shortcuts, automatic registry uninstall entries, no hardcoded version or
# host paths, no active signing, no PATH/file-association writes.
#
# Usage: scripts/check-windows-installer.sh [--iss PATH]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ISS="$REPO_ROOT/packaging/windows/bitty.iss"

while [[ $# -gt 0 ]]; do
  case "$1" in
  --iss)
    ISS="${2:-}"
    shift 2
    ;;
  -h | --help)
    echo "usage: $0 [--iss PATH]"
    exit 0
    ;;
  *)
    echo "usage: $0 [--iss PATH]" >&2
    exit 2
    ;;
  esac
done

FAIL=0
fail() {
  echo "check-windows-installer: FAIL: $1" >&2
  FAIL=1
}

[[ -f "$ISS" ]] || {
  echo "check-windows-installer: FAIL: installer script missing: $ISS" >&2
  exit 1
}

# Version flows in via /DMyAppVersion (release workflow: tag > input > Cargo).
grep -qF '#ifndef MyAppVersion' "$ISS" || fail "missing MyAppVersion define guard"
grep -qF 'AppVersion={#MyAppVersion}' "$ISS" || fail "AppVersion must use {#MyAppVersion}"
grep -qF 'OutputBaseFilename=bitty-{#MyAppVersion}-windows-x86_64-setup' "$ISS" || fail "OutputBaseFilename must embed {#MyAppVersion}"
if grep -Eq '^(AppVersion|OutputBaseFilename)=.*[0-9]+\.[0-9]+\.[0-9]+' "$ISS"; then
  fail "hardcoded version in AppVersion/OutputBaseFilename (pass /DMyAppVersion)"
fi

# Required wizard + shortcut + registry-uninstall surface.
for needle in '[Setup]' '[Files]' '[Icons]' '[Tasks]' '[Languages]' '[Run]' \
  'AppId={' 'DefaultDirName={autopf}' 'WizardStyle=modern' \
  'PrivilegesRequired=lowest' 'ArchitecturesAllowed=x64compatible' \
  'MinVersion=10.0' 'desktopicon' '{group}' '{autodesktop}' '{uninstallexe}' \
  'MessagesFile: "compiler:Default.isl"'; do
  grep -qF -- "$needle" "$ISS" || fail "missing required token: $needle"
done

# Owner-bar specifics.
grep -qF 'Source: "{#MySourceDir}' "$ISS" || fail "Files must stage from {#MySourceDir} (absolute stage dir)"
grep -qi 'unsigned' "$ISS" || fail "must state the unsigned 0.1.0 status"
grep -qF '#1810' "$ISS" || fail "must reference #1810 for the signing deferral"
grep -qi 'SmartScreen' "$ISS" || fail "must mention the SmartScreen click-through"
grep -qi 'Uninstall.*automatically\|uninstall entry automatically' "$ISS" || fail "must document the automatic registry uninstall entry"

# Negatives: nothing that claims trust or machine state it must not touch.
if grep -Eq '^[[:space:]]*SignTool=' "$ISS"; then
  fail "active SignTool= ships trust 0.1.0 does not have (keep the commented stub only)"
fi
if grep -Eq 'ChangesEnvironment=yes|Subkey: "Environment"|Session Manager\\Environment|HKCU\\Environment' "$ISS"; then
  fail "no PATH environment writes in 0.1.0 (deferred decision)"
fi
# scratch-paths-exempt: pattern-defining lint line (must spell the forbidden
# host-path spellings to reject them in the installer script).
if grep -Eq 'C:\\Users|/home/[A-Za-z0-9]|/Users/[A-Za-z0-9]|/mnt/' "$ISS"; then
  fail "hardcoded host-absolute path in the installer script"
fi

if [[ "$FAIL" -ne 0 ]]; then
  exit 1
fi
echo "check-windows-installer: PASS ($ISS)"
