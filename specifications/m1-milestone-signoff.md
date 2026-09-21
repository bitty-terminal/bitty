# M1 evidence-slice sign-off (narrowed, CTX-0617)

> Status: **narrowed independent-reviewer sign-off** for Issue #1138
> (`M1-12`), Task CTX-0617. It signs off the M1-03..M1-11 evidence slice
> only. It does **not** declare M1 / epic #969 complete: 13 sibling M1
> sub-issues remain open (listed below), so full-milestone completion is
> explicitly withheld.

## Scope

- Issue: `bitty-terminal/bitty#1138` (M1-12, P0, milestone v0.1.0).
- Epic: `bitty-terminal/bitty#969` (M1 / v0.1 correctness).
- Normative source: `compatibility-milestone-rfc.md`, Acceptance evidence
  requirements, evidence rule 3 (independent reviewer sign-off).
- Declared dependencies: M1-03..M1-11. All nine are CLOSED; each closed
  through a merged PR with committed artifacts (table below).

## Dependency evidence (all CLOSED, merged to `origin/main`)

| Item | Issue | Delivery (CTX / PR / commit) | Committed artifact |
| ---- | ----- | ---------------------------- | ------------------ |
| M1-03 DECSET 2026 end-to-end | #1128 | CTX-0570 / #1169 / `dbb72db` | `crates/bitty-runtime/tests/m1_synchronized_update.rs` |
| M1-04 OSC 10/11 round trip | #1129 | CTX-0570 / #1169 / `dbb72db` | `crates/bitty-runtime/tests/m1_osc_color.rs` |
| M1-05 OSC 0/2 window title | #1131 | CTX-0570 / #1169 / `dbb72db` | `crates/bitty-runtime/tests/m1_color_title.rs` |
| M1-06 fuzz targets VT/OSC/DCS/APC | #1132 | CTX-0568 / #1160 / `5daf686` | `fuzz/fuzz_targets/` (`vt_parser`, `osc_string`, `dcs_apc_string`, `common`) plus retained corpora and `fuzz/README.md` |
| M1-07 differential oracle corpus | #1133 | CTX-0573 / #1171 / `6ed5362` | `crates/bitty-compat-lab/src/oracle.rs`, `src/bin/oracle_runner.rs`, `tests/compat/oracle/scenarios/` |
| M1-08 mode/input goldens | #1134 | CTX-0571 / #1170 / `abb318b` | `crates/bitty-compat-lab/tests/m1_mode_golden.rs` |
| M1-09 color/title snapshots | #1135 | CTX-0571 / #1170 / `abb318b` | `crates/bitty-compat-lab/tests/m1_color_golden.rs` |
| M1-10 Tier 1 matrix in CI | #1136 | CTX-0574 / #1172 / `3fc6014` | `scripts/m1-matrix.sh`, `m1-matrix` aggregate job in `.github/workflows/ci.yml` (linux-x11, linux-wayland, macos, windows) |
| M1-11 throughput baseline | #1137 | CTX-0576 / #1174 / `da43f45` | `crates/bitty-perf/baselines/parser-throughput.json`, `parser_throughput_regression` test |

## Verification performed (this sign-off session, Linux x86_64)

Commands run in the `ctx-0617/m1-signoff` worktree; every suite green:

| Command | Result |
| ------- | ------ |
| `cargo test -p bitty-compat-lab --test m1_mode_golden --test m1_color_golden` | 10 + 7 passed, 0 failed |
| `cargo test -p bitty-runtime --test m1_mode_input --test m1_color_title --test m1_osc_color --test m1_synchronized_update` | 3 + 3 + 5 + 6 passed, 0 failed |
| `cargo test -p bitty-compat-lab --test oracle` | 10 passed, 0 failed |
| `cargo test -p bitty-runtime --test m1_shell_coverage` | 16 passed, 0 failed |
| `cargo test -p bitty-perf --test parser_throughput_regression` | 4 passed, 0 failed |
| `bash scripts/tests/m1-matrix.test.sh` | OK |

Total: 65 tests green plus the matrix-driver fixture test. Tier 1
cross-platform proof beyond Linux rests on the CI `m1-matrix` aggregate
job, which was not re-run from this session.

## Remaining gates (full-milestone completion withheld)

1. **Open M1 sub-issues (13):** M1-14 (#1140), M1-15 (#1141), M1-18
   (#1144), M1-20 (#1146), M1-21 (#1147), M1-22 (#1148), M1-23 (#1149),
   M1-24 (#1150), M1-28 (#1154), M1-29 (#1155), M1-30 (#1156), M1-31
   (#1157), M1-32 (#1158). Each must close with its own evidence before
   epic #969 can be declared complete.
2. **Fuzz re-run automation:** evidence rule 2 requires fuzzing and
   differential-corpus evidence to re-run on every parser change. The
   differential side is covered by the CI `m1-matrix` job; recurring fuzz
   has no CI wiring and is tracked separately (CTX-0640 / #1066).
3. **Tier 1 CI confirmation:** this session verified Linux locally; the
   macos/windows/linux-wayland legs are proven only by the CI
   `m1-matrix` aggregate on the merge commits above.

## Sign-off

The M1-03..M1-11 evidence slice meets the RFC acceptance-evidence shape
for its rows (committed goldens, oracle corpus, fuzz targets with
retained seeds, Tier 1 matrix wiring, committed throughput baseline)
and re-verifies green locally. Full M1 milestone sign-off is **not
granted** until the remaining gates above close.
