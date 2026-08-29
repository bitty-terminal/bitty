<!-- markdownlint-disable MD025 -->

# TUI Compatibility (`tests/compat/tui`)

TUI corpus — `nvim`, `tmux`, `htop`, `fzf`, `lazygit`, Starship; `vttest`-style fullscreen and Ghostty/kitty/WezTerm differential.

## Source

- TUI captures: `script --timing` logs of `nvim --headless` (`:terminal`), `tmux -L bitty-test new -n test`, `htop`/`fzf` alt-screen entry (`1049h`/`47h`), `lazygit` mouse+resize interactions. Each `corpus/*.bin` is a bounded slice (< 8 KiB) of the raw PTY stream, not a screenshot.
- Ghostty / kitty / WezTerm differential — feed same TUI stream headlessly to `bitty-vt`/`bitty-term-state` and to reference terminals (offline dumps), then diff `Snapshot` text/attrs/cursor/modes and `Damage` regions. Reference dumps are JSON grid snapshots, not pixels, to keep differential deterministic.
- Existing baseline — `replay.rs::fixture_fullscreen_app_replay` (scroll region `2;10r`, `5S/3T`, `H/J/K/X`, `TabForward/Backward`, `RequestDeviceStatus`, `CursorStyle`, `AlternateScreen`), `fixture_escape_storm_replay` (malformed resync), plus `bitty-runtime` headless soak `soak.rs::soak_headless_1000_ticks_bounded_and_deterministic`.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`, no unbounded allocation on PTY→VT→State path.
- No window/GPU — TUI corpora are PTY bytes; `State` renders no pixels here.

## Layout

```text
tui/
  README.md
  corpus/
    01-nvim-tmux.bin          # curated CSI for fullscreen app
    02-htop-fzf.bin           # alt-screen + SGR
    placeholder.bin
```
