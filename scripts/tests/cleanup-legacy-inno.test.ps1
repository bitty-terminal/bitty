<#
.SYNOPSIS
  Executed test for the legacy Inno Setup cleanup in scripts/install.ps1
  (CTX-1750, bitty#3038).

.DESCRIPTION
  The shipped function bodies are extracted from scripts/install.ps1 with the
  PowerShell parser, so this test cannot drift from the script it protects,
  and then driven against throwaway fixtures:

    * the Start Menu group folder loses the two shortcuts Inno created and
      keeps anything the user put there (a recursive delete used to take a
      user-authored shortcut with the folder);
    * a second run is a no-op: no removals and no SHChangeNotify;
    * a -InstallDir nested anywhere inside the legacy directory is never
      deleted, and the legacy uninstall key survives while the directory does;
    * a desktop shortcut is removed only when its target resolves into the
      legacy directory;
    * a failed directory removal keeps the uninstall key.

  The file-level legs run on any host; the .lnk/registry/sharing-violation
  legs are skipped off Windows (Windows CI runs all of them).

  Usage (from anywhere):
    pwsh -NoProfile -File scripts/tests/cleanup-legacy-inno.test.ps1
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
  param([bool]$Condition, [string]$Message)
  if (-not $Condition) {
    throw "FAIL: $Message"
  }
}

$onWindows = $true
try {
  $onWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows)
} catch {
  $onWindows = ("$env:OS" -eq 'Windows_NT')
}

$repoRoot = Split-Path -Path (Split-Path -Path $PSScriptRoot -Parent) -Parent
$installScript = Join-Path $repoRoot 'scripts/install.ps1'
$issScript = Join-Path $repoRoot 'packaging/windows/bitty.iss'
Assert-True (Test-Path -LiteralPath $installScript) "missing $installScript"
Assert-True (Test-Path -LiteralPath $issScript) "missing $issScript"

# --- load the shipped function bodies -------------------------------------
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
  $installScript, [ref]$tokens, [ref]$parseErrors)
Assert-True (@($parseErrors).Count -eq 0) `
  "install.ps1 does not parse: $(@($parseErrors) | Select-Object -First 1 | ForEach-Object { $_.Message })"

$wantedFunctions = @(
  'Write-BittyLog'
  'Get-BittyStartMenuDir'
  'Send-BittyShellIconChange'
  'Test-BittyPathWithin'
  'Get-BittyLegacyInnoUninstallKey'
  'Remove-BittyLegacyDesktopShortcut'
  'Remove-BittyLegacyInnoSetup'
)
$functionText = foreach ($name in $wantedFunctions) {
  $astFunction = $ast.Find({
      param($node)
      $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
  Assert-True ($null -ne $astFunction) "install.ps1 has no function $name (this test drives the shipped bodies)"
  $astFunction.Extent.Text
}
. ([scriptblock]::Create(($functionText -join [Environment]::NewLine)))

# --- the AppId the legacy key path derives from ---------------------------
# bitty.iss:51 is the source of truth; assert the installer's copy agrees,
# then point the variable at a throwaway AppId so this test never creates or
# deletes a real legacy uninstall key.
$appIdFromScript = [regex]::Match(
  (Get-Content -LiteralPath $installScript -Raw),
  "LegacyInnoAppId\s*=\s*'(\{[0-9A-Fa-f-]{36}\})'")
Assert-True $appIdFromScript.Success 'install.ps1 does not pin $script:LegacyInnoAppId'
$appIdFromIss = [regex]::Match(
  (Get-Content -LiteralPath $issScript -Raw),
  '(?m)^AppId=\{\{([0-9A-Fa-f-]{36})\}')
Assert-True $appIdFromIss.Success 'bitty.iss has no AppId={{<guid>} setup line'
# Inno escapes the leading brace, so "{{<guid>}" is the literal "<guid>":
# rebuild the registry spelling the installer derives its key path from.
$appIdFromIssValue = '{' + $appIdFromIss.Groups[1].Value + '}'
Assert-True ($appIdFromScript.Groups[1].Value -eq $appIdFromIssValue) `
  "legacy AppId drift: install.ps1 has $($appIdFromScript.Groups[1].Value), bitty.iss has $appIdFromIssValue"
