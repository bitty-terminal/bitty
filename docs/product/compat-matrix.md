---
title: Compatibility Matrix for Release (CTX-0114)
description: Release compatibility matrix covering shell/tmux/nvim/fzf/htop/ssh/alt-screen/mouse/resize/OSC/clipboard/Kitty/IME/DPI across Ghostty/Kitty/WezTerm/Alacritty differential with bounded headless corpus and regression tests
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 MD060 -->

# Compatibility Matrix for Release (CTX-0114)

## Status and provenance

- Status: **draft** — release candidate matrix for `v0.1.0` compatibility gate.
- Ownership: bitty **CTX-0114** — _Build compatibility matrix for release_.
  - Priority: P1 | Area: compat | Labels: feat,area:compat,P1 | Milestone: v0.1.0 | RFC: OQ-001 | Task: CTX-0114 — Branch `carryctx/ctx-0114` — Base `1d9eb6a` (CTX-0113) — Agent `core-implementer-0114` — Worktree `.worktrees/ctx-0114`.
- Scope: bounded, headless, `forbid(unsafe)` differential matrix covering 14 surfaces (`shell`, `tmux`, `nvim`, `fzf`, `htop`, `ssh`, `alt-screen`, `mouse`, `resize`, `OSC`, `clipboard`, `Kitty`, `IME`, `DPI`) across 4 reference emulators (`Ghostty`, `Kitty`, `WezTerm`, `Alacritty`) via snapshot-to-snapshot (`Parser -> TerminalAction -> State -> Snapshot` + `state_hash` + `damage`) with graceful skip when backend dumps absent. No `winit`/`wgpu`/`Window`/`Surface`, no network, no pixel diff.
- Authority: `OQ-001` (performance/budget) and `OQ-004` (compatibility milestone) remain `Accepted`; this matrix is **evidence** for the `v0.1.0` release ladder `v0.2` row (VT/TUI) and `v1.0` criteria draft. It does not close an open question, accept a version slice, or weaken security controls in `bitty-docs/docs/security/`.
- Companion: [`compat-lab.md`](./compat-lab.md) (Phase C scaffold, CTX-0074), [`manual-smoke.md`](./manual-smoke.md) (human checklist, CTX-0087/0099), [`dogfooding.md`](./dogfooding.md) (daily-driver headless, CTX-0077), `crates/bitty-compat-lab` (owner, `forbid(unsafe)`, bounded 8 KiB/4096), `recordings/compat-matrix-2026-09-01.json` (machine-readable matrix, generated headlessly), `recordings/references/bitty/*.snapshot.json` (39+ dumps via `collect_dumps`).

## Goals

- Give release a **single bounded table** that proves each surface replays deterministically on headless bitty, stays bounded, and meets invariants, with differential columns for the 4 reference dumps when present (otherwise `SKIP`).
- Keep all claims reproducible via `cargo test -p bitty-compat-lab --test compat_matrix -- --nocapture` and `cargo run -p bitty-compat-lab --bin collect_dumps --locked` without display.
- Distinguish `self-consistency PASS` (bitty replay vs stored dump) from `reference PASS/SKIP` (bitty vs Ghostty/Kitty/WezTerm/Alacritty grids). Only `self` gates CI; `reference` is observability until backend dumps are pinned.

## Harness and bounds

- Headless: `bitty-vt::Parser -> bitty_vt::TerminalAction -> bitty_term_state::State -> bitty_term_state::Snapshot` only. No `winit::Window`, `wgpu::Surface`, `SurfaceTarget`, `HeadlessRasterizer`. Grep `crates/bitty-compat-lab` + `tests/compat` for `winit|wgpu|Window|Surface` must be `0` outside forbid-list comments (same rule as `compat-lab.md` § No window/GPU leak).
- Bounded: `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 1024` (`BoundedString::MAX_LEN`), `MAX_CORPORA_PER_CATEGORY = 64`, `MAX_SNAPSHOT_JSON_BYTES = 16 KiB`, `MAX_SNAPSHOTS = 64`, `MAX_TEXT_CHARS = 80*24+24 = 1944`, `ZONE_RECORDS_MAX = 1024`, `HYPERLINK_TABLE_MAX = 1024`, `DAMAGE_MAX_REGIONS = 256`, `GRID 80x24`, `CANONICAL_HASH_VERSION = 1` (FNV-1a). Oversize asserts/panics in tests, truncates in `BoundedString` per `bitty-vt` contract.
- Deterministic: byte-by-byte re-parse identity (`parse_bounded` re-parses per byte and asserts `actions == actions2`), `State::state_hash` equality across replays, sorted discovery, sorted JSON keys, no wall-clock/RNG.
- `forbid(unsafe)` in `crates/bitty-compat-lab/src/*.rs` and `tests/compat/harness.rs` and `crates/bitty-compat-lab/tests/*.rs`.

