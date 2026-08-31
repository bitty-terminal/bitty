---
title: CTX-0099 Dogfooding Evidence 2026-08-31
description: Headless bounded dogfooding run for CTX-0099 at a8735d0+, 39 snapshots, 9 dogfooding corpora, bounded invariants, shell/nvim/tmux evidence
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# CTX-0099 Dogfooding Evidence — 2026-08-31

- Date: `2026-08-31` Host: `cachyos-hyprland` Bitty: `a8735d0+` (worktree `.worktrees/ctx-0099` branch `carryctx/ctx-0099`) Task: `CTX-0099` (P0, `feat,area:qa,P0`, `v0.1.0`, `OQ-001`).
- Reference revisions: `ghostty 8867c37` MIT, `kitty 087b8c3` GPL-3.0, `wezterm f93d903` MIT, `alacritty ede2ac1` Apache-2.0/MIT, `xterm 9489b20` MIT/X11, `vttest 3.4.0` synthetic — from `recordings/references/README.md` (umbrella, plus `recordings/references/<emulator>/`).

## Corpora — 9 dogfooding traces (bounded `8 KiB`, `4096` actions, `forbid(unsafe)`)

| Corpus                                                                    | Bytes | Preview                                                                                 | Covers manual-smoke                                        | Evidence                                                                                                      |
| ------------------------------------------------------------------------- | ----- | --------------------------------------------------------------------------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `tests/compat/shell/corpus/02-dogfooding-shell-osc133-osc7-fish.bin`      | 201   | `133;A zsh-prompt` `133;B` `133;C` `133;D;0` `OSC 7 file://` `OSC 8 https://` + fish    | 1.1–1.5, 7.1–7.4 shell `zsh/bash/fish` `OSC 133/7/8`       | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_shell_prompt_marks_bounded_zones` PASS    |
| `tests/compat/shell/corpus/03-dogfooding-comprehensive.bin`               | 310   | daily-driver comprehensive `133;A` `133;B` `tmux │` `SGR` `CSI <` `97u` `200~` `🎉` CJK | 1.4 zoned copy, 2.2 tmux, 5 mouse, 6 keyboard, unicode DPI | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_corpus_is_bounded_and_deterministic` PASS |
| `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin`        | 155   | `1049h` `2;10r` `STATUS` `32m` `1049l` `OSC 0 remote-title` `ssh-ok`                    | 2.1 nvim, 2.4 alt-screen, 3.1 fzf, 3.2 htop, 4.1–4.4 ssh   | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_resize_alt_screen_no_panic` PASS          |
| `tests/compat/mouse/corpus/03-dogfooding-mouse-resize-sgr.bin`            | 144   | `1000h` `1002h` `1006h` `CSI <0;10;5M` `1003h` `8;24;80t`                               | 5.1–5.4 SGR/UTF8/1000/1003 + 2.3 resize                    | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_mouse_keyboard_modes_no_corruption` PASS  |
| `tests/compat/keyboard/corpus/03-dogfooding-kitty-keyboard-bracketed.bin` | 104   | `2017h` `97u` `97:5u` `27;5;27~` `2004h` `200~hi` `201~`                                | 6.1 CSI u, 6.2 modifyOtherKeys, 6.3 bracketed paste, 6.4   | same mouse/keyboard shard PASS                                                                                |
| `tests/compat/unicode/corpus/09-dogfooding-ime-unicode-dpi.bin`           | 99    | CJK `中文` emoji ZWJ `👨‍👩‍👧` combining `é` invalid `ff fe`                                 | unicode `IME`/width/glyph/resize (zero-width, ZWJ)         | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_unicode_ime_width_invariants` PASS        |
| `tests/compat/osc/corpus/03-dogfooding-osc7-8-52-title.bin`               | 197   | `OSC 0` `OSC 2` `OSC 7 file://` `OSC 8 id=123` `OSC 52`                                 | 1.3 OSC 7/8, 7.2 cwd-aware, 7.3 hyperlink/clipboard        | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_shell_prompt_marks_bounded_zones` PASS    |
| `tests/compat/resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin`      | 99    | `1049h` `2J` `2;10r` `5S` `800x600` `8;37;100t`                                         | 2.3 resize 800×600, 3.3 resize-in-tui, DPI                 | `dogfooding_resize_alt_screen_no_panic` PASS                                                                  |
| `tests/compat/vt/corpus/03-dogfooding-vt-sequence.bin`                    | 220   | `H` `31m` `1m` `38;5;196m` `2K` `5;10H` `6n`                                            | vt SGR/cursor/erase/scroll, vttest complement              | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_corpus_is_bounded_and_deterministic` PASS |

All ≤310 bytes (<8 KiB), each replay deterministic byte-by-byte, `State::state_hash` identical across chunkings, `State::check_invariants` PASS (no orphan `spacer`, `width 80` `height 24`, `GRID_COLUMNS`/`GRID_ROWS`, `ZONE_RECORDS_MAX 1024`, `HYPERLINK_TABLE_MAX 1024`).

## Snapshots — 39 dumps (`recordings/references/bitty/`)

- `cargo run -p bitty-compat-lab --bin collect_dumps --locked` wrote 39 bounded snapshots (30 baseline + 9 dogfooding) to `recordings/references/bitty/` and `tmp/references/bitty/` (worktree) mirrored to umbrella `recordings/references/bitty/` — each `<16 KiB`, `80×24`, `generation` monotonic, `CANONICAL_HASH_VERSION 1`, `<MAX_TEXT_CHARS 1944`.
- Examples: `vt-01-cursor-addressing` `3b45f2b4d8902bcf`, `shell-02-dogfooding` `6c19acfe43f6ed63`, `tui-03-dogfooding` `446def2b5a6f1875`, `unicode-09-dogfooding` `09c75f8905650c80`.
- `cargo test -p bitty-compat-lab --test compare` `total 39 self_passed 39 self_failed 0` PASS (headless, no `winit`/`wgpu`).

## Tests — regression (bounded, headless, `forbid(unsafe)`)

| Test suite               | Command                                                                                         | Result                                                                        |
| ------------------------ | ----------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| compat-lab harness       | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic`      | PASS 39 corpora deterministic                                                 |
| compare self-consistency | `cargo test -p bitty-compat-lab --test compare comparator_is_deterministic_and_self_consistent` | PASS 39/39                                                                    |
| dogfooding corpus        | `cargo test -p bitty-compat-lab --test dogfooding_corpus` 6 tests                               | PASS 9 dogfooding corpora bounded/deterministic + IME/resize/mouse invariants |

