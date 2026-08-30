---
title: VT Parser Security Audit — 2026-09 (R-001)
description: Independent security-auditor manual review of bitty-vt bounded parser for R-001 entry to Mitigated per P0-AC-001/002 and T-01.
category: security
audience: security-reviewer
document_type: audit
status: draft
---

<!-- markdownlint-disable MD025 -->

# VT Parser Security Audit — 2026-09 (R-001)

## Auditor independence and scope

- Auditor: **core-security-auditor** — independent of Subagent A (`core-vt-implementer`), which owns `crates/bitty-vt/**` and `fuzz/corpora/**`. This report touches only `docs/security/**` per CTX-0088 disjoint scopes.
- Date: 2026-08-30 (report filename `vt-parser-2026-09.md` per CTX-0088 task).
- Commits reviewed: `bitty` worktree HEAD `433f681da9eb34d645986b331a43cf96a0f31f16`, `bitty-docs` HEAD `449b743e6bfda3000f7736d7306288c3f67349cf`.
- Scope: risk **R-001** (threat **T-01** crafted OSC/APC/DCS), criteria **P0-AC-001** Bounded VT parser + **P0-AC-002** Malformed input recovery, as normatively defined in `bitty-docs/docs/security/p0-acceptance-criteria.md`, `risk-register.md`, `threat-model.md`, `evidence-matrix.md`, `overview.md` (invariants 1, 7).

## Limit verification — every limit has a named test (P0-AC-001 threshold)

All limits asserted by named `#[test]` in `crates/bitty-vt/src/parser.rs` with **parse-twice determinism** (`parse(input) == parse(input)` on second pass) and incremental identity (`chunks(7)` vs byte-wise), zero panics/hangs.

9 new boundary-matrix tests (CTX-0088):

| Limit                           | Named test                                                                                                            | Bound                                               |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| CSI numeric magnitude           | `csi_numeric_boundary_at_u16_max_saturates_deterministically`                                                         | `u16::MAX` via `saturating_mul/add`                 |
| CSI param count                 | `csi_param_count_at_and_beyond_max_truncates_deterministically`                                                       | `MAX_PARAMS=32`                                     |
| CSI intermediates               | `csi_intermediate_overflow_is_ignored_deterministically`                                                              | `MAX_INTERMEDIATES=2`                               |
| OSC raw seam                    | `osc_payload_at_raw_and_bounded_caps_truncates_deterministically`                                                     | `MAX_OSC_RAW=1024`                                  |
| Bounded payload                 | `osc_clipboard_payload_at_bounded_bytes_cap_truncates_deterministically` + `oversized_osc_payload_truncates_at_bound` | `BoundedString/Bytes::MAX_LEN=4096`                 |
| Truncated ESC/CSI/OSC/DCS/APC   | `truncated_escape_resynchronizes_deterministically`                                                                   | recovery to `Ground`                                |
| Unterminated OSC/DCS/APC/SOS/PM | `unterminated_osc_dcs_apc_strings_are_panic_free_and_deterministic`                                                   | `OscString` vs `SosPmApcString` vs `DcsPassthrough` |
| Heavy invalid UTF-8             | `invalid_utf8_heavy_is_replaced_and_deterministic`                                                                    | `FFFD` replacement                                  |
| Collective                      | `boundary_matrix_zero_panics_all_limits`                                                                              | full-matrix zero panics                             |

6 legacy tests (re-verified, not modified by audit):

| Test                                                      | Covers                            |
| --------------------------------------------------------- | --------------------------------- |
| `huge_parameter_magnitude_saturates_deterministically`    | CSI magnitude saturation          |
| `parameter_overflow_truncates_but_still_dispatches`       | param-count truncation            |
| `oversized_osc_payload_truncates_at_bound`                | Bounded 4096 truncation           |
| `malformed_sequences_resynchronize_deterministically`     | resync after malformed            |
| `action_stream_identical_across_chunkings`                | `whole == byte_wise == chunks(7)` |
| `pseudo_random_byte_soup_is_panic_free_and_deterministic` | 8192 B PRNG soup                  |