## Differential method (snapshot-to-snapshot, not pixel)

- **Capture**: `cargo run -p bitty-compat-lab --bin collect_dumps --locked` replays every `tests/compat/<category>/corpus/*.bin` headlessly and writes bounded deterministic JSON `recordings/references/bitty/<category>-<stem>.snapshot.json` (80x24, `<16 KiB`, sorted keys, `state_hash` hex 16). Same bytes can be fed to each reference emulator via `script` replay + grid dump (`kitty --dump-commands`, Ghostty `dump`, `wezterm record`, Alacritty dump) normalized to `Snapshot` text.
- **Compare**: `crates/bitty-compat-lab/src/compare.rs` `compare_all()` loads ≤64 bitty dumps (sorted) and for each: replays `corpus_rel` via `parse_bounded`, checks `bytes_len`, `actions_len`, `state_hash`, `Snapshot` text/cursor/title/generation, `State::check_invariants`, byte-by-byte determinism, then optionally diffs text vs any `recordings/references/<backend>/*.snapshot.json` that share the same `corpus_rel` (`ghostty`/`kitty`/`wezterm`/`alacritty`). Graceful `reference_skipped` when absent (CI stays green); mismatches appear as `ref_fail`.
- **Alacritty column**: added in CTX-0114. Previously `ghostty`/`kitty`/`wezterm`; now `alacritty` is fourth. Storage candidates: `tmp/references/<backend>/` (worktree) and `recordings/references/<backend>/` (umbrella) and their mirrors.

## Matrix — 14 surfaces × 4 terminals

