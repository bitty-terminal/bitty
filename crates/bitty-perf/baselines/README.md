---
title: Parser Throughput Baseline (M1-11)
description: Committed deterministic parser-throughput baseline for bitty-vt Parser::advance over reused VT/escape corpora, with the exact command, environment, machine class, and generous regression gate
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 51
---

<!-- markdownlint-disable MD025 -->

# Parser Throughput Baseline (M1-11)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the committed
  parser-throughput baseline; it does not close a budget and does not make
  PB-6 a hard cross-platform gate.
- Ownership: bitty **CTX-0576** — _perf(m1): commit the parser throughput
  baseline_.
  - Priority: P0 | Area: perf | Labels: feat,P0,area:compat,area:perf |
    Milestone: v0.1.0
  - Issue: #1137 (M1-11) | RFC: `compatibility-milestone-rfc.md`
    "Performance guardrail"
  - Task: CTX-0576
- Scope: a deterministic benchmark over the real `bitty-vt::Parser::advance`
  API using reused corpora; a committed baseline artifact with the exact
  command, revision, and machine class; a `just` recipe; a bounded CI
  regression check with a generous threshold.
- Authority: `performance-budget-rfc.md#pb-6-throughput-floor` (PB-6,
  ≥ 40 MB/s sustained parse-and-render) is the accepted budget anchor. This
  document records a **measurement of the parser stage only**, not budget
  compliance: PB-6 covers parse-and-render on the slowest Tier 1 reference
  machine, and the RFC's reference-hardware open item is still open. The
  canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/`); this file is repository-local evidence.

## What M1-11 requires

`compatibility-milestone-rfc.md` acceptance evidence table, "Performance
guardrail" row:

> Parser throughput benchmark recorded (baseline number committed); no
> pathological regression versus plain-text throughput beyond an agreed
> factor.

Before this task the repository had `benches/vt_throughput.rs` (a synthetic
pattern benchmark) and `benches/terminal_state.rs` (apply-only), but no
committed baseline number, no reused corpus, no ratio-versus-plain guard, and
no CI check. This task adds all four.

## Harness

- `crates/bitty-perf/src/parser_throughput.rs` — bounded measurement module:
  corpus loading, median-of-rounds measurement, baseline parsing, and the
  regression gate. `#![forbid(unsafe_code)]`, headless (no `winit::Window`,
  no `wgpu::Surface`), no clock-dependent assertion beyond the documented
  thresholds.
- `benches/parser_throughput.rs` — human-facing `harness = false` bench that
  prints per-corpus MiB/s and the gate verdict, and can regenerate the
  artifact with `--write-baseline`.
- `crates/bitty-perf/tests/parser_throughput_regression.rs` — CI half, run by
  plain `cargo test` (and therefore by `just check` and the `Quality gates`
  job) with a bounded 512 KiB/corpus, 3-round measurement.
- `crates/bitty-perf/baselines/parser-throughput.json` — the committed
  baseline artifact (numbers plus provenance).

### Corpora (reused, deterministic, bounded)

| Corpus          | Source                                                                          |
| --------------- | ------------------------------------------------------------------------------- |
| `plain_text`    | `crates/bitty-vt/seeds/01-plain-text.bin` — the plain-text reference            |
| `mixed_seeds`   | all `crates/bitty-vt/seeds/*.bin` — SGR, cursor, DECSET, OSC, DCS, malformed    |
| `compat_corpus` | all `tests/compat/*/corpus/*.bin` — M1 surfaces (modes, color, osc, mouse, tui) |
| `escape_storm`  | synthetic heavy escape density (SGR/DECSET/OSC/DCS/cursor) worst case           |

Each corpus segment is repeated to `MAX_SEGMENT_BYTES` (64 KiB) and the
measurement repeats that segment to `sample_bytes`. Parsing runs through a
single `Parser` in `MAX_CHUNK_BYTES` (8 KiB, matching `bitty-pty::READ_CHUNK_SIZE`)
chunks; decoded actions are bounded per chunk at `MAX_ACTIONS` (4096). A
chunking-invariance witness asserts a bounded prefix decodes identically when
fed whole and byte-by-byte. Corpus bytes are always ≤ 8 KiB per file, so the
bound `read_compat_corpora` enforces is the same one the compat harness uses.

## Baselines (committed, MiB/s)

Command (exact):

```text
just perf-parser-baseline
# equivalently:
cargo bench -p bitty-perf --bench parser_throughput -- --nocapture --write-baseline crates/bitty-perf/baselines/parser-throughput.json
```

