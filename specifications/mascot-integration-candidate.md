# Bittie mascot integration (candidate, CTX-0729)

> Status: **candidate**. Tracks
> [issue #1318](https://github.com/bitty-terminal/bitty/issues/1318).
> Only the `--mascot` / first-run splash slice described under
> "Shipped slice" is implemented; everything else is a proposal awaiting
> owner art and review. It is not an acceptance decision and does not
> close #1318 on its own.

## Why

The Bittie mascot (see `recording/bitty-mascot/`, `recording/bitty-ascii.txt`,
the portrait header in `crates/bitty-app/src/main.rs`, and the vendored
`crates/bitty-app/assets/mascot.txt`) needs one defined path into the
product: a first-run greeting that never interferes with startup, plus a
handoff of web-ready assets to `bitty-website`. Without a contract, art
drifts between the workspace scratch tree and the binary, and every new
surface re-decides width handling, suppression, and persistence.

## Asset contract

| Kind | Lives in | Format | Rule |
| ---- | -------- | ------ | ---- |
| Source art (user-drawn, work in progress) | `recording/bitty-mascot/` (workspace scratch, gitignored or untracked) | text, sixel, block, pixel drafts | Never compiled in; freely iterated. |
| Vendored binary art | `crates/bitty-app/assets/mascot.txt` | pure ASCII, `include_str!` | Byte-identical to the chosen source file; refreshed only by copying the accepted source over it. Single owner: `crates/bitty-app/src/mascot.rs`. |
| Web assets | `bitty-website` (separate repo) | PNG/SVG | Exported from source art, never from the vendored `.txt`; tracked there, not here. |

Rules:

1. **One vendored file.** The binary embeds exactly one mascot text asset.
   The sixel/block variants stay out of the binary (they assume escape
   support the headless path cannot prove).
2. **ASCII-only, bounded.** Vendored art is pure ASCII, at most 32 lines,
   at most 80 columns wide, no trailing whitespace. The width bound is
   asserted by unit test (`mascot_width`), so an asset refresh fails in
   `cargo test` instead of wrapping on an 80-column terminal.
3. **Width-aware fallback.** When the window width is provably narrower
   than the art, print the one-line fallback
   (`bitty! (mascot skipped: window too narrow for the art)`) instead of
   a wrapped mess. Unknown width (piped headless, `COLUMNS` unset or
   garbage) prints the full art: pure text is always safe.
4. `crates/bitty-app/src/init.rs` keeps its `INIT_MASCOT_*` names as thin
   re-exports of the mascot module so the `bitty init` wizard greeting
   and the splash can never disagree.

## Init-splash hook point

Normal terminal startup in `crates/bitty-app/src/main.rs`, placed after
every subcommand dispatch (`run`, `config`, `init`, `doctor`, `ctl`,
`list`, `inspect`, `dev`, `plugin`) and before user-config loading:

- Subcommand and machine flows never splash: `--headless` and
  `--test-mode` skip it (deterministic CI/stdout contract), and every
  subcommand returns before the hook.
- The hook is three pure steps over injected values
  (`mascot::should_show_splash`, `mascot::splash_marker_path`,
  `mascot::record_splash_shown`), so unit tests cover the policy
  without touching the host filesystem.
- First-run detection is a marker file, not config state:
  `$XDG_DATA_HOME/bitty/splash-shown` (fallback
  `~/.local/share/bitty/splash-shown`, same root policy as the plugin
  store). Absent marker means first run. The marker is created
  best-effort; a write failure is swallowed so the splash can never
  block shell spawn.
- `--no-splash` suppresses the splash for one launch without touching
  the marker. `--mascot` prints the art to stdout, records the marker
  best-effort, and exits 0 (local class: no config, no instance, no
  plugin VM, no network, no stdin read).
- No network, no blocking: the path does only stdout writes plus one
  small best-effort file create. It never reads stdin and never waits.

## Website handoff (bitty-website)

What the website repo needs once owner art lands (out of scope for the
shipped slice, recorded here so the export is not re-decided later):

1. A raster export (PNG, transparent background) and a vector export
   (SVG) rendered from the accepted source art, not from the ASCII
   vendoring.
2. A 16/32 px favicon-grade crop that stays legible at small sizes.
3. License/provenance line per file (the workspace `PROVENANCE.md`
   already tracks sources; carry the attribution into the export).

## Shipped slice (CTX-0729)

- `crates/bitty-app/src/mascot.rs`: art constant, width/fallback
  selection, marker path resolution, show/record policy. All pure over
  injected values except the two documented live edges (env vars,
  best-effort marker write in `main`).
- `--mascot` / `--no-splash` CLI flags with help text.
- First-run splash on the normal startup path only.
- Unit tests (`src/tests.rs`) plus binary integration test
  (`tests/cli_mascot.rs`).

## Open points

1. Owner art is still being drawn (`recording/bitty-mascot/`); the
   vendored `assets/mascot.txt` is the current placeholder until the
   accepted portrait replaces it byte-for-byte.
2. Whether `bitty init` success should also record the marker (the
   wizard already greets, so a later first-launch splash would repeat).
   Deferred: needs a hermetic data-root in `InitEnv`.
3. Animated (`animations/`) and pixel (`pixel/`) variants have no
   in-terminal surface proposal; website-only until someone scopes one.
4. Splash styling (color, centering) is deferred; current output is
   plain stdout text for pipe-safety.

## Acceptance criteria

- `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked`
  with `-D warnings`, `cargo test -p bitty-app --locked`, and
  `cargo +1.85 check --workspace --all-targets --locked` are green.
- `bitty --mascot` prints the vendored art and exits 0; `bitty
  --no-splash` parses and suppresses; the marker makes the splash
  once-only; marker write failure never changes the exit code.
