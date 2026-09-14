<!-- markdownlint-disable MD025 -->

# Scrollback Compatibility (`tests/compat/scrollback`)

Scrollback retention, viewport scrolling, and alternate-screen isolation;
headless bounded.

## Source

- Bounded synthetic traces derived from the CTX-0404 M1/M2 compatibility
  matrix scope: lines pushed past the 80x24 viewport, scroll-region scroll (`CSI
S`/`CSI T`), and `?1049h`/`?1049l` alternate-screen save/restore of the
  primary buffer and scrollback.
- Scrollback capacity and invariant behaviour is owned by
  `bitty-term-state` (`SCROLLBACK_MAX_LINES`, `State::scrollback_len`,
  `scroll_under_screen_bottom_captures_scrollback`,
  `configured_scrollback_capacity_bounds_retention`). These corpora replay the
  same byte shape through the compat-lab harness so the M1/M2 matrix can cite a
  checked-in corpus.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`,
  `MAX_ACTIONS = 4096`.
- No window/GPU — corpora are escape bytes, no display.

## Layout

```text
scrollback/
  README.md
  corpus/
    01-scrollback-basic.bin       # 30 lines + CSI 3S scroll + cursor home
    02-scrollback-alt-screen.bin  # 1049h/1049l primary save and restore
```
