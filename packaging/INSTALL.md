# Install Bitty

Part of: bitty-terminal/bitty#1847 | Task: CTX-1097 | Milestone: v0.1.0

Every supported install channel in one page. Supported means
release-matrix-verified: a CI-built binary is not support evidence (see
`packaging/README.md` `Support evidence`). All channels verify downloads
before installing and fail closed on mismatch; the Debian/Ubuntu APT channel
verifies cryptographic GPG signatures on repository metadata (`InRelease`),
while other channels remain hash-only (Windows binary signing deferred past 0.2.0).

## One-line installers (recommended)

Linux and macOS:

```sh
curl -fsSL https://cdn.bitty.run/install/install.sh | bash
```

With an explicit version:

```sh
curl -fsSL https://cdn.bitty.run/install/install.sh | bash -s -- --version 0.0.23
```

Windows (PowerShell):

```powershell
irm https://cdn.bitty.run/install/install.ps1 | iex
```

Both scripts resolve the release through one pointer,
`https://cdn.bitty.run/install/latest.txt` (holds the TAG, e.g.
`v0.0.23`), fetch the per-OS artifact plus its `.sha256` sidecar from
`https://cdn.bitty.run/releases/<TAG>/`, verify the hash, and install.
No Rust toolchain is needed — only `curl` + `sha256sum` (Linux/macOS) or
PowerShell 5.1+ (Windows). Failing closed: hash mismatch, unknown
OS/arch, and (on Windows) an unsigned-installer SmartScreen dialog, which is
expected — see `WINDOWS-SMARTSCREEN.md`.

What each script installs:

- Linux x86_64/aarch64 glibc and x86_64 musl: the versioned `.tar.zst`
  bundle (binary, desktop entry, icons, AppStream metainfo).
- macOS x86_64/arm64: the bare per-arch binary (the DMG is interactive and
  unsuitable for headless install; the same binary is the Homebrew fetch
  artifact).
- Windows x64: the portable ZIP (`bitty.exe` + LICENSE/README/CHANGELOG).
  Windows arm64: the bare exe (the ZIP matrix builds x64 only).
- FreeBSD (Tier 2): not a bootstrap target — manual download from the
  release page instead.

## Package managers

- **Arch Linux (AUR)**: `paru -S bitty-bin` (prebuilt, recommended) or
  `paru -S bitty` (build from source). They conflict; install one. `bitty-bin`
  downloads from the R2-backed CDN
  (`https://cdn.bitty.run/releases/<TAG>/bitty-x86_64-unknown-linux-gnu`) and
  verifies the sha256 in the recipe; the GitHub Releases URL is the documented
  fallback. Only a tag-time published recipe counts as supported (both
  publishes are gated on secrets). See the top-level README `Install` section.
- **Homebrew**: only a tag-time published formula in the external tap
  `bitty-terminal/homebrew-tap` counts as supported (no in-repo copy); a
  CI-built macOS binary is not support evidence. The formula downloads from
  the R2-backed CDN and embeds the published sha256, with the GitHub Releases
  URL kept as a documented fallback.
- **Scoop**: only a tag-time published manifest in the external bucket
  `bitty-terminal/scoop-bucket` counts as supported; it ships the bare
  exes, sidestepping the setup-installer SmartScreen dialog. Manifest URLs and
  `autoupdate` point at the R2-backed CDN with the published sha256 hash; its
  `notes` field documents the GitHub Releases fallback. Windows may still show
  a first-run reputation check. See `packaging/README.md`.
- **Debian / Ubuntu (APT)**: `curl -fsSL https://cdn.bitty.run/apt/setup.sh | sudo bash && sudo apt install -y bitty`; see `packaging/debian.md`.
- **Fedora / EPEL (Copr)**: `sudo dnf copr enable -y xuepoo/bitty-terminal && sudo dnf install -y bitty`; see `packaging/fedora.md`.
- **Nix / NixOS (Flake & Cachix)**: `nix run github:bitty-terminal/bitty` with binary cache `https://bitty.cachix.org`; see `packaging/nix.md`.
- **Alpine**: verified via the release `verify-install` Alpine leg; see
  `packaging/alpine.md`.
- **openSUSE**: best-effort, not supported (no release `verify-install`
  leg); see `packaging/opensuse.md`.

## Prebuilt binaries (manual download)

The R2-backed CDN at `https://cdn.bitty.run/releases/<TAG>/` is the preferred
download source for the channels above; [GitHub
Releases](https://github.com/bitty-terminal/bitty/releases) carry the same
binaries for Linux, macOS, and Windows plus `.deb`, `.rpm`, `.apk`, and Arch
packages and remain the documented fallback. Only release-matrix-verified
artifacts count as supported (see `packaging/README.md` `Release Matrix` and
`Support evidence`). `r2-mirror` publishes every file under `dist/` with
read-back hash verification. Every bare binary and archive has a `.sha256`
sidecar plus a `SHA256SUMS` entry — every channel verifies it, and you should
too before running anything manually. Two exceptions: the `.deb`, `.rpm`,
`.apk`, and Arch packages ship only their `SHA256SUMS` line (no per-package
sidecar), and FreeBSD publishes one `bitty-x86_64-unknown-freebsd.sha256`
covering both the binary and the `.tar.xz`.

## Build from source

Requires Rust (pinned channel in `rust-toolchain.toml`, MSRV 1.85) plus
fontconfig/freetype development packages:

```sh
git clone --recurse-submodules https://github.com/bitty-terminal/bitty.git
cd bitty
cargo build --release --locked -p bitty-terminal
./target/release/bitty
```

Or in one step:

```sh
cargo +1.98.1 install --git https://github.com/bitty-terminal/bitty.git bitty-terminal --locked
```

Note: the binary crate is `bitty-terminal` (`publish = false`) and the
`bitty` name on crates.io belongs to an unrelated project, so registry
installs do not work. Registry publication is tracked separately (#1512).

## Proxies and CDNs

The scripts use the system `curl` (Linux/macOS) or `Invoke-WebRequest`
(Windows) and honor the standard proxy environment (`https_proxy`,
`HTTPS_PROXY`). No custom CA handling: the CDN serves valid TLS and `--proto
=https` is enforced. If your network intercepts TLS, set the proxy or
download manually and verify the `.sha256` sidecar.

## Channels that do not exist yet

- **stable / nightly update channels**: only `latest.txt` (one moving
  pointer) exists in 0.1.0. Version-pinned installs (`--version`) are the
  supported way to stay put; in-place updates are a follow-up.
- **Signed Windows installer**: ships unsigned in 0.1.0; signing lands past
  0.2.0. See `WINDOWS-SMARTSCREEN.md`.
