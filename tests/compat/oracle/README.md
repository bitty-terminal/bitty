<!-- markdownlint-disable MD025 -->

# M1 Differential Oracle Corpus (`tests/compat/oracle`)

Differential-testing oracle for the M1 VT surface (CTX-0573, Issue #1133),
built on the `bitty-compat-lab` harness. Every scenario's expected bytes and
state derive from an **external authority or the authoritative
control-sequence specification** — never from Bitty's own output. The runner
executes each scenario against the Bitty build, diffs the observed state, and
emits a machine-readable per-scenario summary.

## Purpose and non-goals

- **Purpose.** Catch Bitty VT divergences from the M1 protocol matrix
  (`docs/specifications/compatibility-milestone-rfc.md`) by comparing against
  an independently derived oracle.
- **Corpus scope.** The corpus is **spec/reference-derived** (xterm
  `ctlseqs.txt` + source, the ghostty/kitty reference trees, and the accepted
  RFCs). It is **not** a vttest run and contains **no captured terminal
  sessions**. The M1 RFC's wider evidence requirement (a vttest subset plus
  captured Ghostty/kitty/WezTerm sessions) is therefore only _partially_
  satisfied by this task: this corpus supplies the spec-side differential
  checks and the runtime-emission checks; live vttest execution and
  reference-session capture remain tracked follow-ups (see Follow-ups). Claim
  no more than that.
- **Non-goal.** This is not a self-golden corpus. A scenario whose expectation
  merely re-records Bitty's current output is rejected: structured provenance
  (`authority` + `cite`) must be backed by the committed citation index and,
  where resolvable, re-found verbatim in the source.

## Layout

```text
tests/compat/oracle/
  README.md                              # this file
  authority-cites.txt                    # authority<TAB>cite index (guard source)
  scenarios/
    <id>.bin                             # raw VT bytes for one scenario
    <id>.expected                        # externally derived expectation
  divergences/
    mouse-1007-misclassified.bin         # deliberate-divergence fixture
    mouse-1007-misclassified.expected    # the pre-CTX-0175 WRONG reading
    alt-screen-47-cursor-restore.bin     # real Bitty ?47 cursor divergence
    alt-screen-47-cursor-restore.expected# reference expectation Bitty misses
```

The module lives at `crates/bitty-compat-lab/src/oracle.rs`; the runner binary
is `crates/bitty-compat-lab/src/bin/oracle_runner.rs`; the regression test is
`crates/bitty-compat-lab/tests/oracle.rs`.

## Running

```text
cargo test -p bitty-compat-lab --test oracle --locked
cargo run  -p bitty-compat-lab --bin oracle_runner --locked
cargo run  -p bitty-compat-lab --bin oracle_runner --locked -- --out recording/oracle-summary.json
```

Exit codes: `0` all scenarios pass, `1` at least one divergence, `2` usage or
load failure. The JSON summary (`schema_version: 2`) records
`summary.total/passed/failed`, per-area counts, and each scenario's `engine`,
structured `provenance` (`kind`, `authority`, `source`, `cite`), `status`, and
per-check `expected`/`actual`.

## Engines

| Engine            | Path                                   | Covers                                                   |
| ----------------- | -------------------------------------- | -------------------------------------------------------- |
| `state` (default) | `bitty-vt` parser → `bitty-term-state` | modes, cursor, title, grid, parse actions, DSR/DA1 bytes |
| `runtime`         | `bitty-runtime` end-to-end             | OSC 10/11 query replies, mouse coordinate emission       |

