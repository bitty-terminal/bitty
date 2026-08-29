<!-- markdownlint-disable MD025 -->

# Keyboard Compatibility (`tests/compat/keyboard`)

Keyboard encoding corpus — kitty keyboard protocol, `modifyOtherKeys`, and `xterm`-style sequences; headless bounded harness.

## Source

- Kitty keyboard protocol (`CSI ? 2017` progressive, `CSI u` encode), `modifyOtherKeys` (`CSI ? 4`), and legacy `xterm` encodings. Captured from `kitty +kitten show_key -f` and `wezterm show-keys`, plus `vttest` keyboard menus.
- Ghostty / kitty / WezTerm differential — feed `corpus/*.bin` containing raw key-sequence bytes and compare `bitty_vt`/`bitty_term_state` interpretation (future `bitty-platform::keyboard` mapping) to reference `KeyEvent` logs. This scaffold is `Parser` side only; `bitty-platform` keyboard mapping is out-of-scope for Phase C.
- Existing baseline — `crates/bitty-platform/tests/keyboard_input.rs` and `crates/bitty-runtime/tests/keyboard_input.rs` (bounded pending queues).

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`.
- No window/GPU — no `winit::Window` constructed; keyboard corpus is bytes, not events.

## Layout

```text
keyboard/
  README.md
  corpus/
    01-kitty-keyboard.bin     # CSI u encodings
    02-modifyOtherKeys.bin    # CSI 27 ; modifier
    placeholder.bin
```
