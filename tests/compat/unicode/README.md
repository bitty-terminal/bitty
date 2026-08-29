<!-- markdownlint-disable MD025 -->

# Unicode Compatibility (`tests/compat/unicode`)

Unicode corpus — width, `wcwidth`, combining, emoji ZWJ/variation selector, `vttest` charset menus.

## Source

- Width: `char_cell_width` (East Asian Wide/Fullwidth, emoji `1F300..1F9FF`, CJK, zero-width `0300..036F`, `200B..200F`). Captured from `vttest` charset menus and from `unicode-width` test vectors (normalized to bitty's `char_cell_width`).
- Ghostty / kitty / WezTerm differential — feed same Unicode + SGR stream and compare `Snapshot` cell `width`/`spacer` invariants (invariant 2: no orphan spacers) and displayed `glyph` to reference grid dump (`kitty --dump-commands` reports `wcwidth`).
- Existing baseline — `crates/bitty-vt/seeds/13-utf8-invalid-split.bin` (`\xff` → `U+FFFD` one cell), `replay.rs::fixture_fullscreen_app_replay` (`🎉` wide), `crates/bitty-term-state/src/cell.rs::char_cell_width` tests.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`.
- Deterministic UTF-8 policy — invalid bytes → `U+FFFD` one cell, delegated to `vte` collector.

## Layout

```text
unicode/
  README.md
  corpus/
    01-wide-emoji.bin         # 🎉, U+1F600, CJK
    02-combining.bin          # e + 0301 → é (zero-width)
    placeholder.bin
```
