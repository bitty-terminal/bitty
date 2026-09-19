<!-- markdownlint-disable MD025 -->

# Mode Compatibility (`tests/compat/modes`)

M1 private-mode and input golden corpus — mouse tracking, encodings, focus,
alternate scroll, bracketed paste, synchronized updates, alternate screen,
and application cursor keys; headless and bounded.

## Source

- Compatibility Milestone RFC (`docs/specifications/compatibility-milestone-rfc.md`)
  M1 rows "Mouse" (1000/1002/1003/1006), "Focus events" (1004), "Alternate
  scroll" (1007), "Bracketed paste" (2004), "Synchronized updates" (2026),
  "Screen modes" (alternate screen 47/1049, DECCKM), and "Cursor shape/style"
  (DECSCUSR).
- Input and pointer RFC (`docs/specifications/input-pointer-rfc.md`) for the
  DECCKM/DECKPAM encoder matrix and SGR-only M1 coordinate encoding.
- Terminal state RFC (`docs/specifications/terminal-state-rfc.md`) replay
  guarantees: the canonical state hash plus pending reply bytes pin every
  fixture.
- Wire shapes follow xterm `ctlseqs.txt` private modes; the byte sequences are
  curated from the accepted M1 matrix, not captured from a live terminal.

## Bounds

- `#![forbid(unsafe_code)]`, headless — `Parser -> TerminalAction -> State`
  only; no `winit`/`wgpu`/`Window`/`Surface`.
- `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096` per file.
- No window/GPU leak: the category contains escape bytes only.

## Layout

```text
modes/
  README.md
  corpus/
    01-mouse-1000-*.bin          # DECSET/DECRST 1000 normal tracking
    02-mouse-1002-*.bin          # DECSET/DECRST 1002 button-event tracking
    03-mouse-1003-*.bin          # DECSET/DECRST 1003 any-event tracking
    04-mouse-sgr-1006-*.bin      # 1000 + SGR 1006 coordinate encoding
    05-alt-scroll-1007-*.bin     # alternate scroll
    06-legacy-1005-*.bin         # UTF-8 coordinate encoding
    07-legacy-1015-*.bin         # urxvt coordinate encoding
    08-bracketed-paste-2004-*.bin
    09-focus-1004-*.bin          # FocusIn/FocusOut reporting
    10-sync-2026-*.bin           # synchronized updates
    11-alt-screen-1049-*.bin     # alt screen clear + restore
    12-alt-screen-47-*.bin       # alt screen (no clear)
    13-decckm-*.bin              # application cursor keys
    14-mode-transitions.bin      # every M1 mode set then cleared
    15-mode-set-sweep.bin        # every M1 mode left set
    16-decscusr.bin              # cursor shape/style
    17-mode-status-reply.bin     # DSR 6 + DA1 emitted reply bytes
```

Every `*-on`/`*-off` pair locks the canonical `State::state_hash` of the set
state and of the set-then-cleared state; the golden test
(`crates/bitty-compat-lab/tests/m1_mode_golden.rs`) asserts both plus the
semantic mode register and any emitted reply bytes.
