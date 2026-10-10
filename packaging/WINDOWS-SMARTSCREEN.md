# Windows SmartScreen Guidance for the Unsigned 0.1.0 Installer

Part of: bitty-terminal/bitty#1866 | Task: CTX-1094 | Milestone: v0.1.0

## Why you see a warning

The 0.1.0 Windows setup installer (`bitty-<VERSION>-windows-x86_64-setup.exe`)
ships **unsigned**: no `SignTool` step is configured (only a commented stub for
the post-0.2.0 Azure Trusted Signing / OV revisit, per the owner decision
on #1810). Windows SmartScreen therefore cannot verify a publisher and shows the
standard unknown-publisher warning. This is expected, not a defect in the
download: the installer must not claim trust it does not have.

Verify before proceeding: the release page publishes a `.sha256` sidecar next
to the setup executable. Compare its checksum with your download
(`Get-FileHash .\bitty-<VERSION>-windows-x86_64-setup.exe -Algorithm SHA256`
in PowerShell) before clicking through.

## What the dialog looks like

Running the unsigned setup shows a blue **Windows protected your PC** dialog:

- Heading: `Windows protected your PC`
- Body: `Microsoft Defender SmartScreen prevented an unrecognized app from
starting. Running this app might put your PC at risk.`
- App line names the setup executable; publisher shows as
  **Unknown publisher**.

## Click-through steps (More info -> Run anyway)

1. In the **Windows protected your PC** dialog, click **More info**. The dialog
   expands to show the app name and **Unknown publisher**, plus a
   **Run anyway** button.
2. Click **Run anyway**. The Inno Setup wizard starts normally from there;
   the rest of the install (destination page, icons task, Start Menu group)
   is identical to a signed install.
3. If you declined: re-run the setup executable to get the dialog again.
   There is no partial install to clean up — declining runs nothing.

## What changes after 0.2.0 signing

Paid signing is deferred past 0.2.0. Once the installer is signed with a
trusted certificate, SmartScreen shows the publisher name instead of
**Unknown publisher**, and on an established reputation the warning stops
appearing. The `.sha256` sidecars remain as an independent check regardless
of signing.

## Paths that never show this dialog

- **Scoop**: ships the bare `bitty-*.exe` assets, never the setup installer,
  so it sidesteps SmartScreen by construction.
- **Portable ZIP**: `install.ps1` fetches the portable ZIP; extracting and
  running `bitty.exe` does not trigger the installer warning. (Windows may
  still show a first-run reputation check for the bare exe; that is separate
  from the installer dialog above.)
