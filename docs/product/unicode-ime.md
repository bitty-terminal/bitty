---
title: Unicode and IME Model (Phase C deep)
description: Draft text-domain width/corpus contract and IME pipeline (Platform Input -> Raw keyboard vs IME Preedit overlay vs Commit to PTY) for Phase C deep — CTX-0079
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Unicode and IME Model (Phase C deep)

## Status and provenance

- Status: **draft**. Phase C deep scaffold for **CTX-0079** — _Implement Unicode/Text compatibility and IME model (Phase C deep)_ — branch `ctx-0079/unicode-ime` at `91705be` (worktree `.worktrees/ctx-0079-unicode-ime`), agent `opencode-commander`.
- Ownership: bitty **CTX-0079** — _Implement Unicode/Text compatibility and IME model (Phase C deep)_.
  - Priority: P0 | Area: vt | Labels: feat,area:vt,P0 | Milestone: v0.1.0 | RFC: OQ-004 | Task: CTX-0079
- Scope: extend `tests/compat/unicode/` corpus for wide/CJK/emoji/ZWJ/combining/ambiguous; ensure `crates/bitty-vt` width logic is bounded, deterministic, headless, `forbid(unsafe)`; scaffold IME model — **Platform Input -> Raw keyboard vs IME Preedit overlay vs Commit to PTY**; draft terminfo/`TERM` contract stub; keep `#![forbid(unsafe_code)]`, headless, bounded.
- Authority: OQ-004 remains `Proposed` until `compatibility-milestone-rfc.md` is accepted. This draft does not close OQ-004, does not accept the `v0.2` slice, and does not weaken normative security controls in `bitty-docs/docs/security/`. Nothing here is accepted direction until an ADR/RFC accepts it; do not cite as normative.

## Goals

- Provide a reviewable Unicode text-domain corpus and width contract for `v0.2` VT/Unicode compatibility without claiming a closed text RFC.
- Scaffold a bounded, deterministic, headless IME pipeline contract that separates raw keyboard input from preedit overlay from PTY commit, with reviewable state/ownership.
- Record the terminfo/`TERM` contract stub so later work has a pinned default and evolution path.

## Unicode — width and corpora

### Width logic (bounded, deterministic, headless, `forbid(unsafe)`)

- Single implementation: `crates/bitty-term-state/src/cell.rs::char_cell_width`.
- Algorithm: `if cp < 0x0300 { 1 } else if is_zero_width(cp) { 0 } else if is_wide(cp) { 2 } else { 1 }`. `is_zero_width` covers combining marks (`0300..036F` and many script blocks), variation selectors (`FE00..FE0F`, `E0100..E01EF`), zero-width spaces/direction marks (`200B..200F`, `202A..202E`, `2060..2064`, `FEFF`), ZWJ (`200D` zero-width); `is_wide` covers CJK Unified/Hangul/Fullwidth/Extension B/emoji blocks (`2E80..9FFF`, `AC00..D7A3`, `FF00..FF60`, `1F300..1F9FF`, `20000..3FFFD`, etc.). Pure `matches!` tables, no allocation, no `unsafe`, no I/O, deterministic. Marked as compact approximation pending the authoritative text RFC (ADR-0004 open item).
- Ambiguous-width chars (`U+00B7`, `U+2014`, `U+2192`, `U+2500`, `U+00A1`, `U+2026`, …) are `1` in bitty — no `EAW=Ambiguous → 2` rule. Documented deviation from some `wcwidth` tables; differential harness may list it as known divergence.
- `State::apply(Print(GraphemeCell))` maps `0` → drop scalar (combining), `1` → one cell, `2` → lead + `Cell::wide_spacer` trailing half. `State::check_invariants` enforces `cell.width ∈ {1,2}`, no orphan spacers (RFC invariant 2), headless, bounded by `GRID_COLUMNS` × `GRID_ROWS`. No `unsafe` in this path (workspace `unsafe_code = "deny"`; crates declare `#![forbid(unsafe_code)]`).
- Deterministic UTF-8 policy: invalid bytes → `U+FFFD` one cell, delegated to `vte`'s collector (`crates/bitty-vt/src/parser.rs`); identical offline and live, byte-by-byte re-parse identity asserted in `tests/compat/harness.rs::parse_bounded`.

### Corpus — `tests/compat/unicode/corpus/`

```text
unicode/corpus/
  01-wide-emoji.bin       # 🎉 U+1F389, U+FFFD, mixed narrow/wide
  02-combining.bin        # e + U+0301 → e + zero-width combining
  03-cjk-wide.bin         # CJK U+4E2D U+65E5, Hangul U+AC00, Fullwidth U+FF21-FF23, Extension B U+20000
  04-emoji-zwj.bin        # ZWJ family U+1F468 U+200D U+1F469 U+200D…, U+2764 U+FE0F variation selector
  05-ambiguous.bin        # Ambiguous EAW → 1 cell in bitty (U+00B7 U+2014 U+2192 U+2500)
  06-zero-width.bin       # U+0301 stacked, U+200B/C/D, U+FE0E/FE0F, U+E0100
  07-invalid-utf8.bin     # raw 0xFF 0xFE 0x80 bytes → U+FFFD (deterministic replay, split-chunk)
  08-mixed-width.bin      # interleaved narrow/wide/combining/emoji + SGR 31m/0m
```

