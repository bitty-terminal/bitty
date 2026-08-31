---
title: Compatibility Lab (Phase C)
description: Headless bounded compatibility lab scaffolding for M1 Hardening Phase C — vttest, Ghostty/kitty/WezTerm differential, existing bitty-vt corpora, and per-category placeholder corpora with harness
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Compatibility Lab (Phase C)

## Status and provenance

- Status: **draft**. Scaffold for M1 Hardening Phase C, branch `ctx-0074/chore-compat-lab` (CTX-0074), worktree `.worktrees/ctx-0074-chore-compat-lab` — also exposed as `ctx-compat-lab` — agent `opencode-commander`.
- Ownership: bitty **CTX-0074** — *Implement compatibility lab (vttest/differential/corpus) (Phase C)*.
  - Priority: P0 | Area: vt | Labels: chore,area:vt,P0 | Milestone: v0.1.0 | RFC: OQ-004 | Task: CTX-0074
- Scope: scaffold `tests/compat/{vt,osc,keyboard,mouse,resize,unicode,shell,tui}/` with placeholder corpora and headless bounded harness referencing `vttest`, Ghostty/kitty/WezTerm differential, and existing `bitty-vt` tests. Keep headless, bounded, `forbid(unsafe)`, no window/GPU leak. Add `docs/product/compat-lab.md` (this file) and update `release-ladder.md` v0.2 gate.
- Authority: OQ-004 (compatibility milestone) remains `Proposed` until `compatibility-milestone-rfc.md` is accepted. This lab does not close OQ-004, does not accept the `v0.2` slice, and does not weaken normative security controls in `bitty-docs/docs/security/`.

## Goals

- Provide a reviewable harness and corpora layout for `v0.2` VT/TUI compatibility work without forking existing `bitty-vt` replay guarantees.
- Make differential testing against `vttest` and against Ghostty/kitty/WezTerm grid dumps reproducible, headless, and bounded.
- Keep the lab `forbid(unsafe)`, headless, and window/GPU-free so CI can run it without display.

## Harness — `tests/compat/harness.rs`

- `#![forbid(unsafe_code)]` — header asserts no `unsafe` in harness or corpora.
- Headless — only `bitty-vt::Parser` → `TerminalAction` → `bitty-term-state::State` → `Snapshot`. No `winit::Window`, no `wgpu::Surface`, no `HeadlessRasterizer`. Grep `tests/compat` for `winit`/`wgpu`/`Window`/`Surface` must be 0 except in this forbid list.
- Bounded — `MAX_CORPUS_BYTES = 8 KiB` (`bitty-pty::READ_CHUNK_SIZE` × 1), `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 1024` (`BoundedString::MAX_LEN`), `MAX_CORPORA_PER_CATEGORY = 64`. `parse_bounded` asserts `bytes.len() <= MAX_CORPUS_BYTES` and `actions.len() <= MAX_ACTIONS`; `BoundedString`/`BoundedBytes` truncate per `bitty-vt` contract (see `replay.rs::fixture_osc_sweep_replay`). Larger corpora must be split.
- Deterministic — `parse_bounded` re-parses byte-by-byte and asserts `actions == actions2` (same pattern as `parser::tests::action_stream_identical_across_chunkings` and `replay.rs::parse_twice`).
- Functions: `parse_bounded`, `actions_to_snapshot`, `diff_snapshots` (snapshot text diff against reference dumps), `list_corpus` (bounded directory walk, no symlink follow), plus `#[cfg(test)]` self-tests.

Usage headlessly:

```text
let bytes = std::fs::read("tests/compat/vt/corpus/01-cursor-addressing.bin").unwrap();
assert!(bytes.len() <= harness::MAX_CORPUS_BYTES);
let actions = harness::parse_bounded(&bytes);
let snapshot = harness::actions_to_snapshot(&actions);
// optional differential:
let reference = std::fs::read_to_string("tests/compat/vt/reference/01.txt").unwrap();
assert!(harness::diff_snapshots(&snapshot, &reference).is_none());
```

## Corpora layout

```text
tests/compat/
  harness.rs                  # headless bounded harness, forbid(unsafe)
  README.md                   # lab root, invariants, layout, no-leak rule
  vt/corpus/*.bin + README    # vttest VT100/220/420 menus 1–12, CSI/ESC/DCS
  osc/corpus/* + README       # OSC 0/7/8/52/133, clipboard, hyperlink, cwd
  keyboard/corpus/* + README  # kitty keyboard, modifyOtherKeys
  mouse/corpus/* + README     # SGR/UTF8/urxvt mouse reports
  resize/corpus/* + README    # reflow, scroll region, alt-screen
  unicode/corpus/* + README   # width, emoji ZWJ, combining, wcwidth
  shell/corpus/* + README     # OSC 133 prompt marks, shell integration
  tui/corpus/* + README       # nvim/tmux/htop/fzf/lazygit traces
```

Each `corpus/*.bin` placeholder < 1 KiB and < 8 KiB bound; real captures will be pinned in `recordings/references/` before check-in.

## Per-category corpora

### `vt/` — VT conformance

`vttest` menus 1 (cursor), 2 (screen), 3 (charsets), 4 (double-size), 6 (VT220), 8 (VT420), 11 (SGR), 12 (status). Curated CSI/ESC/DCS + `script` captures. Differential compares `Snapshot` text/attrs/cursor/modes to `vttest` expected grid.

Baseline: `crates/bitty-vt/seeds/03-cursor-addressing.bin`, `04-sgr-colors.bin`, `05-decset-decrst.bin`, `06-erase-scroll.bin`, `11-malformed-resync.bin`, `12-dcs-and-status.bin`, `14-param-stress.bin`, and `replay.rs::fixture_fullscreen_app_replay` (vttest-style `SetScrollRegion`, `ScrollUp/Down`, `EraseInDisplay/Line`, tabs, device status).

