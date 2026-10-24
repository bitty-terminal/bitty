# Windows Package Manager (winget)

Windows 10 (1809+) and Windows 11 include the Windows Package Manager (`winget`).
Bitty is packaged for the official community repository
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) under the
package identifier:

```text
BittyTerminal.Bitty
```

## User Installation

Install Bitty with `winget`:

```powershell
winget install BittyTerminal.Bitty
```

Or using the moniker:

```powershell
winget install bitty
```

To upgrade an existing installation:

```powershell
winget upgrade BittyTerminal.Bitty
```

## Manifest Architecture

The package targets the official Inno Setup 6 installer executable
(`bitty-<version>-windows-x86_64-setup.exe`), published on the global Cloudflare
R2 CDN (`https://cdn.bitty.run/releases/v<version>/`) with GitHub Releases
as the fallback.

In accordance with Windows Package Manager schema v1.6.0, manifests are
structured as a multi-file package under:

```text
manifests/b/BittyTerminal/Bitty/<version>/
├── BittyTerminal.Bitty.yaml              # Version manifest
├── BittyTerminal.Bitty.installer.yaml    # Installer architecture, URLs, hashes & switches
└── BittyTerminal.Bitty.locale.en-US.yaml # English metadata, descriptions & tags
```

### Installer Parameters

- **InstallerType**: `inno`
- **Scope**: `user` (installs to `{autopf}\Bitty` or user app data without mandatory elevation)
- **ProductCode**: `{3E8B4A2C-7F1D-4B6E-9A0C-5D2F8E1B7A40}_is1` (declared in `packaging/windows/bitty.iss`)
- **Silent Flags**: `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`

## Automation and Release Updates

Manifests can be generated automatically via `scripts/generate-winget-manifest.sh`:

```sh
scripts/generate-winget-manifest.sh --version 0.0.27 \
  --installer-sha256 6de07964fcf79c2b7410c80bbfe0001d920cde256473f8a4f28e980bc8e93ca8
```

Alternatively, upstream pull requests to `microsoft/winget-pkgs` can be submitted
using Microsoft's `wingetcreate` CLI tool:

```powershell
wingetcreate update BittyTerminal.Bitty `
  --urls https://cdn.bitty.run/releases/v0.0.27/bitty-0.0.27-windows-x86_64-setup.exe `
  --version 0.0.27
```
