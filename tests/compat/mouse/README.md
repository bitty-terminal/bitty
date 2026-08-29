<!-- markdownlint-disable MD025 -->

# Mouse Compatibility (`tests/compat/mouse`)

Mouse tracking corpus — SGR, UTF-8, urxvt, and normal encodings; headless bounded.

## Source

- Mouse modes: `1000` (normal), `1002` (button), `1003` (all motion), `1006` (SGR), `1015` (urxvt), `1005` (UTF-8). Captured from `vttest` mouse menus and from `tmux`/`nvim` mouse interactions (`script` replay with mouse events).
- Ghostty / kitty / WezTerm differential — feed mouse-report bytes (`CSI < ... M/m`, `CSI M ...`) and compare `bitty-vt` `MouseTrackingMode` parse to reference terminal's `Cell` grid mouse handler dump. Differential is snapshot-level (cursor moves from mouse reports are inert in `State` until mouse RFC lands; see `TerminalAction::SetMode { MouseTracking }`).
- Existing baseline — `replay.rs::fixture_escape_storm_replay` sets `MouseTracking::Button` + `MouseCoordinateEncoding::Sgr` via `CSI ? 1002 ; 1006 h`; `crates/bitty-term-state` tracks modes inertly.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`.
- No window/GPU — mouse corpus is escape bytes, no display.

## Layout

```text
mouse/
  README.md
  corpus/
    01-sgr-mouse.bin          # CSI < 0 ; 10 ; 10 M
    02-utf8-mouse.bin         # CSI M + utf8 coords
    placeholder.bin
```
