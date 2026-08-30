---
title: VT Fuzz Corpus — R-001 (P0-AC-001/002)
description: Retained VT/UTF-8/OSC/DCS/APC fuzz corpus for CTX-0088, closing R-001 per risk evidence RFC RS-1..RS-7 — boundary matrix, chunking determinism, and in-repo corpus hash for the Verified gate.
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 MD060 -->

# VT Fuzz Corpus — R-001 (P0-AC-001/002)

## Status and provenance

- Status: **research** — produced by CTX-0088
  `ctx-0088/feat-vt-r001-verification` for the `Verified` gate on R-001.
  This document alone does not move R-001; the owning
  `docs/security/**` evidence-matrix / manual-audit report (Subagent B)
  carries the `Open → Mitigated` transition.
- Priority: P0 | Area: vt | Labels: feat,area:vt,P0 | Milestone: v0.1.0
  | RFC: OQ-007 | Task: CTX-0088
- Scope: `crates/bitty-vt/**`, `fuzz/corpora/**`, `tests/compat/**`
  (disjoint from `docs/security/**`). Scopes are
  `crates/bitty-vt/** + fuzz/corpora/** + tests/compat/**`.
- Earlier state: `crates/bitty-vt` unit 48 tests + harness 1 + replay 5,
  both `cargo check --workspace` hosts green, 14 seeds under
  `crates/bitty-vt/seeds/*.bin` with recorded sha256.
  Gap: no `fuzz/corpora/` directory, boundary-matrix coverage and explicit
  fuzz-corpus hash retention needed to reach `Verified`.

## Corpus retention (P0-AC-002 `adversarial: corpus retained in-repo`)

- Path: `fuzz/corpora/vt/` — 30 `*.bin` files, 35 655 B total on
  `ctx-0088/feat-vt-r001-verification` at seeding.
- Manifest: `fuzz/corpora/vt/SHA256SUMS` (`sha256sum *.bin | sort`),
  committed alongside the binaries so drift is diff-visible.
- Human-readable index: `fuzz/README.md` (per-file dimension → P0-AC →
  limit table).
- Verification:
  `sha256sum fuzz/corpora/vt/*.bin | diff -u fuzz/corpora/vt/SHA256SUMS -`
  must exit 0. CI can gate on this; the manifest is the content-addressable
  artifact cited by the risk-evidence RFC (`corpus hash`).

## Boundary matrix (P0-AC-001 `every limit has a named test`, RS-1..RS-7)

All of the following are exercised by **named unit tests** in
`crates/bitty-vt/src/parser.rs` under `#[cfg(test)] mod tests`, each
asserting parse-twice determinism and chunk-splitting identity
(`parse(bytes)` vs `byte-wise` vs `chunks(7)`), and zero panics/hangs:

| Limit                                   | vte ceiling                                         | Named test(s)                                                                                                                                                                                         |
| --------------------------------------- | --------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CSI numeric magnitude                   | `u16::MAX` via `saturating_*`                       | `csi_numeric_boundary_at_u16_max_saturates_deterministically` + legacy `huge_parameter_magnitude_saturates_deterministically`                                                                         |
| CSI param count (& subparams)           | `MAX_PARAMS = 32`                                   | `csi_param_count_at_and_beyond_max_truncates_deterministically` + `parameter_overflow_truncates_but_still_dispatches`                                                                                 |
| CSI intermediates                       | `MAX_INTERMEDIATES = 2`                             | `csi_intermediate_overflow_is_ignored_deterministically`                                                                                                                                              |
| OSC payload (vte raw)                   | `MAX_OSC_RAW = 1024`                                | `osc_payload_at_raw_and_bounded_caps_truncates_deterministically` (1024/1025/2048 at the raw seam)                                                                                                    |
| OSC/title + clipboard payload (Bounded) | `BoundedString/Bytes::MAX_LEN = 4096`               | `osc_payload_at_raw_and_bounded_caps_truncates_deterministically` + `osc_clipboard_payload_at_bounded_bytes_cap_truncates_deterministically` + `oversized_osc_payload_truncates_at_bound` (4096/5000) |
| Truncated ESC/CSI/OSC/DCS/APC           | parser state recovery                               | `truncated_escape_resynchronizes_deterministically`                                                                                                                                                   |
| Unterminated OSC/DCS vs APC/SOS/PM      | `OscString` vs `SosPmApcString` vs `DcsPassthrough` | `unterminated_osc_dcs_apc_strings_are_panic_free_and_deterministic` (OSC/DCS emit on `ST`/`BEL`; APC/PM/SOS stay inert even after `ST`, which is `vte::anywhere` semantics)                           |
| Invalid / heavy UTF-8 + split surrogate | `InvalidBytes` → `U+FFFD`                           | `invalid_utf8_heavy_is_replaced_and_deterministic` + `utf8_invalid_bytes_replace_with_fffd` + `utf8_split_across_chunks_continues_state_machine`                                                      |
| Malformed resync                        | `Ground`/`CsiEntry` re-entry                        | `malformed_sequences_resynchronize_deterministically`                                                                                                                                                 |
| Full adversarial soup                   | all of the above simultaneously                     | `boundary_matrix_zero_panics_all_limits` + `pseudo_random_byte_soup_is_panic_free_and_deterministic`                                                                                                  |