Terminal state is deliberately inert for OSC dynamic colors
(`crates/bitty-term-state/src/state.rs`, "the runtime owns the active palette,
query replies, and the gated set path"), so the OSC round trip and mouse
emitted bytes are only observable through the `runtime` engine.

## Corpus and provenance

Every expectation carries structured provenance:

- `authority:` — one of the closed set in `oracle.rs` `AUTHORITIES`
  (`xterm-ctlseqs`, `xterm-charproc`, `ghostty-modes`, `ghostty-stream`,
  `ghostty-terminal`, `kitty-window`, `m1-rfc`, `text-rendering-rfc`);
- `cite:` — a **verbatim token** from that exact source that defines the
  exercised behavior.

The committed index `tests/compat/oracle/authority-cites.txt` records every
accepted pair. `oracle_citations_are_backed_by_their_authority` requires each
scenario pair to be indexed and, when the source is resolvable, re-finds the
token verbatim in it, so a mis-citation fails instead of being rubber-stamped.

| Area                | Scenario                                                                  | Authority (cite)                                                                                   |
| ------------------- | ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| synchronized-update | `sync-2026-set`                                                           | M1 RFC "Synchronized updates … DECSET 2026"                                                        |
| synchronized-update | `sync-2026`                                                               | ghostty `synchronized_output = 2026` (ctlseqs does **not** define 2026)                            |
| osc-color           | `osc-10-query`, `osc-11-query`, `osc-10-set`, `osc-11-set`                | xterm-ctlseqs OSC Ps 10/11 and `"?"` query                                                         |
| osc-color           | `osc-10-11-roundtrip`                                                     | kitty-window `rgb:{r:02x}/{g:02x}/{b:02x}` reply form (runtime)                                    |
| osc-title           | `osc-0-title`, `osc-2-title-st`                                           | xterm-ctlseqs "Ps = 0 Change Icon Name…", "Ps = 2 Change Window Title…"                            |
| mouse-tracking      | `mouse-9-x10`, `mouse-1000-normal`, `mouse-1002-button`, `mouse-1003-any` | xterm-ctlseqs DECSET Ps 9 / 1000 / 1002 / 1003                                                     |
| mouse-encoding      | `mouse-1005-utf8`, `mouse-1006-sgr`, `mouse-1015-urxvt`                   | xterm-ctlseqs Ps 1005 / 1006 / 1015                                                                |
| mouse-encoding      | `mouse-encoding-exclusive`                                                | xterm-charproc "a reset is only effective against the matching mode" (not ctlseqs)                 |
| mouse-encoding      | `mouse-emit-x10`, `mouse-emit-sgr`, `mouse-emit-urxvt`, `mouse-emit-utf8` | xterm-ctlseqs wire forms (runtime `emit:`)                                                         |
| alternate-scroll    | `alt-scroll-1007`, `alt-scroll-1007-reset`                                | xterm-ctlseqs Ps 1007                                                                              |
| cursor-style        | `cursor-style-steady-block`, `cursor-style-blinking-bar`                  | xterm-ctlseqs Ps 2 / Ps 5                                                                          |
| cursor-style        | `cursor-style-default`                                                    | text-rendering-rfc "maps `0` to the configured default style" (ctlseqs says Ps 0 = blinking block) |
| alternate-screen    | `alt-screen-1049-roundtrip`, `alt-screen-47-no-clear`                     | xterm-ctlseqs Ps 1049 / Ps 47                                                                      |
| cursor-keys         | `cursor-keys-decckm`, `cursor-keys-decckm-reset`                          | xterm-ctlseqs Ps 1 application/normal cursor keys                                                  |
| device-status       | `dsr-5-status`, `dsr-6-cursor`, `da1-primary`                             | xterm-ctlseqs DSR 5/6, Primary DA `CSI ? 6 c`                                                      |

Every cite above is also present verbatim in its source at:
`recording/references/{xterm/ctlseqs.txt,xterm/charproc.c,ghostty/src/terminal/*,kitty/kitty/window.py}`,
`docs/specifications/compatibility-milestone-rfc.md`, and
`docs/specifications/text-rendering-rfc.md`.

## How to add a scenario

1. Add the raw VT bytes as `scenarios/<area-slug>-<detail>.bin` (≤ `MAX_CORPUS_BYTES` = 8 KiB).
2. Write the sibling `scenarios/<detail>.expected` with:
   - `area:` one of the module's [`AREAS`](../../../crates/bitty-compat-lab/src/oracle.rs);
   - `authority:` an id from `AUTHORITIES`, and `cite:` a verbatim token from
     that source's file (add the pair to the generator's `cites` array so the
     index is regenerated);
   - `grid: WxH` (canonical `80x24`);
   - a state assertion (`grid_text: blank|unchecked`, `text:`, `row N:`,
     `cursor: R C visible|hidden`, `mode: name = val`, `cursor_style:`,
     `title:`, `action:`, `reply:`, `emit:`);
   - `engine: runtime` plus `stimulus:`/`at_cell:` for reply/emission checks.