Environment for this capture:

| Field         | Value                                                                                              |
| ------------- | -------------------------------------------------------------------------------------------------- |
| Revision      | `6ed5362651e923285912caf9aade45ebac8d87af` (`main` at capture, pre-implementation baseline commit) |
| Profile       | cargo `bench` profile (release/optimized)                                                          |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                                                             |
| OS            | CachyOS Linux (Arch derivative), kernel 7.2.3, x86_64                                              |
| Machine class | desktop x86_64 24-core hybrid P/E (Intel i7-14650HX class), 31 GiB RAM, NVMe                       |
| Sample        | 4 MiB per corpus × 5 measured rounds, median reported                                              |

| Corpus          | Median (MiB/s) | Ratio vs `plain_text` |
| --------------- | -------------: | --------------------: |
| `plain_text`    |         108.84 |                1.0000 |
| `mixed_seeds`   |         176.58 |                1.6223 |
| `compat_corpus` |         120.62 |                1.1082 |
| `escape_storm`  |         125.00 |                1.1484 |

Observed actions per 4 MiB: `plain_text` 4 194 304, `mixed_seeds` 706 784,
`compat_corpus` 1 679 872, `escape_storm` 1 206 016. Plain text emits exactly
one action per byte; escape-heavy corpora emit fewer actions per byte, which
is why their byte throughput is higher.

Interpretation, stated honestly:

- All four corpora are **above** the PB-6 floor of 40 MB/s on this host, but
  PB-6 requires parse-and-render on the slowest Tier 1 reference machine; this
  capture measures the parser alone on one desktop. It is a regression anchor,
  not a budget-compliance claim.
- Escape-heavy corpora are faster than plain text here because printable runs
  emit one action per byte while escape sequences emit few actions per byte;
  the milestone guardrail is a ratio floor, so a mixed corpus getting much
  slower relative to plain text is the pathological signal.

## Regression gate

`ParserThroughputReport` is compared against the committed baseline with two
guards:

1. **Ratio gate (the milestone guardrail):** for every non-plain corpus, the
   measured `corpus/plain_text` ratio must be `>= committed_ratio / 4.0`
   (`REGRESSION_FACTOR`). A 4× collapse relative to plain text fails. The
   factor is deliberately generous: shared CI runners and debug-profile
   `cargo test` builds are noisy, and the guard is meant to catch a
   pathological (order-of-magnitude) regression, not normal variance.
2. **Absolute optimized-build floor:** the `mixed_seeds` throughput must be
   `>= 10 MiB/s` (`ABSOLUTE_FLOOR_MB_S`), skipped under `debug_assertions`
   because debug builds are much slower by construction.

Baseline drift is explicit: the ratio gate uses the committed values, so
updating the artifact changes the gate. Regenerate only with a recorded
revision/environment and treat the new numbers as reviewed evidence.

## Verification

```text
just perf-parser                                 # optimized ratio-gate verdict + per-corpus MiB/s
cargo test -p bitty-perf --test parser_throughput_regression --locked
cargo bench -p bitty-perf --bench parser_throughput -- --nocapture
cargo bench --no-run                             # bench must compile headlessly
just check                                        # fmt + clippy -D warnings + test + lint gates
```

Expected: the ratio gate passes on the committed baseline; the deliberately
regressed fixture (`pathological_regression_is_caught`) fails, proving the
gate is not vacuous.

## Limitations and confidence

- Single-host capture; no p50/p99 across machines. The artifact records the
  machine class, not a normalized score.
- The harness measures the parser stage in isolation; it does not include
  `State::apply`, rendering, or PTY I/O, so it cannot stand in for PB-6
  parse-and-render compliance.
- The `escape_storm` corpus is synthetic and intentionally adversarial; it is
  a bounded worst case, not a captured session.
- Cross-platform claims require CI evidence on the named platform; this host
  proves nothing for other platforms.

## Affected contracts

No budget, M1 requirement, or normative control changes. This document adds
repository-local measurement evidence; the accepted performance budget RFC in
`bitty-terminal-docs` remains the single source for PB-6.

## References

- `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`
  (`bitty-terminal-docs` submodule mount)
- `docs/specifications/compatibility-milestone-rfc.md` ("Performance
  guardrail" acceptance evidence row; submodule mount)
- `docs/product/perf-baseline.md` (Phase F harness map)
- `crates/bitty-perf/baselines/parser-throughput.json`