### `osc/` — OSC

OSC 0/2 title, 7 cwd, 8 hyperlink, 52 clipboard, 133 prompt marks, 4 color, unknown. Baseline: `replay.rs::fixture_osc_sweep_replay` and `seeds/08-osc-title-hyperlink.bin`, `09-osc-hyperlink-prompt.bin`, `10-osc-clipboard-truncated.bin`.

### `keyboard/` — Keyboard

Kitty keyboard `CSI u`, `modifyOtherKeys`. Future `bitty-platform::keyboard` mapping; this scaffold is `Parser` bytes only.

Baseline: `crates/bitty-platform/tests/keyboard_input.rs`.

### `mouse/` — Mouse

SGR (`1006`), UTF-8 (`1005`), urxvt (`1015`). Baseline: `replay.rs::fixture_escape_storm_replay` (`1002;1006h`).

### `resize/` — Resize

`State::resize` reflow, `RuntimeConfig::grid_from_pixels`, zero-size skip, scroll region, alt-screen. Baseline: `crates/bitty-runtime/tests/v01_minimal_terminal.rs::v01_resize_*`, `crates/bitty-term-state/tests/resize_scrollback.rs`.

### `unicode/` — Unicode

`char_cell_width` (wide, zero-width, emoji), `U+FFFD` split, charset shifts. Baseline: `seeds/13-utf8-invalid-split.bin`, `replay.rs::fixture_fullscreen_app_replay` (`🎉` wide).

### `shell/` — Shell integration

`OSC 133 A/B/C/D` zones, `OSC 7` cwd. Bounded `ZONE_RECORDS_MAX = 1024`. Baseline: `replay.rs::fixture_shell_session_replay` (full `133;A/B/C/D` flow).

### `tui/` — TUI

`nvim`/`tmux`/`htop`/`fzf`/`lazygit` alt-screen fullscreen. Baseline: `replay.rs::fixture_fullscreen_app_replay` and soak `soak.rs::soak_headless_1000_ticks_bounded_and_deterministic`.

## `vttest` runbook

1. Pin `vttest` source in `recordings/references/vttest/` (record revision + license, per `recordings/references/` rule).
2. Build `vttest` (`./configure && make`).
3. Run `script -c "./vttest" vttest.log` and `script --timing=vttest.timing` for menus 1–12; also harvest curated sequences from `vttest.c` expected grids.
4. Split `vttest.log` into bounded `tests/compat/vt/corpus/vttest-*.bin` slices (< 8 KiB each) with accompanying `vt/reference/*.txt` grid dumps.
5. Drive harness `parse_bounded` → `State` → `Snapshot`; assert snapshot text equals reference dump via `diff_snapshots`. Failures become tracked follow-ups, not silent skips.
6. Never copy `recordings/references/` code into `tests/compat`; corpora are bytes + reference dumps only.

## Ghostty / kitty / WezTerm differential

- **Capture:** feed identical `tests/compat/*/*/corpus/*.bin` byte stream to Ghostty, kitty, WezTerm (headless `script` replay or `expect` harness). Dump grid via `kitty --dump-commands` JSON, Ghostty `dump` (xterm dump JSON), `wezterm record` JSON.
- **Compare:** normalize reference dump to `Snapshot`-like text/attrs/cursor/modes and diff via `harness::diff_snapshots`. Differential is snapshot-to-snapshot, not pixel.
- **Out of scope for this scaffold:** pixel snapshots, GPU pixel diff, or live `winit`/`wgpu` windows. Rendering is tested separately in `bitty-render` headless fixtures.
- **Reference storage:** checked-in `tests/compat/*/reference/*.txt` (text) and `*.json` (grid dump) per category, each < 8 KiB, bounded.

## Link to existing `bitty-vt` tests

- `crates/bitty-vt/tests/replay.rs` — deterministic replay fixtures; this lab mirrors its `parse_twice` determinism in `harness::parse_bounded`.
- `crates/bitty-vt/seeds/*.bin` — 14 seeds used by `seeds_corpus_is_panic_free_and_deterministic`; lab extends per-category corpora similarly.
- `crates/bitty-term-state/tests/replay_determinism.rs`, `parser_seeds.rs`, `property_invariants.rs` — `State` invariants and replay determinism same harness exercises.

## Bounds and determinism

- Every corpus file asserted `<= MAX_CORPUS_BYTES`; every action vector `<= MAX_ACTIONS`; OSC payloads `<= BoundedString::MAX_LEN` (truncate).
- No `unsafe`, no window, no GPU: CI runs `cargo test --workspace --locked` and `just check` headlessly.
- Determinism proved by byte-by-byte re-parse identity and by `State::state_hash` (RFC replay guarantee 2) — future `compat_corpus_determinism` test will hash `State` after full corpus and compare across chunkings.

## Relationship to `release-ladder.md`

`v0.2` row (VT/TUI compatibility: `bitty-vt`/`bitty-term-state` fidelity, compatibility matrix OQ-004) now links to this lab. This file is the `v0.2` gate sketch detail; `release-ladder.md` retains the ladder and crate focus, this file owns the runbook. See update in `release-ladder.md` “## v0.2 compatibility lab (Phase C scaffold)”.

## Next

- Wire `tests/compat/**/corpus/*.bin` into a `cargo test` harness (`compat_lab_is_deterministic_and_bounded`) that runs headlessly under `just test`.
- Pin `vttest` revision/license in `recordings/references/vttest/` and land first real captures (bounded slices) plus Ghostty/kitty/WezTerm reference dumps.
- Record `compat-lab.md` revision in `bitty-docs` when OQ-004 RFC accepts the differential contract.
