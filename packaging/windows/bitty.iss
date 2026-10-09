; bitty.iss — Inno Setup 6 script for the Bitty Windows setup installer.
;
; CTX-1064 / GitHub issue #1810 ([P1] Windows: setup-style installer).
; Owner decisions are settled: 0.1.0 ships UNSIGNED (paid signing deferred
; past 0.2.0); the setup must offer Start Menu + Desktop shortcuts and write
; registry uninstall entries; Scoop stays portable on a separate track.
;
; Tech choice: Inno Setup 6 over WiX v4/v5 and NSIS. Rationale (also recorded
; in the PR body): the stock wizard already provides install/uninstall pages,
; Start Menu + opt-in Desktop shortcuts, and per-user/per-machine registry
; uninstall entries from declarative [Setup]/[Tasks]/[Files]/[Icons]
; sections; the compiler is a single ISCC.exe installed on windows-latest via
; Chocolatey with no paid infra and no .NET/WiX toolchain; NSIS would need
; hand-written registry/uninstall scripting for the same bar.
;
; Build inputs (never hardcoded here):
;   MyAppVersion — release version X.Y.Z, passed as
;     ISCC.exe /DMyAppVersion="<version>" /DMySourceDir="<stage>" bitty.iss
;     The release workflow resolves it from the tag, the workflow input, or
;     the Cargo workspace version, in that order.
;   MySourceDir  — absolute directory staging bitty.exe plus LICENSE,
;     README.md, and CHANGELOG.md (the portable-ZIP payload). Absolute
;     because relative Source paths resolve against the script directory.
;
; Unsigned 0.1.0: no SignTool is configured below (only a commented stub for
; the post-0.2.0 Azure Trusted Signing / OV revisit). The installer must NOT
; claim trust it does not have: SmartScreen shows the standard unsigned
; warning and the docs guidance is More info -> Run anyway. The release
; verify job asserts Get-AuthenticodeSignature reports NotSigned so a silent
; signing without updating this script and the README fails CI instead of
; shipping quietly.
;
; Registry: Inno writes the uninstall entry automatically under
; HKCU (per-user, PrivilegesRequired=lowest) or HKLM (per-machine)
; Software\Microsoft\Windows\CurrentVersion\Uninstall\<AppId>_is1 with
; DisplayName/DisplayVersion/UninstallString. No manual [Registry] PATH or
; file-association writes ship in 0.1.0; those stay deferred decisions.
#ifndef MyAppVersion
  #error "MyAppVersion is required: ISCC.exe /DMyAppVersion=X.Y.Z packaging/windows/bitty.iss"
#endif
#ifndef MySourceDir
  #define MySourceDir "."
#endif

#define MyAppName "Bitty"
#define MyAppPublisher "Bitty Terminal"
#define MyAppURL "https://github.com/bitty-terminal/bitty"

[Setup]
AppId={{3E8B4A2C-7F1D-4B6E-9A0C-5D2F8E1B7A40}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\Bitty
DefaultGroupName=Bitty
AllowNoIcons=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern
Compression=lzma2/ultra
SolidCompression=yes
SetupMutex=BittySetupMutex
UninstallDisplayName={#MyAppName} {#MyAppVersion}
UninstallDisplayIcon={app}\bitty.exe
OutputDir=dist
OutputBaseFilename=bitty-{#MyAppVersion}-windows-x86_64-setup
; Code signing stays a post-0.2.0 revisit (Azure Trusted Signing / OV cert).
; When signing lands, configure SignTool here and update the verify job's
; NotSigned assert plus packaging/README.md together. Example only:
; SignTool=signtool sign /tr http://timestamp.digicert.com /td sha256 /fd sha256 $f

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#MySourceDir}\bitty.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MySourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MySourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#MySourceDir}\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Bitty"; Filename: "{app}\bitty.exe"
Name: "{group}\Uninstall Bitty"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Bitty"; Filename: "{app}\bitty.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\bitty.exe"; Description: "{cm:LaunchProgram,Bitty}"; Flags: nowait postinstall skipifsilent
