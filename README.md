# Bitty

Bitty is a terminal workspace: a small-core GPU-rendered terminal emulator
and multi-pane window manager for the shell, written in Rust. It runs your
shell in a PTY, parses VT output into terminal truth, and renders panels with
`wgpu` — horizontal/vertical splits, stacks, floating overlays, per-view
appearance, and panel animations — configured with Lua, with plugin extensions
gated by consent and activation grants.

Bitty is pre-1.0 and pre-alpha. The current release line is `v0.0.21`; there is
no stable public API, and behavior, configuration keys, and package names can
change between releases. Canonical platform documentation lives in
[bitty-terminal-docs](https://github.com/bitty-terminal/bitty-terminal-docs),
mounted at `docs/` as a Git submodule; shared governance, decisions, reviews,
and the security corpus live in
[bitty-docs](https://github.com/bitty-terminal/bitty-docs).

## Status

Feature status below is verified against the code, tests, and releases in this
repository. Anything not marked shipped is not a compatibility promise.

| Area                                                                                         | Status                       |
| -------------------------------------------------------------------------------------------- | ---------------------------- |
| Terminal core: PTY, VT parser, grid/scrollback, damage tracking                              | Shipped                      |
| Windowed rendering (`wgpu`) with headless/software fallback                                  | Shipped                      |
| Layouts: splits, stack, overlay, workspaces, focus, resize                                   | Shipped                      |
| Small-Core: tab-strip, search/copy-mode UI, workspace bar, and network moved out             | Shipped                      |
| Scrollback search and selection mechanisms (View-owned, bounded, clipboard gate)             | Shipped (UI in plugins)      |
| Leader key (`input.leader` / `leader_key`, timeout, hints)                                   | Shipped                      |
| OSC 8 hyperlinks, OSC 22 pointer shapes, OSC 9/777/99 notifications (RC-8 + cap)             | Shipped                      |
| Plaintext URL detection (`bitty-url-detector`) and notify bridge (`bitty-platform-services`) | Shipped (fail-closed)        |
| Consent-issued automation bearers, activation and network grants                             | Shipped                      |
| 30 built-in theme presets (aliases for many), `bitty list themes`                            | Shipped                      |
| `bitty init` guided setup wizard                                                             | Shipped                      |
| Lua `init.lua` config, XDG paths, named profiles                                             | Shipped                      |
| Appearance overrides: CLI flags and per-view `views.*`                                       | Shipped                      |
| Decoration: gaps, border, radius, outline colors, content inset                              | Shipped                      |
| Panel open/close/focus/workspace animations                                                  | Shipped                      |
| Overlay scrollbar                                                                            | Shipped                      |
| IME composition (bounded preedit overlay + commit)                                           | Shipped                      |
| New-pane cwd inheritance (OSC 7)                                                             | Shipped                      |
| Close confirmation for running jobs                                                          | Shipped                      |
| Safe startup (`--safe`) and `--headless` CI mode                                             | Shipped                      |
| CLI: `run`, `ctl`, `config`, `init`, `doctor`, `list`, `inspect`, `dev`, `plugin`            | Shipped                      |
| Plugin host and `bitty plugin` manifest management                                           | Early                        |
| Third-party plugin ecosystem and SDK                                                         | Early                        |
| Stable public Rust API (1.0)                                                                 | Not yet                      |
| Remote UI and a `bittyd` daemon                                                              | Not yet (post-1.0 candidate) |

Small-Core means Core owns correctness, invariants, resources, and protocol
handling, while presentation moves to plugins and coprocesses: the candidate
Core tab-strip is deleted in favor of the `bar` plugin, search and copy-mode
policy live in the `search` and `copy-mode` plugins over the bounded host
mechanisms (`State::search`, `search_host` ops, selection semantics, the
`CLIPBOARD_MAX_BYTES` gate, `HistoryGate`, modal containment), the workspace
bar is the `bar` plugin over Core chrome geometry, the embedded network path
is removed in favor of out-of-process components over `bitty-network-wire`,
and dead bundled-panel helpers are deleted. Search and copy-mode have no Core
default chords: the retired `Ctrl+Shift+F` / `Ctrl+Shift+Space` spellings fail
closed as unknown actions and both chords are shell input again.

"Early" means the mechanism exists and is tested, but its external contract is
still changing; do not depend on it yet. The design corpus for text/Unicode and
IME, plugins, packages, IPC, and agent access remains under review across
[bitty-terminal-docs](https://github.com/bitty-terminal/bitty-terminal-docs),
[bitty-ai-docs](https://github.com/bitty-terminal/bitty-ai-docs), and
[bitty-plugins-docs](https://github.com/bitty-terminal/bitty-plugins-docs),
with shared governance in `bitty-docs`.

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

Release artifacts are also mirrored to R2 by the `r2-mirror` job in
`.github/workflows/release.yml`: every file under `dist/` (plus generated
`SHA256SUMS` and `provenance.json`) is synced to the `bitty` bucket at
`releases/<tag>/` with immutable cache headers and verified by read-back
hash comparison. No public CDN URL is claimed here; the workflow is the
source of truth for the mirror location.

### Build from source

Requires Rust — the pinned channel in `rust-toolchain.toml` is installed
automatically by `rustup`, and the MSRV is `1.85` — plus the fontconfig and
freetype development packages. A GPU and display are used when available;
without them Bitty falls back to a headless path.

The `docs/` submodule carries the canonical platform documents and is not
needed to build; clone with `--recurse-submodules` to get it in one step (an
existing checkout runs `git submodule update --init`):

```sh
git clone --recurse-submodules https://github.com/bitty-terminal/bitty.git
cd bitty
cargo build --release --locked -p bitty-terminal
./target/release/bitty
```

The produced binary is named `bitty`. It is not published on crates.io
(`bitty-terminal` is `publish = false`, and the unrelated `bitty` crate name on
crates.io is a different project), so install through the AUR, a release
artifact, a source build, or the `cargo install --git` command below. Nine
`bitty-*` library crates are published at `0.0.1`, but they are not a stable
API.

### Cargo

`cargo install --git` builds and installs the `bitty` binary in one step, so
you can skip the manual clone and build above:

```sh
cargo +1.98.1 install --git https://github.com/bitty-terminal/bitty.git bitty-terminal --locked
```

The installed executable is `bitty`. This is a **git source install**, not a
crates.io install: the binary crate is `bitty-terminal` and it is `publish = false`,
so `cargo install bitty-terminal` from the registry would fail. Plain
`cargo install bitty` is also impossible — the `bitty` name on crates.io is an
unrelated project. Pin the toolchain with `+1.98.1` (rustup installs it on
demand): unlike an in-tree build, `cargo install --git` runs from your current
directory and does **not** read the repository's `rust-toolchain.toml`, so the
pinned channel must be selected explicitly. Pass `--locked` to build the pinned
dependency set, and `--force` to overwrite an existing install.

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
Dracula, Nord, Rose Pine, Everforest, and more); many carry short aliases such
as `catppuccin` and `gruvbox`:

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
bitty --headless --fail-loud     # same smoke, but exit non-zero if the shell
                                 # or IPC servo fails to start
bitty doctor                     # diagnose install, GPU, fonts, PTY, terminfo
bitty ctl view split --right     # control a running instance
bitty --help                     # full flag and subcommand reference
```

Default chrome chords use Alt as the modifier (configurable with `mod_key`):
`Alt+h/j/k/l` and `Ctrl+Alt+arrows` move focus, `Shift+Alt+h/j/k/l` splits,
`Shift+Ctrl+h/j/k/l` resizes, `Alt+1..9` jumps to a workspace, `Alt+z/m/f`
toggles pane zoom, `Alt+w` closes the workspace, and `Ctrl+Shift+C/V` copy and
paste. Search and copy-mode have no Core default chords: both `Ctrl+Shift+F`
and `Ctrl+Shift+Space` are shell input again and their UI lives in the
`search` and `copy-mode` plugins.

Leader sequences arm with the prefix chord (default `Alt+Space`,
`Ctrl+Space` on Windows; override with `input.leader` or `leader_key`,
timeout with `input.timeout_len` or `leader_timeout_ms` in milliseconds,
default `1000`, and `hints_enabled = false` keeps the Leader from arming).
A keymap change cancels an armed Leader session, and live reload adopts
Leader binding changes on the same tick.

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
  input = { leader = "alt+space", timeout_len = 1000 },
  hints_enabled = true,
}
```

The canonical Leader surface is `input.leader` / `input.timeout_len`
(`CTX-1002`, issue #1650), winning over the legacy `leader_key` /
`leader_timeout_ms` aliases when both are present. Absent means the platform
default (`Alt+Space`, `Ctrl+Space` on Windows, `1000` ms fail-open window).

Inspect the resolved file and merged values with:

```sh
bitty config path    # resolved config file path
bitty config check   # validate and print per-key sources
bitty config edit    # open it in $VISUAL/$EDITOR
```

The configuration schema, XDG layout, profiles, plugins, and security model are
documented in the canonical docs:
[Lua configuration and filesystem layout](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/configuration/lua-and-xdg.md),
the [Configuration Model RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/configuration-model-rfc.md),
and the [CLI reference](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/interfaces/cli.md).
Those documents are draft design contracts; where they differ from the shipped
`init.lua` schema above, the shipped code and `bitty config check` are
authoritative.

## Workspace crates

The Core workspace (`Cargo.toml` `[workspace] members`) holds only the
terminal mechanism and the retained test harnesses:

| Crate                | Role                                                                  |
| -------------------- | --------------------------------------------------------------------- |
| `bitty-terminal`     | `bitty` binary, thin composition root                                 |
| `bitty-runtime`      | Runtime orchestration (PTY, parser, state, render, plugin side queue) |
| `bitty-vt`           | VT parser producing terminal actions (wraps `vte`)                    |
| `bitty-term-state`   | Terminal truth: grid, state, damage, scrollback                       |
| `bitty-pty`          | PTY wrapper with backpressure and resize                              |
| `bitty-render`       | GPU rendering pipeline (`wgpu` + `crossfont`)                         |
| `bitty-platform`     | Window and event-loop adapter (wraps `winit`)                         |
| `bitty-ui`           | View, layout, and selection primitives                                |
| `bitty-rich`         | Rich presentation helpers (draft)                                     |
| `bitty-config`       | Typed, validated configuration pipeline                               |
| `bitty-lua`          | Deterministic Lua VM budgets for plugins                              |
| `bitty-plugin-host`  | Draft plugin platform host                                            |
| `bitty-package`      | Package lifecycle and integrity verification                          |
| `bitty-winjob`       | Win32 Job Object adapter for owned process trees                      |
| `bitty-test-support` | Shared test-harness helpers                                           |
| `bitty-test-vm`      | VM test-tier controller                                               |

Validation suites (external): `bitty-compat-lab` (headless compatibility lab)
and `bitty-perf` (performance baseline harness and benches) live in their own
repositories since the W-105 relocation and consume a pinned `bitty` revision;
they are not workspace members and are never linked into product artifacts.
The product change path keeps thin required invocations against the pinned
suite revisions (see `validation-pins.env`, `scripts/m1-matrix.sh`, and
`scripts/compat-matrix.sh`).

Extension model: `bitty-ipc` (independent repository, pulled as an exact-rev
git dependency) stays linked as Core's inbound local socket mechanism.
`bitty-agent`, `bitty-network`, and `bitty-observability` are independent
repositories; Core links no agent crate and no network code. Native
capabilities run as separately installed, on-demand stdio coprocesses
(DIR-030): the `bitty-runtime` component broker resolves, verifies
(SHA-256 before every spawn), spawns, and grants them, and links only the
dependency-free `bitty-network-wire` codec. The first component is `net`
(`bitty-net`, built from the bitty-network repository); the Lua-facing
request surface is a follow-up. Consent-issued automation bearers carry a
session revocation surface, and per-plugin network grants bind granted
`network.connect:*` capabilities to the plugin id for attribution.
`bitty dev trace` measurement lives in the external `bitty-perf` validation
suite and is never linked into this binary; the verb always reports that it
is not linked into this build (exit 1). Likewise `bitty dev capture|synthesize|dump|overlay`
are compiled only with the opt-in `dev-tools` feature and otherwise report
`built without dev-tools feature` (exit 1).

Terminal affordances: OSC 8 hyperlinks parse (`OSC 8 ; params ; URI`) and
activate with click-to-open live UX (hover feedback, TUI interception,
View-bound grid resolution, fail-closed while scrolled into history);
`file:` links are never presented and clipboard writes stay gated. OSC 22
pointer shapes map to cursor icons. OSC 9 / OSC 777 (`notify ; title ; body`)
and Kitty OSC 99 desktop-notification chunks parse into bounded
notifications under the RC-8 rate budget, with a defensive second cap in the
linked `bitty-platform-services` bridge (fail-closed `NoopBackend`; no OS
delivery yet). Plaintext URLs are detected by the linked
`bitty-url-detector` matcher (`forbid(unsafe)`, `std` only) over the same
`ValidatedUrl` + `ActivationGesture` pipeline as OSC 8, with `Ctrl`-gated
hover and click-to-open.

Linked extension revisions (exact-rev git pins in
`crates/bitty-runtime/Cargo.toml`): `bitty-ipc` at `e9714e7`,
`bitty-network-wire` at `c69fe57`, `bitty-url-detector` at `5c65773`,
`bitty-platform-services` at `2fc794a`.

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

Canonical platform documentation lives in
[bitty-terminal-docs](https://github.com/bitty-terminal/bitty-terminal-docs)
and is mounted at `docs/` as a Git submodule pinned by commit:

- New clone: `git clone --recurse-submodules …` (or
  `git submodule update --init` in an existing checkout).
- Bump the pin: `git submodule update --remote docs`, then `git add docs` and
  commit the pointer change; without a checkout, `just docs-pin [<rev>]`
  stages a merged `bitty-terminal-docs` commit from a sibling checkout.
- Edit: change documents in the `bitty-terminal-docs` repository, never in
  the pinned `docs/` copy.
- Read: [`docs/README.md`](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/docs/README.md)
  is the documentation map.

Shared governance, decisions, reviews, and the security corpus live in
[bitty-docs](https://github.com/bitty-terminal/bitty-docs). The AI-core and
plugin-ecosystem corpora live in
[bitty-ai-docs](https://github.com/bitty-terminal/bitty-ai-docs) and
[bitty-plugins-docs](https://github.com/bitty-terminal/bitty-plugins-docs).

- [CHANGELOG.md](CHANGELOG.md) — release history.
- [`packaging/README.md`](packaging/README.md) — distribution and packaging.