Collective threshold (`adversarial: zero panics/hangs across the full boundary
matrix`) is satisfied by the conjunction of these tests; the `boundary_matrix`
test re-proves it in a single corpus so CI need not infer it from the sum.

## Incremental chunking invariant (P0-AC-001/002 replay)

`action_stream_identical_across_chunkings` is preserved and extended:
`boundary_matrix_zero_panics_all_limits` asserts byte-wise vs bulk identity,
and every corpus file under `fuzz/corpora/vt/*.bin` is replayed both whole
and byte-by-byte in `crates/bitty-vt/tests/replay.rs` and
`crates/bitty-vt/tests/harness.rs` proxy paths — determinism is therefore
checked at three sites (parser lib, replay fixtures, compat harness).

## Deterministic truncation and char-boundary

`crates/bitty-vt/src/bounded.rs` `BoundedString::MAX_LEN = 4096`
truncation at char boundary is proved by
`bounded::tests::string_truncates_at_cap_on_char_boundary` (4-byte emoji
`\u{1F600}` repeated 2048 times collapses to exactly `MAX_LEN` with `% 4 == 0`)
plus `string_deterministic_truncation` (same over-cap input → same output).

## Seeds and compat lab linkage

- `crates/bitty-vt/seeds/*.bin`: 14 files remain canonical for
  `seeds_corpus_is_panic_free_and_deterministic` (≥10 asserted); mirrored
  into `fuzz/corpora/vt/01-14` so the fuzz retention subsumes the seed
  retention — the two hashes for `01–14` are byte-identical (`sha256sum`
  equality), proving copy fidelity.
- `tests/compat/**` corpora: `crates/bitty-vt/tests/harness.rs`
  `vt_corpus_bounded_and_deterministic_for_bitty_vt` exercises every
  `tests/compat/*/corpus/*.bin` (≥16 asserted) with the
  `MAX_CORPUS_BYTES = 8 KiB` / `MAX_ACTIONS = 4096` bounds and parse-twice
  oracle — bounded determinism is therefore proven at the compat-lab scale
  as well as at the parser scale.

## Why no `docs/security/**` edits here

`docs/security/**` (evidence-matrix R-001 row flip, manual-audit report)
belongs to Subagent B under the CTX-0088 disjoint scopes. This document
records the _product_ half of the evidence (corpus + boundary matrix) with
citables; the security half must cite this path and manifest rather than
duplicating corpora. Do not edit `docs/security/**` from this worktree
without commander scope expansion.

## How to re-verify

```sh
cargo test -p bitty-vt --all-targets --locked
sha256sum fuzz/corpora/vt/*.bin | diff -u fuzz/corpora/vt/SHA256SUMS -
cargo check --workspace --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked
```

All must be green before the security-auditor moves R-001. The model is
`Specified → Accepted → Implemented → Verified`; this corpus + matrix moves
the project from `Implemented` (tests existed) to `Verified` on R-001.
