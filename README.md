# Bitty

Bitty is a Rust terminal workspace: a GPU-rendered terminal emulator and
multi-pane window manager for shell sessions. It is pre-1.0 and pre-alpha; the
current release line is `v0.0.31`, there is no stable public API, and behavior,
configuration keys, and package names may change between releases.

The product site is [bitty.run](https://bitty.run). Release artifacts and
release notes are published on
[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) and on the
R2-backed CDN at `https://cdn.bitty.run/releases/<tag>/` (the preferred
download source).

## Install

Every supported install channel verifies downloads before installing and fails
closed on mismatch. Detailed instructions, repository keys, and troubleshooting
are documented in [`packaging/INSTALL.md`](packaging/INSTALL.md).

### One-line installer (recommended)

Linux and macOS:

```sh
curl -fsSL https://cdn.bitty.run/install/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://cdn.bitty.run/install/install.ps1 | iex
```

### Package managers

#### Linux

- **Arch Linux (AUR)**:

  ```sh
  paru -S bitty-bin  # prebuilt release binary (recommended)
  paru -S bitty      # build from source
  ```

- **Debian / Ubuntu (APT)**:

  ```sh
  curl -fsSL https://cdn.bitty.run/apt/setup.sh | sudo bash
  sudo apt install -y bitty
  ```

  _(See [`packaging/debian.md`](packaging/debian.md))_

- **Fedora / EPEL (Copr)**:

  ```sh
  sudo dnf copr enable -y xuepoo/bitty-terminal
  sudo dnf install -y bitty
  ```

  _(See [`packaging/fedora.md`](packaging/fedora.md))_

- **Nix / NixOS**:

  ```sh
  nix run github:bitty-terminal/bitty
  ```

  _(Binary cache on `bitty.cachix.org`, see [`packaging/nix.md`](packaging/nix.md))_

- **AppImage (Universal Portable Linux)**:
  Download `bitty-<target>.AppImage`, `chmod +x`, and run. _(See [`packaging/appimage.md`](packaging/appimage.md))_
- **openSUSE (Zypper / OBS)**:
  _(See [`packaging/opensuse.md`](packaging/opensuse.md))_
- **Alpine Linux**:
  _(Native musl `APKBUILD` template in [`packaging/alpine.md`](packaging/alpine.md))_
- **Gentoo Linux**:
  _(Portage overlay ebuilds in [`packaging/gentoo.md`](packaging/gentoo.md))_
- **Void Linux**:
  _(xbps-src template in [`packaging/void.md`](packaging/void.md))_
- **Solus Linux**:
  _(ypkg recipe in [`packaging/solus.md`](packaging/solus.md))_

#### macOS

- **Homebrew**:

  ```sh
  brew install bitty-terminal/tap/bitty
  ```

- **Apple Disk Image (DMG)**:
  Download `Bitty-<version>-universal.dmg` from [GitHub Releases](https://github.com/bitty-terminal/bitty/releases) or the [CDN](https://cdn.bitty.run/releases/v0.0.31/).

#### Windows

- **Windows Package Manager (winget)**:

  ```powershell
  winget install BittyTerminal.Bitty
  # or using the moniker
  winget install bitty
  ```

  _(See [`packaging/winget.md`](packaging/winget.md))_

- **Scoop**:

  ```powershell
  scoop bucket add bitty https://github.com/bitty-terminal/scoop-bucket
  scoop install bitty
  ```

- **Setup wizard (Inno Setup)**:
  Download `bitty-<version>-windows-x86_64-setup.exe` from [GitHub Releases](https://github.com/bitty-terminal/bitty/releases) or the [CDN](https://cdn.bitty.run/releases/v0.0.31/). _(See [`packaging/WINDOWS-SMARTSCREEN.md`](packaging/WINDOWS-SMARTSCREEN.md))_

#### BSD

- **FreeBSD Ports**:
  Ports recipe in `packaging/freebsd/x11/bitty/`. _(See [`packaging/freebsd.md`](packaging/freebsd.md))_

- **OpenBSD Ports**:
  Ports recipe in `packaging/openbsd/x11/bitty/`. _(See [`packaging/openbsd.md`](packaging/openbsd.md))_

- **NetBSD pkgsrc**:
  pkgsrc recipe in `packaging/netbsd/x11/bitty/`. _(See [`packaging/netbsd.md`](packaging/netbsd.md))_

### Release binary (manual download)

Download a platform package or archive from the R2-backed CDN at
`https://cdn.bitty.run/releases/<tag>/` (preferred) or
[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) (the same
bytes, documented fallback). Supported release artifacts include Linux, macOS,
and Windows binaries, plus selected Linux packages. Every bare binary and archive
has a `.sha256` sidecar, and the install scripts and package-manager channels
verify it. Packaging details are in [`packaging/README.md`](packaging/README.md).

### Build from source

The repository pins the Rust toolchain in `rust-toolchain.toml`; Rustup installs
it when needed. Linux source builds also need fontconfig and freetype
development packages.

```sh
git clone --recurse-submodules https://github.com/bitty-terminal/bitty.git
cd bitty
cargo build --release --locked -p bitty-terminal
./target/release/bitty
```

The binary crate is `bitty-terminal` and is not published on crates.io. A git
source install is also available:

```sh
cargo +1.98.1 install --git https://github.com/bitty-terminal/bitty.git bitty-terminal --locked
```

## Quick Start

Run the setup wizard once, then launch Bitty:

```sh
bitty init
bitty
```

Use `bitty init --yes` for default answers, `--force` to replace an existing
configuration, and `bitty --help` for the current CLI surface. Headless smoke
execution is available with `bitty --headless`.

## Documentation

Canonical platform documentation is maintained in
[bitty-terminal-docs](https://github.com/bitty-terminal/bitty-terminal-docs),
whose documentation map is at
[`docs/README.md`](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/docs/README.md).
The checkout mounts that repository at `docs/` as a submodule; edit the
canonical docs repository rather than the mount.

Shared governance, decisions, reviews, and security material live in
[bitty-docs](https://github.com/bitty-terminal/bitty-docs). AI and plugin
documentation live in
[bitty-ai-docs](https://github.com/bitty-terminal/bitty-ai-docs) and
[bitty-plugins-docs](https://github.com/bitty-terminal/bitty-plugins-docs).

See [`CHANGELOG.md`](CHANGELOG.md) for release history and
[`CONTRIBUTING.md`](CONTRIBUTING.md) for contribution guidance.

## Inspiration

Bitty is shaped by the tools its authors use every day:

- [Neovim](https://neovim.io/) for a keyboard-first workspace, Lua
  configuration, and an extension model that keeps the core small.
- [Hyprland](https://hyprland.org/) for tiling window management, workspaces,
  and the idea that a terminal workspace can be visually deliberate.
- [Kitty](https://sw.kovidgoyal.net/kitty/),
  [WezTerm](https://wezfurlong.org/wezterm/),
  [Alacritty](https://alacritty.org/), and
  [Foot](https://codeberg.org/dnkl/foot) for GPU rendering, terminal protocol
  coverage, performance discipline, and configuration ergonomics.

Bitty does not share code with these projects; the inspiration is in the
product ideas and the standards they helped define.

## Diagnostics

Release builds arm a built-in hang watchdog: if a frame or a cold plugin
command stops making progress past its bound, Bitty writes one bounded report
to `$XDG_STATE_HOME/bitty/stall-watchdog.log` (falling back to
`~/.local/state/bitty/stall-watchdog.log`) with the current phase, tick
durations, queue and plugin counts, retained content bytes, file-descriptor
count, RSS, and per-thread kernel states. If the window stops repainting or
accepting input, capture the same report from a live process without killing
it:

```sh
kill -USR1 "$(pgrep -n bitty)"
```

`SIGUSR1` is Unix-only and writes one dump per second at most. The watchdog is
on by default in release builds; set `BITTY_HANG_WATCHDOG=0` to disable it, or
`BITTY_HANG_WATCHDOG=1` to enable it in a debug build. `BITTY_HANG_WATCHDOG_MS`
overrides the generic bound (the tick and command phases keep their own
tighter bounds).

## Development

Prerequisites and the pinned development tools are documented in
[`CONTRIBUTING.md`](CONTRIBUTING.md). Common commands are:

```sh
just setup
just check
just test
```

The workspace contains the terminal core and its supporting crates. External
compatibility and performance validation suites are maintained separately.

## License

Released under the `MIT OR Apache-2.0` license. See [`LICENSE`](LICENSE).
