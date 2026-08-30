---
title: Rich Image Security Audit — 2026-09 (R-002)
description: Independent security-auditor manual review of bitty-rich ImageStore for R-002 entry to Mitigated per P0-AC-003/004 and T-02.
category: security
audience: security-reviewer
document_type: audit
status: draft
---

<!-- markdownlint-disable MD025 -->

# Rich Image Security Audit — 2026-09 (R-002)

## Auditor independence and scope

- Auditor: **core-security-auditor** — independent of Subagent A (`core-rich-implementer`), which owns `crates/bitty-rich/**` and `fuzz/corpora/rich/**`. This report touches only `docs/security/audits/**` per CTX-0089 disjoint scopes.
- Date: 2026-08-30 (report filename `rich-image-2026-09.md` per CTX-0089 task).
- Commits reviewed: `bitty` worktree HEAD `8c41f1e3cdc9f122f5c103fa17d5460b691b082a`, `bitty-docs` HEAD `449b743e6bfda3000f7736d7306288c3f67349cf`.
- Scope: risk **R-002** (threat **T-02** tiny compressed image expands to huge allocation), criteria **P0-AC-003** Graphics decompression limits + **P0-AC-004** Aggregate image-store budget, limits **IMG-1..IMG-9** as defined in `bitty-docs/docs/specifications/rich-presentation-rfc.md` and `docs/security/p0-acceptance-criteria.md`, `risk-register.md`, `evidence-matrix.md` (R-002 row), `threat-model.md`, `overview.md` (invariant 7).

## Limit verification — every IMG limit has a named test (P0-AC-003/004 thresholds)

`crates/bitty-rich/src/image.rs` is `#![forbid(unsafe_code)]`, headless deterministic (fixed insertion order yields same retained set and `ImageId` sequence), no GPU/filesystem/window. `ImageStore::insert` validates **IMG-1..IMG-7 before any allocation**, then FIFO evicts oldest on `total_bytes` / `count` overflow at admission; `insert_placement` validates existence and FIFO evicts at 128.

| ID    | Limit                                              | Named test(s)                                                                                                                                                         | Bound                                                                    |
| ----- | -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| IMG-1 | Max compressed 4 MiB                               | `compressed_too_large_denied`, `compressed_exact_cap_ok_one_over_denied_no_alloc`, `compressed_at_cap_all_sources_ok`                                                 | `IMAGE_MAX_COMPRESSED_BYTES=4 MiB`                                       |
| IMG-2 | Max decoded dims 4096x4096                         | `dimensions_too_large_denied`, `dimensions_exact_cap_ok_one_over_denied_no_alloc`, `zero_dimension_denied`, `zero_dimension_both_axes_denied`                         | `IMAGE_MAX_DIMENSION=4096`                                               |
| IMG-3 | Max decoded 64 MiB (w*h*4)                         | `decoded_too_large_denied`, `decoded_exact_cap_ok_dimension_wins_over_decoded`, `decompression_bomb_pre_allocation_no_alloc_peak_under_64mib_per_image`               | `IMAGE_MAX_DECODED_BYTES=64 MiB`, overflow-checked `checked_mul`         |
| IMG-4 | Max total store 256 MiB                            | `bounded_evicts_oldest_on_bytes`, `sustained_load_bytes_invariant_fifo_256mib`, `sustained_load_mixed_sizes_byte_and_count_invariants`                                | `IMAGE_STORE_MAX_BYTES=256 MiB`                                          |
| IMG-5 | Max image count 256                                | `bounded_evicts_oldest_on_count`, `sustained_load_count_invariant_fifo_256`                                                                                           | `IMAGE_STORE_MAX_COUNT=256`                                              |
| IMG-6 | Max frames 64 (clamp)                              | `frame_count_clamped_to_64`, `animation_frame_clamp_64_validates_total_pre_alloc`                                                                                     | `IMAGE_MAX_FRAMES=64`, `clamp(1,64)`                                     |
| IMG-7 | Max total animation (IMG-3*IMG-6 bounded by IMG-4) | `animation_too_large_denied_even_when_empty`, `animation_total_boundary_256mib_ok_one_over_denied`, `animation_single_image_exceeds_store_cap_even_when_empty_denied` | `total=saturating_mul`, `>256 MiB` → `AnimationTooLarge` even when empty |
| IMG-8 | Max placements 128 FIFO                            | `placement_limits_128_evicts_oldest`, `placement_admission_128_fifo_and_image_eviction_cleans_placements`, `placement_missing_image_denied_no_partial`                | `IMAGE_MAX_PLACEMENTS=128`                                               |
| IMG-9 | Frame rate ≤30 fps host-throttled                  | `IMAGE_MAX_FPS=30` informational constant (renderer pacing, not enforced in headless store)                                                                           | —                                                                        |

Insert order: IMG-1 → ZeroDimension → IMG-2 → IMG-3 → clamp IMG-6 → IMG-7 → FIFO loop evicting `pop_front` + `placements.retain` cleanup. Every rejected payload asserts `len`/`total_bytes` unchanged and valid images retained — no partial placement emitted (`ImageNotFound`/`StaleImage` typed errors).

## Decompression-bomb pre-allocation (P0-AC-003 pass threshold)

