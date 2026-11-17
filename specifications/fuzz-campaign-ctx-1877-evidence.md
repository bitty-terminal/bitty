# Fuzz campaign machinery and runbook (CTX-1877; bitty#3227)

Risk R-001 / acceptance criterion P0-AC-002 requires a long-running fuzz
campaign over the VT parser family with zero crashes, hangs, or memory-safety
findings. The cargo-fuzz targets (`fuzz/fuzz_targets/`) and the retained
corpora (`fuzz/corpora/`) already existed, and SEC-19 recorded a hand-run
300 s-per-target campaign; what was missing was the machinery that makes a
campaign repeatable: a scheduled run, one budget rule, and an evidence record a
security review can cite.

This document is the runbook for that machinery plus the measured run that
validated it. It lands in this repository only; the canonical security corpus
and the R-001 risk-state move stay with the separate `bitty-docs` task. It does
**not** claim P0-AC-002 closure: the acceptance also needs a campaign recorded
against a named revision and independent auditor review (see "How this feeds
P0-AC-002").

## What was added

| Surface                                            | Role                                                                                                                                                                |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scripts/fuzz-campaign.sh`                         | Bounded campaign driver: seeds the scratch corpus, runs libFuzzer per target, classifies artifacts, writes the evidence record, exits non-zero on a finding         |
| `.github/workflows/fuzz-campaign.yml`              | Nightly `schedule` plus `workflow_dispatch`; enumerated target matrix, nightly toolchain, version-pinned cargo-fuzz, corpus cache, artifact and summary publication |
| `scripts/tests/fuzz-campaign.test.sh`              | Stubbed-`cargo` regression test for plan, clamp, evidence schema, finding classes, and the "not a PR gate" wiring                                                   |
| `justfile` (`fuzz-campaign`, `fuzz-campaign-test`) | Owner-facing long run; the test is part of `just check`                                                                                                             |
| `fuzz/README.md` ("Campaign")                      | Points the target-crate reader at the driver and the nightly workflow                                                                                               |

The nightly workflow is deliberately **not** part of the PR-required check set:
`ci.yml` and branch protection are untouched. A multi-minute campaign on every
push would spend review budget without new signal, and the driver test above
fails if `ci.yml` ever starts referencing the campaign.

## Running a campaign

Local, owner-driven:

```sh
just fuzz-campaign 600                    # every target, 600 s each
just fuzz-campaign 3600 --targets vt_parser
just fuzz-campaign 120 --out-dir recording/fuzz-campaign/owner
just fuzz-campaign 600 --generate-cap-seeds
```

Prerequisites are nightly and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install --locked --version 0.13.2 cargo-fuzz
```

`scripts/fuzz-campaign.sh --dry-run` prints the resolved plan (targets, budget,
libFuzzer flags, corpus sources, evidence directory) without touching the
toolchain, which is also how the regression test asserts the plan.

CI, scheduled or manual:

- Nightly at 04:47 UTC, 600 s per target.
- `workflow_dispatch` takes `duration_secs` (10..1800). The 1800 s ceiling plus
  the driver's 300 s hard-kill slack, a cold fuzz build, and one-time cap-seed
  generation fit the job's 45-minute timeout; the accounting is in the workflow.
- Each target runs in its own matrix job, so one target's failure does not erase
  the others' evidence (`fail-fast: false`).

Bounds owned by the driver: `--secs` 10..86400, `-timeout` 1..600 (default 25),
`-rss_limit_mb` 256..16384 (default 2048), `-workers=1`. The scratch corpus
`fuzz/corpus/<target>` is trimmed to 2000 units per target after a run so a
persisted corpus cache stays bounded; the committed corpora are never modified.

Seeds come from `fuzz/corpora/<target>` when that directory exists;
`parser_to_state` replays the canonical R-001 corpus `fuzz/corpora/vt`
(`vt_parser` holds a byte-identical copy); `control_string_cap` has no committed
seeds because its inputs are at and past the ~4 MiB parser cap, so the driver
generates them through the ignored `generate_cap_seeds` test when asked and
passes `-max_len=4198432` for that target only.

Exit codes: `0` every target clean, `1` at least one crash, OOM, or timeout,
`2` usage error, `3` environment or runner error (toolchain or cargo-fuzz
missing, build failure, wrapper timeout) with no finding recorded.

## Evidence record

Every run writes, under the evidence directory (`recording/fuzz-campaign/`
locally, a per-target upload in CI):

- `summary.json` — the machine-readable record.
- `summary.md` — the same record as a table plus a findings list; CI also posts
  it to the run's step summary.
- `targets.tsv` — the raw per-target rows the two summaries are rendered from.
- `logs/<target>.log` — the complete libFuzzer output (the caller sees the tail
  only).
- `artifacts/<target>/` — any `crash-*`, `oom-*`, `timeout-*`, `slow-unit-*`, or
  `leak-*` file a run added, copied out of the gitignored `fuzz/artifacts/`.

`summary.json` fields that matter to a review:

| Field                                                     | Meaning                                                                                      |
| --------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `revision`, `revision_dirty`                              | Commit the campaign ran against; a dirty tree is recorded, not hidden                        |
| `toolchain`, `cargo_fuzz`                                 | `rustc` and `cargo-fuzz` versions actually used                                              |
| `libfuzzer`                                               | `-max_total_time`, `-timeout`, `-rss_limit_mb`, `-workers` for the run                       |
| `targets[].secs_fuzzed`                                   | libFuzzer's own `Done <runs> runs in <secs> second(s)` budget                                |
| `targets[].secs_actual`                                   | Wall clock of the whole per-target step, including its build                                 |
| `targets[].runs`, `exec_per_sec`, `new_units`             | Executions and coverage the budget bought                                                    |
| `targets[].corpus_seeds`, `corpus_delta`                  | Seed units at start; units the run added before the retention trim                           |
| `targets[].peak_rss_mb`                                   | Peak resident set reported by libFuzzer                                                      |
| `targets[].crashes`, `oom`, `timeouts`, `other_artifacts` | Finding classes counted from artifacts the run itself added (pre-existing files are ignored) |
| `targets[].exit_code`, `result`                           | Process status and `clean` / `finding` / `error`                                             |
| `totals`, `result`                                        | Whole-campaign roll-up; one `result` decides a run page                                      |

No host path is recorded, so a summary is safe to attach to a security-corpus
entry as-is. The `report` job merges the per-target records into one
`fuzz-campaign-summary-<run id>` artifact for the whole run.

## Measured run (this change)

Machinery smoke on the authoring host: Linux x86_64, 24 hardware threads,
`cargo 1.100.0-nightly`, `cargo-fuzz 0.13.2`, budget 60 s per target, target
cache outside the worktree.

```sh
CARGO_TARGET_DIR=<task cache> just fuzz-campaign 60 --generate-cap-seeds
```

| Target               | Secs fuzzed | Runs    | Exec/s | New units | Seeds | Peak RSS (MB) | Crashes | OOM | Timeouts |
| -------------------- | ----------- | ------- | ------ | --------- | ----- | ------------- | ------- | --- | -------- |
| `vt_parser`          | 61          | 39,132  | 641    | 800       | 2,030 | 653           | 0       | 0   | 0        |
| `osc_string`         | 61          | 73,834  | 1,210  | 1,753     | 592   | 543           | 0       | 0   | 0        |
| `dcs_apc_string`     | 61          | 25,872  | 424    | 1,486     | 21    | 647           | 0       | 0   | 0        |
| `control_string_cap` | 63          | 98      | 1      | 20        | 12    | 461           | 0       | 0   | 0        |
| `parser_to_state`    | 61          | 2,544   | 41     | 536       | 30    | 392           | 0       | 0   | 0        |
| **total**            | 307         | 141,480 | –      | 4,595     | –     | 653           | 0       | 0   | 0        |

Result `clean`, exit 0: no `crash-*`, `oom-*`, `timeout-*`, `slow-unit-*`, or
`leak-*` artifact, `slowest_unit_time_sec: 0` on every target. The
`vt_parser` seed count is high because the run resumed a scratch corpus left by
an aborted attempt, which also exercised the retention trim (766 oldest units
removed, 2000 kept). `control_string_cap` executes slowly by design: its inputs
are megabytes at the parser cap, so 98 executions in 63 s is the expected shape,
not a stall.

This is a machinery record, **not** campaign evidence for P0-AC-002: 60 s per
target is a smoke budget. The 300 s-per-target SEC-19 record
(`sec19-fuzz-campaign-ctx-1202-evidence.md`) remains the longest reported run
until the nightly has accumulated real history or an owner campaign runs for
hours.

## How this feeds P0-AC-002

The criterion asks for a long-running campaign with zero crashes, hangs, or
memory-safety findings. The machinery supplies the two things a reviewer needs
to accept that:

1. **Repeatability** — the same driver, flags, and budget on every run, against
   the enumerated target set, so a later claim is comparable to an earlier one
   instead of being a one-off host measurement. A new target in
   `fuzz/Cargo.toml` joins the matrix without an edit here.
2. **A citable record** — one `summary.json` per run naming the revision,
   toolchain, `cargo-fuzz` version, budget, executions, and finding counts, plus
   the raw logs and any artifact. A clean nightly is `result: clean` with
   `crashes`, `oom`, `timeouts` all zero for every target.

What still has to happen for closure, and who owns it:

- An owner campaign long enough to count as "long-running" (hours, not
  minutes), run at a named revision, with the resulting `summary.json` recorded
  in the canonical `bitty-docs` security corpus by the task that owns it.
- Independent auditor review of that record under the risk-evidence rules
  (RS-1..RS-7) — the same gate every earlier R-001 record went through.
- A finding is not a closure blocker left in place: it is fixed, then the
  minimized input (`cargo +nightly fuzz tmin <target> <artifact>`) is committed
  to `fuzz/corpora/<target>/` with its coverage-table row and manifest regen per
  `fuzz/README.md`, so ordinary `cargo test` covers the regression after the
  campaign that found it is long gone.

## Cost, risk, and known limits

- **CI cost.** Five matrix jobs per night: one fuzz build each, a cached
  cargo-fuzz binary, and 600 s of fuzzing by default. The 1800 s dispatch
  ceiling keeps a manual run inside the 45-minute job timeout; it is a ceiling,
  not a default.
- **Corpus cache growth.** The corpus cache key includes `github.run_id`, so it
  adds an entry per target per night and the newest matching entry wins on
  restore. Unit trimming bounds each entry; the discovered corpus is also
  uploaded as an artifact so a failed run still leaves its coverage behind. The
  committed `fuzz/corpora/` remains the canonical seed set.
- **Nightly churn.** The toolchain is not date-pinned, so the fuzz build is
  usually cold. That is a deliberate trade: the fuzz crate should track current
  nightly, and the evidence records which nightly ran.
- **Flakiness.** `-timeout=25` is far above the observed `slowest_unit_time_sec:
0`; a `timeout-*` artifact on a loaded runner is still treated as a finding,
  which is the correct fail-closed direction. Do not relax the timeout to hide
  one: investigate the input instead.
- **`act` and the dynamic matrix.** `act -n --list` resolves the workflow, and
  `act -n ... -j targets` dry-runs the enumeration job, but `act` cannot
  evaluate a matrix built from `needs.<job>.outputs`, so the campaign job is
  syntax-checked by `actionlint` and by a real `workflow_dispatch` rather than by
  an `act` dry-run. `just ci-local` still covers the `Quality gates` job that
  `just check` (including `fuzz-campaign-test`) runs in CI.
- **No coverage gate.** The criterion is zero findings, not a coverage
  threshold; `cov`/`ft` lines stay in the logs for reading, and no numeric
  coverage target is asserted.

## Gates for this change

- `actionlint -color .github/workflows/fuzz-campaign.yml` — clean.
- `act -n --workflows .github/workflows/fuzz-campaign.yml --list` — three jobs
  (`targets`, `campaign`, `report`); `act -n ... workflow_dispatch -j targets
-P ubuntu-24.04=catthehacker/ubuntu:act-24.04` — job succeeds in dry run.
- `shellcheck -S warning scripts/fuzz-campaign.sh
scripts/tests/fuzz-campaign.test.sh` and `shfmt -d` — clean.
- `just fuzz-campaign-test` — PASS, and wired into `just check`.
- The measured run above — `result: clean`, exit 0.

## Open points

- Whether the nightly budget should grow (for example a weekly 1800 s run on
  top of the 600 s nightly) depends on CI cost the owner has not yet budgeted.
- The canonical security-corpus entry for future campaign records, and the
  R-001 risk-state move, belong to the `bitty-docs` task that owns the corpus.
- Corpus-cache retention: if artifact and cache storage becomes a cost problem,
  the next step is to persist corpora weekly instead of nightly rather than to
  drop the corpus upload on failure.