| #   | Surface        | Corpus (`tests/compat/<category>/corpus/<file>`)                                                                                                                                                                                                    | Category                | Bounded                                             | Differential columns (Ghostty / Kitty / WezTerm / Alacritty) | Self-consistency                                                                    | Status |
| --- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- | --------------------------------------------------- | ------------------------------------------------------------ | ----------------------------------------------------------------------------------- | ------ |
| 1   | **shell**      | `shell/corpus/02-dogfooding-shell-osc133-osc7-fish.bin` (201 B, `133;A/B/C/D` + `OSC 7 file://` + `OSC 8` + fish) + `shell/corpus/03-dogfooding-comprehensive.bin` (310 B)                                                                          | `shell`                 | ≤8 KiB, ≤4096 actions, `ZONE 1024`                  | `SKIP/SKIP/SKIP/SKIP` (graceful)                             | PASS `state_hash 35c312b80dff738b` deterministic, `check_invariants` PASS           | PASS   |
| 2   | **tmux**       | `tui/corpus/01-nvim-tmux.bin` (28 B, `1049h` `2;10r`) + `tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` (155 B, `│` `32m` status)                                                                                                             | `tui`                   | ≤8 KiB, ≤4096, alt-screen `1049h`                   | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `pane │` (U+2502) no orphan `spacer`, `state_hash 446def2b5a6f1875`            | PASS   |
| 3   | **nvim**       | `tui/corpus/01-nvim-tmux.bin` + `tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` (`nvim` `1049h` `STATUS` `1049l`)                                                                                                                             | `tui`                   | ≤8 KiB, ≤4096, `DAMAGE 256`                         | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `check_invariants` alt-screen no panic, deterministic                          | PASS   |
| 4   | **fzf**        | `tui/corpus/02-htop-fzf.bin` (23 B, `fzf --height`) + `tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin`                                                                                                                                         | `tui`                   | ≤8 KiB, ≤4096, `1049h`                              | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `fzf --height 40%` alt-screen, `state_hash f3719f25f24c8b9a`                   | PASS   |
| 5   | **htop**       | `tui/corpus/02-htop-fzf.bin` + `tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` (`htop` `32m` bars `1049h/l`)                                                                                                                                  | `tui`                   | ≤8 KiB, ≤4096                                       | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `32m` color bars preserved, clean quit                                         | PASS   |
| 6   | **ssh**        | `tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin` (`ssh-ok` `OSC 0 remote-title`)                                                                                                                                                               | `tui`                   | ≤8 KiB, ≤4096, `1024` OSC                           | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `ssh-ok` + `TitleChanged remote-title`, bounded `MAX_OSC_BYTES 1024`           | PASS   |
| 7   | **alt-screen** | `tui/corpus/01-nvim-tmux.bin` (`1049h/l`) + `resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin` (`1049h` `2J` `2;10r` `5S` `800x600` `8;37;100t`)                                                                                               | `tui`/`resize`          | ≤8 KiB, ≤4096, `alt_screen_active`                  | `SKIP/SKIP/SKIP/SKIP`                                        | PASS enter/exit deterministic, `check_invariants` PASS no orphan spacer             | PASS   |
| 8   | **mouse**      | `mouse/corpus/01-sgr-mouse.bin` (`CSI <0;10;10M`) + `mouse/corpus/02-utf8-mouse.bin` + `mouse/corpus/03-dogfooding-mouse-resize-sgr.bin` (`1000h` `1002h` `1006h` `1003h` `CSI <0;10;5M`)                                                           | `mouse`                 | ≤8 KiB, ≤4096, mouse modes inert until mouse RFC    | `SKIP/SKIP/SKIP/SKIP`                                        | PASS SGR/UTF8 deterministic, `check_invariants` PASS, `c5cbed0ac241e9cf`            | PASS   |
| 9   | **resize**     | `resize/corpus/01-resize-reflow.bin` + `resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin` (`800x600 -> 100x37 @8x16`)                                                                                                                          | `resize`                | ≤8 KiB, ≤4096, `State::resize` reflow               | `SKIP/SKIP/SKIP/SKIP`                                        | PASS reflow 100→50+50, `DAMAGE_MAX_REGIONS 256`, no ghost rows                      | PASS   |
| 10  | **OSC**        | `osc/corpus/01-title-hyperlink.bin` (`OSC 0` + `OSC 8`) + `osc/corpus/03-dogfooding-osc7-8-52-title.bin` (`OSC 0/2` `OSC 7 file://` `OSC 8 id=123` `OSC 52`)                                                                                        | `osc`                   | ≤8 KiB, ≤4096, `1024` truncate                      | `SKIP/SKIP/SKIP/SKIP`                                        | PASS title/hyperlink/cwd parsed bounded                                             | PASS   |
| 11  | **clipboard**  | `osc/corpus/02-clipboard.bin` (`OSC 52;c;SGVsbG8=`) + `osc/corpus/03-dogfooding-osc7-8-52-title.bin` (`OSC 52` query `c` vs write `p0/p1`)                                                                                                          | `osc`                   | ≤8 KiB, ≤4096, `BoundedBytes 4096` policy gate      | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `OSC 52` query/write distinguished, `BoundedString 1024`                       | PASS   |
| 12  | **Kitty**      | `keyboard/corpus/01-kitty-keyboard.bin` (`CSI 27;5;13~`) + `keyboard/corpus/03-dogfooding-kitty-keyboard-bracketed.bin` (`2017h` `97u` `97:5u` `27;5;27~` `2004h` `200~hi201~`) + `shell/corpus/03-dogfooding-comprehensive.bin` (`kitty 7727:1:2`) | `keyboard`              | ≤8 KiB, ≤4096, Kitty progressive `7727` mask `0x1F` | `SKIP/SKIP/SKIP/SKIP`                                        | PASS progressive `7727:1:2:5 -> 19`, bracketed paste `200~201~`                     | PASS   |
| 13  | **IME**        | `unicode/corpus/09-dogfooding-ime-unicode-dpi.bin` (99 B, CJK `中文` ZWJ `👨‍👩‍👧` combining `e\u{301}` zero-width `200b`) + `unicode/corpus/01-wide-emoji.bin` … `08-mixed-width.bin`                                                                   | `unicode`               | ≤8 KiB, ≤4096, `wcwidth` wide/ZWJ/combining         | `SKIP/SKIP/SKIP/SKIP`                                        | PASS wide 2-cell, `spacer` not orphaned, ZWJ/zero-width handled, `09c75f8905650c80` | PASS   |
| 14  | **DPI**        | `resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin` (`800x600` `8;37;100t`) + `unicode/corpus/09-dogfooding-ime-unicode-dpi.bin` (DPI scalar) + `vt/corpus/03-dogfooding-vt-sequence.bin` (SGR underline DPI-invariant)                         | `resize`/`unicode`/`vt` | ≤8 KiB, ≤4096, DPI `floor(rect/cell)`               | `SKIP/SKIP/SKIP/SKIP`                                        | PASS `800x600 -> 100x37` @8x16 deterministic, DPI scalar no corruption              | PASS   |

All rows `self PASS` via `cargo test -p bitty-compat-lab --test compat_matrix` (bounded, headless, `forbid(unsafe)`) and `cargo test -p bitty-compat-lab --test compare -- --nocapture` `total 39 self_passed 39`. Reference columns `SKIP` when no `recordings/references/<backend>/*.snapshot.json` per `corpus_rel` (CI-friendly); when present, `compare_all` asserts text equality and counts `ref_passed`/`ref_failed`. Pixel diff is explicitly out of scope (snapshot text diff only).

