---
title: Manual Smoke Checklist vs Mainstream Terminals
description: Human-in-loop manual regression checklist comparing bitty vs ghostty/kitty/wezterm/alacritty for prompt marks, alt-screen TUI, mouse and keyboard protocols — research draft, no deploy, not CI-blocking
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Manual Smoke Checklist vs Mainstream Terminals

## Status and provenance

- Status: **draft**. Research only — no deploy, no CI gate, no `v0.1` acceptance claim until OQ-004 accepts the compatibility contract.
- Ownership: bitty **CTX-0087** — _Manual smoke checklist vs mainstream terminals_.
  - Priority: P1 | Area: vt | Labels: chore,area:vt,P1 | Milestone: v0.1.0 | RFC: OQ-004 | Task: CTX-0087
  - Issue: [#127](https://github.com/bitty-terminal/bitty/issues/127) — `chore,area:vt,P1` — milestone `v0.1.0`
  - Branch: `ctx-0087/manual-smoke` — worktree `.worktrees/ctx-0087-manual-smoke` — agent `opencode-commander`
  - Base: `main` descendant of `78d8876` (CTX-0077 dogfooding harness) and `compat-lab` scaffold (CTX-0074).
- Companion tasks:
  - **CTX-0085** — differential comparator (`tests/compat` grid hash/damage vs reference dumps, headless `forbid(unsafe)`).
  - **CTX-0086** — pinned reference dumps (`ghostty`/`kitty`/`wezterm`/`alacritty`/`xterm` under `tmp/references/<emulator>/`, revision+license).
- Scope: human-in-loop manual regression — the same 7 surfaces as the automated compat lab (`prompt marks`, `nvim`, `tmux`, `fzf/htop`, `ssh`, `mouse`, `kitty keyboard/modifyOtherKeys`) plus shell-integration parity (`ghostty`/`kitty` vs `bitty`). Automated leg stays bounded/headless; this doc is the **manual extension**.
- Authority: OQ-004 remains `Proposed` until `compatibility-milestone-rfc.md` is accepted. This doc does not close OQ-004, does not claim daily-driver completeness, and does not weaken normative security controls in `bitty-docs/docs/security/`.

## Goals

- Give a maintainer a single human-run checklist that can be completed in one Hyprland session (~45 min) to spot regressions against ghostty/kitty/wezterm/alacritty without turning the check into CI.
- Keep the split honest: **automated** = bounded, headless, `forbid(unsafe)`, CI-green (`dogfooding.rs` + `bitty-compat-lab` harness); **manual** = human-in-loop, windowed, screenshot-backed, explicitly **not CI-blocking**.
- Make every row collect `expected vs actual` so a later reader can tell whether bitty diverged from every reference, one reference, or only under a modifier.

## Scope — automated vs manual

| Leg       | Where                                                                                              | What                                                                                                                                                   | Bound                                                                             | CI-blocking                                            | Evidence                                                     |
| --------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------- | ------------------------------------------------------ | ------------------------------------------------------------ |
| Automated | `crates/bitty-runtime/tests/dogfooding.rs` + `tests/compat/harness.rs` + `crates/bitty-compat-lab` | `Parser -> TerminalAction -> State -> Snapshot` on `<=8 KiB` corpora, `<=4096` actions, `ZONE_RECORDS_MAX 1024`, deterministic re-parse + `state_hash` | `READ_CHUNK_SIZE 8 KiB`, `MAX_CORPUS_BYTES 8192`, `MAX_ACTIONS 4096`, wall `90 s` | yes — `just check` + `cargo test --workspace --locked` | `eprintln!` findings table, `cargo test -p bitty-compat-lab` |
| Manual    | this doc                                                                                           | Windowed bitty vs ghostty/kitty/wezterm/alacritty on the same bytes/key/mouse sequence                                                                 | human bounded (~15 s per row, `grim` file `<2 MiB`)                               | **no** — human-run, not wired to `just check`          | `tmp/manual-smoke/<date>/` + table below                     |

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
5. Store artefacts under `tmp/manual-smoke/<YYYY-MM-DD>/` (ignored, not committed) — commit only the **filled table** to the checkpoint note, not the PNGs.

## 1 — Prompt marks — `OSC 133` zones + `OSC 7` cwd

`bitty-term-state` zones (`ZONE_RECORDS_MAX 1024`, `BoundedString` truncates `OSC 133;...`) are bounded and deterministic; see `crates/bitty-vt/tests/replay.rs::fixture_shell_session_replay` and `tests/compat/shell/corpus/01-prompt-marks.bin`.

| #   | Scenario                             | Steps (both terminals)                                                                                | Expected (ghostty/kitty/wezterm/alacritty)                                                                                        | Actual (bitty) | Evidence                                                                      | Status |
| --- | ------------------------------------ | ----------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | -------------- | ----------------------------------------------------------------------------- | ------ |
| 1.1 | `133;A` prompt start + `133;B` input | `zsh` with `OSC 133` integration (`ghostty`/`kitty` shell-integration plugin enabled), type `echo hi` | kitty/ghostty mark `A` at prompt, `B` at cursor; wezterm zone `A`/`B` in `wezterm record` JSON; alacritty ignores `133` (no zone) |                | `grim` before/after `B`                                                       | draft  |
| 1.2 | `133;C` output start + `133;D` end   | Run `ls`, `cargo check` colored; observe zone `C` → `D;0` (success) vs `D;1` (failure)                | ghostty `shell-integration` zone JSON shows `C`/`D` with exit code; kitty `marks` reflect `D`; alacritty no zone                  |                | `State::zones()` log vs ghostty JSON                                          | draft  |
| 1.3 | `OSC 7` cwd + `OSC 8` hyperlink      | `cd /tmp && pwd`, then `echo -e '\e]8;;https://example.com\a link \e]8;;\a'`                          | cwd title/zone `file://<host>/tmp` in ghostty/kitty/wezterm `OSC 7` dump; hyperlink underline in kitty/ghostty                    |                | `OSC 7/8` dump vs `Snapshot` hyperlink                                        | draft  |
| 1.4 | Zoned scroll/copy                    | Select across prompt boundary after 1.1–1.3, copy, paste in `nvim` scratch                            | ghostty/kitty preserve prompt/semantic boundaries; alacritty plain copy                                                           |                | clipboard paste diff                                                          | draft  |
| 1.5 | Headless corpus replay               | `cat tests/compat/shell/corpus/01-prompt-marks.bin` into each emulator's PTY (`script` replay)        | `harness::actions_to_snapshot` zones == reference `wezterm record` zones (snapshot-to-snapshot)                                   |                | `cargo test -p bitty-compat-lab --test harness compat_corpus_is_bounded` PASS | draft  |

## 2 — `nvim` / `tmux` / alt-screen

Alt-screen `CSI ? 1049h`/`47h`, scroll region `CSI r`, `State::resize` reflow. Baselines: `fixture_fullscreen_app_replay`, `crates/bitty-runtime/tests/v01_minimal_terminal.rs::v01_resize_*`.

| #   | Scenario                     | Steps                                                                                                   | Expected                                                                                              | Actual | Evidence                                             | Status |
| --- | ---------------------------- | ------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | ------ | ---------------------------------------------------- | ------ |
| 2.1 | `nvim` fullscreen entry/exit | `nvim -u NONE README.md`, `:help`, `<C-w>s` split, `:qa` — observe alt-screen switch                    | ghostty/kitty/wezterm/alacritty enter `1049h`, statusline rendered, exit restores prior scrollback    |        | `grim` nvim before/after + `State::alt_screen`       | draft  |
| 2.2 | `tmux` pane + status bar     | `tmux -L bitty-smoke new`, `split-window -h`, `ls --color`, detach/reattach                             | pane border `│` rendered, status bar `42m` green, reattach restores grid; all four references match   |        | `grim` tmux with `│` crop                            | draft  |
| 2.3 | Resize reflow 800×600        | Drag Hyprland dwindle split to `800x600` logical → `100x37` @8×16, then `hyprctl dispatch resizeactive` | 100→50+50 on split, no orphan `spacer` (`State::check_invariants` PASS), same as kitty/wezterm reflow |        | `PhysicalSize` vs `Snapshot width 80`                | draft  |
| 2.4 | Headless alt-screen corpus   | `cat tests/compat/tui/corpus/01-nvim-tmux.bin` replay                                                   | snapshot deterministic, `diff_snapshots` vs Ghostty dump `None`                                       |        | `cargo test -p bitty-compat-lab` virtual-screen text | draft  |

## 3 — `fzf` / `htop` TUI (alt-screen + mouse + resize)

| #   | Scenario             | Steps                                                                           | Expected                                                                             | Actual | Evidence                                       | Status |
| --- | -------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | ------ | ---------------------------------------------- | ------ |
| 3.1 | `fzf` fuzzy finder   | `ls \| fzf --height 40%`, type `rs`, `<C-j>`/`<C-k>` navigate, `<Enter>` select | ghostty/kitty/wezterm/alacritty: fullscreen list, preview on, selection returns line |        | `grim` fzf list vs bitty `Snapshot`            | draft  |
| 3.2 | `htop` process table | `htop`, arrow navigate, `F2` setup, `q` quit                                    | alt-screen `1049h`, color bars, no leftover artefacts after quit                     |        | `grim` before/after quit                       | draft  |
| 3.3 | Resize during TUI    | Open `fzf`, resize window while list visible                                    | list reflows without ghost rows; wezterm/kitty reflow identical                      |        | resize `grim` sequence                         | draft  |
| 3.4 | Headless TUI corpus  | `cat tests/compat/tui/corpus/02-htop-fzf.bin` replay                            | `parse_bounded` ≤8 KiB / ≤4096 actions, `state_hash` deterministic                   |        | `cargo test -p bitty-compat-lab` corpus ledger | draft  |

## 4 — `ssh` remote

Synthetic + real PTY pattern mirrors `dogfooding.rs::dogfood_real_pty_graceful_smoke` (graceful skip when host unreachable).

| #   | Scenario            | Steps                                                                            | Expected                                                                                                 | Actual | Evidence                               | Status |
| --- | ------------------- | -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- | ------ | -------------------------------------- | ------ |
| 4.1 | Remote echo + title | `ssh -o ConnectTimeout=3 <lab-host> 'echo ssh-ok; printf "\e]0;remote-title\a"'` | ghostty/kitty/wezterm show remote `ssh-ok` + `TitleChanged("remote-title")`; alacritty title via `OSC 0` |        | remote PTY log + `Snapshot` title      | draft  |
| 4.2 | Remote `nvim`       | `ssh <host> nvim -u NONE README.md` then `:qa`                                   | alt-screen over ssh, no CSI corruption                                                                   |        | `grim` remote nvim                     | draft  |
| 4.3 | Keepalive / idle    | Idle ssh 30 s, then `echo again`                                                 | no spurious `tick` (frame-on-demand)                                                                     |        | idle `tick() is None` log              | draft  |
| 4.4 | Headless ssh corpus | `cat tests/compat/shell/corpus/ssh-*.bin` equivalent `corpus_ssh()` replay       | bounded ≤8 KiB, `TitleChanged` observed in `cold_queue_len ≤256`                                         |        | `cargo test --test dogfooding` ssh row | draft  |

## 5 — Mouse — SGR / UTF-8 / urxvt

Mouse tracking modes `1000/1002/1003/1006/1015/1005`; corpus `tests/compat/mouse/corpus/01-sgr-mouse.bin`, `02-utf8-mouse.bin`; baseline `fixture_escape_storm_replay` (`1002;1006h`).

| #   | Scenario              | Steps                                                                             | Expected                                                                                                                | Actual | Evidence                                                      | Status |
| --- | --------------------- | --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------------------- | ------ |
| 5.1 | SGR `1006` click/drag | Enable `CSI ? 1002;1006h`, click pane, drag select in `nvim`/`tmux`, scroll wheel | kitty/wezterm/ghostty emit `CSI < 0;col;row M/m`; alacritty SGR identical; bitty parses `CSI <` without orphan `spacer` |        | mouse byte log vs `TerminalAction::SetMode { MouseTracking }` | draft  |
| 5.2 | UTF-8 `1005` fallback | Switch to `CSI ? 1005h`, same click/drag (if terminal supports)                   | ghostty/kitty/wezterm fall back to SGR when `1005` deprecated; alacritty `1005` still emits `CSI M` UTF-8 bytes         |        | `parse_bounded` UTF-8 branch                                  | draft  |
| 5.3 | Normal `1000` vs 1003 | `1000h` (button only) vs `1003h` (all motion) while hovering `htop`               | `1000` only on press, `1003` streams motion; bitty modes inert to grid until mouse RFC lands, no corruption             |        | mode flag dump                                                | draft  |
| 5.4 | Headless mouse corpus | `cat tests/compat/mouse/corpus/01-sgr-mouse.bin` replay                           | `State` modes tracked, `check_invariants` PASS                                                                          |        | `cargo test -p bitty-compat-lab` mouse shard                  | draft  |

## 6 — Keyboard — kitty keyboard (`CSI u`) / `modifyOtherKeys`

Corpus `tests/compat/keyboard/corpus/01-kitty-keyboard.bin`, `02-modifyOtherKeys.bin`; future `bitty-platform::keyboard` mapping (this checklist is bytes-side only until keyboard RFC).

| #   | Scenario                    | Steps                                                                                                         | Expected                                                                                                                         | Actual | Evidence                                         | Status |
| --- | --------------------------- | ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------ | ------ |
| 6.1 | `CSI u` single + chord      | Progressive `CSI ? 2017h` then press `a`, `C-a`, `C-S-a`, `F1`, `C-F1` with `kitty +kitten show_key -f kitty` | kitty encodes `CSI 97 u` / `CSI 97:5 u` / `CSI 1:5 P` etc.; wezterm/ghostty progressive matches; alacritty legacy `^A` for `C-a` |        | `show_key` log vs `bitty-vt` `Key` bytes         | draft  |
| 6.2 | `modifyOtherKeys` level 1/2 | `CSI ? 4;1h` then `C-[`, `CSI ? 4;2h` then same                                                               | level 1 `CSI 27;5;27~` style, level 2 `CSI 27;...` extended; xterm compat in all four refs                                       |        | corpus `02-modifyOtherKeys.bin` vs reference log | draft  |
| 6.3 | Bracketed paste             | `CSI ? 2004h`, paste multiline `echo "hi\nbye"`                                                               | all refs wrap `200~...201~`; bitty `BracketedPaste` delimiters preserved, no truncation (`BoundedString` 1024)                   |        | paste delimiters byte dump                       | draft  |
| 6.4 | Headless keyboard corpus    | `cat tests/compat/keyboard/corpus/*.bin` replay                                                               | `parse_bounded` deterministic, `MAX_ACTIONS 4096` respected                                                                      |        | `cargo test -p bitty-compat-lab` keyboard shard  | draft  |

## 7 — Shell integration parity — ghostty / kitty vs bitty

Shell integration ownership stays host-shell side (`zsh`/`fish` `OSC 133/7` plugins); bitty reads `133`/`7`/`8` bytes into `State::zones` + cold/side queues bounded `256/128`, mirroring ghostty/kitty zone models per `docs/product/compat-lab.md` § `shell/`.

| #   | Scenario                           | Steps                                                                                                                                                   | Expected (ghostty/kitty)                                                              | Actual (bitty)                                                                          | Evidence                                                        | Status |
| --- | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | --------------------------------------------------------------- | ------ |
| 7.1 | Prompt-line jump                   | In `zsh` with ghostty `shell-integration` / kitty `shell Integration`, run 3 commands then `ghostty: jump to previous prompt` / `kitty: scroll to mark` | ghostty `prompt-marks` zone `A..D` navigable; kitty marks list shows 3 prompts        | zones `A/B/C/D` navigable via bitty marks API (future, today `State::zones()` length 3) | ghostty `dump` zones JSON vs `State::zones()`                   | draft  |
| 7.2 | Cwd-aware new tab                  | `cd /tmp/foo && ghostty new-tab` / `kitty new window with cwd`                                                                                          | new pane inherits `OSC 7 file://.../foo`; wezterm `SpawnCommand` cwd                  | `OSC 7` stored truncated ≤1024, `cwd` side-channel bounded 128                          | cwd payload byte log                                            | draft  |
| 7.3 | Hyperlink + clipboard (`OSC 8/52`) | `echo -e '\e]8;;https://example.com\a click \e]8;;\a'` then `OSC 52` copy (`printf '\e]52;c;...`)                                                       | ghostty/kitty underline hyperlink + clipboard policy prompt; alacritty hyperlink only | hyperlink parsed `OSC 8`, clipboard inert (policy gate, no `allow-all`)                 | `TerminalAction::Osc8` vs reference dump                        | draft  |
| 7.4 | Headless shell-integration corpus  | `cat tests/compat/shell/corpus/01-prompt-marks.bin` + `tests/compat/osc/corpus/*` replay                                                                | `ZONE_RECORDS_MAX 1024` oldest dropped, snapshot identical across chunkings           |                                                                                         | `cargo test -p bitty-compat-lab` shell/osc shards deterministic | draft  |

## Comparison matrix — expected vs actual (fill per run)

One matrix row per manual scenario; **Expected** is what the reference panel showed, **Actual** is bitty on the same bytes/keys/mouse. Fill `Verdict` as `PASS` / `DIFF:<reason>` / `SKIP:<tool missing>` and file a follow-up when `DIFF`.

| Area       | #   | Scenario                | References exercised | Expected (panel consensus = ghostty/kitty/wezterm/alacritty) | Actual (bitty) | Verdict | Artefact (`tmp/manual-smoke/<date>/…`) |
| ---------- | --- | ----------------------- | -------------------- | ------------------------------------------------------------ | -------------- | ------- | -------------------------------------- |
| prompt     | 1.1 | `133;A/B`               | g/k/w/a              | zones `A`+`B` visible in reference dumps                     |                |         | `01-prompt-AB.png`                     |
| prompt     | 1.2 | `133;C/D`               | g/k/w                | `C`/`D` with exit code in ghostty JSON, marks in kitty       |                |         | `02-prompt-CD.png`                     |
| prompt     | 1.3 | `OSC 7/8` cwd+hyperlink | g/k/w/a              | cwd `file://...`, hyperlink underline                        |                |         | `03-cwd-hyperlink.png`                 |
| prompt     | 1.4 | zoned copy              | g/k                  | prompt-aware select                                          |                |         | `04-zoned-copy.png`                    |
| alt-screen | 2.1 | `nvim`                  | g/k/w/a              | `1049h` entry/exit restores                                  |                |         | `05-nvim.png`                          |
| alt-screen | 2.2 | `tmux`                  | g/k/w/a              | pane `│` + `42m` bar                                         |                |         | `06-tmux.png`                          |
| alt-screen | 2.3 | resize 800×600          | g/k/w/a              | 100→50+50, no orphan `spacer`                                |                |         | `07-resize.png`                        |
| TUI        | 3.1 | `fzf`                   | g/k/w/a              | fullscreen filter list                                       |                |         | `08-fzf.png`                           |
| TUI        | 3.2 | `htop`                  | g/k/w/a              | color bars, clean quit                                       |                |         | `09-htop.png`                          |
| TUI        | 3.3 | resize-in-tui           | g/k/w                | reflow no ghosts                                             |                |         | `10-fzf-resize.png`                    |
| ssh        | 4.1 | remote echo+title       | g/k/w/a              | `ssh-ok` + `TitleChanged`                                    |                |         | `11-ssh.png`                           |
| ssh        | 4.2 | remote `nvim`           | g/k/w/a              | alt-screen over ssh                                          |                |         | `12-ssh-nvim.png`                      |
| mouse      | 5.1 | SGR `1006`              | g/k/w/a              | `CSI <` streams                                              |                |         | `13-mouse-sgr.png`                     |
| mouse      | 5.2 | UTF-8 `1005`            | g/k/w/a              | `1005` or fallback SGR                                       |                |         | `14-mouse-utf8.png`                    |
| mouse      | 5.3 | `1000` vs `1003`        | g/k/w/a              | press-only vs motion                                         |                |         | `15-mouse-motion.png`                  |
| keyboard   | 6.1 | `CSI u`                 | k/w/g                | `CSI u` chords                                               |                |         | `16-kitty-kbd.png`                     |
| keyboard   | 6.2 | `modifyOtherKeys`       | g/k/w/a              | `CSI 27;...~`                                                |                |         | `17-modifyOtherKeys.png`               |
| keyboard   | 6.3 | bracketed paste         | g/k/w/a              | `200~...201~`                                                |                |         | `18-bracketed-paste.png`               |
| shell      | 7.1 | prompt jump             | g/k                  | zone-navigable marks                                         |                |         | `19-prompt-jump.png`                   |
| shell      | 7.2 | cwd-aware new tab       | g/k/w                | new pane `file://` cwd                                       |                |         | `20-cwd-newtab.png`                    |
| shell      | 7.3 | `OSC 8/52`              | g/k/a                | hyperlink + clipboard gate                                   |                |         | `21-osc8-52.png`                       |

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
  mkdir -p tmp/manual-smoke/$(date +%F)
  # list windows on workspace 9, then capture one by address
  hyprctl clients -j | jq -r '.[] | select(.workspace.id==9) | "\(.address) \(.class) \(.title)"'
  grim -g "$(hyprctl clients -j | jq -r '.[] | select(.class=="bitty") | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" tmp/manual-smoke/$(date +%F)/01-bitty.png
  grim -g "$(hyprctl clients -j | jq -r '.[] | select(.class=="kitty")  | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" tmp/manual-smoke/$(date +%F)/01-kitty.png
  # or interactive (human only)
  grim -g "$(slurp)" tmp/manual-smoke/$(date +%F)/manual-$(date +%H%M%S).png
  ```

- **Full-workspace fallback (when addresses drift):** `grim tmp/manual-smoke/$(date +%F)/workspace-9-$(date +%H%M%S).png`.
- **Grid dump (text) alongside PNGs** — prefer text diffs for the comparator (CTX-0085) and keep PNGs as visual sanity:

  ```bash
  # kitty / ghostty / wezterm dumps of the same PTY bytes
  kitty --dump-commands > tmp/manual-smoke/$(date +%F)/kitty-dump.json
  wezterm record --cwd . > tmp/manual-smoke/$(date +%F)/wezterm-record.json
  # bitty headless snapshot for the same bytes
  cargo test -p bitty-compat-lab -- --nocapture > tmp/manual-smoke/$(date +%F)/bitty-snapshot.txt
  ```

- **Storage:** `tmp/manual-smoke/<YYYY-MM-DD>/` is git-ignored and stays out of the PR — it is not `tmp/references/` (which is revision-pinned). Commit only the filled comparison matrix, not the PNGs.
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
- Reference dumps (CTX-0086, read-only): `tmp/references/<emulator>/` per `tmp/references/README.md` (ghostty `8867c37` MIT, kitty `087b8c3` GPL-3.0, wezterm `f93d903` MIT, alacritty/xterm pinned separately).
- Comparator harness (CTX-0085, grid hash/damage vs dumps): `crates/bitty-compat-lab/tests/harness.rs` (`compat_corpus_is_bounded_and_deterministic`, bounded `forbid(unsafe)`).
- Release ladder + `v0.1`–`v1.0` crates: [`release-ladder.md`](./release-ladder.md).
- Security gates for `v1.0` remain normative in [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md); this manual checklist does not weaken them.

## Revision history

- `2026-08-30` CTX-0087 `ctx-0087/manual-smoke` — draft research checklist created at base `main` post `78d8876`; adds `docs/product/manual-smoke.md` (this file, 7-area human matrix ghostty/kitty/wezterm/alacritty, prompt marks `OSC 133/7`, `nvim`/`tmux` alt-screen, `fzf`/`htop` TUI, `ssh`, mouse `SGR`/`UTF-8`, kitty keyboard/`modifyOtherKeys`, shell integration `ghostty`/`kitty` vs `bitty`, expected vs actual columns, `hyprctl`/`grim` manual screenshot guidance not CI-blocking), integrates with dogfooding harness as optional extension, bounded/headless note for automated leg, no window/GPU leak in automated checks; cross-refs `dogfooding.md` and `compat-lab.md`; gates `just check` + `act -n` + `cargo test -p bitty-compat-lab` required PASS; worktree left **dirty** per task.
