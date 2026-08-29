<!-- markdownlint-disable MD025 -->

# VT Compatibility (`tests/compat/vt`)

VT parser conformance corpus — `vttest`, CSI/ESC/DCS stress, and differential against Ghostty/kitty/WezTerm.

## Source

- `vttest` menus 1 (cursor movement), 2 (screen features), 3 (character sets), 4 (double-size), 6 (VT220), 8 (VT420), 11 (SGR), 12 (status strings). Capture via `script -c "vttest"` or curated escape sequences from `vttest` source (`vttest.c`, expected cell snapshots). Each menu yields one `corpus/vttest-*.bin` (bounded < 8 KiB).
- Ghostty / kitty / WezTerm differential — feed same `corpus/*.bin` to reference terminals, dump grid (`kitty --dump-commands` JSON, Ghostty `xterm` dump, `wezterm record` JSON) and compare `Snapshot` text/attrs via `harness::diff_snapshots`.
- Existing `bitty-vt` baseline — `crates/bitty-vt/tests/replay.rs` (`fixture_fullscreen_app_replay` covers `vttest`-style fullscreen: `SetScrollRegion`, `ScrollUp/Down`, `EraseInDisplay/Line`, `TabForward/Backward`, device status), `crates/bitty-vt/seeds/03-cursor-addressing.bin`, `04-sgr-colors.bin`, `05-decset-decrst.bin`, `06-erase-scroll.bin`, `11-malformed-resync.bin`, `12-dcs-and-status.bin`, `14-param-stress.bin`. New corpora extend without forking.

## Bounds

- `#![forbid(unsafe_code)]` — harness `tests/compat/harness.rs` forbids unsafe; corpora are bytes, not code.
- Headless — `Parser -> TerminalAction -> State` only; no `winit`/`wgpu`.
- `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096` enforced by harness. `BoundedString::MAX_LEN = 1024` for OSC.
- No window/GPU leak — grep `vt/` for `Window`/`Surface` must be 0.

## Layout

```text
vt/
  README.md
  corpus/
    01-cursor-addressing.bin   # curated CUP/HVP, ED/EL, from vttest menu 1
    02-sgr-underline.bin       # SGR 4:3 curly underline, from vttest menu 11
    vttest-placeholder.bin     # placeholder until real vttest capture
```

Future: wire `corpus/*.bin` into `#[test] fn vt_corpus_deterministic` that calls `harness::parse_bounded` and `harness::actions_to_snapshot`, then diffs `Snapshot` against checked-in reference dumps under `vt/reference/`.
