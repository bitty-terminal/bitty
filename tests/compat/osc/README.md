<!-- markdownlint-disable MD025 -->

# OSC Compatibility (`tests/compat/osc`)

OSC corpus — title, clipboard, hyperlink, prompt marks (OSC 133), and `vttest`-style OSC sweeps; differential against Ghostty/kitty/WezTerm.

## Source

- OSC 0 / 2 (title), 7 (cwd `file://`), 8 (hyperlink `id=`, `uri`), 52 (clipboard `p/c`), 133 (prompt marks `A/B/C/D`), 4 (color), and `OSC Unknown` fallback. Captured from `vttest` OSC menus and from real shell prompts (`zsh`/`fish` `OSC 133` sequences).
- Ghostty (OSC 8 hyperlink rendering), kitty (OSC 52 clipboard policy), WezTerm (OSC 133 semantic zones) dumps for differential — same bytes fed to each reference, grid dump diffed via `harness::diff_snapshots`.
- Existing `bitty-vt` baseline — `replay.rs::fixture_osc_sweep_replay` (clipboard read/write, cwd, unknown id 4, truncated title at `BoundedString::MAX_LEN`), `seeds/08-osc-title-hyperlink.bin`, `09-osc-hyperlink-prompt.bin`, `10-osc-clipboard-truncated.bin`. New corpora keep `BoundedString`/`BoundedBytes` truncation contract.

## Bounds

- `#![forbid(unsafe_code)]` — harness forbids unsafe.
- Headless — `Parser -> TerminalAction` only; clipboard/file/URL policy gates remain in `bitty-term-state` (inert until policy channel lands).
- `MAX_CORPUS_BYTES = 8 KiB`, `MAX_OSC_BYTES = 1024` per payload, `MAX_ACTIONS = 4096`. Oversized OSC truncates to `BoundedString::MAX_LEN` per `bitty-vt` (see `fixture_osc_sweep_replay`).

## Layout

```text
osc/
  README.md
  corpus/
    01-title-hyperlink.bin    # OSC 0/8 curated
    02-clipboard.bin          # OSC 52 c/p/? with base64
    03-dogfooding-osc7-8-52-title.bin  # OSC 7/8/52 + title dogfooding
    04-kitty-clipboard-5522.bin        # OSC 5522 non-goal lock (see below)
    vttest-osc-placeholder.bin
```

## Non-goal: kitty clipboard extension (OSC 5522)

- `corpus/04-kitty-clipboard-5522.bin` records a read-shaped and a write-shaped packet. CTX-0757 (bitty#1360) declares the extension a permanent non-goal for M1/v0.1.0: no typed action, no session reassembly, no reply. The corpus must keep replaying to bounded inert `OscUnknown { id: 5522 }` with no clipboard effect, locked by `crates/bitty-rich/tests/kitty_5522_nongoal.rs::kitty_5522_locked_as_inert_nongoal` and referenced as `ci` negative evidence from the compat-lab report row. Revisit only through a future RFC with security review.

No window/GPU leak — this lab never opens display.
