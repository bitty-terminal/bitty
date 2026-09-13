# Bitty

Bitty is a terminal workspace: a GPU-rendered terminal emulator and multi-pane
window manager for the shell, written in Rust. It runs your shell in a PTY,
parses VT output into terminal truth, and renders panels with `wgpu` —
horizontal/vertical splits, stacks, floating overlays, per-view appearance, and
panel animations — configured with Lua.

Bitty is pre-1.0 and pre-alpha. The current release line is `v0.0.20`; there is
no stable public API, and behavior, configuration keys, and package names can
change between releases. Canonical product, architecture, security, and
configuration documentation lives in
[bitty-docs](https://github.com/bitty-terminal/bitty-docs).

## Status

Feature status below is verified against the code, tests, and releases in this
repository. Anything not marked shipped is not a compatibility promise.

| Area                                                                              | Status                       |
| --------------------------------------------------------------------------------- | ---------------------------- |
| Terminal core: PTY, VT parser, grid/scrollback, damage tracking                   | Shipped                      |
| Windowed rendering (`wgpu`) with headless/software fallback                       | Shipped                      |
| Layouts: splits, stack, overlay, workspaces, focus, resize                        | Shipped                      |
| Scrollback search and selection                                                   | Shipped                      |
| 30 built-in theme presets with aliases, `bitty list themes`                       | Shipped                      |
| `bitty init` guided setup wizard                                                  | Shipped                      |
| Lua `init.lua` config, XDG paths, named profiles                                  | Shipped                      |
| Appearance overrides: CLI flags and per-view `views.*`                            | Shipped                      |
| Decoration: gaps, border, radius, outline colors, content inset                   | Shipped                      |
| Panel open/close/focus/workspace animations                                       | Shipped                      |
| Overlay scrollbar                                                                 | Shipped                      |
| IME composition (bounded preedit overlay + commit)                                | Shipped                      |
| New-pane cwd inheritance (OSC 7)                                                  | Shipped                      |
| Close confirmation for running jobs                                               | Shipped                      |
| Safe startup (`--safe`) and `--headless` CI mode                                  | Shipped                      |
| CLI: `run`, `ctl`, `config`, `init`, `doctor`, `list`, `inspect`, `dev`, `plugin` | Shipped                      |
| Plugin host and `bitty plugin` manifest management                                | Early                        |
| Third-party plugin ecosystem and SDK                                              | Early                        |
| Stable public Rust API (1.0)                                                      | Not yet                      |
| Remote UI and a `bittyd` daemon                                                   | Not yet (post-1.0 candidate) |

"Early" means the mechanism exists and is tested, but its external contract is
still changing; do not depend on it yet. The design corpus for text/Unicode and
IME, plugins, packages, IPC, and agent access remains under review in
`bitty-docs`.

## Install

### Arch Linux (AUR)

Bitty is published to the AUR as two recipes. `bitty-bin` installs the prebuilt
release binary and needs no local compile; `bitty` builds the workspace from
source. They conflict, so install one:

```sh
paru -S bitty-bin   # prebuilt (recommended)
paru -S bitty       # build from source
```

### Prebuilt binaries

[GitHub Releases](https://github.com/bitty-terminal/bitty/releases) carry
binaries for Linux (`x86_64`), macOS (`x86_64`, Apple silicon), and Windows
(`x86_64`, arm64), plus `.deb`, `.rpm`, `.apk`, and Arch packages for Linux.

### Build from source

Requires Rust — the pinned channel in `rust-toolchain.toml` is installed
automatically by `rustup`, and the MSRV is `1.85` — plus the fontconfig and
freetype development packages. A GPU and display are used when available;
without them Bitty falls back to a headless path.

```sh
git clone https://github.com/bitty-terminal/bitty.git
cd bitty
cargo build --release --locked -p bitty-app
./target/release/bitty
```

The produced binary is named `bitty`. It is not published on crates.io
(`bitty-app` is `publish = false`, and the unrelated `bitty` crate name on
crates.io is a different project), so install through the AUR, a release
artifact, or a source build. Nine `bitty-*` library crates are published at
`0.0.1`, but they are not a stable API.

### Other packaging

In-repo recipes for Homebrew, Scoop, and a Nix flake are documented in
[`packaging/README.md`](packaging/README.md).

## Quick start

Run the guided setup wizard once, then launch:

```sh
bitty init    # writes $XDG_CONFIG_HOME/bitty/init.lua
bitty         # launch your $SHELL (or /bin/sh)
```

`bitty init` prompts for a shell, theme, font family and size, panel decoration
(gaps, border, radius), scrollback, close-confirmation mode, and a Vim keymap
preset. Use `--yes` for defaults without a terminal and `--force` to overwrite
an existing config (it keeps an `.bak` backup). Every step can also be answered
with a flag:

```sh
bitty init --yes --theme tokyo-night --font-family "JetBrainsMono Nerd Font"
```

### Themes

Bitty ships 30 built-in presets (Tokyo Night, Catppuccin, Gruvbox, Solarized,
Dracula, Nord, Rose Pine, Everforest, and more), each with aliases:

```sh
bitty list themes
bitty --theme catppuccin
```

### Common flags and commands

```sh
bitty --split v                  # vertical split
bitty --layout stack:2           # stacked panes
bitty --layout overlay:5,5,20,10
bitty --headless                 # one deterministic headless tick (CI/smoke)
bitty doctor                     # diagnose install, GPU, fonts, PTY, terminfo
bitty ctl view split --right     # control a running instance
bitty --help                     # full flag and subcommand reference
```

Default chrome chords use Alt as the modifier (configurable with `mod_key`):
`Alt+h/j/k/l` and `Ctrl+Alt+arrows` move focus, `Shift+Alt+h/j/k/l` splits,
`Shift+Ctrl+h/j/k/l` resizes, `Alt+1..9` jumps to a view, `Alt+z/m/f` toggles
zoom/maximize/fullscreen, `Alt+w` closes, and `Ctrl+Shift+C/V` copy and paste.

## Configuration

Configuration is a Lua table returned from
`$XDG_CONFIG_HOME/bitty/init.lua` (default `~/.config/bitty/init.lua`;
`config.lua`, `--config`, and `BITTY_CONFIG` are also honored). Unknown keys
fail closed.

```lua
return {
  theme = "tokyo-night",
  font = { family = "JetBrainsMono Nerd Font", size = 12 },
  window = { opacity = 0.95, padding = 8 },
  terminal = { scrollback = 10000 },
  scrollbar = { mode = "auto" },
  close_confirm = "when_busy",
  appearance = { animations = { enabled = true } },
}
```

Inspect the resolved file and merged values with:

```sh
bitty config path    # resolved config file path
bitty config check   # validate and print per-key sources
bitty config edit    # open it in $VISUAL/$EDITOR
```

The configuration schema, XDG layout, profiles, plugins, and security model are
documented in bitty-docs:
[Lua configuration and filesystem layout](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/configuration/lua-and-xdg.md),
the [Configuration Model RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/configuration-model-rfc.md),
and the [CLI reference](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/interfaces/cli.md).
Those documents are draft design contracts; where they differ from the shipped
`init.lua` schema above, the shipped code and `bitty config check` are
authoritative.

## Build and test

All checks run through the justfile (never `npm`/`npx`/`yarn`; JavaScript tools
run through `bun`):

```sh
just setup    # fetch deps, install Git hooks, provision pinned dev tools
just check    # fmt-check + clippy + test + scratch-path/PTY gates + actionlint + markdownlint
```

Individual recipes: `just fmt-check`, `just clippy`, `just test`,
`just typecheck`, `just actionlint`, `just markdownlint`. See
[CONTRIBUTING.md](CONTRIBUTING.md) for prerequisites and the development loop.

## Project workflow (CarryCtx)

Bitty's task, decision, and checkpoint history is managed with CarryCtx.
CarryCtx engineering state is not cloned; a fresh checkout restores it from the
in-repo `refs/heads/carryctx-snapshots` branch:

```sh
just workflow-import-dry   # fetch + validate the snapshot; no DB writes
just workflow-import       # initialize CarryCtx state if needed, then import
```

The commander's merge closeout publishes a redacted snapshot with
`just workflow-publish`. Snapshots are publish-only: never merge one back, and
rotate at the source any secret that leaked before rotation.

## License

Released under the `MIT OR Apache-2.0` license. See [LICENSE](LICENSE).

## Documentation

- [bitty-docs](https://github.com/bitty-terminal/bitty-docs) — canonical
  product, architecture, security, configuration, and interface documents.
- [CHANGELOG.md](CHANGELOG.md) — release history.
- [`packaging/README.md`](packaging/README.md) — distribution and packaging.