$script:LegacyInnoAppId = '{F1750000-0000-4000-8000-000000000000}'

# --- instrument SHChangeNotify -------------------------------------------
# Send-BittyShellIconChange only Add-Type's its P/Invoke when the type is
# missing, so predefining an instrumented type of the same name counts the
# calls the real code makes (and still proves the spy can fire).
if (-not ('Bitty.ShellNativeMethods' -as [type])) {
  Add-Type -TypeDefinition @'
namespace Bitty {
  public static class ShellNativeMethods {
    [System.Runtime.InteropServices.DllImport("shell32.dll")]
    private static extern void SHChangeNotifyNative(int wEventId, uint uFlags, System.IntPtr dwItem1, System.IntPtr dwItem2);
    public static int Calls = 0;
    public static void SHChangeNotify(int wEventId, uint uFlags, System.IntPtr dwItem1, System.IntPtr dwItem2) {
      Calls++;
      try { SHChangeNotifyNative(wEventId, uFlags, dwItem1, dwItem2); } catch { }
    }
  }
}
'@
}

# --- containment predicate (the guard that protects the fresh install) ----
$pathCases = @(
  @{ Candidate = 'C:\Apps\Bitty'; Root = 'C:\Apps\Bitty'; Want = $true; Why = 'equal paths' }
  @{ Candidate = 'C:\Apps\Bitty\bin'; Root = 'C:\Apps\Bitty'; Want = $true; Why = 'direct child' }
  @{ Candidate = 'c:\apps\bitty\BIN\deep'; Root = 'C:\Apps\Bitty'; Want = $true; Why = 'case-insensitive descendant' }
  @{ Candidate = 'C:\Apps\Bitty2'; Root = 'C:\Apps\Bitty'; Want = $false; Why = 'sibling sharing a prefix' }
  @{ Candidate = 'C:\Apps'; Root = 'C:\Apps\Bitty'; Want = $false; Why = 'ancestor is not a descendant' }
  @{ Candidate = 'C:\Apps\Bitty\bin\'; Root = 'C:\Apps\Bitty\'; Want = $true; Why = 'trailing separators' }
  @{ Candidate = 'D:\Bitty'; Root = 'C:\Apps\Bitty'; Want = $false; Why = 'unrelated drive' }
)
foreach ($pathCase in $pathCases) {
  $got = Test-BittyPathWithin -Candidate $pathCase.Candidate -Root $pathCase.Root
  Assert-True ($got -eq $pathCase.Want) `
    "Test-BittyPathWithin('$($pathCase.Candidate)', '$($pathCase.Root)') = $got, want $($pathCase.Want) [$($pathCase.Why)]"
}
Write-Host "PASS: containment predicate ($($pathCases.Count) cases)"

# --- fixtures -------------------------------------------------------------
$root = Join-Path ([System.IO.Path]::GetTempPath()) ('bitty-legacy-cleanup-' + [Guid]::NewGuid().ToString('N'))
$savedAppData = "$env:AppData"
$savedLocalAppData = "$env:LocalAppData"
$legacyKey = ''
$nestedKey = ''
$shell = $null

try {
  $appData = Join-Path $root 'appdata'
  $localAppData = Join-Path $root 'localappdata'
  $desktop = Join-Path $root 'desktop'
  $currentDir = Join-Path $root 'current\bitty\bin'
  $groupDir = Join-Path $appData 'Microsoft\Windows\Start Menu\Programs\Bitty'
  $legacyDir = Join-Path $localAppData 'Programs\Bitty'
  New-Item -ItemType Directory -Force -Path $groupDir, $legacyDir, $desktop, $currentDir | Out-Null
  Set-Content -LiteralPath (Join-Path $groupDir 'Bitty.lnk') -Value 'inno start menu shortcut'
  Set-Content -LiteralPath (Join-Path $groupDir 'Uninstall Bitty.lnk') -Value 'inno uninstall shortcut'
  $userShortcut = Join-Path $groupDir 'My Notes.lnk'
  Set-Content -LiteralPath $userShortcut -Value 'shortcut the user created'
  Set-Content -LiteralPath (Join-Path $legacyDir 'bitty.exe') -Value 'stale inno payload'
  Set-Content -LiteralPath (Join-Path $legacyDir 'unins000.exe') -Value 'stale inno uninstaller'
  Set-Content -LiteralPath (Join-Path $currentDir 'bitty.exe') -Value 'fresh payload'

  $env:AppData = $appData
  $env:LocalAppData = $localAppData
  $legacyKey = Get-BittyLegacyInnoUninstallKey

  $legacyDeskShortcut = Join-Path $desktop 'Bitty.lnk'
  $userDeskShortcut = Join-Path $desktop 'My Notes.lnk'
  if ($onWindows) {
    New-Item -Path $legacyKey -Force | Out-Null
    New-ItemProperty -Path $legacyKey -Name 'DisplayName' -Value 'Bitty 0.0.1 (legacy)' `
      -PropertyType String -Force | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($legacyDeskShortcut)
    $link.TargetPath = Join-Path $legacyDir 'bitty.exe'
    $link.Save()
    $link = $shell.CreateShortcut($userDeskShortcut)
    $link.TargetPath = Join-Path $root 'notes.txt'
    $link.Save()
  }

  # --- run 1: the fixture must be cleaned, and only the Inno parts --------
  [Bitty.ShellNativeMethods]::Calls = 0
  $firstRun = Remove-BittyLegacyInnoSetup -CurrentInstallDir $currentDir -DesktopDir $desktop
  Assert-True ($firstRun -eq $true) 'first run reported no removals, so it saw nothing to clean'
  Assert-True ([Bitty.ShellNativeMethods]::Calls -gt 0) `
    'first run did not call SHChangeNotify (the spy never fired, so the second-run assert would be vacuous)'
  Assert-True (-not (Test-Path -LiteralPath (Join-Path $groupDir 'Bitty.lnk'))) 'legacy Start Menu Bitty.lnk survived'
  Assert-True (-not (Test-Path -LiteralPath (Join-Path $groupDir 'Uninstall Bitty.lnk'))) 'legacy Start Menu Uninstall Bitty.lnk survived'
  Assert-True (Test-Path -LiteralPath $userShortcut) 'a user-authored shortcut inside the Start Menu folder was deleted'
  Assert-True (-not (Test-Path -LiteralPath $legacyDir)) 'legacy install directory survived'
  Assert-True (Test-Path -LiteralPath (Join-Path $currentDir 'bitty.exe')) 'the current install was damaged'
  if ($onWindows) {
    Assert-True (-not (Test-Path -LiteralPath $legacyDeskShortcut)) 'legacy desktop shortcut pointing into the legacy directory survived'
    Assert-True (Test-Path -LiteralPath $userDeskShortcut) 'a user-authored desktop shortcut was deleted'
    Assert-True (-not (Test-Path -LiteralPath $legacyKey)) 'legacy uninstall key survived although its directory is gone'
  }
  Write-Host 'PASS: first run removed only what Inno created (user shortcut kept, legacy key dropped with the tree)'

  # --- run 2: a no-op, or the installer would churn the shell every run ---
  [Bitty.ShellNativeMethods]::Calls = 0
  $secondRun = Remove-BittyLegacyInnoSetup -CurrentInstallDir $currentDir -DesktopDir $desktop
  Assert-True ($secondRun -eq $false) 'second run reported removals; the cleanup is not idempotent'
  Assert-True ([Bitty.ShellNativeMethods]::Calls -eq 0) `
    "second run called SHChangeNotify $([Bitty.ShellNativeMethods]::Calls) time(s); a no-op run must not"
  Assert-True (Test-Path -LiteralPath $userShortcut) 'second run deleted the user-authored shortcut'
  Write-Host 'PASS: second run is a no-op (no removals, no SHChangeNotify)'

  # --- nested -InstallDir: the legacy tree holds the fresh binary --------
  $nestedAppData = Join-Path $root 'nested\appdata'
  $nestedLocalAppData = Join-Path $root 'nested\localappdata'
  $nestedLegacyDir = Join-Path $nestedLocalAppData 'Programs\Bitty'
  $nestedInstallDir = Join-Path $nestedLegacyDir 'bin'
  New-Item -ItemType Directory -Force -Path $nestedInstallDir, $nestedAppData | Out-Null
  Set-Content -LiteralPath (Join-Path $nestedInstallDir 'bitty.exe') -Value 'fresh payload'
  Set-Content -LiteralPath (Join-Path $nestedLegacyDir 'bitty.exe') -Value 'stale inno payload'
  $env:AppData = $nestedAppData
  $env:LocalAppData = $nestedLocalAppData
  $nestedKey = Get-BittyLegacyInnoUninstallKey
  if ($onWindows) {
    New-Item -Path $nestedKey -Force | Out-Null
  }
  $nestedRun = Remove-BittyLegacyInnoSetup -CurrentInstallDir $nestedInstallDir -DesktopDir (Join-Path $root 'nested\desktop')
  Assert-True (Test-Path -LiteralPath $nestedInstallDir) '-InstallDir nested inside the legacy directory was deleted'
  Assert-True (Test-Path -LiteralPath (Join-Path $nestedInstallDir 'bitty.exe')) 'the nested install lost the binary this run installed'
  Assert-True ($nestedRun -eq $false) 'nested-install run reported removals'
  if ($onWindows) {
    Assert-True (Test-Path -LiteralPath $nestedKey) 'legacy uninstall key was deleted while its directory survives'
  }
  Write-Host 'PASS: -InstallDir nested inside the legacy directory is never deleted'

  # --- failed removal (Windows only): the key must survive the warning ----
  if ($onWindows) {
    $lock = [System.IO.File]::Open(
      (Join-Path $nestedLegacyDir 'bitty.exe'),
      [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::None)
    try {
      $warnedRun = Remove-BittyLegacyInnoSetup -CurrentInstallDir (Join-Path $root 'elsewhere') -DesktopDir (Join-Path $root 'nested\desktop')
    } finally {
      $lock.Dispose()
    }
    Assert-True (Test-Path -LiteralPath $nestedLegacyDir) 'a directory held open was reported as removed'
    Assert-True (Test-Path -LiteralPath $nestedKey) 'legacy uninstall key was deleted although the directory removal only warned'
    Assert-True ($warnedRun -eq $false) 'a warned-about removal run reported removals'
    Write-Host 'PASS: uninstall key kept while a failed directory removal leaves the tree behind'
  } else {
    Write-Host 'SKIP: WScript.Shell shortcut resolution, HKCU uninstall keys, and the sharing-violation leg need Windows'
  }
} finally {
  $env:AppData = $savedAppData
  $env:LocalAppData = $savedLocalAppData
  if ($onWindows) {
    foreach ($key in @($legacyKey, $nestedKey)) {
      if ($key -ne '' -and (Test-Path -LiteralPath $key)) {
        Remove-Item -LiteralPath $key -Recurse -Force -ErrorAction SilentlyContinue
      }
    }
  }
  if (Test-Path -LiteralPath $root) {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
  }
}

Write-Host 'cleanup-legacy-inno-test: OK'