3. Run `cargo test -p bitty-compat-lab --test oracle --locked` and the
   `oracle_runner` binary. A new scenario that fails is a real divergence to
   fix or to document as a tracked follow-up — never silence it by editing the
   expectation to match Bitty.
4. Register new areas in `AREAS` and in this table.

`scripts/gen-oracle-scenarios.sh` regenerates the committed corpus and the
`authority-cites.txt` index from one source of truth.

## Bounds and determinism

- Bounded: `MAX_CORPUS_BYTES` (8 KiB), `MAX_ACTIONS` (4096), `MAX_SCENARIOS`
  (64), expectation files ≤ `MAX_EXPECTED_BYTES` (16 KiB), summary < 256 KiB.
- Deterministic: sorted discovery, canonical JSON, no clock, RNG, network,
  display, sleep, or host path. Replaying a scenario twice yields identical
  checks; runtime stimuli use a bounded pixel scan to hit an exact cell rather
  than a wall clock.
- Headless: `Parser -> TerminalAction -> State`, or a headless `Runtime`
  (`theme_resolved: true`, no PTY/GPU/display).

## Differential power (divergence-catch proof)

`crates/bitty-compat-lab/tests/oracle.rs::oracle_runner_catches_every_deliberate_divergence`
proves the runner fails on a real divergence in both directions:

- **Oracle wrong, Bitty right:** `divergences/mouse-1007-misclassified.expected`
  records the pre-CTX-0175 wrong reading of mode 1007 (`focus_events`) while
  the build correctly reports `alternate_scroll`; the runner returns `FAIL`.
- **Bitty diverges, reference oracle right:** the spec oracle for
  `sync-2026-set` expects `synchronized_update = on`; feeding it a query-only
  mutation (`CSI ? 2026 $ p`) makes Bitty observe `off`, and the runner
  returns `FAIL`.

`oracle_rejects_a_mis_citation` additionally proves the citation guard is not
vacuous: a fabricated ctlseqs citation fails verification.

### Open divergence found by this corpus

`divergences/alt-screen-47-cursor-restore.expected` is a **real** Bitty
divergence surfaced while building the oracle (kept out of the green corpus
until fixed): xterm (`charproc.c` `srm_ALTBUF`) and ghostty
(`SwitchScreenMode .@"47"`, "only copies the cursor") keep the cursor global
across a `?47` alternate-buffer switch — they do not save/restore it the way
`?1049` does. Bitty's `switch_alt_screen` saves and restores the cursor for
`Via47` as well as `Via1049`, so `X ?47h Y ?47l Z` leaves the cursor after `X`
(column 1) instead of after `Z`. The runner detects it (`FAIL`) and it is
tracked as a follow-up issue (see the PR) rather than silenced.

## Follow-ups

- Live `vttest` execution and captured Ghostty/kitty/WezTerm sessions (the rest
  of the M1 RFC evidence requirement) are not in this corpus; the
  `capture|`/reference-dump path in `crates/bitty-compat-lab/src/compare.rs`
  is the intended home.
- The `?47` cursor save/restore divergence above is a tracked fix.
