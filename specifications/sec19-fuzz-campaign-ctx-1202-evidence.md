# SEC-19 bounded VT fuzz campaign evidence (CTX-1202; Issue #1088)

Source: `evidence-matrix.md` R-001 / P0-AC-002; `risk-register.md` R-001.
Branch: `ctx-1202/chore-sec19-phase-e`. No product code changed in this
task: 24 fuzzer-distilled regression seeds plus this evidence record only.
Risk states stay `Open`: this record is `Implemented`-only evidence pending
independent auditor review per RS-1..RS-7, plus the recorded `Open ->
Mitigated` matrix move in `bitty-docs` (out of scope for the `bitty`
repository). No control is weakened; no bypass, ambient authority, or
threshold change is included.

## Scope and contract

- Advances the R-001 P0-AC-002 long-running `cargo-fuzz` campaign item,
  named outstanding in every prior SEC-19 assessment (CTX-0701 verify-first
  record, CTX-0708, CTX-0723, CTX-0738): targets existed (`5daf686`, PR
  #1160, Issue #1132) with only bounded smokes run.
- This change runs a bounded, reported, reproducible campaign at HEAD
  `f31ab9cd` (300 s per target plus a retained 60 s confirmatory run),
  distills the resulting corpora with `cargo fuzz cmin`, and pins 24
  coverage-retained inputs as deterministic regression seeds that run in
  ordinary `cargo test` CI via the existing `replay.rs` /
  `harness.rs` gates.
- It does NOT complete P0-AC-002: the hours-long campaign plus auditor
  sign-off remain open and are recorded as residual work below.

## Method

Toolchain: `rustc 1.100.0-nightly (330d31712 2026-09-17)`,
`cargo-fuzz 0.13.2`, stable `1.98.1` for the repo build. Base revision
`f31ab9cd` (origin/main at campaign start; worktree clean before and
after apart from the 24 seeds plus this file). Each target ran with
`-workers=1 -print_final_stats=1` against its committed corpus under
`fuzz/corpora/`; new units landed in the committed corpus dirs during the
run and were moved out afterwards, so the only corpus delta in this change
is the 24 pinned seeds. Full worker logs for the confirmatory runs are
retained at `recording/ctx-1202-fuzz/*.log` (workspace scratch, not
committed); the stats below are quoted from the final libFuzzer lines.

Campaign commands (run from `fuzz/`):

- `cargo +nightly fuzz run vt_parser -- -max_total_time=300 -workers=1 -print_final_stats=1 ./corpora/vt_parser`
- `cargo +nightly fuzz run osc_string -- -max_total_time=300 -workers=1 -print_final_stats=1 ./corpora/osc_string`
- `cargo +nightly fuzz run dcs_apc_string -- -max_total_time=300 -workers=1 -print_final_stats=1 ./corpora/dcs_apc_string`
- `cargo +nightly fuzz run parser_to_state -- -max_total_time=300 -workers=1 -print_final_stats=1 ./corpora/vt`
- Distillation: per-target `cargo +nightly fuzz cmin <target>
<staged committed+new corpus>` on the same filesystem (libFuzzer merge
  requires it), then promotion of up to 6 retained new inputs per target
  that are non-empty, at most 8 KiB (`harness.rs MAX_CORPUS_BYTES`), and
  contain `ESC` (parser-relevant), smallest first.

## Results (300 s campaign, zero findings)

| Target            | Runs    | Exec/s | New units on disk | cmin kept (staged) | Peak RSS | Crashes / hangs |
| ----------------- | ------- | ------ | ----------------- | ------------------ | -------- | --------------- |
| `vt_parser`       | 117,949 | 391    | 1,725             | 1,409 / 1,756      | 443 MB   | 0               |
| `osc_string`      | 108,232 | 359    | 1,242             | 1,000 / 1,283      | 511 MB   | 0               |
| `dcs_apc_string`  | 45,956  | 152    | 1,026             | 826 / 1,046        | 683 MB   | 0               |
| `parser_to_state` | 3,561   | 11     | 575               | 499 / 606          | 365 MB   | 0               |

Total: 275,698 executions, 4,568 new coverage units, zero `crash-*` /
`timeout-*` / `oom-*` artifacts (artifact dirs verified empty after every
run; every run exited 0). `slowest_unit_time_sec: 0` on all targets: no
near-hang input observed. The `parser_to_state` rate is lower because each
execution applies up to 4,096 actions to a live `State` (debug invariants
on); that target is the only one covering `Parser -> State` application,
the others assert fresh-run determinism plus chunking identity per
`fuzz/fuzz_targets/common.rs`.

Confirmatory 60 s re-runs (same flags, logs retained, all exit 0, zero
artifacts): `vt_parser` 34,025 runs / 557 exec/s, `osc_string` 54,056 /
886, `dcs_apc_string` 10,581 / 173, `parser_to_state` 1,065 / 17.

## Pinned regression seeds (CI evidence)

24 files under `crates/bitty-vt/seeds/15-38-*.bin`, automatically covered
by `seeds_corpus_is_panic_free_and_deterministic` (`replay.rs`: fresh-run
determinism) and `vt_seeds_bounded_and_deterministic_for_bitty_vt`
(`harness.rs`: batched-vs-bytewise chunking identity plus the 8 KiB /
4,096-action bounds). Verified: `cargo test -p bitty-vt` 5 passed +
doctest 1 passed; `cargo test -p bitty-vt --test harness` passed.

| Seeds                         | Source target     | Sizes (bytes)          |
| ----------------------------- | ----------------- | ---------------------- |
| `15-20-fuzz-ctx1202-vtparser` | `vt_parser`       | 6, 6, 9, 9, 10, 11     |
| `21-26-fuzz-ctx1202-osc`      | `osc_string`      | 1, 3, 3, 3, 3, 4       |
| `27-32-fuzz-ctx1202-dcsapc`   | `dcs_apc_string`  | 5, 5, 6, 6, 6, 6       |
| `33-38-fuzz-ctx1202-ptostate` | `parser_to_state` | 15, 17, 18, 23, 23, 23 |

SHA-256 (short) of the pinned set: `4e6dd7bb` `f77f12d9` `a053d41d`
`4f51709a` `5ea69a85` `926b7502` (vtparser 15-20); `77adfc95` `5cad69b7`
`17af5e76` `0da98b74` `0e30a5a8` `e94b03eb` (osc 21-26); `2327c746`
`60e354fc` `5449ab0b` `609b0acc` `35da0423` `8ce2ae46` (dcsapc 27-32);
`a1318588` `c7978bb6` `0c33a7ad` `fac92a3d` `59c421a9` `1513889f`
(ptostate 33-38). Full hashes: `sha256sum crates/bitty-vt/seeds/15-*`
through `38-*` reproduces them.

## Residual work (not closed by this change)

1. P0-AC-002 long-running campaign: 300 s per target is bounded evidence,
   not the extended campaign the matrix requires. Still needed: a
   multi-hour run (plus `parser_to_state` at higher throughput or sharded
   workers) with its report, then auditor review.
2. Matrix move: R-001 stays `Open` until `bitty-docs`
   `docs/security/evidence-matrix.md` records the auditor-authorized `Open
-> Mitigated` transition (the `vt-parser-2026-09.md` audit at `8c41f1e`
   PR #130 authorizes only the move, per the matrix's own review gates).
   That edit lives in the `bitty-docs` repository and is out of scope here.
3. The 4,568 distilled-but-unpinned new units were not committed (raw
   fuzzer output stays out of the tree); the next campaign can re-derive
   coverage from the pinned seeds plus the committed `fuzz/corpora/` sets.

## Verification

- `cargo test -p bitty-vt` green (incl. the 24 new seeds through both
  corpus gates), `cargo +nightly fuzz build` green, all four campaign runs
  and all four confirmatory runs exit 0 with empty artifact dirs.
- `git status --short` shows only the 24 seeds plus this file; `fuzz/`
  working dirs (`corpus/`, `artifacts/`, staging) are gitignored or removed,
  and the incidental `fuzz/Cargo.lock` build churn was reverted.
- Headless only; no headed behavior changed, so no headed proof applies.
  MSRV 1.85 unaffected (no product code changed; seeds are data).
