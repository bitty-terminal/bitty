<!-- markdownlint-disable MD025 -->

# M1 Differential Oracle Corpus (`tests/compat/oracle`)

Differential-testing oracle for the M1 VT surface (CTX-0573, Issue #1133),
built on the `bitty-compat-lab` harness. Every scenario's expected bytes and
state derive from an **external reference or the authoritative
control-sequence specification** — never from Bitty's own output. The runner
executes each scenario against the Bitty build, diffs the observed state, and
emits a machine-readable per-scenario summary.

## Purpose and non-goals

- **Purpose.** Catch Bitty VT divergences from the M1 protocol matrix
  (`docs/specifications/compatibility-milestone-rfc.md`) by comparing against
  an independently derived oracle, satisfying the RFC's "Differential test
  corpus run against at least one reference oracle (vttest subset plus
  captured Ghostty/kitty/WezTerm sessions)" evidence requirement.
- **Non-goal.** This is not a self-golden corpus. A scenario whose expectation
  merely re-records Bitty's current output is rejected by the
  `oracle_expectations_are_externally_derived_not_self_golden` test.

## Layout

```text
tests/compat/oracle/
  README.md                              # this file
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
load failure. The JSON summary (`schema_version: 1`) records
`summary.total/passed/failed`, per-area counts, and each scenario's
`provenance`, `status`, and per-check `expected`/`actual`.

## Corpus and provenance

All expectations come from the read-only reference snapshot
`recording/references/xterm/ctlseqs.txt` (**xterm patch #411, 2026/08/23**,
`version.h` `XTERM_PATCH 411`) and the accepted M1 RFC. The `provenance:` line
in each `.expected` file records `spec|<citation>` or
`capture|<terminal> <version> <rev>`.

| Area                | Scenario                                                                            | Authoritative source                                                                                       |
| ------------------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| synchronized-update | `sync-2026-set`, `sync-2026`                                                        | M1 RFC (DECSET 2026 Required); ctlseqs.txt; ghostty `synchronized_output = 2026`                           |
| osc-color           | `osc-10-query`, `osc-11-query`, `osc-10-set`, `osc-11-set`                          | ctlseqs.txt "OSC Ps ; Pt ST": Ps 10/11, `?` query, `#RRGGBB` / `rgb:R/G/B` set                             |
| osc-title           | `osc-0-title`, `osc-2-title-st`                                                     | ctlseqs.txt "Ps=0 Change Icon Name and Window Title", "Ps=2 Change Window Title"                           |
| mouse-tracking      | `mouse-9-x10`, `mouse-1000-normal`, `mouse-1002-button`, `mouse-1003-any`           | ctlseqs.txt DECSET Ps 9 / 1000 / 1002 / 1003                                                               |
| mouse-encoding      | `mouse-1005-utf8`, `mouse-1006-sgr`, `mouse-1015-urxvt`, `mouse-encoding-exclusive` | ctlseqs.txt Ps 1005 / 1006 / 1015, mutual-exclusion semantics                                              |
| alternate-scroll    | `alt-scroll-1007`, `alt-scroll-1007-reset`                                          | ctlseqs.txt Ps 1007; M1 RFC classification correction (CTX-0175)                                           |
| cursor-style        | `cursor-style-steady-block`, `cursor-style-blinking-bar`, `cursor-style-default`    | ctlseqs.txt "CSI Ps SP q Set cursor style (DECSCUSR)" Ps 0/2/5                                             |
| alternate-screen    | `alt-screen-1049-roundtrip`, `alt-screen-47`                                        | ctlseqs.txt Ps 1049 (save + clear) / 47 (no clear); 1049l/47l restore                                      |
| cursor-keys         | `cursor-keys-decckm`, `cursor-keys-decckm-reset`                                    | ctlseqs.txt Ps 1 Application/Normal Cursor Keys (DECCKM)                                                   |
| device-status       | `dsr-5-status`, `dsr-6-cursor`, `da1-primary`                                       | ctlseqs.txt CSI Ps n DSR (5 -> `CSI 0 n`, 6 -> `CSI r;c R`) and CSI Ps c Primary DA (VT102 -> `CSI ? 6 c`) |

Reference-terminal captures: `recording/references/{ghostty,kitty,wezterm,alacritty}`
are pin-able dumps consumed by the existing comparator
(`crates/bitty-compat-lab/src/compare.rs`). The oracle's own expectations use
the spec citations above; a maintainer adding a capture-derived scenario
records the terminal, version, and upstream revision in the `provenance:` line.

## How to add a scenario

1. Add the raw VT bytes as `scenarios/<area-slug>-<detail>.bin` (≤ `MAX_CORPUS_BYTES` = 8 KiB).
2. Write the sibling `scenarios/<detail>.expected` with:
   - `area:` one of the module's [`AREAS`](../../../crates/bitty-compat-lab/src/oracle.rs);
   - `provenance:` `spec|<citation>` or `capture|<terminal> <version> <rev>`;
   - `grid: WxH` (canonical `80x24`);
   - a state assertion (`grid_text: blank|unchecked`, `text:`, `row N:`,
     `cursor: R C visible|hidden`, `mode: name = val`, `cursor_style:`,
     `title:`, `action:`, `reply:`).
3. Run `cargo test -p bitty-compat-lab --test oracle --locked` and the
   `oracle_runner` binary. A new scenario that fails is a real divergence to
   fix or to document as a tracked follow-up — never silence it by editing the
   expectation to match Bitty.
4. Register new areas in `AREAS` and in this table.

`scripts/gen-oracle-scenarios.sh` regenerates the committed corpus
byte-identically.

## Bounds and determinism

- Bounded: `MAX_CORPUS_BYTES` (8 KiB), `MAX_ACTIONS` (4096), `MAX_SCENARIOS`
  (64), expectation files ≤ `MAX_EXPECTED_BYTES` (16 KiB), summary < 256 KiB.
- Deterministic: sorted discovery, canonical JSON, no clock, RNG, network,
  display, sleep, or host path. Replaying a scenario twice yields identical
  checks.
- Headless: `Parser -> TerminalAction -> State` only; no `winit`/`wgpu`.

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

### Open divergence found by this corpus

`divergences/alt-screen-47-cursor-restore.expected` is a **real** Bitty
divergence surfaced while building the oracle (kept out of the green corpus
until fixed): xterm (`charproc.c` `srm_ALTBUF`) and ghostty
(`SwitchScreenMode .@"47"`, "The screen is not erased") keep the cursor global
across a `?47` alternate-buffer switch, only copying it — they do not
save/restore it the way `?1049` does. Bitty's `switch_alt_screen` saves and
restores the cursor for `Via47` as well as `Via1049`, so `X ?47h Y ?47l Z`
leaves the cursor after `X` (column 1) instead of after `Z`. The runner
detects it (`FAIL`) and it is recorded here as a tracked follow-up rather than
silenced by editing the expectation to match Bitty.
