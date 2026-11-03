# Idle RSS baseline: attribution, scenario, and target (bitty#2859)

Records the idle-resident-memory work for the 0.2.0 line: an opt-in
per-subsystem attribution dump, an idle-baseline soak scenario, and the target
this work tracks against. It also states plainly what is measured and what is
still pending an owner run.

Nothing here changes a product budget. PB-2 (`PB2_IDLE_RSS_MB = 80`, idle RSS
p50 for one window after 60 s) stays the accepted target and is not a CI gate
(`OQ-100`); the interim figure below is a tracked engineering target for this
line, not a revision of PB-2.

## Baseline under investigation

The owner's idle window on the reference desktop measured:

| metric | value |
| --- | --- |
| VmRSS | 236 MiB |
| PSS | 187 MiB |
| Private_Dirty | 151 MiB |
| threads | 20 |
| shape | flat over a 5-minute window |

A total says nothing about which subsystem to work on, and the gap to PB-2's
80 MB is large enough that guessing would spend a cycle on the wrong row. The
deliverables below exist to replace the guess with a per-row figure.

## Attribution method

`bitty_runtime::runtime::memory` renders one bounded report from three inputs:
the runtime's own retained caches, the kernel's accounting for the process, and
the plugin Lua heaps the composition root samples.

| row | source | provenance |
| --- | --- | --- |
| `glyph-atlas` | glyph cache retained coverage + atlas texture + block-mask table (`GridRenderer::retained_bytes`) | measured |
| `fonts` | sum of `Rss` over file-backed mappings with a font extension in `/proc/self/smaps` | measured |
| `scrollback` | `State::retained_heap_bytes` summed over every live grid (screens, scrollback ring, hyperlink/zone/damage/reply/image stores, title/cwd) | measured |
| `lua-vm` | plugin Lua heap totals from the live VMs | measured |
| `surface` | surface extent × 4 bytes per texel × resident swap-chain images (`Fifo` keeps two) | estimated |
| `thread-stacks` | thread count × per-thread stack reservation | reservation |
| `unattributed` | process RSS minus every measured and estimated row | residual |

Three rules keep the table honest, and each has a test:

- **No silent zero.** A source that cannot be read on this platform, or at
  this moment (a plugin VM borrowed elsewhere), is `unavailable` with `0`
  bytes — never a measured `0`.
- **Reservations never enter the RSS arithmetic.** Thread stacks are virtual
  reservations whose resident pages are counted wherever the kernel put them.
  The row is printed for completeness, its `%rss` cell is `-`, and
  `accounted_bytes` excludes it, so `unattributed` stays exactly
  `rss − (measured + estimated)`.
- **Over-attribution is named.** If the rows ever exceed RSS, the report sets
  `over_attributed` instead of silently clamping.

The report is 8 KiB-bounded, written `0600` by an atomic rename, and costs
nothing on a hot path: the session hook is one `Option` check and one `Instant`
compare per tick, and the `/proc` read plus rendering only run when an interval
is due. The font row reads `/proc/self/smaps` once per report (a page walk),
which is why it is a report-time cost and never a per-frame one.

## Reproduction

Owner window (their normal graphical session, opt-in, no agent involvement):

```sh
BITTY_MEMORY_REPORT_SECS=300 bitty
```

The session writes `$XDG_STATE_HOME/bitty/memory-report-<pid>.txt` immediately
on the first frame and then every 300 s of ticked wall clock (`$XDG_STATE_HOME`
follows the XDG state convention; the fallback chain is the same one the crash
and exit reports use). A frame is ticked whenever the event loop wakes, so a
window that parks completely between wakeups gets its reports on the wakeups
rather than on a timer thread. Anything else — unset, empty, `0`, malformed —
leaves the dump off.

Idle-baseline scenario (no window, no display server, no GPU):

```sh
BITTY_SOAK_IDLE_PANES=4 BITTY_SOAK_EVIDENCE_DIR="$PWD/recording/idle-rss" \
  just soak-campaign 1800
```

