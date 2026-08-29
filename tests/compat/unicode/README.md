<!-- markdownlint-disable MD025 -->

# Unicode Compatibility (`tests/compat/unicode`)

Unicode text-domain corpus — `wcwidth`, combining, emoji ZWJ/variation selector, ambiguous-width, zero-width, invalid-UTF-8, and mixed-width differential.

Phase C deep (CTX-0079): extends the Phase C scaffold (`tests/compat/harness.rs`, `forbid(unsafe)`, headless, bounded) with bounded deterministic corpora covering wide/CJK/emoji/ZWJ/combining/ambiguous per the text-compatibility draft.

## Source

- Width: `bitty_term_state::char_cell_width` — compact East Asian Width approximation: `0` for combining/variation/ZWJ/direction marks (`0300..036F`, `200B..200F`, `FE00..FE0F`, `200D`, etc.), `2` for CJK Unified/Hangul/Fullwidth/Extension B/emoji blocks (`1F300..1F9FF`, `2E80..9FFF`, `AC00..D7A3`, `FF00..FF60`, `20000..3FFFD`), else `1`. Ambiguous-width chars (`00B7`, `2014`, `2192`, `2500`, `00A1`, `2026`) are `1` (no `EAW=A => 2` rule). See `crates/bitty-term-state/src/cell.rs` (`char_cell_width`, `is_zero_width`, `is_wide`).
- Determinism: invalid bytes → `U+FFFD` one cell, delegated to `vte`'s collector; `State` width invariants are headless, bounded, and `forbid(unsafe)`. Byte-by-byte re-parse identity is asserted in `tests/compat/harness.rs::parse_bounded`.
- Ghostty / kitty / WezTerm differential — feed same Unicode + SGR stream and compare `Snapshot` cell `width`/`spacer` invariants (invariant 2: no orphan spacers) and displayed `glyph` to reference grid dump (`kitty --dump-commands` reports `wcwidth`). Differential is snapshot-to-snapshot, not pixel.
- Existing baseline — `crates/bitty-vt/seeds/13-utf8-invalid-split.bin` (`\xff` → `U+FFFD` one cell), `replay.rs::fixture_fullscreen_app_replay` (`🎉` wide), `crates/bitty-term-state/src/cell.rs::char_cell_width` unit tests (`narrow_ascii_is_one_cell`, `cjk_and_fullwidth_are_two_cells`, `combining_marks_are_zero_cells`).

## Bounds and determinism

- `#![forbid(unsafe_code)]`, headless (no `winit`/`wgpu`/`Window`/`Surface`), `MAX_CORPUS_BYTES = 8 KiB` (`bitty-pty::READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 4096` (`BoundedString::MAX_LEN`). Every `corpus/*.bin` is `< 100 B` in this corpus; larger corpora must be split.
- `forbid(unsafe)` — `crates/bitty-vt` and `crates/bitty-term-state` both declare `#![forbid(unsafe_code)]` (workspace `unsafe_code = "deny"`); width logic (`char_cell_width`, `is_zero_width`, `is_wide`) is pure, bounded, and deterministic.
- Deterministic UTF-8 policy — invalid bytes → `U+FFFD` one cell, identically offline and live (`vte` collector). See corpus `07-invalid-utf8.bin`.
- Headless — `Parser -> TerminalAction -> State -> Snapshot` only; corpora never construct `winit::Window`/`wgpu::Surface`.

## Layout

```text
unicode/
  README.md
  corpus/
    01-wide-emoji.bin         # 🎉 U+1F389, U+FFFD, mixed narrow/wide
    02-combining.bin          # e + U+0301 → zero-width combining
    03-cjk-wide.bin           # CJK U+4E2D U+65E5, Hangul U+AC00, Fullwidth U+FF21-FF23, Extension B U+20000
    04-emoji-zwj.bin          # ZWJ family U+1F468 U+200D U+1F469 U+200D…, U+2764 U+FE0F variation selector
    05-ambiguous.bin          # Ambiguous EAW (U+00B7 U+2014 U+2192 U+2500) → 1 cell in bitty
    06-zero-width.bin         # U+0301, stacked U+0302+0303, U+200B/C/D, U+FE0E/FE0F, U+E0100
    07-invalid-utf8.bin       # raw 0xFF 0xFE 0x80 bytes → U+FFFD (deterministic replay, split-chunk)
    08-mixed-width.bin        # interleaved narrow/wide/combining/emoji + SGR 31m/0m
    placeholder.bin           # scaffold placeholder retained for layout compat
```

Each `corpus/*.bin` is consumed headlessly via `tests/compat/harness.rs::parse_bounded` → `actions_to_snapshot`; CI asserts `width ∈ {1,2}`, spacer pairing (invariant 2), and deterministic re-parse. Failures are tracked follow-ups, not silent skips.

## VT width logic (bounded, deterministic, headless, `forbid(unsafe)`)

- `char_cell_width`: `if cp < 0x0300 { 1 } else if is_zero_width(cp) { 0 } else if is_wide(cp) { 2 } else { 1 }`. Pure `matches!` tables, no allocation, no `unsafe`, no I/O.
- `State::apply(Print(GraphemeCell))` maps `0` → drop scalar (combining), `1` → one cell, `2` → lead + `wide_spacer`; `State::check_invariants` enforces `cell.width ∈ {1,2}` and no orphan spacers. All headless and bounded by `GRID_COLUMNS`/`GRID_ROWS`.
- See `docs/product/unicode-ime.md` (§ Width) and `docs/product/text-compatibility.md` (draft) for the normative width table reference and text-domain open items.
