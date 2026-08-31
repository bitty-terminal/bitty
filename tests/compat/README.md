<!-- markdownlint-disable MD025 -->

# Compatibility Lab (Phase C)

Headless, bounded, `forbid(unsafe)` compatibility scaffolding for M1 Hardening Phase C.

## Scope

Lab root for `tests/compat/{vt,osc,keyboard,mouse,resize,unicode,shell,tui}/`. Each category holds placeholder corpora (`corpus/*.bin`, `*.txt`) and a `README.md` describing capture method, bounds, and differential harness. No window, no GPU, no network — `Parser -> TerminalAction -> State` only.

## Invariants

- `#![forbid(unsafe_code)]` — see `harness.rs` header. No `unsafe` in harness or corpora.
- Headless — no `winit::Window`, no `wgpu::Surface`, no `HeadlessRasterizer`. Only `bitty-vt::Parser` and `bitty-term-state::State`/`Snapshot`.
- Bounded — `MAX_CORPUS_BYTES = 8 KiB` per file (`bitty-pty::READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096` per parse, `MAX_OSC_BYTES = 1024` (`BoundedString::MAX_LEN`), `MAX_CORPORA_PER_CATEGORY = 64` per shard. Oversized input panics in tests, truncates in `BoundedString`/`BoundedBytes` per `bitty-vt` contract.
- Deterministic — every corpus re-parsed byte-by-byte must yield identical `TerminalAction` stream (checked in `harness::parse_bounded` via `parser::tests::action_stream_identical_across_chunkings` pattern).

## Harness

- `harness.rs` — headless bounded helper. `parse_bounded` asserts `MAX_CORPUS_BYTES` and determinism, `actions_to_snapshot` feeds `State`, `diff_snapshots` does text diff against Ghostty/kitty/WezTerm reference dumps, `list_corpus` walks `corpus/` bounded. See file header for `vttest` / Ghostty / kitty / WezTerm references and `crates/bitty-vt/tests/replay.rs` baseline.
- Run: `cargo test --workspace --locked` exercises existing `bitty-vt` replay/seeds; this lab adds corpora without new `cargo test` shard yet (Phase C scaffold). Future phase wires `tests/compat/**/corpus/*` into `#[test] fn compat_corpus_is_deterministic` that calls `harness::parse_bounded` + `State` and optionally diffs against checked-in reference snapshots under `tests/compat/*/reference/`.

## References to existing tests

- `crates/bitty-vt/tests/replay.rs` — `fixture_shell_session_replay`, `fixture_escape_storm_replay`, `fixture_fullscreen_app_replay`, `fixture_osc_sweep_replay`, `seeds_corpus_is_panic_free_and_deterministic`.
- `crates/bitty-vt/seeds/*.bin` — 14 seeds (1.3 KiB total) used as seed corpus in `replay.rs`; this lab mirrors that layout per-category.
- `crates/bitty-term-state/tests/replay_determinism.rs`, `parser_seeds.rs`, `property_invariants.rs` — deterministic replay, invariant checks feeding the same `State`.

## Differential corpora

- `vttest` — VT100/220/420 menus 1–12. Capture `script` logs from upstream `vttest` binary and check `Snapshot` against `vttest` expected grid/modes. See `vt/README.md`.
- Ghostty / kitty / WezTerm — offline differential: feed identical byte stream to each terminal, dump grid (`kitty --dump-commands`, Ghostty `dump`, `wezterm record`) and compare to `Snapshot` text/attrs/cursor/modes. Differential is snapshot-to-snapshot, not pixel. See `tui/README.md` and `vt/README.md`.

## Layout

```text
tests/compat/
  harness.rs                         # headless bounded harness, forbid(unsafe)
  README.md                          # this file
  vt/corpus/*.bin + README.md        # vttest + VT conformance
  osc/corpus/* + README.md           # OSC 0/7/8/52/133, clipboard, hyperlink
  keyboard/corpus/* + README.md      # kitty keyboard, modifyOtherKeys
  mouse/corpus/* + README.md         # SGR / UTF8 / urxvt mouse
  resize/corpus/* + README.md        # reflow, scroll region, alt-screen
  unicode/corpus/* + README.md       # width, wcwidth, emoji ZWJ, combining
  shell/corpus/* + README.md         # OSC 133 prompt marks, shell integration
  tui/corpus/* + README.md           # nvim/tmux/htop/fzf traces
```

## No window/GPU leak

Search `tests/compat` — must contain zero `winit`, `wgpu`, `Window`, `Surface`, `HeadlessRasterizer`, `gpu`, `create_surface` references except in this `README.md`'s forbid list. CI greps for leaks.

## Next

Phase C scaffold leaves corpora as placeholders (each `< 1 KiB`, bounded). Real captures land in Phase C follow-up after `vttest` runbook and Ghostty/kitty/WezTerm dump tooling are pinned in `recordings/references/`.
