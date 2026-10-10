# Install Bitty

Part of: bitty-terminal/bitty#1847 | Task: CTX-1097 | Milestone: v0.1.0

Every supported install channel in one page. All channels verify downloads
before installing and fail closed on mismatch; there are no signatures in
0.1.0 (verification is hash-only, Windows signing deferred past 0.2.0).

## One-line installers (recommended)

Linux and macOS:

```sh
curl -fsSL https://cdn.bitty.run/bitty/install/install.sh | bash
```

With an explicit version:

```sh
curl -fsSL https://cdn.bitty.run/bitty/install/install.sh | bash -s -- --version 0.0.23
```

Windows (PowerShell):

```powershell
irm https://cdn.bitty.run/bitty/install/install.ps1 | iex
```

Both scripts resolve the release through one pointer,
`https://cdn.bitty.run/bitty/install/latest.txt` (holds the TAG, e.g.
`v0.0.23`), fetch the per-OS artifact plus its `.sha256` sidecar from
`https://cdn.bitty.run/bitty/releases/<TAG>/`, verify the hash, and install.
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
  `paru -S bitty` (build from source). They conflict; install one. See the
  top-level README `Install` section.
- **Homebrew**: the formula is generated at tag time into the external tap
  `bitty-terminal/homebrew-tap` (no in-repo copy).
- **Scoop**: the manifest is generated at tag time into the external bucket
  `bitty-terminal/scoop-bucket` and ships the bare exes, sidestepping
  SmartScreen by construction. See `packaging/README.md`.
- **Alpine/openSUSE**: see `packaging/alpine.md` and
  `packaging/opensuse.md`.

## Prebuilt binaries (manual download)

[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) carry
binaries for Linux, macOS, and Windows plus `.deb`, `.rpm`, `.apk`, and Arch
packages. Everything is mirrored to R2 at `releases/<tag>/` with read-back
hash verification. Each artifact has a `.sha256` sidecar — check it before
running anything.

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
