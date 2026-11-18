# Fuzz campaign machinery and runbook (CTX-1877; bitty#3227; evidence completeness CTX-1880; bitty#3235)

Risk R-001 / acceptance criterion P0-AC-002 requires a long-running fuzz
campaign over the VT parser family with zero crashes, hangs, or memory-safety
findings. The cargo-fuzz targets (`fuzz/fuzz_targets/`) and the retained
corpora (`fuzz/corpora/`) already existed, and SEC-19 recorded a hand-run
300 s-per-target campaign; what was missing was the machinery that makes a
campaign repeatable: a scheduled run, one budget rule, and an evidence record a
security review can cite. CTX-1880 then closed the gap between "the record has
finding counts" and "the record is evidence": it now carries the sanitizer
build and its flags, a corpus digest artifact, and a bounded coverage signal,
and a run that cannot state them is `inconclusive` rather than `clean` (see
"Evidence completeness").

This document is the runbook for that machinery, plus the CTX-1877 60 s smoke
campaign retained below as history. It lands in this repository only; the
canonical security corpus and the R-001 risk-state move stay with the separate
`bitty-docs` task. It does **not** claim P0-AC-002 closure: the acceptance also
needs a campaign recorded against a named revision and independent auditor
review (see "How this feeds P0-AC-002"). The CTX-1880 change that made the
provenance fields mandatory was authored on a host with no nightly toolchain and
reports no campaign of its own: the historical run below predates every field
that turns a record into evidence, and the current rules are pinned by the
stubbed driver test instead.

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
  generation fit the job's 60-minute timeout; the accounting is in the workflow.
- Each target runs in its own matrix job, so one target's failure does not erase
  the others' evidence (`fail-fast: false`).

Bounds owned by the driver: `--secs` 10..86400, `-timeout` 1..600 (default 25),
`-rss_limit_mb` 256..16384 (default 2048), `-workers=1`. The scratch corpus
`fuzz/corpus/<target>` is trimmed to 2000 units per target after a run so a
persisted corpus cache stays bounded; the committed corpora are never modified.
The driver runs each target in its own session under `timeout`, and traps
INT/TERM/HUP to terminate that session and remove its work directory, so an
interrupted owner campaign leaves no orphaned fuzzer.

Every artifact class is a finding: `crash-*`, `leak-*` (LSan memory safety),
`slow-unit-*` (the hang class), `oom-*`, `timeout-*`, and any unclassified
artifact. libFuzzer's own error exit (`77` by default) is a finding even when no
artifact survived, because that is how it reports an abort; any other non-zero
exit is a runner/build `error`, and a run that exits 0 without executing
anything is `inconclusive` — never `clean`.

**Artifact state assumed at campaign start: the target's artifact directory is
empty.** That is `fuzz/artifacts/<target>/` by default, or
`<--artifacts-dir>/<target>/`. The driver does not clear that directory — doing
so would delete artifacts an earlier run left as evidence — and it does not
trust a name diff alone. libFuzzer derives an artifact name from the input that
produced it (`leak-<sha1>`, `timeout-<sha1>`, `crash-<sha1>`), so a run that
re-discovers an input a previous campaign, a local `just fuzz`, or a shared
cache left behind rewrites the same file and adds no *new* name. Any artifact
present for the target after the run is therefore a finding, regardless of
whether this run wrote it, and `clean` is not recorded while one is there. The
record names what it saw in `pre_existing_artifacts` and
`pre_existing_artifact_names`, and the per-class counters stay at zero because
they count only what the run itself added — a target in that state reports
`finding` with nothing new, which is the guard firing, not a second defect. CI
meets the assumption by construction (a fresh checkout per matrix job); a local
run meets it by pointing `--artifacts-dir` at the evidence directory of that
particular run, or by triaging a finding and clearing the directory before the
next campaign. Clearing is a deliberate act: triage first
(`cargo +nightly fuzz tmin <target> <artifact>`), then remove the file, so an
artifact is never dropped unseen.

Seeds come from `fuzz/corpora/<target>` when that directory exists;
`parser_to_state` replays the canonical R-001 corpus `fuzz/corpora/vt`
(`vt_parser` holds a byte-identical copy); `control_string_cap` has no committed
seeds because its inputs are at and past the ~4 MiB parser cap, so the driver
generates them through the ignored `generate_cap_seeds` test when asked and
passes `-max_len` for that target only. That value is not a driver literal: the
driver reads `fuzz/control_string_cap.max_len`, whose single owner is
`CONTROL_STRING_MAX_BYTES` in `crates/bitty-vt/src/parser.rs` plus the generator
headroom in `FUZZ_MAX_LEN`; the generator test
(`control_string_cap::fuzz_max_len_file_matches_generator`) and this repo's
driver test both fail on drift, so the campaign cannot fuzz a bound different
from the one it records.