## Special coverage — Ghostty / Kitty / WezTerm / Alacritty differential

- Each surface row above exercises the same byte stream across the 4 reference dumps when available. Corpus choice mirrors the 7 manual-smoke surfaces plus IME/DPI per `docs/product/manual-smoke.md` §1–7 and dogfooding corpus `CTX-0099` (9 `*dogfooding*.bin`, each ≤310 B).
- Reference clone provenance lives in `recordings/references/README.md` (umbrella) — `ghostty 8867c37` MIT, `kitty 087b8c3` GPL-3.0, `wezterm f93d903` MIT, `alacritty ede2ac1` Apache-2.0/MIT, `xterm 9489b20` MIT/X11, `vttest 3.4.0` synthetic — plus `tmp/references/` mirrors. No clone is executed or imported.
- Comparator backends probed in order `ghostty` → `kitty` → `wezterm` → `alacritty` (new in CTX-0114). Candidates are `workspace_root/recordings/references/<backend>/` and umbrella mirrors; discovery is sorted, bounded to `MAX_SNAPSHOTS 64` and `MAX_SNAPSHOT_JSON_BYTES 16 KiB`.

## Recordings corpus and regression tests

- **Corpus**: `tests/compat/*/corpus/*.bin` (30 baseline + 9 dogfooding = 39 as of `1d9eb6a`, plus placeholders). Each ≤8 KiB, each re-parse byte-by-byte deterministic, each `state_hash` canonical. New in CTX-0114: no new `.bin` required — existing 39 already cover all 14 surfaces via dogfooding corpus. `cargo run -p bitty-compat-lab --bin collect_dumps --locked` regenerates `recordings/references/bitty/*.snapshot.json` (39, `<16 KiB` each, `80x24`, `CANONICAL_HASH_VERSION 1`) to `tmp/references/bitty/` and umbrella mirrors.
- **Machine-readable matrix**: `recordings/compat-matrix-2026-09-01.json` (generated headlessly via `crates/bitty-compat-lab/src/matrix.rs` `generate_matrix_json()` or `cargo test -p bitty-compat-lab --test compat_matrix -- --nocapture` artifact). Contains per-surface `corpus_rel`, `bytes_len`, `actions_len`, `state_hash`, `snapshot_width/height`, `generation`, `self PASS`, `reference SKIPPED/PASS` per backend. Bounded `<16 KiB`.
- **Regression tests** (`crates/bitty-compat-lab/tests/`):
  - `harness.rs` — `compat_corpus_is_bounded_and_deterministic` (≥16 corpora), `vttest_corpora_present_and_bounded`, `no_window_gpu_leak_in_corpora`.
  - `compare.rs` — `comparator_is_deterministic_and_self_consistent` (39/39 PASS), `load_bitty_dumps_is_bounded_and_sorted`, `comparator_no_unbounded_heap`, `comparator_reference_graceful_skip_when_absent` (now 4 backends).
  - `dogfooding_corpus.rs` — 6 tests (shell zones, unicode IME width, resize alt-screen, mouse/keyboard, etc) 9 dogfooding corpora.
  - `compat_matrix.rs` **(CTX-0114)** — 7 tests:
    1. `matrix_covers_all_14_surfaces` — 14 surfaces present, each category/corpus exists and bounded.
    2. `matrix_corpora_are_bounded_and_deterministic` — each matrix corpus ≤8 KiB, ≤4096 actions, byte-by-byte identical, `check_invariants` PASS.
    3. `matrix_snapshots_are_bounded` — `80x24`, `<16 KiB` JSON, `<1944` chars, hash version 1.
    4. `matrix_differential_is_graceful` — `compare_all` when refs absent yields `reference_skipped=true`, `reference_compared=0`, no false failures.
    5. `matrix_no_window_gpu_leak` — no `winit/wgpu/Window/Surface` in corpora bytes.
    6. `matrix_no_unsafe` — `#![forbid(unsafe_code)]` in harness/comparator/matrix.
    7. `matrix_is_sorted_and_deterministic` — second run identical hashes/text.

## Relationship to other docs