One window with four panes, no input at all, sampled on real wall clock at the
1-minute, 5-minute, and 30-minute marks of the nominal window; the budget knob
scales the marks, so `just soak-campaign 120` samples the 4 s / 20 s / 120 s
points of the same schedule. Each mark writes the full attribution report plus
the sampled series into the evidence directory. The scenario is `#[ignore]`d,
so `cargo nextest run --workspace` pays nothing for it.

Focused runs:

```sh
cargo test -p bitty-runtime --test memory_attribution
cargo test -p bitty-runtime --lib memory::
cargo test -p bitty-terminal --bin bitty memory_report::
BITTY_SOAK_CAMPAIGN_SECS=120 BITTY_SOAK_IDLE_PANES=4 \
  cargo test -p bitty-runtime --test soak_campaign idle_baseline -- --ignored --nocapture
```

## Measured run (this change, headless harness)

Executed on the authoring host with the commands above: budget 120 s, four
panes, no input. The process is the compiled test binary with the deterministic
rasterizer, so it has no window, no GPU, no font files, and no plugin VM. The
numbers describe the harness, not an owner session; they are here to prove the
scenario and the attribution run, not to stand in for the baseline.

| mark | elapsed_ms | panes | cycles | rss_kib | pss_kib | private_dirty_kib | threads | fds | max_tick_ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| mark1 (1 min) | 4,039 | 4 | 40 | 28,928 | 25,732 | 5,420 | 10 | 20 | 13 |
| mark2 (5 min) | 20,002 | 4 | 199 | 28,928 | 25,732 | 5,420 | 10 | 20 | 13 |
| mark3 (30 min) | 120,081 | 4 | 1,196 | 28,928 | 25,731 | 5,420 | 10 | 20 | 13 |

RSS growth across the window: 0 KiB. Attribution at the last mark:

| row | bytes | KiB | %rss | provenance |
| --- | --- | --- | --- | --- |
| glyph-atlas | 263,146 | 256 | 0.9% | measured |
| fonts | 0 | 0 | 0.0% | measured |
| scrollback | 995,911 | 972 | 3.4% | measured |
| lua-vm | 0 | 0 | – | unavailable |
| surface | 2,779,136 | 2,714 | 9.4% | estimated |
| thread-stacks | 27,262,976 | 26,624 | – | reservation |
| unattributed | 25,584,079 | 24,984 | 86.4% | residual |

Accounted 4,038,193 B (13.6%) of 28,928 KiB RSS. The residual dominates in the
harness because the binary's own text/data, allocator arenas, and the test
framework's pages are real resident bytes nobody has charged — which is exactly
the quantity the owner run has to split on a real session, where the GPU
driver, font faces, and plugin heaps are present.

What this run does not prove: that an owner session is flat, that the GUI rows
have these values, or that PB-2 is reachable. It proves the scenario samples
three ordered marks on real wall clock, that the dump attributes the rows it
claims, and that no row exceeds RSS.

## Target

- **PB-2 is unchanged.** `PB2_IDLE_RSS_MB = 80` (p50 RSS, one window, 60 s
  idle, bundled plugins only) stays the accepted product target in the
  external performance suite, and this document does not redefine, split, or
  waive it.
- **Flatness is a target, not an observation.** The idle window must not grow
  more than 32 MiB RSS between its first and last mark for a four-pane window
  with bundled plugins only; the scenario enforces that cap
  (`MAX_IDLE_RSS_GROWTH_KIB`) and fails the run when it is exceeded.
- **Interim target for this line: idle PSS p50 ≤ 160 MiB at the 30-minute
  mark.** 160 MiB is a 15% cut from the measured 187 MiB PSS, chosen because
  the first three rows an owner run will separate — swap-chain residency,
  mapped font faces, and retained scrollback — can plausibly account for that
  much of the current total without an architectural change, while a larger
  cut would be a number invented ahead of the breakdown. It is stated on PSS
  rather than RSS because PSS is what a second terminal actually costs on a
  shared desktop, and it is measured at the 30-minute mark rather than 60 s
  because the owner's growth shape is a long-uptime one.
- **Provisional by construction.** Once the first owner breakdown lands, the
  interim figure is re-derived from the row sizes and either tightened toward
  PB-2 or replaced by explicit per-row budgets. The binding target remains
  PB-2.

