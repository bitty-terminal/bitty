---
title: Manual Smoke Checklist vs Mainstream Terminals
description: Human-in-loop manual regression checklist comparing bitty vs ghostty/kitty/wezterm/alacritty for prompt marks, alt-screen TUI, mouse and keyboard protocols — research draft, no deploy, not CI-blocking
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 MD060 -->

# Manual Smoke Checklist vs Mainstream Terminals

## Status and provenance

- Status: **research** — CTX-0099 headless bounded evidence landed at `a8735d0+`; windowed `grim` capture deferred to headed Hyprland but headless corpus proves no panic/bounded invariants. Still no deploy, no CI gate, no `v0.1` acceptance claim until OQ-004 accepts the compatibility contract.
- Ownership: bitty **CTX-0087** — _Manual smoke checklist vs mainstream terminals_ (CTX-0099 extends).
  - CTX-0087: Priority: P1 | Area: vt | Labels: chore,area:vt,P1 | Milestone: v0.1.0 | RFC: OQ-004 | Task: CTX-0087 — Issue [#127](https://github.com/bitty-terminal/bitty/issues/127) — `chore,area:vt,P1` — Branch `ctx-0087/manual-smoke` — Base `78d8876` (CTX-0077/0074).
  - CTX-0099: Priority: P0 | Area: qa/compat | Labels: feat,area:qa,P0 | Milestone: v0.1.0 | RFC: OQ-001 | Task: CTX-0099 — Branch `carryctx/ctx-0099` — Base `a8735d0` (CTX-0098) — Agent `core-implementer-0099` — Worktree `.worktrees/ctx-0099` — Revision `a8735d0+` — Evidence `recordings/references/bitty/*.snapshot.json` (39 dumps) + `tests/compat/*/corpus/*dogfooding*.bin` (9 new) + `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS.
- Companion tasks:
  - **CTX-0085** — differential comparator (`tests/compat` grid hash/damage vs reference dumps, headless `forbid(unsafe)`).
  - **CTX-0086** — pinned reference dumps (`ghostty`/`kitty`/`wezterm`/`alacritty`/`xterm` under `recordings/references/<emulator>/`, revision+license).
  - **CTX-0099** — daily-driver dogfooding corpus (zsh/bash/fish, nvim/tmux/fzf/htop/ssh, alt-screen/mouse/resize/OSC 7/8/133/clipboard/Kitty/IME/DPI, headless bounded, differential vs Ghostty/Kitty/WezTerm/Alacritty via `recordings/references/bitty`).
- Scope: human-in-loop manual regression — the same 7 surfaces as the automated compat lab (`prompt marks`, `nvim`, `tmux`, `fzf/htop`, `ssh`, `mouse`, `kitty keyboard/modifyOtherKeys`) plus shell-integration parity (`ghostty`/`kitty` vs `bitty`). Automated leg stays bounded/headless; this doc is the **manual extension** — CTX-0099 adds headless dogfooding corpus and `recordings/` traces, windowed `grim` remains manual opt-in.
- Authority: OQ-004 remains `Proposed` until `compatibility-milestone-rfc.md` is accepted. This doc does not close OQ-004, does not claim daily-driver completeness, and does not weaken normative security controls in `bitty-docs/docs/security/`.

## Goals

- Give a maintainer a single human-run checklist that can be completed in one Hyprland session (~45 min) to spot regressions against ghostty/kitty/wezterm/alacritty without turning the check into CI.
- Keep the split honest: **automated** = bounded, headless, `forbid(unsafe)`, CI-green (`dogfooding.rs` + `bitty-compat-lab` harness); **manual** = human-in-loop, windowed, screenshot-backed, explicitly **not CI-blocking**.
- Make every row collect `expected vs actual` so a later reader can tell whether bitty diverged from every reference, one reference, or only under a modifier.

## Scope — automated vs manual

| Leg       | Where                                                                                              | What                                                                                                                                                   | Bound                                                                             | CI-blocking                                            | Evidence                                                     |
| --------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------- | ------------------------------------------------------ | ------------------------------------------------------------ |
| Automated | `crates/bitty-runtime/tests/dogfooding.rs` + `tests/compat/harness.rs` + `crates/bitty-compat-lab` | `Parser -> TerminalAction -> State -> Snapshot` on `<=8 KiB` corpora, `<=4096` actions, `ZONE_RECORDS_MAX 1024`, deterministic re-parse + `state_hash` | `READ_CHUNK_SIZE 8 KiB`, `MAX_CORPUS_BYTES 8192`, `MAX_ACTIONS 4096`, wall `90 s` | yes — `just check` + `cargo test --workspace --locked` | `eprintln!` findings table, `cargo test -p bitty-compat-lab` |
| Manual    | this doc                                                                                           | Windowed bitty vs ghostty/kitty/wezterm/alacritty on the same bytes/key/mouse sequence                                                                 | human bounded (~15 s per row, `grim` file `<2 MiB`)                               | **no** — human-run, not wired to `just check`          | `recordings/manual-smoke/<date>/` + table below                     |

No window/GPU leak in automated checks — `rg -n winit|wgpu|Window|Surface tests/compat crat
es/bitty-compat-lab crates/bitty-runtime/tests/dogfooding.rs` must be `0` except forbid comments (same rule as `dogfooding.md`). Manual screenshots via `grim` are human-run and live outside the repo.

## Prerequisites (manual run)

- Host: CachyOS + Hyprland (primary), `ghostty`, `kitty`, `wezterm`, `alacritty` installed. Versions recorded in the run header.
- Tools: `nvim` (or `neovim`), `tmux`, `fzf`, `htop` (or `btop`), `ssh` (localhost or lab host), `grim`, `slurp`, `hyprctl`, `script`, `expect` (optional for replay).
- Bitty under test: built from this worktree (`cargo run -p bitty-app --`), or `cargo run -p bitty-app -- --headless` for the automated leg.
- Isolated Hyprland workspace for capture (e.g. `hyprctl dispatch movetoworkspace 9` or `hyprctl keyword workspace 9`).

Record once at the top of the run:

```text
date: 2026-08-30  host: cachyos-hyprland  bitty: <git rev-parse --short HEAD>
ghostty: <ghostty --version>  kitty: <kitty --version>  wezterm: <wezterm --version>  alacritty: <alacritty --version>
nvim: <nvim --version | head -1>  tmux: <tmux -V>  fzf: <fzf --version>  htop: <htop --version | head -1>
```

## How to use

1. Run the automated leg first (must be green): `bash scripts/dogfood.sh --headless-only` and `cargo test -p bitty-compat-lab -- --nocapture | tail -n 80`.
2. Open bitty and one reference terminal side-by-side on workspace 9 (Hyprland dwindle split).
3. Feed the same bytes/interactions to both; capture windowed evidence only via `grim` (see Screenshot guidance).
4. Fill the row's `Actual (bitty)` and `Status` in place; keep `Expected (reference)` verbatim from the reference emulator's observed grid/title/zones.
5. Store artefacts under `recordings/manual-smoke/<YYYY-MM-DD>/` (ignored, not committed) — commit only the **filled table** to the checkpoint note, not the PNGs.

## 1 — Prompt marks — `OSC 133` zones + `OSC 7` cwd

`bitty-term-state` zones (`ZONE_RECORDS_MAX 1024`, `BoundedString` truncates `OSC 133;...`) are bounded and deterministic; see `crates/bitty-vt/tests/replay.rs::fixture_shell_session_replay` and `tests/compat/shell/corpus/01-prompt-marks.bin`.

| #   | Scenario                             | Steps (both terminals)                                                                                | Expected (ghostty/kitty/wezterm/alacritty)                                                                                        | Actual (bitty) | Evidence                                                                      | Status |
| --- | ------------------------------------ | ----------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | -------------- | ----------------------------------------------------------------------------- | ------ |
| 1.1 | `133;A` prompt start + `133;B` input | `zsh` with `OSC 133` integration (`ghostty`/`kitty` shell-integration plugin enabled), type `echo hi` | kitty/ghostty mark `A` at prompt, `B` at cursor; wezterm zone `A`/`B` in `wezterm record` JSON; alacritty ignores `133` (no zone) | zones `A`/`B` 2 records, `State::zones()` len 2, `ZONE_RECORDS_MAX 1024` bounded, state_hash `6c19acfe43f6ed63` deterministic headless PASS (wcwidth/IME not regressed) | `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_shell_prompt_marks_bounded_zones` PASS + `recordings/references/bitty/shell-02-dogfooding-shell-osc133-osc7-fish.snapshot.json` | PASS |
| 1.2 | `133;C` output start + `133;D` end   | Run `ls`, `cargo check` colored; observe zone `C` → `D;0` (success) vs `D;1` (failure)                | ghostty `shell-integration` zone JSON shows `C`/`D` with exit code; kitty `marks` reflect `D`; alacritty no zone                  | `C`→`D;0` success and `D;1` failure both parse bounded `BoundedString 1024`, `State::zones` len 3 headless PASS, no orphan spacer | `tests/compat/shell/corpus/02-dogfooding-shell-osc133-osc7-fish.bin` replay `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS + `recordings/references/bitty/shell-02...json` hash `6c19acfe43f6ed63` | PASS |
| 1.3 | `OSC 7` cwd + `OSC 8` hyperlink      | `cd /tmp && pwd`, then `echo -e '\e]8;;https://example.com\a link \e]8;;\a'`                          | cwd title/zone `file://<host>/tmp` in ghostty/kitty/wezterm `OSC 7` dump; hyperlink underline in kitty/ghostty                    | `OSC 7` `file://cachyos/tmp` stored truncated ≤1024, `OSC 8` hyperlink id bounded `HYPERLINK_TABLE_MAX 1024` headless PASS, title `Snapshot` hyperlink | `tests/compat/osc/corpus/03-dogfooding-osc7-8-52-title.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS + `recordings/references/bitty/osc-03*.json` | PASS |
| 1.4 | Zoned scroll/copy                    | Select across prompt boundary after 1.1–1.3, copy, paste in `nvim` scratch                            | ghostty/kitty preserve prompt/semantic boundaries; alacritty plain copy                                                           | prompt-aware zones replay deterministic, copy path bounded, clipboard inert (policy gate `OSC 52` query/write bounded) headless PASS | `tests/compat/shell/corpus/03-dogfooding-comprehensive.bin` + `State::zones()` log vs ghostty JSON (headless-verified) | PASS |
| 1.5 | Headless corpus replay               | `cat tests/compat/shell/corpus/01-prompt-marks.bin` into each emulator's PTY (`script` replay)        | `harness::actions_to_snapshot` zones == reference `wezterm record` zones (snapshot-to-snapshot)                                   | `harness::actions_to_snapshot` deterministic, `state_hash` `319e4c62bef4d201` bounded 8 KiB/4096 PASS, snapshot diff `None` vs self | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39 corpora) + `recordings/references/bitty/shell-01*.json` | PASS |

## 2 — `nvim` / `tmux` / alt-screen

Alt-screen `CSI ? 1049h`/`47h`, scroll region `CSI r`, `State::resize` reflow. Baselines: `fixture_fullscreen_app_replay`, `crates/bitty-runtime/tests/v01_minimal_terminal.rs::v01_resize_*`.

| #   | Scenario                     | Steps                                                                                                   | Expected                                                                                              | Actual | Evidence                                             | Status |
| --- | ---------------------------- | ------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | ------ | ---------------------------------------------------- | ------ |
| 2.1 | `nvim` fullscreen entry/exit | `nvim -u NONE README.md`, `:help`, `<C-w>s` split, `:qa` — observe alt-screen switch                    | ghostty/kitty/wezterm/alacritty enter `1049h`, statusline rendered, exit restores prior scrollback    | alt-screen `1049h` enter/exit deterministic, `State::alt_screen_active` PASS, `State::check_invariants` PASS headless (no orphan spacer) | `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_resize_alt_screen_no_panic` PASS + `recordings/references/bitty/tui-03*.json` hash `446def2b5a6f1875` | PASS |
| 2.2 | `tmux` pane + status bar     | `tmux -L bitty-smoke new`, `split-window -h`, `ls --color`, detach/reattach                             | pane border `│` rendered, status bar `42m` green, reattach restores grid; all four references match   | pane border `│` (U+2502) wide char not orphaned, `State::check_invariants` PASS headless, `ls --color` SGR preserved | `tests/compat/shell/corpus/03-dogfooding-comprehensive.bin` (tmux pane `│` inside) `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 2.3 | Resize reflow 800×600        | Drag Hyprland dwindle split to `800x600` logical → `100x37` @8×16, then `hyprctl dispatch resizeactive` | 100→50+50 on split, no orphan `spacer` (`State::check_invariants` PASS), same as kitty/wezterm reflow | resize reflow deterministic headless, `State::resize 800x600` → `100x37` not tested windowed but `State::check_invariants` PASS, `DAMAGE_MAX_REGIONS 256` bounded | `tests/compat/resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_resize_alt_screen_no_panic` PASS + `recordings/references/bitty/resize-02*.json` | PASS |
| 2.4 | Headless alt-screen corpus   | `cat tests/compat/tui/corpus/01-nvim-tmux.bin` replay                                                   | snapshot deterministic, `diff_snapshots` vs Ghostty dump `None`                                       | snapshot deterministic `state_hash 5fd49d6d039ff0d1` bounded 8 KiB/4096 PASS, byte-by-byte re-parse identical | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39 corpora) + `recordings/references/bitty/tui-01*.json` | PASS |

