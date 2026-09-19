<!-- markdownlint-disable MD025 -->

# Color and Title Compatibility (`tests/compat/color`)

M1 color/title golden corpus — SGR 0–255 and truecolor cell attributes,
OSC 10/11 dynamic default colors, and OSC 0/2 window title; headless and
bounded.

## Source

- Compatibility Milestone RFC (`docs/specifications/compatibility-milestone-rfc.md`)
  M1 rows "Color" (16/256/truecolor SGR), "Window title" (OSC 0/2 set;
  OSC 10/11 query + set), and its acceptance evidence "Snapshot tests for
  SGR 0–255 and truecolor cell attributes; automated test for OSC 10/11
  query-response round trip".
- Terminal state RFC (`docs/specifications/terminal-state-rfc.md`) replay
  guarantees: each fixture's canonical `State::state_hash` pins the resolved
  cell attributes and title.
- OSC 10/11 payload parsing is bounded fail-closed in `bitty-vt`
  (`CTX-0381`); the runtime owns query replies and the capability-gated set
  path, so the OSC color fixtures are semantically inert for grid truth.

## Bounds

- `#![forbid(unsafe_code)]`, headless — `Parser -> TerminalAction -> State`
  only; no `winit`/`wgpu`/`Window`/`Surface`.
- `MAX_CORPUS_BYTES = 8 KiB`, `MAX_OSC_BYTES = 1024`, `MAX_ACTIONS = 4096`.
- No window/GPU leak: the category contains escape bytes only.

## Layout

```text
color/
  README.md
  corpus/
    01-sgr-16-fg-bg.bin        # SGR 30–37/90–97 and 40–47/100–107 sweep
    02-sgr-256-indexed.bin     # SGR 38;5;N / 48;5;N across cube and ramp
    03-sgr-truecolor.bin       # SGR 38;2;R;G;B / 48;2;R;G;B
    04-osc-title.bin           # OSC 0 (ST) then OSC 2 (BEL) title set
    05-osc-color-query.bin     # OSC 10/11 queries (runtime answers)
    06-osc-color-set-query.bin # OSC 10/11 sets then queries
    07-osc-title-reset.bin     # OSC 2 set with SGR text, OSC 0 empty reset
```

The golden test (`crates/bitty-compat-lab/tests/m1_color_golden.rs`) pins the
canonical hash plus resolved foreground/background attributes per fixture; the
runtime test (`crates/bitty-runtime/tests/m1_color_title.rs`) pins the OSC
10/11 reply bytes and the default palette/theme application digest.