## Residual gap (pending an owner run)

The numeric per-subsystem breakdown of the 236 MiB baseline is **pending an
owner run**. Producing it requires a real graphical session — a window, a
swap-chain, mapped font faces, and the owner's plugin set — and driving the
owner's desktop is out of scope for automated work. The exact command is:

```sh
BITTY_MEMORY_REPORT_SECS=300 bitty
```

Leave the session idle for at least 30 minutes, then read the newest
`memory-report-*.txt` under the state directory. That file carries VmRSS, PSS,
Private_Dirty, the thread count, and every row above; the `unattributed`
residual is what remains to be split after the measured rows are charged.

Until then: the mechanism, the scenario, and the write path are verified
headlessly (tests and the run above); the baseline split is not measured, and
no MiB figure for a GUI row appears in this document.

## Verification

All commands run in the repository worktree for this change; exit codes are
the real ones.

| command | exit |
| --- | --- |
| `just fmt-check` | 0 |
| `just clippy` | 0 |
| `cargo check -p bitty-runtime --tests --offline` | 0 |
| `cargo check -p bitty-terminal --all-targets --offline` | 0 |
| `cargo test -p bitty-runtime --lib memory::` | 0 (19 passed) |
| `cargo test -p bitty-runtime --test memory_attribution` | 0 (7 passed) |
| `cargo test -p bitty-terminal --bin bitty memory_report::` | 0 (6 passed) |
| `cargo test -p bitty-terminal --bin bitty the_memory_report_dump_writes` | 0 (1 passed) |
| `BITTY_SOAK_CAMPAIGN_SECS=15 cargo test -p bitty-runtime --test soak_campaign -- --ignored --nocapture --test-threads=1` | 0 (3 passed) |
| `cargo +1.85 check -p bitty-runtime --offline` | 0 |
| `cargo +1.85 check -p bitty-terminal --offline` | 0 |
| `just check` | 100 (two pre-existing local liveness budgets, below) |
| `CI=1 BITTY_PERF_DIR="$BITTY_WORKSPACE/bitty-perf" just check` | 139 (only `markdownlint`, below) |

Two environment findings, both outside this change:

- The plain `just check` stops in `test` on two soak suites that assert a 60 s
  *local* liveness budget: `opencode_shaped_soak_stays_bounded` (65.0 s) and
  `soak_headless_1000_ticks_bounded_and_deterministic` (66.7 s). Both accept a
  240 s budget under `CI`/`GITHUB_ACTIONS`; neither touches the files changed
  here. The `CI=1` run below is the CI-equivalent invocation.
- `CI=1 BITTY_PERF_DIR="$BITTY_WORKSPACE/bitty-perf" just check` passes every
  recipe up to the last one, including `cargo nextest run --workspace`
  (10,109 tests run, 10,109 passed, 14 skipped). It exits 139 in
  `markdownlint` because `bunx` terminates with `SIGSEGV` on this host; the
  Markdown gate therefore rests on CI rather than a local run. This document
  was reviewed against the repository's markdownlint configuration by hand
  (fenced-code languages, blank lines around fences/tables, no trailing
  whitespace, single top-level heading).
- `BITTY_PERF_DIR` has to be set explicitly in a worktree: the justfile default
  (`../bitty-perf`) resolves relative to the checkout root, which is
  `.worktrees/` here.

## References

- `crates/bitty-runtime/src/runtime/memory.rs` — attribution rows, `/proc`
  sampling, rendering, and the write path.
- `crates/bitty-runtime/src/runtime/memory.rs` tests and
  `crates/bitty-runtime/tests/memory_attribution.rs` — row arithmetic on a live
  runtime.
- `crates/bitty-terminal/src/memory_report.rs` — the opt-in knob, cadence, and
  state-directory policy.
- `crates/bitty-runtime/tests/soak_campaign.rs` — the idle-baseline scenario.
- `crates/bitty-render/src/grid.rs` — `GridRenderer::retained_bytes`.
- PB-2's constant and window definition live in the external performance
  suite's `crates/bitty-perf/src/lib.rs`; the accepted-target status is
  recorded in `OQ-100`.
