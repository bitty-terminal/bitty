<#
.SYNOPSIS
  First-class Bitty installer for Windows (CTX-1047, #1847, #2926).

.DESCRIPTION
  Entry point (stable, served from the CDN):
    irm https://cdn.bitty.run/install/install.ps1 | iex

  Resolves the release through one pointer:
    https://cdn.bitty.run/install/latest.txt
  which holds TAG (for example v0.0.23) with a trailing newline. VERSION is
  TAG without the leading v and is what the portable ZIP name embeds.

  Fetch contract: literal per-OS/arch URL templates from the R2 layout
  decision (packaging/README.md, CTX-1056, closes #1794). Only TAG and
  VERSION resolve at install time:
    Windows x64:
      https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-windows-x86_64.zip
    Windows arm64:
      https://cdn.bitty.run/releases/<TAG>/bitty-aarch64-pc-windows-msvc.exe
  Each row has a <artifact>.sha256 sidecar at the same URL with .sha256
  appended, verified before installing (fail closed on mismatch). There are
  no signatures in 0.1.0: verification is hash-only (signing deferred past
  0.2.0 per #1810).

  Windows x64 fetches the portable ZIP (bitty.exe plus bitty.ico, LICENSE,
  README.md, CHANGELOG.md at the archive root, assembled by
  scripts/make-windows-zip.sh and verified by verify-windows-zip) and installs
  bitty.exe plus the display icon from it. Windows arm64 fetches the bare exe
  because the ZIP matrix builds x64 only; both arches install first-class,
  only the container differs until the ZIP matrix extends (recorded
  follow-up, not a parity gap). arm64 shortcuts fall back to the executable
  icon until #2929 embeds the mascot in bitty.exe.

  Scoop-portable parity: the Scoop manifest (generated at tag time into the
  external scoop-bucket repo) ships bare bitty exes from the same R2-backed
  CDN, while this script fetches the portable ZIP (x64) or bare exe (arm64)
  from that CDN. GitHub Releases carries the same bytes and is the documented
  fallback for both paths. Parity is payload parity — the same bitty.exe runs
  in both paths — not URL parity. Like Scoop, this script installs user-local
  with no admin rights and no Rust toolchain.

  Windows discoverability: after the smoke test the script creates a per-user
  Start Menu shortcut (Bitty.lnk), appends the install directory to the user
  Path in HKCU\Environment (broadcasting WM_SETTINGCHANGE so new shells
  resolve bitty), and registers a per-user app-list entry under
  HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Bitty so Bitty
  appears in Settings > Apps > Installed apps with its version and an
  Uninstall button. The entry points at a generated uninstall.ps1 next to
  bitty.exe, which removes the entry, the shortcut, the exact Path entry this
  installer added, and the install directory. Every action is best-effort: a
  failure warns and prints the manual step instead of failing the install.
  -NoShortcut, -NoPathUpdate, and -NoRegistry skip each action for scripted
  installs.

  FreeBSD is not a bootstrap target (manual tar.xz download); Linux/macOS
  must use install.sh (curl -fsSL https://cdn.bitty.run/install/install.sh | bash).

.PARAMETER Version
  Release version to install (X.Y.Z or vX.Y.Z). Default resolves latest.txt.
  Prerelease versions (containing -) are rejected: download manually.

.PARAMETER InstallDir
  Directory bitty.exe is installed into. Default is $env:LocalAppData\bitty\bin
  (user-local, no admin). The directory is created when missing.

.PARAMETER CdnBase
  CDN base override for tests only. Default https://cdn.bitty.run.

.PARAMETER Target
  Target override for tests only: x64 or arm64 (also accepts the full
  triple form x86_64-pc-windows-msvc / aarch64-pc-windows-msvc).

.PARAMETER DryRun
  Resolve and print the fetch/install plan without downloading or installing.

.PARAMETER PrintUrl
  Print the artifact URL and its .sha256 URL, then exit (no network fetch
  beyond the version pointer unless -Version was given).

.PARAMETER NoShortcut
  Skip the per-user Start Menu shortcut and print the manual step instead.

.PARAMETER NoPathUpdate
  Skip the user Path update and print the manual step instead.

.PARAMETER NoRegistry
  Skip the per-user app-list entry and print the manual step instead. The
  generated uninstall.ps1 is still written next to bitty.exe.

.EXAMPLE
  irm https://cdn.bitty.run/install/install.ps1 | iex

.EXAMPLE
  iex "& { $(irm https://cdn.bitty.run/install/install.ps1) } -Version 0.0.23 -InstallDir $env:USERPROFILE\bin"

.NOTES
  PowerShell 5.1 and 7+ compatible. No Rust toolchain needed. Fail-closed on
  checksum mismatch, unsupported arch, or non-Windows hosts.
#>
[CmdletBinding()]
param(
  [string]$Version = '',
  [string]$InstallDir = '',
  [string]$CdnBase = 'https://cdn.bitty.run',
  [string]$Target = '',
  [switch]$DryRun,
  [switch]$PrintUrl,
  [switch]$NoShortcut,
  [switch]$NoPathUpdate,
  [switch]$NoRegistry
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-BittyLog {
  param([string]$Message)
  Write-Host "install.ps1: $Message"
}

function Get-BittyStartMenuDir {
  # Per-user Start Menu folder; Windows Search only indexes shortcuts and
  # registered applications, so a bare exe elsewhere stays invisible.
  $appData = "$env:AppData"
  if ([string]::IsNullOrWhiteSpace($appData)) {
    return ''
  }
  return Join-Path $appData 'Microsoft\Windows\Start Menu\Programs'
}

function New-BittyShortcut {
  param([string]$TargetExe, [string]$WorkingDirectory, [string]$IconSource)
  $startMenuDir = Get-BittyStartMenuDir
  if ($startMenuDir -eq '') {
    Write-BittyLog 'warning: AppData is not set; skipping the Start Menu shortcut'
    Write-BittyLog "manual step: create a shortcut to $TargetExe yourself"
    return
  }
  # IconSource is the shipped bitty.ico when the payload carries one and the
  # executable otherwise; #2929 embeds the mascot in bitty.exe, at which point
  # the exe becomes the single icon source everywhere.
  if ([string]::IsNullOrWhiteSpace($IconSource)) {
    $IconSource = $TargetExe
  }
  $shortcutPath = Join-Path $startMenuDir 'Bitty.lnk'
  try {
    if (-not (Test-Path $startMenuDir)) {
      New-Item -ItemType Directory -Path $startMenuDir -Force | Out-Null
    }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $TargetExe
    $shortcut.WorkingDirectory = $WorkingDirectory
    $shortcut.IconLocation = "$IconSource,0"
    $shortcut.Description = 'Bitty terminal'
    # Save() writes in place, so a re-run replaces the existing shortcut.
    $shortcut.Save()
    Write-BittyLog "created Start Menu shortcut: $shortcutPath (icon: $IconSource)"
  } catch {
    Write-BittyLog "warning: could not create Start Menu shortcut ${shortcutPath}: $($_.Exception.Message)"
    Write-BittyLog "manual step: create a shortcut to $TargetExe in $startMenuDir"
  }
}

function Send-BittySettingChange {
  # Broadcast WM_SETTINGCHANGE so Explorer and new shells reload the user
  # environment after the HKCU\Environment write.
  if (-not ('Bitty.NativeMethods' -as [type])) {
    Add-Type -Namespace 'Bitty' -Name 'NativeMethods' -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Auto)]
public static extern System.IntPtr SendMessageTimeout(
    System.IntPtr hWnd, uint msg, System.UIntPtr wParam, string lParam,
    uint flags, uint timeout, out System.UIntPtr result);
'@
  }
  $wmSettingChange = 0x001A
  $smtAbortIfHung = 0x0002
  $broadcast = [System.IntPtr]0xFFFF
  $result = [System.UIntPtr]::Zero
  $sent = [Bitty.NativeMethods]::SendMessageTimeout(
    $broadcast, $wmSettingChange, [System.UIntPtr]::Zero, 'Environment', $smtAbortIfHung, 5000, [ref]$result)
  if ($sent -eq [System.IntPtr]::Zero) {
    throw 'SendMessageTimeout returned 0 (no window accepted the broadcast)'
  }
}

function Add-BittyUserPath {
  param([string]$Directory)
  $current = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($null -eq $current) {
    $current = ''
  }
  $target = $Directory.TrimEnd([char[]]@('\', '/'))
  # Idempotent: compare case-insensitively and ignore trailing separators so
  # a re-run never duplicates the entry.
  foreach ($entry in @($current -split ';')) {
    $candidate = $entry.Trim().TrimEnd([char[]]@('\', '/'))
    if ($candidate -ne '' -and $candidate.Equals($target, [System.StringComparison]::OrdinalIgnoreCase)) {
      Write-BittyLog "user Path already contains $Directory"
      # Pre-existing entry: the uninstaller must leave it alone unless an
      # earlier Bitty install recorded that it appended it.
      return $false
    }
  }
  $updated = $Directory
  if ($current.Trim() -ne '') {
    $updated = "$($current.TrimEnd(';'));$Directory"
  }
  Set-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Value $updated -Type ExpandString
  Write-BittyLog "added $Directory to the user Path"
  try {
    Send-BittySettingChange
    Write-BittyLog 'broadcast WM_SETTINGCHANGE; new shells can run bitty'
  } catch {
    Write-BittyLog "warning: could not broadcast WM_SETTINGCHANGE: $($_.Exception.Message)"
    Write-BittyLog 'new shells may need a fresh sign-in to see the user Path update'
  }
  return $true
}

function Get-BittyUninstallKeyPath {
  # Per-user app-list entry (Settings > Apps > Installed apps). HKCU only:
  # the installer never elevates, so HKLM stays off limits (#2926).
  return 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Bitty'
}

function Get-BittyUninstallHost {
  # A registry-invoked uninstaller cannot rely on PATH, on PowerShell 7 being
  # installed, or on the bootstrap script still existing: use the inbox
  # Windows PowerShell that ships with every supported Windows, derived from
  # SystemRoot instead of a literal drive.
  $systemRoot = "$env:SystemRoot"
  if (-not [string]::IsNullOrWhiteSpace($systemRoot)) {
    $candidate = Join-Path $systemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
      return $candidate
    }
  }
  $psHome = "$PSHOME"
  if (-not [string]::IsNullOrWhiteSpace($psHome)) {
    $candidate = Join-Path $psHome 'powershell.exe'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
      return $candidate
    }
  }
  throw 'could not locate powershell.exe for the uninstall entry'
}

function Get-BittyInstallState {
  param([string]$Directory)
  $statePath = Join-Path $Directory 'bitty-install-state.json'
  if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
    return $null
  }
  try {
    return (Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json)
  } catch {
    Write-BittyLog "warning: could not read install state ${statePath}: $($_.Exception.Message)"
    return $null
  }
}

function Test-BittyStatePathAdded {
  param($State)
  if ($null -eq $State) {
    return $false
  }
  # StrictMode forbids reading a missing property, so probe the member list.
  $property = $State.PSObject.Properties['pathAdded']
  if ($null -eq $property) {
    return $false
  }
  return ($property.Value -eq $true)
}

function Save-BittyInstallState {
  param([string]$Directory, [string]$InstalledVersion, [bool]$PathAdded)
  $statePath = Join-Path $Directory 'bitty-install-state.json'
  # Records whether THIS installer appended the user Path entry, so the
  # generated uninstaller removes exactly that entry and never one the user
  # owned before installing (#2926).
  $state = [ordered]@{
    schemaVersion = 1
    version = $InstalledVersion
    pathAdded = $PathAdded
  }
  $state | ConvertTo-Json -Compress | Set-Content -LiteralPath $statePath -Encoding ASCII
}

function Get-BittyInstallSizeKb {
  param([string]$Directory)
  $bytes = 0
  foreach ($file in @(Get-ChildItem -LiteralPath $Directory -File -ErrorAction SilentlyContinue)) {
    $bytes += [int64]$file.Length
  }
  if ($bytes -le 0) {
    return 1
  }
  # Windows shows EstimatedSize in KB (DWORD); round up so a non-empty
  # install never reports 0 KB.
  return [int][Math]::Ceiling($bytes / 1024.0)
}

function Get-BittyUninstallerTemplate {
  # Single-quoted here-string: New-BittyUninstaller substitutes the __TOKENS__
  # below, so the generated script keeps its own $variables verbatim. The
  # generated file is self-contained because the registry launcher runs it
  # long after the `irm | iex` pipeline is gone (#2926).
  return @'
<#
.SYNOPSIS
  Uninstaller for Bitty __VERSION__, generated by install.ps1.

.DESCRIPTION
  Invoked from the per-user app-list entry
  HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Bitty
  (Settings > Apps > Installed apps > Bitty > Uninstall), or by hand:

    powershell -NoProfile -ExecutionPolicy Bypass -File <this file>

  Removes the app-list entry, the Start Menu shortcut, the user Path entry
  the installer added (never one the user already had), and the install
  directory itself. Nothing outside __INSTALL_DIR__ is touched.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$InstallDir = '__INSTALL_DIR__'
$UninstallKey = '__UNINSTALL_KEY__'
$InstalledVersion = '__VERSION__'
$StatePath = Join-Path $InstallDir 'bitty-install-state.json'

function Write-BittyUninstallLog {
  param([string]$Message)
  Write-Host "uninstall.ps1: $Message"
}

function Send-BittySettingChange {
  # Explorer and new shells only reload the user environment after this
  # broadcast; failure is a warning, never a failed uninstall.
  if (-not ('Bitty.NativeMethods' -as [type])) {
    $nativeSource = @(
      '[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Auto)]'
      'public static extern System.IntPtr SendMessageTimeout('
      '    System.IntPtr hWnd, uint msg, System.UIntPtr wParam, string lParam,'
      '    uint flags, uint timeout, out System.UIntPtr result);'
    ) -join [Environment]::NewLine
    Add-Type -Namespace 'Bitty' -Name 'NativeMethods' -MemberDefinition $nativeSource
  }
  $wmSettingChange = 0x001A
  $smtAbortIfHung = 0x0002
  $broadcast = [System.IntPtr]0xFFFF
  $result = [System.UIntPtr]::Zero
  $sent = [Bitty.NativeMethods]::SendMessageTimeout(
    $broadcast, $wmSettingChange, [System.UIntPtr]::Zero, 'Environment', $smtAbortIfHung, 5000, [ref]$result)
  if ($sent -eq [System.IntPtr]::Zero) {
    throw 'SendMessageTimeout returned 0 (no window accepted the broadcast)'
  }
}

function Remove-BittyUserPath {
  param([string]$Directory)
  $current = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($null -eq $current -or $current.Trim() -eq '') {
    Write-BittyUninstallLog 'user Path is empty; nothing to remove'
    return
  }
  $target = $Directory.TrimEnd([char[]]@('\', '/'))
  $kept = @()
  $removed = 0
  foreach ($part in @($current -split ';')) {
    $candidate = $part.Trim().TrimEnd([char[]]@('\', '/'))
    if ($candidate -ne '' -and $candidate.Equals($target, [System.StringComparison]::OrdinalIgnoreCase)) {
      $removed++
      continue
    }
    # Every other entry, including empty ones, is preserved verbatim.
    $kept += $part
  }
  if ($removed -eq 0) {
    Write-BittyUninstallLog "user Path does not contain $Directory; leaving it unchanged"
    return
  }
  $updated = ($kept -join ';')
  if ($updated -eq $current) {
    return
  }
  Set-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Value $updated -Type ExpandString
  Write-BittyUninstallLog "removed $Directory from the user Path"
  try {
    Send-BittySettingChange
    Write-BittyUninstallLog 'broadcast WM_SETTINGCHANGE'
  } catch {
    Write-BittyUninstallLog "warning: could not broadcast WM_SETTINGCHANGE: $($_.Exception.Message)"
  }
}

Write-BittyUninstallLog "removing Bitty $InstalledVersion from $InstallDir"
# Run from outside the install directory so Remove-Item can delete it; the
# registry launcher does not set a working directory.
try {
  Set-Location -LiteralPath ([System.IO.Path]::GetTempPath())
} catch {
  Write-BittyUninstallLog "warning: could not change directory: $($_.Exception.Message)"
}

$programs = [Environment]::GetFolderPath('Programs')
if (-not [string]::IsNullOrWhiteSpace($programs)) {
  $shortcut = Join-Path $programs 'Bitty.lnk'
  if (Test-Path -LiteralPath $shortcut) {
    Remove-Item -LiteralPath $shortcut -Force
    Write-BittyUninstallLog "removed Start Menu shortcut: $shortcut"
  }
}

$pathAdded = $false
if (Test-Path -LiteralPath $StatePath -PathType Leaf) {
  try {
    $state = Get-Content -LiteralPath $StatePath -Raw | ConvertFrom-Json
    $property = $state.PSObject.Properties['pathAdded']
    if ($null -ne $property -and $property.Value -eq $true) {
      $pathAdded = $true
    }
  } catch {
    Write-BittyUninstallLog "warning: could not read ${StatePath}: $($_.Exception.Message)"
  }
}
if ($pathAdded) {
  try {
    Remove-BittyUserPath -Directory $InstallDir
  } catch {
    Write-BittyUninstallLog "warning: could not update the user Path: $($_.Exception.Message)"
  }
} else {
  Write-BittyUninstallLog 'user Path unchanged (the installer did not add it)'
}

if (Test-Path -LiteralPath $UninstallKey) {
  Remove-Item -LiteralPath $UninstallKey -Recurse -Force
  Write-BittyUninstallLog "removed app-list entry: $UninstallKey"
}

if (Test-Path -LiteralPath $InstallDir) {
  $attempt = 0
  while ($true) {
    try {
      Remove-Item -LiteralPath $InstallDir -Recurse -Force
      break
    } catch {
      $attempt++
      if ($attempt -ge 2) {
        throw "could not remove ${InstallDir}: $($_.Exception.Message)"
      }
      # One bounded retry covers an antivirus scan still holding bitty.exe.
      Start-Sleep -Milliseconds 500
    }
  }
  Write-BittyUninstallLog "removed $InstallDir"
}
Write-BittyUninstallLog 'uninstall complete'
'@
}

function New-BittyUninstaller {
  param([string]$Directory, [string]$InstalledVersion)
  $uninstallerPath = Join-Path $Directory 'uninstall.ps1'
  # Single-quote the substituted values: a path may legally contain ' and the
  # generated script embeds it inside single-quoted PowerShell strings.
  $escapedDir = $Directory.Replace("'", "''")
  $escapedVersion = $InstalledVersion.Replace("'", "''")
  $content = Get-BittyUninstallerTemplate
  $content = $content.Replace('__INSTALL_DIR__', $escapedDir)
  $content = $content.Replace('__UNINSTALL_KEY__', (Get-BittyUninstallKeyPath))
  $content = $content.Replace('__VERSION__', $escapedVersion)
  # UTF-16LE with BOM: both Windows PowerShell 5.1 and PowerShell 7 read it
  # unambiguously, so a non-ASCII install path (localized user names) survives
  # the round trip.
  Set-Content -LiteralPath $uninstallerPath -Value $content -Encoding Unicode
  return $uninstallerPath
}

function New-BittyUninstallEntry {
  param(
    [string]$InstalledVersion,
    [string]$InstallLocation,
    [string]$IconPath,
    [string]$UninstallCommand,
    [int]$EstimatedSizeKb
  )
  $keyPath = Get-BittyUninstallKeyPath
  if (-not (Test-Path -LiteralPath $keyPath)) {
    New-Item -Path $keyPath -Force | Out-Null
  }
  # One fixed key path and -Force on every value: a re-install updates
  # DisplayVersion/EstimatedSize in place and never duplicates the entry.
  New-ItemProperty -Path $keyPath -Name 'DisplayName' -Value 'Bitty' -PropertyType String -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'DisplayVersion' -Value $InstalledVersion -PropertyType String -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'Publisher' -Value 'Bitty Terminal' -PropertyType String -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'InstallLocation' -Value $InstallLocation -PropertyType String -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'DisplayIcon' -Value $IconPath -PropertyType String -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'EstimatedSize' -Value $EstimatedSizeKb -PropertyType DWord -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'NoModify' -Value 1 -PropertyType DWord -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'NoRepair' -Value 1 -PropertyType DWord -Force | Out-Null
  New-ItemProperty -Path $keyPath -Name 'UninstallString' -Value $UninstallCommand -PropertyType String -Force | Out-Null
}

function Get-BittyTag {
  param([string]$RequestedVersion, [string]$Base)
  if ($RequestedVersion -ne '') {
    $raw = $RequestedVersion.Trim() -replace "`r", '' -replace "`n", ''
    if ($raw.Contains('-')) {
      throw "prerelease versions are not served by the bootstrap scripts ($raw); download the release asset manually"
    }
    $bare = $raw.TrimStart('v')
    if ($bare -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
      throw "version must look like X.Y.Z, got: $RequestedVersion"
    }
    return "v$bare"
  }
  $latestUrl = "$Base/install/latest.txt"
  try {
    $response = Invoke-WebRequest -Uri $latestUrl -UseBasicParsing -TimeoutSec 30
  } catch {
    throw "could not fetch version pointer ${latestUrl}: $($_.Exception.Message)"
  }
  # Windows PowerShell returns a string, PowerShell 7 can return bytes for a
  # text/plain response; decode before trimming so the version pointer works
  # on both hosts.
  $content = $response.Content
  if ($content -is [byte[]]) {
    $content = [System.Text.Encoding]::UTF8.GetString($content)
  }
  $tag = "$content".Trim() -replace "`r", '' -replace "`n", ''
  $tag = $tag.Trim()
  if ($tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "version pointer $latestUrl returned unexpected content: $tag"
  }
  return $tag
}

function Get-BittyTarget {
  param([string]$Override)
  if ($Override -ne '') {
    $norm = $Override.Trim().ToLowerInvariant()
    if ($norm -in @('x64', 'x86_64', 'x86_64-pc-windows-msvc', 'windows-x86_64')) {
      return 'x64'
    }
    if ($norm -in @('arm64', 'aarch64', 'aarch64-pc-windows-msvc')) {
      return 'arm64'
    }
    throw "unsupported -Target override: $Override (expect x64 or arm64)"
  }
  $archName = ''
  try {
    $archName = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
  } catch {
    $archName = ''
  }
  if ([string]::IsNullOrWhiteSpace($archName)) {
    $archName = "$env:PROCESSOR_ARCHITECTURE"
  }
  $archName = "$archName".Trim()
  if ($archName -match '^(?i)(x64|amd64|x86_64)$') {
    return 'x64'
  }
  if ($archName -match '^(?i)(arm64|aarch64)$') {
    return 'arm64'
  }
  # RuntimeInformation returns 'X64'/'Arm64' enum names; fall back to env text.
  $envArch = "$env:PROCESSOR_ARCHITECTURE".ToUpperInvariant()
  if ($envArch -eq 'AMD64') {
    return 'x64'
  }
  if ($envArch -eq 'ARM64') {
    return 'arm64'
  }
  throw "unsupported Windows arch: $archName (bootstrap serves x64 and arm64)"
}

function Get-BittyArtifact {
  param([string]$ShortTarget, [string]$VersionBare)
  if ($ShortTarget -eq 'x64') {
    return "bitty-$VersionBare-windows-x86_64.zip"
  }
  if ($ShortTarget -eq 'arm64') {
    return 'bitty-aarch64-pc-windows-msvc.exe'
  }
  throw "no bootstrap artifact for target: $ShortTarget"
}

# Non-Windows hosts fail closed with guidance (PowerShell Core runs cross-platform).
$onWindows = $true
try {
  $onWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows)
} catch {
  if ("$env:OS" -ne 'Windows_NT') {
    $onWindows = $false
  }
}
if (-not $onWindows) {
  throw 'install.ps1 serves Windows only; on Linux/macOS run: curl -fsSL https://cdn.bitty.run/install/install.sh | bash'
}

$CdnBase = "$CdnBase".TrimEnd('/')
if ([string]::IsNullOrWhiteSpace($CdnBase)) {
  throw 'CDN base is empty'
}

$tag = Get-BittyTag -RequestedVersion $Version -Base $CdnBase
$versionBare = $tag.TrimStart('v')
$shortTarget = Get-BittyTarget -Override $Target
$artifact = Get-BittyArtifact -ShortTarget $shortTarget -VersionBare $versionBare
$url = "$CdnBase/releases/$tag/$artifact"
$shaUrl = "$url.sha256"

if ($PrintUrl) {
  Write-Output $url
  Write-Output $shaUrl
  return
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
  $localAppData = "$env:LocalAppData"
  if ([string]::IsNullOrWhiteSpace($localAppData)) {
    throw 'LocalAppData is not set; pass -InstallDir explicitly'
  }
  $InstallDir = Join-Path $localAppData 'bitty\bin'
}
# Absolute so the shortcut target, working directory, and Path entry stay
# valid from any shell.
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)

$kind = 'exe'
if ($shortTarget -eq 'x64') {
  $kind = 'zip'
}

if ($DryRun) {
  Write-BittyLog "dry-run: version pointer: $CdnBase/install/latest.txt"
  Write-BittyLog "dry-run: TAG=$tag VERSION=$versionBare TARGET=$shortTarget KIND=$kind"
  Write-BittyLog "dry-run: artifact URL: $url"
  Write-BittyLog "dry-run: sidecar URL: $shaUrl"
  Write-BittyLog "dry-run: install dir: $InstallDir"
  if ($NoShortcut) {
    Write-BittyLog 'dry-run: Start Menu shortcut skipped (-NoShortcut); create a shortcut to bitty.exe manually'
  } else {
    $startMenuDir = Get-BittyStartMenuDir
    if ($startMenuDir -eq '') {
      Write-BittyLog 'dry-run: Start Menu shortcut unavailable (AppData is not set)'
    } else {
      Write-BittyLog "dry-run: would create Start Menu shortcut: $(Join-Path $startMenuDir 'Bitty.lnk')"
    }
  }
  if ($NoPathUpdate) {
    Write-BittyLog "dry-run: user Path update skipped (-NoPathUpdate); add $InstallDir manually"
  } else {
    Write-BittyLog "dry-run: would add to the user Path: $InstallDir"
  }
  if ($NoRegistry) {
    Write-BittyLog 'dry-run: app-list entry skipped (-NoRegistry); remove the install dir and shortcut manually'
  } else {
    Write-BittyLog "dry-run: would write the uninstaller: $(Join-Path $InstallDir 'uninstall.ps1')"
    Write-BittyLog "dry-run: would register the app-list entry: $(Get-BittyUninstallKeyPath) (Bitty $versionBare)"
  }
  Write-BittyLog 'dry-run: would download, verify SHA256, and install bitty.exe (no changes made)'
  return
}

# A re-install keeps the earlier Path provenance: if a previous run appended
# the Path entry, uninstall must still remove exactly that entry even when
# this run skips the Path update.
$pathAdded = $false
if (Test-BittyStatePathAdded -State (Get-BittyInstallState -Directory $InstallDir)) {
  $pathAdded = $true
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ('bitty-install-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
  Write-BittyLog "resolving bitty $tag for Windows $shortTarget"
  $artifactPath = Join-Path $stage $artifact
  $sidecarPath = "$artifactPath.sha256"
  Write-BittyLog "fetching $url"
  Invoke-WebRequest -Uri $url -OutFile $artifactPath -UseBasicParsing -TimeoutSec 120
  Write-BittyLog "fetching $shaUrl"
  Invoke-WebRequest -Uri $shaUrl -OutFile $sidecarPath -UseBasicParsing -TimeoutSec 60
  $sidecarText = (Get-Content -Path $sidecarPath -Raw).Trim() -split '\s+' | Select-Object -First 1
  $sidecarText = "$sidecarText".Trim()
  if ($sidecarText -notmatch '^[0-9a-fA-F]{64}$') {
    throw "sidecar $shaUrl did not yield a 64-hex digest"
  }
  $actual = (Get-FileHash -Path $artifactPath -Algorithm SHA256).Hash.Trim()
  if ($actual.ToLowerInvariant() -ne $sidecarText.ToLowerInvariant()) {
    throw "checksum mismatch for $artifact (expected $sidecarText, got $actual)"
  }
  Write-BittyLog "checksum OK: $artifact"

  if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  }
  $destExe = Join-Path $InstallDir 'bitty.exe'
  $destIcon = Join-Path $InstallDir 'bitty.ico'
  if ($kind -eq 'zip') {
    $extractDir = Join-Path $stage 'unpacked'
    New-Item -ItemType Directory -Path $extractDir | Out-Null
    Expand-Archive -Path $artifactPath -DestinationPath $extractDir -Force
    $rootExe = Join-Path $extractDir 'bitty.exe'
    if (-not (Test-Path $rootExe -PathType Leaf)) {
      throw "portable ZIP missing bitty.exe at the archive root"
    }
    Copy-Item -Path $rootExe -Destination $destExe -Force
    # The portable ZIP carries the display icon (bitty.ico) at the archive
    # root; keep it next to the exe so the shortcut and the app-list entry can
    # point at it. Older archives without one simply keep the exe icon.
    $rootIcon = Join-Path $extractDir 'bitty.ico'
    if (Test-Path -LiteralPath $rootIcon -PathType Leaf) {
      Copy-Item -Path $rootIcon -Destination $destIcon -Force
    }
  } else {
    Copy-Item -Path $artifactPath -Destination $destExe -Force
  }
  Write-BittyLog "installed bitty to $destExe"
  # One icon source for the shortcut and the app-list DisplayIcon: the shipped
  # .ico when present, the executable otherwise (#2929 will make bitty.exe the
  # single source by embedding the mascot).
  $iconSource = $destExe
  if (Test-Path -LiteralPath $destIcon -PathType Leaf) {
    $iconSource = $destIcon
  }
  Write-BittyLog "icon source: $iconSource"
  $versionOutput = & $destExe --version 2>&1
  if ($LASTEXITCODE -ne 0) {
    throw "install smoke failed: $destExe --version exited $LASTEXITCODE"
  }
  Write-BittyLog "smoke OK: $versionOutput"
  if ($NoShortcut) {
    Write-BittyLog 'skipping the Start Menu shortcut (-NoShortcut)'
    Write-BittyLog "manual step: create a shortcut to $destExe in the Start Menu"
  } else {
    try {
      New-BittyShortcut -TargetExe $destExe -WorkingDirectory $InstallDir -IconSource $iconSource
    } catch {
      Write-BittyLog "warning: could not create the Start Menu shortcut: $($_.Exception.Message)"
      Write-BittyLog "manual step: create a shortcut to $destExe in the Start Menu"
    }
  }
  if ($NoPathUpdate) {
    Write-BittyLog 'skipping the user Path update (-NoPathUpdate)'
    Write-BittyLog "manual step: add $InstallDir to your user Path"
  } else {
    try {
      if (Add-BittyUserPath -Directory $InstallDir) {
        $pathAdded = $true
      }
    } catch {
      Write-BittyLog "warning: could not update the user Path: $($_.Exception.Message)"
      Write-BittyLog "manual step: add $InstallDir to your user Path"
    }
  }
  # The uninstaller and the install state live next to bitty.exe: the registry
  # launches the uninstaller long after this process exits, and the state
  # records whether this installer owns the user Path entry (so uninstall
  # removes exactly that entry and never a pre-existing one).
  $uninstallerPath = Join-Path $InstallDir 'uninstall.ps1'
  $uninstallerReady = $false
  try {
    $uninstallerPath = New-BittyUninstaller -Directory $InstallDir -InstalledVersion $versionBare
    $uninstallerReady = $true
  } catch {
    Write-BittyLog "warning: could not write the uninstaller: $($_.Exception.Message)"
  }
  try {
    Save-BittyInstallState -Directory $InstallDir -InstalledVersion $versionBare -PathAdded $pathAdded
  } catch {
    Write-BittyLog "warning: could not write the install state: $($_.Exception.Message)"
  }
  if ($NoRegistry) {
    Write-BittyLog 'skipping the app-list entry (-NoRegistry)'
    if ($uninstallerReady) {
      Write-BittyLog "manual step: run $uninstallerPath to uninstall"
    } else {
      Write-BittyLog "manual step: delete $InstallDir and its Start Menu shortcut"
    }
  } elseif (-not $uninstallerReady) {
    Write-BittyLog 'skipping the app-list entry: the uninstaller is missing'
    Write-BittyLog "manual step: delete $InstallDir and its Start Menu shortcut"
  } else {
    try {
      $hostExe = Get-BittyUninstallHost
      $uninstallCommand = '"' + $hostExe + '" -NoProfile -ExecutionPolicy Bypass -File "' + $uninstallerPath + '"'
      # DisplayIcon accepts "<file>,<index>"; quoting the file keeps paths with
      # spaces or commas intact.
      $iconValue = '"' + $iconSource + '",0'
      $sizeKb = Get-BittyInstallSizeKb -Directory $InstallDir
      New-BittyUninstallEntry -InstalledVersion $versionBare -InstallLocation $InstallDir `
        -IconPath $iconValue -UninstallCommand $uninstallCommand -EstimatedSizeKb $sizeKb
      Write-BittyLog "registered app-list entry: $(Get-BittyUninstallKeyPath) (Bitty $versionBare)"
    } catch {
      Write-BittyLog "warning: could not register the app-list entry: $($_.Exception.Message)"
      Write-BittyLog "manual step: delete $InstallDir and its Start Menu shortcut"
    }
  }
} finally {
  if (Test-Path $stage) {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
  }
}
