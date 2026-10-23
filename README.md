# Bitty

Bitty is a Rust terminal workspace: a GPU-rendered terminal emulator and
multi-pane window manager for shell sessions. It is pre-1.0 and pre-alpha; the
current release line is `v0.0.27`, there is no stable public API, and behavior,
configuration keys, and package names may change between releases.

The product site is [bitty.run](https://bitty.run). Release artifacts and
release notes are published on
[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) and on the
R2-backed CDN at `https://cdn.bitty.run/releases/<tag>/` (the preferred
download source).

## Install

### Arch Linux

Install one of the AUR packages:

```sh
paru -S bitty-bin  # prebuilt release binary
paru -S bitty      # build from source
```

### Release binary

Download a platform package or archive from the R2-backed CDN at
`https://cdn.bitty.run/releases/<tag>/` (preferred) or
[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) (the same
bytes, documented fallback). Supported release artifacts include Linux, macOS,
and Windows binaries, plus selected Linux packages. Every artifact has a
`.sha256` sidecar, and the install scripts and package-manager channels verify
it. Packaging details are in [`packaging/README.md`](packaging/README.md).

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