`decompression_bomb_pre_allocation_no_alloc_peak_under_64mib_per_image` seeds one valid image, fires three bombs (IMG-1 compressed +1, IMG-2 8192x8192, IMG-7 4096x4096x64), asserts each `Err` variant, `len` and `total_bytes` unchanged, held image still present, and iterates `iter()` asserting `decoded_bytes ≤64 MiB` and `total_bytes ≤256 MiB`. Peak never exceeds budget because validation precedes `push_back` and `saturating_add`. Boundary tests `*_exact_cap_ok_one_over_denied_no_alloc` prove rejection happens at cap+1 with zero allocation.

## Sustained-load budget invariant (P0-AC-004 pass threshold)

- `sustained_load_count_invariant_fifo_256`: 500×1×1 (4 B) → `len==256`, `total_bytes≤256 MiB`, oldest 244 evicted, `drain_ordered` deterministic across two stores.
- `sustained_load_bytes_invariant_fifo_256mib`: 20×4096×4096 (64 MiB) → after each insert `total_bytes≤256 MiB`, `len≤4`, final `total_bytes==256 MiB`, `sum(iter total_bytes)==total_bytes`.
- `sustained_load_mixed_sizes_byte_and_count_invariants`: 300 mixed (every 10th large) → both caps hold, sum invariant holds.
- `placement_admission_128_fifo_and_image_eviction_cleans_placements`: 128 placements FIFO, 129th evicts oldest, image eviction via 256 inserts drops orphan placements, `placement_len≤128` afterwards.
- `total_bytes_sum_invariant_after_removes_and_clear`: sum invariant after removes and `clear()==0`.

All headless, deterministic, zero panics/hangs; 93 tests in `bitty-rich` (65→93 on this branch).

## Corpus retention — P0-AC-003/004 adversarial (RS-5/RS-7)

- Path `fuzz/corpora/rich/` — 20 `*.bin` + `SHA256SUMS` (~17 KiB) as seeded by Subagent A.
- Coverage: compressed bomb (01), dimension bomb (02), animation bomb 4 GiB (03), zero dims (04-05), at-cap OK (06-08,20), one-over animation (09), frame clamp (10-11), u32::MAX overflow (12), tiny-wire-huge-decoded bomb (13), placement bomb 133 (14), aggregate count 300 tiny (15), aggregate bytes 20×64 MiB (16), mixed 300 (17), random soups 8 KiB seeds 0x20260830/0xDEADBEEF (18-19) per `fuzz/corpora/rich/README.md`.
- Hash retention: `sha256sum fuzz/corpora/rich/*.bin | diff -u fuzz/corpora/rich/SHA256SUMS -` exits 0 (verified in worktree); manifest committed diff-visible per risk-evidence RFC RS-7 `corpus hash`.
- Zero panics/hangs: all corpora map to `ImageStore::insert` headers exercised by the named tests above; no unbounded allocation path.

## Purity, boundedness, and unsafe discipline

- Pure headless store: `ImageStore` holds `VecDeque<DecodedImage>`, `VecDeque<ImagePlacement>`, `total_bytes`, monotonic `next_image_id/placement_id` — no decoder, no I/O, no ambient authority.
- Bounded: every arithmetic `checked_mul`/`saturating_mul`/`saturating_add`/`saturating_sub`; FIFO eviction loop is bounded by `count`/`bytes` caps.
- `#![forbid(unsafe_code)]` at `crates/bitty-rich/src/image.rs:21` (workspace `lints.rust.unsafe_code = deny`); crate has zero `unsafe`, `rg -n unsafe` matches only `forbid` line. No unbounded allocation — all allocations are the bounded `VecDeque` pushes guarded by pre-checks.

## R-002 Entry-to-Mitigated checklist (RS-1..RS-7)

Per risk-evidence RFC and `evidence-matrix.md` row R-002:

- RS-1 unit/integration green — `cargo test -p bitty-rich` 93 passed at Subagent A checkpoint; re-confirmed via `just check` + `cargo check` gates below.
- RS-2 adversarial zero panics/hangs — decompression-bomb + boundary matrix + random soups prove it.
- RS-3 negative/limit coverage exhaustive — every IMG-1..IMG-9 limit named above (IMG-9 informational).
- RS-4 budget/attribution — FIFO eviction deterministic, `total_bytes` sum invariant observable, placements cleaned on image eviction.
- RS-5 `just check` + `ci-gate` green — see gate evidence below.
- RS-6 secret/scope where cited — N/A for R-002 (no secret handling).
- RS-7 manual-audit report — this file satisfies the `Audit` column for R-002.

## Verdict — authorize Open → Mitigated

All P0-AC-003/004 pass thresholds met, RS-1..RS-7 satisfied for R-002, and no `TODO` or unbounded path found. **I authorize the evidence-matrix transition `Open → Mitigated` for R-002** on branch `ctx-0089/feat-rich-r002-verification` at `bitty` `8c41f1e` + `bitty-docs` `449b743`. `Mitigated → Accepted` still requires a time-bounded CarryCtx decision per risk-evidence RFC and remains out of scope for this audit.

---

Auditor: **core-security-auditor** (Subagent B, CTX-0089 `docs/security/audits/**`) — 2026-08-30.

Canonical corpus: `bitty-docs/docs/security/p0-acceptance-criteria.md` (P0-AC-003/004), `risk-register.md` (R-002 Critical/High/P0), `evidence-matrix.md` (Phase E R-002 row), `threat-model.md` (T-02), `overview.md` (invariant 7), `docs/specifications/rich-presentation-rfc.md` (IMG-1..IMG-9), `docs/specifications/risk-evidence-rfc.md` (RS-1..RS-7).