Exit codes: `0` every target clean, `1` at least one finding (crash, leak, slow
unit, OOM, timeout, unclassified artifact, or an artifact already in the
target's artifact directory when the run started), `2` usage error, `3`
environment or runner error (toolchain or cargo-fuzz missing, build failure,
wrapper timeout) with no finding recorded, `4` inconclusive record (a target
executed zero iterations, or the run could not state its sanitizer build, its
corpus digest, or its coverage counters — see "Evidence completeness").

## Evidence record

Every run writes, under the evidence directory (`recording/fuzz-campaign/`
locally, a per-target upload in CI):

- `summary.json` — the machine-readable record.
- `summary.md` — the same record as a table plus a findings list; CI also posts
  it to the run's step summary.
- `targets.tsv` — the raw per-target rows the two summaries are rendered from.
- `logs/<target>.log` — the complete libFuzzer output (the caller sees the tail
  only).
- `corpus/<target>.sha256` — the corpus digest artifact: one line,
  `<sha256>  <target>.corpus  units=<n>  algorithm=sha256`, over the name-sorted
  per-unit manifest the run ended with (before the retention trim). A missing
  digest blocks `clean`.
- `artifacts/<target>/` — any `crash-*`, `oom-*`, `timeout-*`, `slow-unit-*`, or
  `leak-*` file a run added, copied out of the gitignored `fuzz/artifacts/`. An
  artifact that was already there when the run started is named in
  `summary.json` and `summary.md` instead of being copied: it is not this run's
  evidence, only the reason the target cannot be called `clean`.

`summary.json` fields that matter to a review:

| Field                                                     | Meaning                                                                                      |
| --------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `revision`, `revision_dirty`                              | Commit the campaign ran against; a dirty tree is recorded, not hidden                        |
| `toolchain`, `cargo_fuzz`                                 | `rustc` and `cargo-fuzz` versions actually used                                              |
| `sanitizer.name`, `.cargo_fuzz_flag`, `.instrumentation`  | The sanitizer build: the forwarded `--sanitizer=<name>`, its `-Zsanitizer=` instrumentation, and the leak-detection runtime it implies |
| `sanitizer.leak_detection`, `.leak_detection_source`      | The leak-checking runtime the build is *intended* to use (`lsan` for ASan, `none` otherwise) — intent, not proof that a check ran |
| `sanitizer.leak_detection_effective`, `.leak_detection_disabled_by` | Whether the leak check actually runs, and the inherited option that would disable it (`ASAN_OPTIONS=detect_leaks=0` / `LSAN_OPTIONS=...`); only `true` makes `leaks: 0` an evidence claim |
| `libfuzzer`                                               | `-max_total_time`, `-timeout`, `-rss_limit_mb`, `-workers`, `-detect_leaks` for the run      |
| `targets[].secs_fuzzed`                                   | libFuzzer's own `Done <runs> runs in <secs> second(s)` budget                                |
| `targets[].secs_actual`                                   | Wall clock of the whole per-target step, including its build                                 |
| `targets[].runs`, `exec_per_sec`, `new_units`             | Executions and new units the budget bought                                                   |
| `targets[].coverage_edges`, `.coverage_features`          | libFuzzer's own `cov` (edges) and `ft` (frontier features) counters; `null` when the log carried none |
| `targets[].corpus_seeds`, `corpus_delta`                  | Seed units at start; units the run added before the retention trim                           |
| `targets[].corpus_units`, `.corpus_digest`                | Units hashed and the sha256 over them; `null` digest blocks `clean`                          |
| `targets[].sanitizer`, `.sanitizer_probe`, `.sanitizer_verified` | The sanitizer claimed for this target, what the binary probe found (`found`/`mismatch`/`absent`/`not-applicable`), and whether it verified |
| `targets[].evidence_missing`                              | Named provenance facts the run could not capture; non-empty means the target cannot be `clean` |
| `targets[].peak_rss_mb`                                   | Peak resident set reported by libFuzzer                                                      |
| `targets[].crashes`, `oom`, `timeouts`, `other_artifacts` | Finding classes counted from artifacts the run itself added (pre-existing files counted separately, below) |
| `targets[].pre_existing_artifacts`, `pre_existing_artifact_names` | Artifacts the directory already held when the run started: count and names; any entry here blocks `clean`, because a re-discovered input reuses its name |
| `targets[].exit_code`, `result`                           | Process status and `clean` / `finding` / `error` / `inconclusive`                            |
| `totals`, `result`                                        | Whole-campaign roll-up; one `result` decides a run page                                      |

No host path is recorded, so a summary is safe to attach to a security-corpus
entry as-is. The `report` job merges the per-target records into one
`fuzz-campaign-summary-<run id>` artifact for the whole run. That merged record
carries the campaign-level `revision`, `revision_dirty`, `toolchain`,
`cargo_fuzz`, `sanitizer`, `libfuzzer`, and `started_utc`/`finished_utc` values
(or `mixed` when the legs disagree — a mixed sanitizer identity also makes the
run `incomplete`), each target's own provenance copy, the enumerated
`expected_targets` beside `received_targets`/`missing_targets`, and a `result`
that also covers the target-set comparison: a leg that dies before writing its
summary makes the run `incomplete`, never `clean`. Per-target diagnostics keep
30 days; the merged record keeps 90 so the citable artifact outlives them and
still states what produced the result.

## Evidence completeness (CTX-1880)

The first record proved only the finding counts: it named the toolchain and the
budget but not the sanitizer build, it carried corpus *counts* instead of a hash,
and its only coverage signal was `new_units_added`. Three additions close that
gap, and a run that cannot produce them is recorded `inconclusive` with the fact
named in `targets[].evidence_missing` — a clean count alone is not accepted
evidence:

| Addition           | Recorded as                                                                                   | How it is captured                                                                                                                                                                                                                                    | Absent means                                                        |
| ------------------ | --------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| Sanitizer identity | `sanitizer.*` (campaign), `targets[].sanitizer`, `.sanitizer_probe`, `.sanitizer_verified`      | `--sanitizer=<name>` is forwarded to `cargo-fuzz` (validated against `address`/`leak`/`memory`/`thread`/`none`), so ASan and its LSan runtime are named by the flag that caused them; the exact binary this run built (`<target-dir>/<host-triple>/release/<target>` — never a bare `release/` copy, which cargo does not write) is probed for the runtime symbol (`__asan_init`, ...) and a binary without it is a contradiction, while an unidentifiable build reports `absent` | `sanitizer-build` on a contradiction; `sanitizer-none` for a `SANITIZER=none` build; `sanitizer-leaks-unchecked` for a sanitizer libFuzzer does not drive LSan with, or a run whose sanitizer options disable the check; `clean` is refused in all cases |
| Corpus digest      | `targets[].corpus_digest`, `.corpus_units`, `corpus/<target>.sha256`                           | sha256 over the name-sorted `<unit-sha256>  <unit-name>` manifest of every regular file in the corpus the run ended with (before the retention trim) — discovered units are named by content hash, so a `*.bin` filter would omit exactly what the run found — and an unreadable unit fails the digest instead of shortening it | `corpus-digest`; a host without `sha256sum`/`shasum` cannot claim clean |
| Coverage signal    | `targets[].coverage_edges`, `.coverage_features`                                               | libFuzzer's own `cov` (edges) and `ft` (frontier features) counters from its last pulse line, parsed as bounded integers (`0..2^32-1`, anything else is absent)                                                                                        | `coverage`; `clean` is refused                                      |

The rules are ordered so that they can only ever subtract a `clean`: a finding,
an artifact left in the directory, libFuzzer's error exit, or a runner error
keeps its precedence, so missing provenance can never hide a finding (the driver
test pins both directions — a `leak-*` with no coverage counters is still
`finding`, exit 1). The sanitizer row covers every build that cannot evidence
the leak clause: `none` links no runtime at all; `leak`/`memory`/`thread` are
sanitizers libFuzzer does not drive LSan with; and an inherited
`ASAN_OPTIONS=detect_leaks=0` (or `LSAN_OPTIONS=…`) makes the runtime skip the
check even when `-detect_leaks=1` is passed — read with the runtime's `:`/`,`
separators, tolerant of whitespace and quoting, with `0`/`false` as the Boolean
— which `leak_detection_effective` records and `leak_detection_disabled_by`
names. All of those runs are recorded
`inconclusive` (`sanitizer-none`, `sanitizer-leaks-unchecked`) rather than
folded into a clean count.

### Field to P0-AC-002 clause

| P0-AC-002 clause                                      | Fields that evidence it                                                                                                                                                                        |
| ----------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A **long-running** campaign, not a one-off            | `requested_secs`, `targets[].secs_fuzzed`, `targets[].secs_actual`, `targets[].runs`, `started_utc`/`finished_utc`; the merged record's `expected_targets`/`received_targets`/`missing_targets` |
| **Zero crashes**                                      | `targets[].crashes`, `artifacts[]`, `pre_existing_artifacts`, `targets[].result`                                                                                                                  |
| **Zero crashes** — the artifact directory was empty at start | `targets[].pre_existing_artifact_names` (with `.pre_existing_artifacts`): a file already present blocks `clean`, because libFuzzer reuses the name of a re-discovered input, so a stale artifact could otherwise launder a clean count |
| **Zero hangs**                                        | `targets[].slow_units`, `targets[].timeouts`, and `libfuzzer.timeout_secs` — the bound that defines a hang                                                                                       |
| **Zero memory-safety findings**                       | `targets[].leaks` **with** `sanitizer.leak_detection_effective: true` and the `sanitizer.instrumentation` that produced it: `leaks: 0` is memory-safety evidence only when the build had the sanitizer *and* the runtime actually ran the leak check |
| **Zero memory-safety findings** — the leak check was requested of the runtime | `libfuzzer.detect_leaks` (the `-detect_leaks=` value the driver forwarded, `1` only for an ASan build whose runtime still has the check enabled; `0` means the campaign never asked for it, so `leaks: 0` proves nothing) |
| **Zero memory-safety findings** — the binary is the sanitizer build the flag names | `targets[].sanitizer_probe` (`found`/`mismatch`/`absent`/`not-applicable`: the located build's runtime symbol), `targets[].sanitizer_verified` (`false` on `absent`/`mismatch`; a `mismatch` is a contradiction and blocks `clean`) |
| **Corpus retained in-repo, with a hash artifact**      | `targets[].corpus_seeds`, `.corpus_units`, `.corpus_digest`, `corpus/<target>.sha256`                                                                                                            |
| **Provenance an auditor can re-run**                  | `revision`, `revision_dirty`, `toolchain`, `cargo_fuzz`, `sanitizer.*`, `libfuzzer`, `targets[].coverage_edges`/`.coverage_features`                                                              |
| **Provenance an auditor can re-run** — nothing silently missing | `targets[].evidence_missing`: empty on every target is a precondition of `clean`; a named gap (`coverage`, `corpus-digest`, `sanitizer-build`, `sanitizer-none`, `sanitizer-leaks-unchecked`) makes the run `inconclusive` whatever its finding counts are, so a reviewer can see which clause the record cannot support instead of reading a bare zero |

A review that finds a `clean` record missing any row of the first table returns
`NEEDS-FIX`. The corpus hash and the sanitizer identity are what make
`leaks: 0` mean "no memory-safety finding under ASan/LSan", instead of "nothing
looked".

## CTX-1877 60 s smoke campaign (historical; predates the provenance fields)

This table is the CTX-1877 machinery smoke run, kept as history. It was recorded
by the driver as it existed then — before this change added the sanitizer
identity, the corpus digest and the coverage counters — which is why it carries
none of those columns. Under the schema in "Evidence record" the same run would
have had to record `sanitizer.*`, a per-target `corpus_digest` and `cov`/`ft`,
and its `clean` would depend on an empty `evidence_missing` on every target.
It is therefore evidence that the driver ran end to end, **not** evidence that
the current `clean` rule holds, and it is not the run that validated this
schema.

Machinery smoke on the CTX-1877 authoring host: Linux x86_64, 24 hardware
threads, `cargo 1.100.0-nightly`, `cargo-fuzz 0.13.2`, budget 60 s per target,
target cache outside the worktree. The CTX-1880 host has no nightly toolchain,
so this run cannot be repeated or extended here.

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

Outcome as recorded then: `result: clean`, exit 0, with no artifact of any class
(`crash-*`, `leak-*`, `slow-unit-*`, `oom-*`, `timeout-*`, unclassified),
`slowest_unit_time_sec: 0` on every target, and every target executing its
budget. That `clean` is a statement about finding counts alone — the fields that
would have let it stand as provenance evidence did not exist yet. The
`vt_parser` seed count is high because the run resumed a scratch corpus left by
an aborted attempt, which also exercised the retention trim (766 oldest units
removed, 2000 kept). `control_string_cap` executes slowly by design: its inputs
are megabytes at the parser cap, so 98 executions in 63 s is the expected shape,
not a stall.

This is a historical machinery record, **not** campaign evidence for P0-AC-002
and not a validation of the current schema: 60 s per target is a smoke budget,
and the columns above predate the sanitizer, corpus-digest and coverage fields
that this change makes preconditions of `clean`. The 300 s-per-target SEC-19
record (`sec19-fuzz-campaign-ctx-1202-evidence.md`) remains the longest reported
run until the nightly has accumulated real history or an owner campaign runs for
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
   toolchain, `cargo-fuzz` version, sanitizer build and flags, budget,
   executions, corpus digest, coverage counters, and finding counts, plus the
   raw logs, the corpus digest artifact, and any artifact. A clean nightly is
   `result: clean` with `crashes`, `oom`, `timeouts` all zero for every target
   **and** an empty `evidence_missing` on every target.

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
  ceiling keeps a manual run inside the 60-minute job timeout; it is a ceiling,
  not a default.
- **Corpus cache growth.** The corpus cache key rolls with the ISO week
  (`fuzz-corpus-<target>-<YYYY-Www>`), so a target adds one entry per week
  instead of one per night against the repository-wide Actions cache quota that
  the required checks also draw on; the newest matching entry wins on restore.
  The save is not success-gated (a finding is when the corpus matters most), and
  a duplicate weekly key is tolerated as a cache miss rather than a campaign
  failure. Unit trimming bounds each entry; the discovered corpus is uploaded as
  an artifact too, so a failed run still leaves its coverage behind — including
  the cap target, whose routine uploads stay excluded because its ~50 MiB seed
  set is derivable. The committed `fuzz/corpora/` remains the canonical seed
  set.
- **Nightly churn.** The toolchain is not date-pinned, so the fuzz build is
  usually cold. That is a deliberate trade: the fuzz crate should track current
  nightly, and the evidence records which nightly ran.
- **Flakiness.** `-timeout=25` is far above the observed `slowest_unit_time_sec:
0`; a `timeout-*` artifact on a loaded runner is still treated as a finding,
  which is the correct fail-closed direction. Do not relax the timeout to hide
  one: investigate the input instead.
- **Artifact state is an input to the record, not something to paper over.**
  Because presence counts, a target whose artifact directory is not empty
  reports `finding` even when the run itself discovered nothing. That is
  deliberate (bitty#3232): P0-AC-002 counts findings, not new file names, and
  libFuzzer reuses the name of a re-discovered input, so a name diff alone
  would let a stale artifact launder a clean record. It costs a nightly run
  nothing — every matrix job starts from a fresh checkout — and it costs a
  local campaign one triage-then-clear step after a real finding. The driver
  does not clean the directory for you: the artifact a run left behind is the
  thing an auditor is meant to inspect.
- **Portability.** The driver needs bash and POSIX `find`. `timeout`/`setsid`
  and GNU `find -printf` are used when present (Linux, CI); without them the
  hard kill and the retention trim degrade with a warning instead of aborting,
  so the `just` path is Linux-first and a macOS host needs coreutils'
  `gtimeout`. `--artifacts-dir` is forwarded to libFuzzer as
  `-artifact_prefix`, so a caller that relocates artifacts still gets their
  classification.
- **`act` and the dynamic matrix.** `act -n --list` resolves the workflow, and
  `act -n ... -j targets` dry-runs the enumeration job, but `act` cannot
  evaluate a matrix built from `needs.<job>.outputs`, so the campaign job is
  syntax-checked by `actionlint` and by a real `workflow_dispatch` rather than by
  an `act` dry-run. `just ci-local` still covers the `Quality gates` job that
  `just check` (including `fuzz-campaign-test`) runs in CI.
- **No coverage gate.** The criterion is zero findings, not a coverage
  threshold: `cov`/`ft` are recorded per target as the coverage signal (see
  "Evidence completeness"), but no numeric coverage target is asserted. A
  missing counter is treated as absent evidence, never as a pass.

## Gates for this change

- `actionlint -color .github/workflows/fuzz-campaign.yml` — clean.
- `act -n --workflows .github/workflows/fuzz-campaign.yml --list` — three jobs
  (`targets`, `campaign`, `report`); `act -n ... workflow_dispatch -j targets
-P ubuntu-24.04=catthehacker/ubuntu:act-24.04` — job succeeds in dry run.
- `shellcheck -S warning scripts/fuzz-campaign.sh
scripts/tests/fuzz-campaign.test.sh` and `shfmt -d` — clean.
- `just fuzz-campaign-test` — PASS; wired into `just check` and executed by the
  `Quality gates` job in `ci.yml`, because PR CI never runs `just check`. The
  campaign itself stays nightly-only; the test guards that split by asserting
  the workflow's triggers are `schedule`/`workflow_dispatch` only and that
  `ci.yml` never invokes the driver.
- The CTX-1877 smoke campaign above — recorded then as `result: clean`, exit 0,
  on a CTX-1877 host. It is retained history, not a gate result for this change:
  it predates the sanitizer, corpus-digest and coverage fields, so it says
  nothing about the CTX-1880 `clean` rule, and the CTX-1880 host has no nightly
  toolchain to re-measure it with.

### Gates run for CTX-1880 (this change)

Run in the CTX-1880 worktree with `CARGO_TARGET_DIR` outside it; every one
exited 0:

- `cargo fmt --all -- --check`
- `cargo clippy -p bitty-vt --all-targets --offline -- -D warnings`
- `cargo test -p bitty-vt --offline` — 297 + 8 + 1 + 5 + 1 tests pass, the new
  `fuzz_max_len_file_matches_generator` among them
- `cargo +1.85 check -p bitty-vt --tests --offline`
- `RUSTDOCFLAGS="-D warnings" cargo doc -p bitty-vt --no-deps --offline`
- `shellcheck -S warning scripts/fuzz-campaign.sh scripts/tests/fuzz-campaign.test.sh`
- `actionlint -color .github/workflows/fuzz-campaign.yml`
- `bash scripts/tests/fuzz-campaign.test.sh` (`just fuzz-campaign-test`) — PASS.
  Two of its assertions carry the weight: the expected target set is derived from
  `fuzz/Cargo.toml` outside the driver, in both directions, so a `list_targets`
  that drops a target fails the suite (verified by excluding `parser_to_state`
  from the enumeration: three assertions fail, exit 1), and the report job's
  merge program is executed over synthetic per-target legs, so the
  sanitizer/`incomplete` rule is pinned by a test rather than a token grep
  (verified by deleting that clause: the merge case fails, exit 1).
- `just scratch-paths` — OK

Not run, and why: `just ci-local` (`act` needs github.com and concurrent `act`
runs kill each other), a live nightly campaign (the authoring host has no
nightly toolchain, so this change claims no measured campaign; the
classification and the new fields are pinned by the stubbed driver test), and
`shfmt` (not installed here). `markdownlint-cli2` still reports the
repository's pre-existing `MD060` table-alignment findings in this file; the new
prose adds no finding of another class.

## Open points

- The cap target's `-max_len` used to be a driver literal because the
  generator's value is a Rust constant. It now has one owner
  (`CONTROL_STRING_MAX_BYTES` in `crates/bitty-vt/src/parser.rs` plus the
  `FUZZ_MAX_LEN` headroom in the generator test), and the driver reads the
  checked-in derived file `fuzz/control_string_cap.max_len`. A generated lockstep
  (the generator writing the file) would remove the file from review diffs but
  needs a build step in the driver; the derived file plus two drift tests is the
  cheaper shape today.

- The binary probe behind `sanitizer_verified` is exact but best-effort: it looks
  at `<CARGO_TARGET_DIR|fuzz/target>/<host-triple>/release/<target>` and nothing
  else. A build cargo placed elsewhere, or a host whose triple cannot be read,
  records `sanitizer_probe: absent` — "declared by the forwarded flag", never
  "verified" — and a bare `release/` copy is deliberately ignored because cargo
  does not write one, so it can only be a stale build. A *located* binary that
  lacks the runtime symbol is the hard contradiction that blocks `clean`, which
  is the direction that matters.
- The corpus digest proves which corpus a run ended with, not that the corpus is
  the committed one: reproducibility still needs the same revision plus that
  digest. Keeping the digest in the merged nightly record is what lets a later
  campaign say "same corpus, more budget" instead of "probably the same".

- Whether the nightly budget should grow (for example a weekly 1800 s run on
  top of the 600 s nightly) depends on CI cost the owner has not yet budgeted.
- The canonical security-corpus entry for future campaign records, and the
  R-001 risk-state move, belong to the `bitty-docs` task that owns the corpus.
- Corpus-cache retention: if artifact and cache storage becomes a cost problem,
  the next step is to persist corpora weekly instead of nightly rather than to
  drop the corpus upload on failure.