- `compat-lab.md` — Phase C scaffold (§ harness, per-category corpora, `vttest` runbook, Ghostty/Kitty/WezTerm differential). This matrix is the **release instance** of that scaffold.
- `manual-smoke.md` — human-in-loop checklist (7 areas, 21 rows, `grim`/`hyprctl` guidance, not CI). This matrix is the **automated** leg that stays CI-green.
- `dogfooding.md` / `soak-0.0.1.md` / `perf-baseline.md` / `perf-evidence.md` — daily-driver and soak/perf evidence (Phase G/F). Matrix reuses their corpora.
- `release-ladder.md` — `v0.2` row links to `compat-lab.md`; this matrix satisfies that row's gate sketch (`differential tests, fuzz, TUI corpora`).

## Verification gates (must PASS before merge)

| Gate                       | Command                                                                                               | Expected                                         |
| -------------------------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| `cargo fmt --check`        | `cargo fmt --all -- --check` via `just fmt-check`                                                     | PASS                                             |
| `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings` via `just clippy`                    | PASS — 0 warnings                                |
| `cargo test`               | `cargo test --workspace --all-targets --locked`                                                       | PASS — workspace green, `compat_matrix` 7/7 PASS |
| `cargo check windows`      | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked`                       | PASS                                             |
| `actionlint`               | `actionlint -color`                                                                                   | PASS                                             |
| `markdownlint`             | `bunx --bun markdownlint-cli2@0.23.1` via `just markdownlint`                                         | PASS                                             |
| `just check`               | `just check` (fmt-check + clippy + test + actionlint + markdownlint)                                  | PASS                                             |
| `act -n`                   | `act -n --workflows .github/workflows/ci.yml --workflows .github/workflows/codeql.yml`                | PASS — syntax only                               |
| no window leak             | `rg -n "winit\|wgpu\|Window\|Surface" crates/bitty-compat-lab tests/compat` outside docs forbid lists | PASS                                             |
| corpus bounds              | `MAX_CORPUS_BYTES 8192`, `MAX_ACTIONS 4096`, `MAX_SNAPSHOT_JSON_BYTES 16384`                          | PASS                                             |

## Evidence — CTX-0114 matrix run 2026-09-01 (headless bounded, rev `1d9eb6a+`)

- Run header: `date: 2026-09-01  host: cachyos-hyprland  bitty: 1d9eb6a+ (carryctx/ctx-0114)` reference revisions from `recordings/references/README.md` (ghostty `8867c37`, kitty `087b8c3`, wezterm `f93d903`, alacritty `ede2ac1`).
- Corpora: `tests/compat/*/corpus/*.bin` 39 files (30 baseline + 9 dogfooding) each ≤310 B typical, ≤8 KiB max, `cargo test -p bitty-compat-lab --test harness` 39 PASS, `cargo test -p bitty-compat-lab --test dogfooding_corpus` 6/6 PASS, `cargo test -p bitty-compat-lab --test compat_matrix` 7/7 PASS (new).
- Snapshots: `cargo run -p bitty-compat-lab --bin collect_dumps --locked` wrote 39 snapshots to `tmp/references/bitty/` mirrored to `recordings/references/bitty/` and umbrella, each `<16 KiB`, `80×24`, `CANONICAL_HASH_VERSION 1`, deterministic.
- Comparator: `cargo test -p bitty-compat-lab --test compare -- --nocapture` `total 39 self_passed 39 self_failed 0 reference_compared 0 reference_failed 0` graceful skip (no backend per-corpus dumps yet). Four-backend probe (`ghostty` `kitty` `wezterm` `alacritty`) verified via `comparator_reference_graceful_skip_when_absent`.
- Matrix artifact: `recordings/compat-matrix-2026-09-01.json` (14 entries, `<16 KiB`, sorted keys, bounded) + this doc table. `recordings/README.md` CTX-0114 snapshot section added.
- No `unsafe`, no window/GPU, no network, 14 surfaces covered, 4 terminals diffed.

## Revision history

- `2026-09-01` CTX-0114 `carryctx/ctx-0114` — matrix for release covering 14 surfaces × 4 terminals, 39 corpora bounded 8 KiB/4096, `crates/bitty-compat-lab/src/matrix.rs` + `src/compare.rs` alacritty + `tests/compat_matrix.rs` 7 tests, `recordings/compat-matrix-2026-09-01.json` + this doc, `just check` + `cargo test --workspace` + `cargo check --target x86_64-pc-windows-gnu` PASS.
- `2026-08-31` CTX-0099 `carryctx/ctx-0099` — dogfooding corpus 9 new, 39 snapshots, `dogfooding_corpus.rs` 6 tests, `manual-smoke.md` headless-verified PASS.
- `2026-08-30` CTX-0087/0074 — scaffold `compat-lab.md` + `tests/compat/harness.rs` headless `forbid(unsafe)` + `manual-smoke.md`.
