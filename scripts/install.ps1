<#
.SYNOPSIS
  First-class Bitty installer for Windows (CTX-1047, #1847).

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

  Windows x64 fetches the portable ZIP (bitty.exe plus LICENSE, README.md,
  CHANGELOG.md at the archive root, assembled by scripts/make-windows-zip.sh
  and verified by verify-windows-zip) and installs bitty.exe from it.
  Windows arm64 fetches the bare exe because the ZIP matrix builds x64 only;
  both arches install first-class, only the container differs until the ZIP
  matrix extends (recorded follow-up, not a parity gap).

  Scoop-portable parity: the Scoop manifest (generated at tag time into the
  external scoop-bucket repo) ships bare bitty exes from the same R2-backed
  CDN, while this script fetches the portable ZIP (x64) or bare exe (arm64)
  from that CDN. GitHub Releases carries the same bytes and is the documented
  fallback for both paths. Parity is payload parity — the same bitty.exe runs
  in both paths — not URL parity. Like Scoop, this script installs user-local
  with no admin rights and no Rust toolchain.

  Windows discoverability: after the smoke test the script creates a per-user
  Start Menu shortcut (Bitty.lnk) and appends the install directory to the
  user Path in HKCU\Environment, broadcasting WM_SETTINGCHANGE so new shells
  resolve bitty. Both actions are best-effort: a failure warns and prints the
  manual step instead of failing the install. -NoShortcut and -NoPathUpdate
  skip each action for scripted installs.

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
  [switch]$NoPathUpdate
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
  param([string]$TargetExe, [string]$WorkingDirectory)
  $startMenuDir = Get-BittyStartMenuDir
  if ($startMenuDir -eq '') {
    Write-BittyLog 'warning: AppData is not set; skipping the Start Menu shortcut'
    Write-BittyLog "manual step: create a shortcut to $TargetExe yourself"
    return
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
    $shortcut.IconLocation = "$TargetExe,0"
    $shortcut.Description = 'Bitty terminal'
    # Save() writes in place, so a re-run replaces the existing shortcut.
    $shortcut.Save()
    Write-BittyLog "created Start Menu shortcut: $shortcutPath"
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
      return
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
  Write-BittyLog 'dry-run: would download, verify SHA256, and install bitty.exe (no changes made)'
  return
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
  if ($kind -eq 'zip') {
    $extractDir = Join-Path $stage 'unpacked'
    New-Item -ItemType Directory -Path $extractDir | Out-Null
    Expand-Archive -Path $artifactPath -DestinationPath $extractDir -Force
    $rootExe = Join-Path $extractDir 'bitty.exe'
    if (-not (Test-Path $rootExe -PathType Leaf)) {
      throw "portable ZIP missing bitty.exe at the archive root"
    }
    Copy-Item -Path $rootExe -Destination $destExe -Force
  } else {
    Copy-Item -Path $artifactPath -Destination $destExe -Force
  }
  Write-BittyLog "installed bitty to $destExe"
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
      New-BittyShortcut -TargetExe $destExe -WorkingDirectory $InstallDir
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
      Add-BittyUserPath -Directory $InstallDir
    } catch {
      Write-BittyLog "warning: could not update the user Path: $($_.Exception.Message)"
      Write-BittyLog "manual step: add $InstallDir to your user Path"
    }
  }
} finally {
  if (Test-Path $stage) {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
  }
}