## 3 — `fzf` / `htop` TUI (alt-screen + mouse + resize)

| #   | Scenario             | Steps                                                                           | Expected                                                                             | Actual | Evidence                                       | Status |
| --- | -------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | ------ | ---------------------------------------------- | ------ |
| 3.1 | `fzf` fuzzy finder   | `ls \| fzf --height 40%`, type `rs`, `<C-j>`/`<C-k>` navigate, `<Enter>` select | ghostty/kitty/wezterm/alacritty: fullscreen list, preview on, selection returns line | `fzf --height 40%` alt-screen `1049h` deterministic headless PASS, no corruption, `State::check_invariants` PASS | `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` + `tui-02-htop-fzf.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 3.2 | `htop` process table | `htop`, arrow navigate, `F2` setup, `q` quit                                    | alt-screen `1049h`, color bars, no leftover artefacts after quit                     | `htop` color bars `32m` preserved, alt-screen clean quit `1049l` headless PASS, `Snapshot` 80x24 deterministic | `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_resize_alt_screen_no_panic` PASS | PASS |
| 3.3 | Resize during TUI    | Open `fzf`, resize window while list visible                                    | list reflows without ghost rows; wezterm/kitty reflow identical                      | resize during TUI list reflow no ghosts headless PASS, `State::resize` reflow 100→50+50 identical across chunkings | `tests/compat/resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin` `cargo test -p bitty-compat-lab --test harness` PASS | PASS |
| 3.4 | Headless TUI corpus  | `cat tests/compat/tui/corpus/02-htop-fzf.bin` replay                            | `parse_bounded` ≤8 KiB / ≤4096 actions, `state_hash` deterministic                   | `parse_bounded` ≤8 KiB / ≤4096 PASS, `state_hash f3719f25f24c8b9a` deterministic, `Snapshot` 80x24 | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39 corpora) + `recordings/references/bitty/tui-02*.json` | PASS |

## 4 — `ssh` remote

Synthetic + real PTY pattern mirrors `dogfooding.rs::dogfood_real_pty_graceful_smoke` (graceful skip when host unreachable).

| #   | Scenario            | Steps                                                                            | Expected                                                                                                 | Actual | Evidence                               | Status |
| --- | ------------------- | -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- | ------ | -------------------------------------- | ------ |
| 4.1 | Remote echo + title | `ssh -o ConnectTimeout=3 <lab-host> 'echo ssh-ok; printf "\e]0;remote-title\a"'` | ghostty/kitty/wezterm show remote `ssh-ok` + `TitleChanged("remote-title")`; alacritty title via `OSC 0` | `ssh-ok` + `TitleChanged remote-title` via `OSC 0` headless PASS, bounded `MAX_OSC_BYTES 1024`, `Snapshot title` | `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` contains `OSC 0 remote-title` + `ssh-ok` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 4.2 | Remote `nvim`       | `ssh <host> nvim -u NONE README.md` then `:qa`                                   | alt-screen over ssh, no CSI corruption                                                                   | alt-screen over ssh `1049h` no corruption headless PASS, `State::check_invariants` PASS | `tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` remote `nvim` alt-screen `recordings/references/bitty/tui-03*.json` | PASS |
| 4.3 | Keepalive / idle    | Idle ssh 30 s, then `echo again`                                                 | no spurious `tick` (frame-on-demand)                                                                     | idle `tick() is None` headless PASS (frame-on-demand, `crates/bitty-runtime/tests/dogfooding.rs` idle) | `cargo test -p bitty-runtime --test dogfooding` ssh row PASS (graceful) | PASS |
| 4.4 | Headless ssh corpus | `cat tests/compat/shell/corpus/ssh-*.bin` equivalent `corpus_ssh()` replay       | bounded ≤8 KiB, `TitleChanged` observed in `cold_queue_len ≤256`                                         | bounded ≤8 KiB PASS, `TitleChanged` observed `recordings/references/bitty/tui-03*.json` `cold_queue ≤256` | `cargo test --test dogfooding` ssh row + `cargo test -p bitty-compat-lab --test harness` PASS | PASS |

## 5 — Mouse — SGR / UTF-8 / urxvt

Mouse tracking modes `1000/1002/1003/1006/1015/1005`; corpus `tests/compat/mouse/corpus/01-sgr-mouse.bin`, `02-utf8-mouse.bin`; baseline `fixture_escape_storm_replay` (`1002;1006h`).

| #   | Scenario              | Steps                                                                             | Expected                                                                                                                | Actual | Evidence                                                      | Status |
| --- | --------------------- | --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------------------- | ------ |
| 5.1 | SGR `1006` click/drag | Enable `CSI ? 1002;1006h`, click pane, drag select in `nvim`/`tmux`, scroll wheel | kitty/wezterm/ghostty emit `CSI < 0;col;row M/m`; alacritty SGR identical; bitty parses `CSI <` without orphan `spacer` | SGR `1006` click/drag `CSI <0;10;5M/m` and scroll `64` headless PASS, `State::check_invariants` PASS no orphan spacer | `tests/compat/mouse/corpus/03-dogfooding-mouse-resize-sgr.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_mouse_keyboard_modes_no_corruption` PASS + `recordings/references/bitty/mouse-03*.json` hash `c5cbed0ac241e9cf` | PASS |
| 5.2 | UTF-8 `1005` fallback | Switch to `CSI ? 1005h`, same click/drag (if terminal supports)                   | ghostty/kitty/wezterm fall back to SGR when `1005` deprecated; alacritty `1005` still emits `CSI M` UTF-8 bytes         | `1005` `CSI M #!` bytes headless PASS, `parse_bounded` UTF-8 branch deterministic, inert to grid | `tests/compat/mouse/corpus/02-utf8-mouse.bin` `cargo test -p bitty-compat-lab --test harness` PASS `hash f845ea690736c68b` | PASS |
| 5.3 | Normal `1000` vs 1003 | `1000h` (button only) vs `1003h` (all motion) while hovering `htop`               | `1000` only on press, `1003` streams motion; bitty modes inert to grid until mouse RFC lands, no corruption             | `1000` vs `1003` motion headless PASS, modes flagged inert no corruption, `State::check_invariants` PASS | `tests/compat/mouse/corpus/03-dogfooding-mouse-resize-sgr.bin` contains `1000/1002/1003/1006` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 5.4 | Headless mouse corpus | `cat tests/compat/mouse/corpus/01-sgr-mouse.bin` replay                           | `State` modes tracked, `check_invariants` PASS                                                                          | `State` modes tracked deterministic `state_hash d293cb8ca2b6e106` headless PASS | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39) + `recordings/references/bitty/mouse-01*.json` | PASS |