- Every `corpus/*.bin` is `< 8 KiB` (`MAX_CORPUS_BYTES = 8 KiB` = `bitty-pty::READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096`, `BoundedString::MAX_LEN = 4096`, all `forbid(unsafe)`, all headless (`Parser -> TerminalAction -> State -> Snapshot` only, no `winit`/`wgpu`/`Window`/`Surface`). Deterministic: `parse_bounded` re-parses byte-by-byte and asserts identity (same pattern as `crates/bitty-vt/src/parser.rs::tests::action_stream_identical_across_chunkings`).
- Differential: feed same stream to Ghostty/kitty/WezTerm, dump grid (`kitty --dump-commands`, Ghostty dump, `wezterm record`), compare `Snapshot` `width`/`spacer` invariants and `glyph` via `tests/compat/harness.rs::diff_snapshots`; pixel diff is out of scope.
- Relationship to existing `bitty-vt` tests: `crates/bitty-vt/seeds/13-utf8-invalid-split.bin` (`\xff` → `U+FFFD` one cell), `replay.rs::fixture_fullscreen_app_replay` (`🎉` wide), `crates/bitty-term-state/src/cell.rs` unit tests.

## IME model — Platform Input -> Raw keyboard vs IME Preedit overlay vs Commit to PTY

### Problem

CJK/emoji input requires an Input Method Editor (IME) that holds an unfinished composition (preedit) before committing text to the application. Raw keyboard events must not double-commit while an IME session is active; preedit must not mutate Terminal Truth; commit must follow a single deterministic PTY write path.

### Pipeline (draft)

```text
Platform Input (winit)
  │
  ├─ Raw keyboard path: KeyEvent -> encode_key_event -> Option<Vec<u8>> -> PTY master
  │    Bounded: MAX_ENCODED_LEN = 8 per key; synthetic/release/filtered keys -> None.
  │    Headless: pure, deterministic, no display server.
  │
  ├─ IME path (when IME active; winit IME events when bridged):
  │    Platform IME Preedit (marked text + cursor + underline style)
  │      │
  │      ├─ Preedit overlay: ephemeral UI layer, NOT State, NOT Snapshot
  │      │    Bounded: preedit text <= 256 scalars, truncated deterministically.
  │      │    Deterministic: same preedit sequence -> same overlay bytes.
  │      │    Headless: overlay state is pure data, tested without window.
  │      │    Rendering: preedit renders as decorated inline overlay above the
  │      │    cursor (underline/caret), never as grid cells; grid invariants
  │      │    (width ∈ {1,2}, no orphan spacers, cell totality) are untouched.
  │      │
  │      └─ Commit -> PTY master (same path as raw keyboard commit)
  │           Commit text is UTF-8; write is bounded (commit_len <= 256 UTF-8 bytes
  │           per commit, larger commits split). Commit bytes are `text.as_bytes()`
  │           of the IME committed string, never synthesized key sequences.
  │
  └─ PTY master (single writer): raw keyboard bytes and IME commit bytes are
       enqueued through one bounded, deterministic write path; no second channel.
```

### Contracts (draft, pending RFC acceptance)

- **Single PTY writer.** Raw keyboard bytes (`encode_key_event`) and IME commit bytes share one bounded PTY write queue. No parallel channel. Order is platform delivery order; tests assert commit order determinism.
- **Preedit is ephemeral overlay, not Terminal Truth.** While composing, `State` and `Snapshot` do not change; only the overlay does. Cancel/escape clears overlay, not grid. Overlay never leaks into `scrollback`, `damage`, `replies`, or `Snapshot::cells`.
- **Bounded and deterministic.** Preedit text/scalar count, overlay width, and commit byte length are bounded (`IME_PREEDIT_MAX_SCALARS = 256`, `IME_COMMIT_MAX_BYTES = 256` per commit). Exceeding truncates deterministically (char-boundary cut, same as `BoundedString`). No `unsafe`, no allocation beyond the caps, headless overlay state.
- **Headless test seam.** `ImeState { preedit: Option<Preedit>, composing: bool }` is pure data (no `winit` type). Field-level mappers (`set_preedit`, `commit`, `cancel`) are unit-testable without a display server. Display-gated winit IME events (`Ime::Preedit`/`Commit`/`Enabled`/`Disabled`) are filtered at the `translate_window_event` seam like keyboard/mouse, and exercised by `bitty-platform` headless unit tests.
- **`forbid(unsafe)`** — overlay and commit logic are `#![forbid(unsafe_code)]`, like `bitty-vt`/`bitty-term-state`/`bitty-platform` and `tests/compat/harness.rs`.

### Deferred / open

- Bridging `winit::event::Ime` into `bitty-platform` (`PlatformEvent::Ime*`) and wiring preedit rendering into `bitty-render`/`bitty-ui` land in a follow-up slice. This draft defines the seam and bounds; it does not add window/GPU code. Overlay rendering will reuse the existing `SurfaceTarget`/damage flow without mutating `Snapshot`.
- Candidate `ImeState` crate placement: `bitty-platform` (adopter of `winit` IME events) owns `PlatformEvent::Ime*` and `ImeState` overlay data; `bitty-runtime` owns commit-to-PTY policy (focus suppression, queue ordering). Both require ADR-0003 input-domain placement decision to be accepted.

## Terminfo / `TERM` contract stub

### Current

- `crates/bitty-pty/src/builder.rs::DEFAULT_TERM = "xterm-256color"`. `PtyBuilder::new` seeds exactly one env entry `TERM=xterm-256color`; all other env entries must be explicitly allowlisted via `PtyBuilder::env` (bounded: `MAX_ENV_ENTRIES = 64`, `MAX_ENV_VALUE_BYTES = 4096`, `MAX_ARGS = 256`, `MAX_ARGV_BYTES = 64 KiB`). Direct argv exec, no shell interpolation, minimal child environment — security defaults per threat model.

### Contract (stub, pending acceptance)

- **Default:** `TERM=xterm-256color` is the shipped default. No distribution override until `compatibility-milestone-rfc` defines the terminfo matrix.
- **Evolution:** when a custom `bitty` terminfo entry exists (`tic`-compiled `bitty` / `bitty-256color` with the accepted `terminfo.src`), `DEFAULT_TERM` moves to `bitty` at a minor-version bump after the entry is published and `compatibility-matrix` records it. Until then, `xterm-256color` is the compatibility anchor (Ghostty/kitty use `xterm-*` generically; `bitty-pty` follows that).
- **Override:** callers may set `TERM` explicitly via `PtyBuilder::env("TERM", "...")` (second call replaces the earlier while keeping insertion position). Policy: `bitty-runtime` does not rewrite `TERM` from config; terminfo is a `bitty-pty` seam only, not a config key.
- **Terminfo source:** a checked-in `terminfo/bitty.ti` draft (stub) will define `bitty`/`bitty-256color` capabilities consistent with the VT features already in `bitty-vt`/`bitty-term-state` (SGR true color 38;2/48;2, underline `4:x`, OSC 8 hyperlink, `ED 3` scrollback clear, `DECSCUSR`, etc.). Until landed, this section is the stub; `just check` + `cargo test` remain the gates, not `tic` compilation.

## Bounds, determinism, headless, `forbid(unsafe)`

- Every unicode `corpus/*.bin` ≤ `MAX_CORPUS_BYTES` (8 KiB), actions ≤ `MAX_ACTIONS` (4096), OSC payloads ≤ `BoundedString::MAX_LEN` (4096). Corpus files in this draft average `< 80 B`; `placeholder.bin` retained for scaffold compat.
- `crates/bitty-vt`, `crates/bitty-term-state`, `crates/bitty-platform`, `tests/compat/harness.rs` all declare `#![forbid(unsafe_code)]` (workspace `unsafe_code = "deny"`). Width logic and IME overlay/commit are pure, headless, allocation-bounded, and deterministic (byte-by-byte re-parse identity, `BoundedString` char-boundary truncation).
- No `winit`, `wgpu`, `Window`, `Surface`, or `HeadlessRasterizer` in `tests/compat/**` corpora or harness (grep must be `0` except forbid-list). IME overlay in this draft is pure data; no window/GPU leak. CI runs fully headlessly (`cargo test --workspace --locked`, `just check`, `act -n`).

## Next

- Wire `tests/compat/unicode/corpus/*.bin` (and existing categories) into a `cargo test` harness that calls `harness::parse_bounded` + `State` and optionally diffs against checked-in `unicode/reference/*.txt` grid dumps (Phase C follow-up after `vttest` pin in `recordings/references/`).
- Land `bitty-platform::PlatformEvent::Ime*` + `ImeState` (bounded overlay data) and `bitty-runtime` commit-to-PTY wiring as the IME implementation slice (requires input-domain ADR acceptance). Until then, this draft is the reviewable seam.
- Land `terminfo/bitty.ti` draft and record the `terminfo` revision in `recordings/references/` when accepted.

## References

- `bitty-docs/docs/specifications/terminal-state-rfc.md` — parser obligations, replay determinism, fuzzing/differential.
- `crates/bitty-term-state/src/cell.rs` — `char_cell_width`, `is_zero_width`, `is_wide`, width invariants.
- `crates/bitty-vt/src/parser.rs`, `crates/bitty-vt/src/bounded.rs` — bounded parsing, deterministic UTF-8 → `U+FFFD`.
- `crates/bitty-pty/src/builder.rs` — `DEFAULT_TERM`, bounded env/argv.
- `tests/compat/harness.rs` — headless bounded harness (`MAX_CORPUS_BYTES`, `MAX_ACTIONS`, `parse_bounded`, `actions_to_snapshot`, `diff_snapshots`, `list_corpus`).
- `docs/product/compat-lab.md` — Phase C scaffold runbook (`vttest`, Ghostty/kitty/WezTerm differential).
- `docs/product/text-compatibility.md` (draft, future) — when accepted, this file folds its Unicode sections into the text-compatibility draft and vice versa.