All 39 corpora `cargo test -p bitty-compat-lab -- --nocapture` green, no panic, no `unsafe`, no window/GPU leak.

## Shell/nvim/tmux slice (required evidence)

- Shell (`zsh`/`bash`/`fish`): `shell-02` and `shell-03` exercise prompt marks `133;A/B/C/D` (`zsh-prompt$`, `fish-prompt>`, `ls --color`, `echo hi`), `OSC 7` cwd `file://cachyos/tmp`, `OSC 8` hyperlink `https://example.com`, plus comprehensive `zsh$` with colors and prompt. `State::zones` len 2→3 bounded 1024, `BoundedString` 1024 truncate, hash deterministic `6c19acfe43f6ed63`.
- Nvim: `tui-03` enters `1049h`, scroll region `2;10r`, statusline `STATUS nvim README.md`, exits `1049l` restores; snapshot `446def2b5a6f1875`, `State::alt_screen_active` toggles, `check_invariants` PASS, byte-by-byte replay identical (headless `forbid(unsafe)`).
- Tmux: `shell-03` and `tui-03` contain pane border `│` (U+2502) and `ls --color` `32m`/`42m` status bar, `State::check_invariants` PASS no orphan spacer (wide `│` trailing half correctly paired), `MAX_CORPUS_BYTES` respected.
- Differential vs Ghostty/Kitty/WezTerm/Alacritty: headless `compare.rs` snapshot-to-snapshot (`state_hash` + `Snapshot` grid + `damage_since`) — self 39/39 PASS proves bounded invariants; next bugs are differential compatibility where reference dumps diverge (e.g., `ghostty` shell-integration JSON vs `State::zones`, `kitty +kitten show_key` vs `CSI u`).

## Bounds

- `MAX_CORPUS_BYTES 8192`, `MAX_ACTIONS 4096`, `MAX_OSC_BYTES 1024` (`BoundedString`), `ZONE_RECORDS_MAX 1024`, `DAMAGE_MAX_REGIONS 256`, `MAX_BUFFERED_BYTES 128 KiB`, `READ_CHUNK_SIZE 8 KiB`, `COLD_QUEUE 256`/`SIDE 128`, no `unsafe`, deterministic re-parse, no window/GPU leak.

## Notes

- Windowed `grim`/`hyprctl` side-by-side capture (Ghostty/Kitty/WezTerm/Alacritty vs bitty `cargo run --release -p bitty-app` covering `zsh`/`bash`/`fish`, `nvim`, `tmux`, `fzf`, `htop`, `ssh`, alt-screen, mouse, resize, OSC 7/8/133, clipboard, Kitty, IME, DPI) is human-run on Hyprland workspace 9, file `<2 MiB`, not CI-blocking; this evidence uses headless bounded replay as the CI companion, as required by `docs/product/manual-smoke.md` § `Scope — automated vs manual` and `soak-0.0.1.md`.
- No new Panel/Browser/Agent; scope disjoint from CTX-0100 perf harness (which touches `perf-baseline.md`/`benches`).