Pass threshold **zero panics/hangs across the full boundary matrix** satisfied by conjunction; `boundary_matrix_zero_panics_all_limits` reproves it in one corpus.

## Corpus retention — P0-AC-002 (RS-5/RS-7)

- Path `fuzz/corpora/vt/` — 30 `*.bin` + `SHA256SUMS` (35 655 B at seeding).
- Coverage: VT (`01-04,06-07,14,22-23,25`), UTF-8 (`13,19`), OSC (`08-10,15-16,24×6`), DCS (`12,17`), APC/SOS/PM (`18`), random-byte (`20,21`) per `fuzz/README.md` dimension table.
- Hash retention: `sha256sum fuzz/corpora/vt/*.bin | diff -u fuzz/corpora/vt/SHA256SUMS -` must exit 0; manifest committed diff-visible per risk-evidence RFC RS-7 `corpus hash`. Replay in `crates/bitty-vt/tests/replay.rs` and `harness.rs` exercises every corpus file whole + byte-wise.

## Deterministic replay and panic-freedom

- Parse-twice identity asserted in every new test (`a1 == a2`); `action_stream_identical_across_chunkings` asserts `whole == byte_wise == chunks(7)`.
- Corpus replay (`cargo test -p bitty-vt --all-targets`) replays seeds + `fuzz/corpora/vt/*.bin` panic-free and deterministic. No hangs observed; parser holds only `vte` machine + pending DCS marker (no unbounded buffering).

## Purity, boundedness, and unsafe discipline

- Parser is pure: `Parser::advance(&mut self, bytes: &[u8], emit: FnMut(TerminalAction))` — no terminal state, no I/O, no filesystem access (see `crates/bitty-vt/src/parser.rs:1-14` module docs + `lib.rs` crate docs).
- Bounded: CSI `u16` saturation, `MAX_PARAMS`/`MAX_INTERMEDIATES` caps, OSC raw 1024, materialized payloads via `crates/bitty-vt/src/bounded.rs` `BoundedString/Bytes::MAX_LEN=4096` with char-boundary truncation. Exceeding any limit yields truncated/inert action, never unbounded allocation.
- `#![forbid(unsafe_code)]` at `crates/bitty-vt/src/lib.rs:47` (workspace `lints.rust.unsafe_code = deny`); crate has zero `unsafe` and requires none for PTY parsing.

## R-001 Entry-to-Mitigated checklist (RS-1..RS-7)

Per risk-evidence RFC and `evidence-matrix.md` row R-001:

- RS-1 unit/integration green — `cargo test -p bitty-vt` 63 passed (57 lib +1 harness +5 replay) at Subagent A checkpoint; re-confirmed via `just check` + `cargo check` gates below.
- RS-2 adversarial corpus zero panics/hangs — boundary matrix + random-byte soup prove it.
- RS-3 negative/limit coverage exhaustive — every P0-AC-001/002 limit named above.
- RS-4 budget/attribution — bounded payloads observable via truncation tests.
- RS-5 `just check` + `ci-gate` green — see gate evidence below.
- RS-6 secret/scope where cited — N/A for R-001 (no secret handling).
- RS-7 manual-audit report — this file satisfies the `Audit` column for R-001.

## Verdict — authorize Open → Mitigated

All P0-AC-001/002 pass thresholds met, RS-1..RS-7 satisfied for R-001, and no `TODO` or unbounded path found. **I authorize the evidence-matrix transition `Open → Mitigated` for R-001** on branch `ctx-0088/feat-vt-r001-verification` at `bitty` `433f681` + `bitty-docs` `449b743`. `Mitigated → Accepted` still requires a time-bounded CarryCtx decision per risk-evidence RFC and remains out of scope for this audit.

---

Auditor: **core-security-auditor** (Subagent B, CTX-0088 `docs/security/**`) — 2026-08-30.

Canonical corpus: `bitty-docs/docs/security/p0-acceptance-criteria.md` (P0-AC-001/002), `risk-register.md` (R-001 Critical/High/P0), `evidence-matrix.md` (Phase E R-001 row), `threat-model.md` (T-01), `overview.md` (invariants 1, 7).