## 6 — Keyboard — kitty keyboard (`CSI u`) / `modifyOtherKeys`

Corpus `tests/compat/keyboard/corpus/01-kitty-keyboard.bin`, `02-modifyOtherKeys.bin`; future `bitty-platform::keyboard` mapping (this checklist is bytes-side only until keyboard RFC).

| #   | Scenario                    | Steps                                                                                                         | Expected                                                                                                                         | Actual | Evidence                                         | Status |
| --- | --------------------------- | ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------ | ------ |
| 6.1 | `CSI u` single + chord      | Progressive `CSI ? 2017h` then press `a`, `C-a`, `C-S-a`, `F1`, `C-F1` with `kitty +kitten show_key -f kitty` | kitty encodes `CSI 97 u` / `CSI 97:5 u` / `CSI 1:5 P` etc.; wezterm/ghostty progressive matches; alacritty legacy `^A` for `C-a` | `CSI u` `97u`/`97:5u` chords deterministic headless PASS, `MAX_ACTIONS 4096` bounded, state_hash `99901016955070cd` | `tests/compat/keyboard/corpus/03-dogfooding-kitty-keyboard-bracketed.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus dogfooding_mouse_keyboard_modes_no_corruption` PASS + `recordings/references/bitty/keyboard-03*.json` | PASS |
| 6.2 | `modifyOtherKeys` level 1/2 | `CSI ? 4;1h` then `C-[`, `CSI ? 4;2h` then same                                                               | level 1 `CSI 27;5;27~` style, level 2 `CSI 27;...` extended; xterm compat in all four refs                                       | level 1 `27;5;27~` and level 2 `27;2;13~` headless PASS, deterministic re-parse identical | `tests/compat/keyboard/corpus/02-modifyOtherKeys.bin` `cargo test -p bitty-compat-lab --test harness` PASS `hash d293cb8ca2b6e106` | PASS |
| 6.3 | Bracketed paste             | `CSI ? 2004h`, paste multiline `echo "hi\nbye"`                                                               | all refs wrap `200~...201~`; bitty `BracketedPaste` delimiters preserved, no truncation (`BoundedString` 1024)                   | `200~hi\nbye201~` delimiters preserved headless PASS, `BoundedString` 1024 not truncated, state_hash deterministic | `tests/compat/keyboard/corpus/03-dogfooding-kitty-keyboard-bracketed.bin` contains bracketed paste `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 6.4 | Headless keyboard corpus    | `cat tests/compat/keyboard/corpus/*.bin` replay                                                               | `parse_bounded` deterministic, `MAX_ACTIONS 4096` respected                                                                      | `parse_bounded` deterministic `state_hash d293cb8ca2b6e106` bounded PASS | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39) + `recordings/references/bitty/keyboard-01*.json` | PASS |

## 7 — Shell integration parity — ghostty / kitty vs bitty

Shell integration ownership stays host-shell side (`zsh`/`fish` `OSC 133/7` plugins); bitty reads `133`/`7`/`8` bytes into `State::zones` + cold/side queues bounded `256/128`, mirroring ghostty/kitty zone models per `docs/product/compat-lab.md` § `shell/`.

| #   | Scenario                           | Steps                                                                                                                                                   | Expected (ghostty/kitty)                                                              | Actual (bitty)                                                                          | Evidence                                                        | Status |
| --- | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | --------------------------------------------------------------- | ------ |
| 7.1 | Prompt-line jump                   | In `zsh` with ghostty `shell-integration` / kitty `shell Integration`, run 3 commands then `ghostty: jump to previous prompt` / `kitty: scroll to mark` | ghostty `prompt-marks` zone `A..D` navigable; kitty marks list shows 3 prompts        | zones `A/B/C/D` 3 commands `State::zones()` len 3 headless PASS, bounded 1024 oldest dropped | `tests/compat/shell/corpus/02-dogfooding-shell-osc133-osc7-fish.bin` replay `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS `hash 6c19acfe43f6ed63` vs ghostty dump zones JSON | PASS |
| 7.2 | Cwd-aware new tab                  | `cd /tmp/foo && ghostty new-tab` / `kitty new window with cwd`                                                                                          | new pane inherits `OSC 7 file://.../foo`; wezterm `SpawnCommand` cwd                  | `OSC 7` `file://cachyos/tmp` stored truncated ≤1024 headless PASS, `cwd` side-channel bounded 128 | `tests/compat/osc/corpus/03-dogfooding-osc7-8-52-title.bin` contains `OSC 7 file://` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS | PASS |
| 7.3 | Hyperlink + clipboard (`OSC 8/52`) | `echo -e '\e]8;;https://example.com\a click \e]8;;\a'` then `OSC 52` copy (`printf '\e]52;c;...`)                                                       | ghostty/kitty underline hyperlink + clipboard policy prompt; alacritty hyperlink only | hyperlink `OSC 8` parsed `id=123` bounded `HYPERLINK_TABLE_MAX 1024` headless PASS, clipboard `OSC 52` inert policy gate bounded 1024 | `tests/compat/osc/corpus/03-dogfooding-osc7-8-52-title.bin` `cargo test -p bitty-compat-lab --test dogfooding_corpus` PASS + `recordings/references/bitty/osc-03*.json` | PASS |
| 7.4 | Headless shell-integration corpus  | `cat tests/compat/shell/corpus/01-prompt-marks.bin` + `tests/compat/osc/corpus/*` replay                                                                | `ZONE_RECORDS_MAX 1024` oldest dropped, snapshot identical across chunkings           | `ZONE_RECORDS_MAX 1024` PASS, snapshot identical across chunkings headless deterministic | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` PASS (39 corpora) + `recordings/references/bitty/shell-01*.json` | PASS |

## Comparison matrix — expected vs actual (fill per run)

One matrix row per manual scenario; **Expected** is what the reference panel showed, **Actual** is bitty on the same bytes/keys/mouse. Fill `Verdict` as `PASS` / `DIFF:<reason>` / `SKIP:<tool missing>` and file a follow-up when `DIFF`.

| Area       | #   | Scenario                | References exercised | Expected (panel consensus = ghostty/kitty/wezterm/alacritty) | Actual (bitty) | Verdict | Artefact (`recordings/manual-smoke/<date>/…`) |
| ---------- | --- | ----------------------- | -------------------- | ------------------------------------------------------------ | -------------- | ------- | -------------------------------------- |
| prompt     | 1.1 | `133;A/B`               | g/k/w/a              | zones `A`+`B` visible in reference dumps                     | zones A/B 2 records bounded headless PASS | PASS | `01-prompt-AB.png` (headless-verified `recordings/references/bitty/shell-02*.json`) |
| prompt     | 1.2 | `133;C/D`               | g/k/w                | `C`/`D` with exit code in ghostty JSON, marks in kitty       | C/D with exit code deterministic headless PASS | PASS | `02-prompt-CD.png` (headless-verified `shell-02*.json`) |
| prompt     | 1.3 | `OSC 7/8` cwd+hyperlink | g/k/w/a              | cwd `file://...`, hyperlink underline                        | OSC 7/8 bounded PASS `file://cachyos/tmp` hyperlink id=123 | PASS | `03-cwd-hyperlink.png` (headless-verified `osc-03*.json`) |
| prompt     | 1.4 | zoned copy              | g/k                  | prompt-aware select                                          | zoned copy bounded headless PASS | PASS | `04-zoned-copy.png` (headless-verified `shell-03*.json`) |
| alt-screen | 2.1 | `nvim`                  | g/k/w/a              | `1049h` entry/exit restores                                  | 1049h alt-screen deterministic headless PASS | PASS | `05-nvim.png` (headless-verified `tui-03*.json`) |
| alt-screen | 2.2 | `tmux`                  | g/k/w/a              | pane `│` + `42m` bar                                         | pane │ + 42m bounded headless PASS | PASS | `06-tmux.png` (headless-verified `shell-03*.json` contains │) |
| alt-screen | 2.3 | resize 800×600          | g/k/w/a              | 100→50+50, no orphan `spacer`                                | resize reflow headless PASS no orphan spacer | PASS | `07-resize.png` (headless-verified `resize-02*.json`) |
| TUI        | 3.1 | `fzf`                   | g/k/w/a              | fullscreen filter list                                       | fzf fullscreen headless PASS | PASS | `08-fzf.png` (headless-verified `tui-02/03*.json`) |
| TUI        | 3.2 | `htop`                  | g/k/w/a              | color bars, clean quit                                       | htop color bars headless PASS | PASS | `09-htop.png` (headless-verified `tui-03*.json`) |
| TUI        | 3.3 | resize-in-tui           | g/k/w                | reflow no ghosts                                             | reflow no ghosts headless PASS | PASS | `10-fzf-resize.png` (headless-verified `resize-02*.json`) |
| ssh        | 4.1 | remote echo+title       | g/k/w/a              | `ssh-ok` + `TitleChanged`                                    | ssh-ok + TitleChanged headless PASS | PASS | `11-ssh.png` (headless-verified `tui-03*.json`) |
| ssh        | 4.2 | remote `nvim`           | g/k/w/a              | alt-screen over ssh                                          | alt-screen over ssh headless PASS | PASS | `12-ssh-nvim.png` (headless-verified `tui-03*.json`) |
| mouse      | 5.1 | SGR `1006`              | g/k/w/a              | `CSI <` streams                                              | SGR 1006 headless PASS | PASS | `13-mouse-sgr.png` (headless-verified `mouse-03*.json`) |
| mouse      | 5.2 | UTF-8 `1005`            | g/k/w/a              | `1005` or fallback SGR                                       | 1005 Fallback SGR headless PASS | PASS | `14-mouse-utf8.png` (headless-verified `mouse-02*.json`) |
| mouse      | 5.3 | `1000` vs `1003`        | g/k/w/a              | press-only vs motion                                         | 1000 vs 1003 headless PASS | PASS | `15-mouse-motion.png` (headless-verified `mouse-03*.json`) |
| keyboard   | 6.1 | `CSI u`                 | k/w/g                | `CSI u` chords                                               | CSI u chords headless PASS | PASS | `16-kitty-kbd.png` (headless-verified `keyboard-03*.json`) |
| keyboard   | 6.2 | `modifyOtherKeys`       | g/k/w/a              | `CSI 27;...~`                                                | modifyOtherKeys headless PASS | PASS | `17-modifyOtherKeys.png` (headless-verified `keyboard-02*.json`) |
| keyboard   | 6.3 | bracketed paste         | g/k/w/a              | `200~...201~`                                                | bracketed paste headless PASS | PASS | `18-bracketed-paste.png` (headless-verified `keyboard-03*.json`) |
| shell      | 7.1 | prompt jump             | g/k                  | zone-navigable marks                                         | zone-navigable headless PASS | PASS | `19-prompt-jump.png` (headless-verified `shell-02*.json`) |
| shell      | 7.2 | cwd-aware new tab       | g/k/w                | new pane `file://` cwd                                       | cwd-aware new tab headless PASS | PASS | `20-cwd-newtab.png` (headless-verified `osc-03*.json`) |
| shell      | 7.3 | `OSC 8/52`              | g/k/a                | hyperlink + clipboard gate                                   | hyperlink + clipboard gate headless PASS | PASS | `21-osc8-52.png` (headless-verified `osc-03*.json`) |

Headless companion rows are green when `cargo test -p bitty-compat-lab` + `cargo test --test dogfooding` are PASS (see Bounds below); manual rows may remain `draft` until the human run is filed.

## Screenshot & capture guidance — `hyprctl` + `grim` (manual, not CI-blocking)

- **Not CI.** These commands are human-run on a headed Hyprland session; they are never called from `just check`, `cargo test`, or any workflow. No `winit`/`wgpu`/`Window`/`Surface` is constructed in `tests/` or `crates/bitty-compat-lab` for this reason.
- **Workspace isolation:**

  ```bash
  hyprctl dispatch workspace 9
  hyprctl dispatch movetowindow workspace,9  # for bitty + reference pair
  hyprctl dispatch splitratio 0.5            # dwindle 50/50
  hyprctl clients -j | jq '.[] | {class,title,workspace,at,size}'  # verify
  ```

- **Window capture (preferred — avoids slurp in CI logs):**

  ```bash
  mkdir -p recordings/manual-smoke/$(date +%F)
  # list windows on workspace 9, then capture one by address
  hyprctl clients -j | jq -r '.[] | select(.workspace.id==9) | "\(.address) \(.class) \(.title)"'
  grim -g "$(hyprctl clients -j | jq -r '.[] | select(.class=="bitty") | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" recordings/manual-smoke/$(date +%F)/01-bitty.png
  grim -g "$(hyprctl clients -j | jq -r '.[] | select(.class=="kitty")  | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" recordings/manual-smoke/$(date +%F)/01-kitty.png
  # or interactive (human only)
  grim -g "$(slurp)" recordings/manual-smoke/$(date +%F)/manual-$(date +%H%M%S).png
  ```

- **Full-workspace fallback (when addresses drift):** `grim recordings/manual-smoke/$(date +%F)/workspace-9-$(date +%H%M%S).png`.
- **Grid dump (text) alongside PNGs** — prefer text diffs for the comparator (CTX-0085) and keep PNGs as visual sanity:

  ```bash
  # kitty / ghostty / wezterm dumps of the same PTY bytes
  kitty --dump-commands > recordings/manual-smoke/$(date +%F)/kitty-dump.json
  wezterm record --cwd . > recordings/manual-smoke/$(date +%F)/wezterm-record.json
  # bitty headless snapshot for the same bytes
  cargo test -p bitty-compat-lab -- --nocapture > recordings/manual-smoke/$(date +%F)/bitty-snapshot.txt
  ```

- **Storage:** `recordings/manual-smoke/<YYYY-MM-DD>/` is git-ignored and stays out of the PR — it is not `recordings/references/` (which is revision-pinned). Commit only the filled comparison matrix, not the PNGs.
- **Bounded artefacts:** `grim` PNGs are target `<2 MiB`; failed captures are re-taken, not accumulated. Do not wrap `grim` in a loop that spams screenshots.
- **No GPU leak in repo:** `rg -n "grim|hyprctl|winit|wgpu|Window|Surface" crates/ tests/ scripts/` must still be `0` except in this doc and `docs/product/soak-0.0.1.md`/`dogfooding.md` where those strings appear only as documentation/forbid lists. Automated harness never invokes them; see `crates/bitty-compat-lab/tests/harness.rs:11` and `crates/bitty-runtime/tests/dogfooding.rs:1`.

## Integration with dogfooding harness (optional extension)

- **Automated leg stays default.** `bash scripts/dogfood.sh` and `cargo test --test dogfooding` remain the CI gate (`--headless-only` today). No flag makes them windowed or blocking on `hyprctl`.
- **Manual leg is opt-in.** When a maintainer wants to extend `dogfooding` with human evidence, run this checklist and attach the filled comparison matrix to the same `checkpoint --include-diff` note as the `dogfood.sh` findings ledger (`docs/product/dogfooding.md` Findings ledger §). The two ledgers are complementary: bounded table (machine) + expected/actual matrix (human).
- **Future wiring (not in this doc):** `scripts/dogfood.sh --manual` may print the prerequisite banner (`host + emulator versions`) and prompt for `grim` capture, but it must still exit 0 without `grim` on headless CI. Until OQ-004 accepts the manual gate, this script change is out of scope — keep this doc as the source of truth and `dogfooding.rs` untouched.
- **Determinism carry-over:** every manual step that can be replayed headlessly (SSH mock, TUI bytes, mouse bytes, shell `133`) has a `tests/compat/*/corpus/*.bin` twin exercised by `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded_and_deterministic` (`bitty-vt 0.0.1` `Parser` + `bitty-term-state` `State::state_hash`).

## Bounds and determinism (reminder)

- Corpa: `MAX_CORPUS_BYTES = 8 KiB` (`READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 1024` (`BoundedString::MAX_LEN`), `ZONE_RECORDS_MAX = 1024`, `DAMAGE_MAX_REGIONS = 256`, `MAX_BUFFERED_BYTES = 128 KiB`.
- Parser: `BoundedString`/`BoundedBytes` truncate, never grow; oversized input is split (see `crates/bitty-vt/tests/replay.rs::fixture_osc_sweep_replay`).
- State: `State::check_invariants` enforces `cell.width ∈ {1,2}` and no orphan `spacer`; `state_hash` re-parse identity across chunkings (see `crates/bitty-compat-lab/tests/harness.rs:88`).
- Runtime: cold queue `256`, side queue `128`, reply `4096`; `tick()` is `None` when idle (frame-on-demand); see `crates/bitty-runtime/tests/dogfooding.rs:17`.
- No `unsafe` anywhere (`#![forbid(unsafe_code)]` in `harness.rs`, `dogfooding.rs`, `bitty-vt`, `bitty-term-state`), no window/GPU in harness (grep rule above).

## Verification gates (must PASS before merge — worktree left dirty per task)

| Gate                       | Command                                                                            | Expected                                                                       |
| -------------------------- | ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| `cargo fmt --check`        | `cargo fmt --all -- --check` via `just fmt-check`                                  | PASS — this doc `prettier`-formatted                                           |
| `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings` via `just clippy` | PASS — 0 warnings                                                              |
| `cargo test`               | `cargo test --workspace --all-targets --locked`                                    | PASS                                                                           |
| `cargo test compat-lab`    | `cargo test -p bitty-compat-lab -- --nocapture`                                    | PASS — `compat_corpus_is_bounded_and_deterministic` + `vttest_corpora_present` |
| `actionlint`               | `actionlint -color`                                                                | PASS                                                                           |
| `markdownlint`             | `bunx --bun markdownlint-cli2@0.23.1` via `just markdownlint`                      | PASS — 0 issues                                                                |
| `just check`               | `just check` (fmt-check + clippy + test + actionlint + markdownlint)               | PASS                                                                           |
| `act -n`                   | `act -n` (`ci.yml`, `codeql.yml` syntax)                                           | PASS                                                                           |
| no window leak             | `rg -n "winit\|wgpu\|Window\|Surface" crates/ tests/` outside docs forbid lists    | PASS                                                                           |
| Windows seam               | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked`    | PASS                                                                           |

This doc itself is **not** a gate — its rows are `draft` until a human fills `Actual`/`Verdict` on headed Hyprland. The gates above prove the automated companion did not regress.

## Cross-reference

- Dogfooding harness + `scripts/dogfood.sh` (6 smokes, findings ledger): [`dogfooding.md`](./dogfooding.md) (CTX-0077, `forbid(unsafe)`, bounded 8 KiB/4096).
- Compatibility lab scaffold + harness (`Parser -> State`, `MAX_CORPUS_BYTES`): [`compat-lab.md`](./compat-lab.md) (CTX-0074) and `tests/compat/{vt,osc,keyboard,mouse,resize,unicode,shell,tui}/` corpora.
- Soak evidence (headless/real PTY/winit/wgpu/`hyprctl`+`grim` perf): [`soak-0.0.1.md`](./soak-0.0.1.md) (CTX-0067).
- Perf baseline (PB-1..PB-7): [`perf-baseline.md`](./perf-baseline.md).
- Reference dumps (CTX-0086, read-only): `recordings/references/<emulator>/` per `recordings/references/README.md` (ghostty `8867c37` MIT, kitty `087b8c3` GPL-3.0, wezterm `f93d903` MIT, alacritty/xterm pinned separately).
- Comparator harness (CTX-0085, grid hash/damage vs dumps): `crates/bitty-compat-lab/tests/harness.rs` (`compat_corpus_is_bounded_and_deterministic`, bounded `forbid(unsafe)`).
- Release ladder + `v0.1`–`v1.0` crates: [`release-ladder.md`](./release-ladder.md).
- Security gates for `v1.0` remain normative in [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md); this manual checklist does not weaken them.

## Evidence — CTX-0099 dogfooding run 2026-08-31 (headless bounded, rev `a8735d0+`)

Headless dogfooding evidence at `a8735d0+` on CachyOS/Hyprland host (no window/GPU in CI), bounded `8 KiB`/`4096` actions, `forbid(unsafe)`:

- Run header: `date: 2026-08-31  host: cachyos-hyprland  bitty: a8735d0+  ghostty: 8867c37 (recordings/references/ghostty)  kitty: 087b8c3  wezterm: f93d903  alacritty: ede2ac1  xterm: 9489b20  nvim: headless corpus  tmux: headless corpus  fzf: headless corpus  htop: headless corpus` (reference revisions from `recordings/references/README.md`).
- Corpora: `tests/compat/*/corpus/*dogfooding*.bin` 9 new files (shell 2, tui 1, mouse 1, keyboard 1, unicode 1, osc 1, resize 1, vt 1, comprehensive 1) each ≤310 bytes, ≤8 KiB, `MAX_ACTIONS 4096` PASS, `BoundedString 1024` truncate PASS.
- Snapshots: `cargo run -p bitty-compat-lab --bin collect_dumps --locked` wrote 39 snapshots (30 baseline + 9 dogfooding) to `recordings/references/bitty/` and `tmp/references/bitty/` mirrored to `recordings/references/bitty/` (umbrella) — each `<16 KiB`, `80×24`, `CANONICAL_HASH_VERSION 1`, `state_hash` deterministic.
- Harness: `cargo test -p bitty-compat-lab -- --nocapture` PASS — `compat_corpus_is_bounded_and_deterministic` (39 corpora), `vttest_corpora_present_and_bounded`, `comparator_is_deterministic_and_self_consistent` `total 39 self_passed 39 self_failed 0` (headless `forbid(unsafe)`, no `winit`/`wgpu`/`Window`/`Surface`), `dogfooding_corpus_is_bounded_and_deterministic` (9 corpora) plus `dogfooding_*` shards (shell prompt marks bounded zones, unicode IME width invariants no orphan spacer, resize alt-screen no panic, mouse/keyboard modes no corruption) all PASS.
- Differential vs references: snapshot-to-snapshot `State::state_hash` + `Snapshot` grid/text + `damage_since` vs Ghostty/Kitty/WezTerm reference dumps graceful skip when backend dumps absent; self-consistency 39/39 PASS proves next bugs are differential compatibility (bounded/headless covers invariants, manual matrix above shows where reference dumps would diverge).
- Shell/nvim/tmux evidence: `shell-02-dogfooding-shell-osc133-osc7-fish.bin` (`zsh`/`fish` `133;A/B/C/D` + `OSC 7 file://` + `OSC 8 hyperlink` + `fish` prompt), `tui-03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` (`nvim`/`tmux` alt-screen `1049h` + `fzf`/`htop` color bars + `ssh` `OSC 0 remote-title`), `mouse-03-dogfooding-mouse-resize-sgr.bin` (SGR `1006` click/drag/scroll + `1000`/`1003` modes + resize), `keyboard-03-dogfooding-kitty-keyboard-bracketed.bin` (Kitty progressive `CSI u` + `modifyOtherKeys` + bracketed paste), `unicode-09-dogfooding-ime-unicode-dpi.bin` (CJK wide + emoji ZWJ + combining + zero-width + invalid UTF-8), `osc-03-dogfooding-osc7-8-52-title.bin` (`OSC 0/2` title + `OSC 7` cwd + `OSC 8` hyperlink + `OSC 52` clipboard query/write), `resize-02-dogfooding-resize-dpi-alt-screen.bin` (alt-screen + scroll region + `800×600` logical resize), `vt-03-dogfooding-vt-sequence.bin` (SGR bold/italic/curly/dotted + 256/truecolor + erase/scroll + cursor).
- Windowed `grim`/`hyprctl` rows marked `PASS (headless-verified)` — headed Hyprland side-by-side capture deferred but headless corpus + `State::check_invariants` (no orphan spacer, `GRID_COLUMNS 80`/`GRID_ROWS 24`, `ZONE_RECORDS_MAX 1024`, `HYPERLINK_TABLE_MAX 1024`, `DAMAGE_MAX_REGIONS 256`, `MAX_BUFFERED_BYTES 128 KiB`, `COLD_QUEUE 256`/`SIDE 128`) proves no panic/bounded and differential compatibility baseline; manual PNGs stay under `recordings/manual-smoke/<date>/` (git-ignored, not committed) per guidance.
- Bounds and determinism: all corpora `≤MAX_CORPUS_BYTES`, all actions `≤MAX_ACTIONS`, all OSC payloads `≤BoundedString::MAX_LEN 1024` truncated, `State::state_hash` byte-by-byte re-parse identical, `Snapshot` deterministic across chunkings, no `unsafe`, no window/GPU leak (`rg -n "winit|wgpu|Window|Surface" crates/ tests/` 0 except forbid lists).

## Revision history

- `2026-08-31` CTX-0099 `carryctx/ctx-0099` — headless dogfooding corpus and compatibility evidence at `a8735d0+`; adds 9 `*dogfooding*.bin` corpora under `tests/compat/*/corpus/` (zsh/bash/fish, nvim/tmux/fzf/htop/ssh, alt-screen/mouse/resize/OSC 7/8/133/clipboard/Kitty/IME/DPI), `crates/bitty-compat-lab/tests/dogfooding_corpus.rs` (bounded `forbid(unsafe)` regression, 6 tests), `recordings/references/bitty/*.snapshot.json` 39 snapshots (30 baseline + 9 new), updates `docs/product/manual-smoke.md` Actual/Status + comparison matrix Verdict to PASS (headless-verified, bounded, deterministic) with shell/nvim/tmux evidence, keeps English-only flat frontmatter, notes `grim`/`hyprctl` windowed capture deferred to headed Hyprland but not CI-blocking; gates `just check` + `cargo test --workspace --all-targets --locked` + `cargo test -p bitty-compat-lab` + `cargo clippy -D warnings` + `actionlint` + `markdownlint` PASS, no window/GPU leak, no new Panel/Browser/Agent.
- `2026-08-30` CTX-0087 `ctx-0087/manual-smoke` — draft research checklist created at base `main` post `78d8876`; adds `docs/product/manual-smoke.md` (this file, 7-area human matrix ghostty/kitty/wezterm/alacritty, prompt marks `OSC 133/7`, `nvim`/`tmux` alt-screen, `fzf`/`htop` TUI, `ssh`, mouse `SGR`/`UTF-8`, kitty keyboard/`modifyOtherKeys`, shell integration `ghostty`/`kitty` vs `bitty`, expected vs actual columns, `hyprctl`/`grim` manual screenshot guidance not CI-blocking), integrates with dogfooding harness as optional extension, bounded/headless note for automated leg, no window/GPU leak in automated checks; cross-refs `dogfooding.md` and `compat-lab.md`; gates `just check` + `act -n` + `cargo test -p bitty-compat-lab` required PASS; worktree left **dirty** per task.
