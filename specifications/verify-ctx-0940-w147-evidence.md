# W-147 final verification report (bitty CTX-0940)

- Task: CTX-0940 (in_progress) | Plan: W-147 | Tracking issue: bitty#1621 (OPEN)
- Production revision verified: `03bfda5c` (origin/main at gate start; branch
  `ctx-0940/final-verification` fast-forwarded to it, no other changes on top
  except this gate's evidence)
- Verifier: `cmd-core-bootstrap-02` / `ctx-0940-impl`
- Date: 2026-10-04

Scope is the verification gate only: prove the small-core program end-state.
No product code was changed. The single added file is test evidence
(`crates/bitty-terminal/tests/verify_w147_small_core_graph.rs`, 8 tests) plus
this report. No echte regressions were found; everything else is filed as
tracked follow-up issues (see §6).

## 1. Safe startup (`bitty --safe`, no-plugin baseline)

Claim: `--safe` starts with zero plugins; the safe-mode startup graph contains
no third-party/plugin edges.

Evidence (trace, not assertion-only):

- `main.rs` wires `args.safe` into all three startup decisions:
  `discover_and_activate(args.safe, …)` (main.rs:771),
  `.with_safe_mode(args.safe)` (main.rs:889), session-restore short-circuit
  `|| args.safe` (main.rs:719-720), and exit-persistence off
  (`!args.safe && !args.headless`, main.rs:914).
- `PluginRuntime` activation fails closed in safe mode:
  `if self.safe_mode && entry.package.source_class.is_third_party()`
  yields `skipped_safe_mode: true` (bitty-runtime
  `plugin_runtime/mod.rs:986-993`). Only `SourceClass::Bundled` is
  first-party; registry/git/local-path are third-party. The binary logs
  `bitty: plugin '{id}' skipped (--safe, {source} source, no VM)`.
- Safe load policy (`bitty-lua` gate): `LoadPolicy::safe_mode()` drops every
  third-party candidate before any VM is built (existing
  `bitty-lua/tests/safe_mode.rs`, 6 tests, green).
- Link time: `cargo tree -p bitty-terminal -e normal --depth 1` shows only
  first-party path crates + two pinned first-party git seams (`bitty-ipc`,
  `bitty-storage`) + `pollster`. Third-party plugins are runtime Lua and never
  a link edge.
- New binary-level trace test
  `safe_headless_startup_skips_hostile_dev_plugin_with_no_vm`: with a hostile
  `BITTY_PLUGIN_DIR` package, `bitty --safe --headless --log-level info`
  exits 0, logs `skipped (--safe`, activates no plugin (`active (` absent),
  and the hostile payload (`os.execute('touch …')`) never executes. Green.

## 2. Dependency graph (production edges)

Method: `cargo tree -e normal` (normal edges only) plus manifest assertions in
the new test module (comment-aware `[dependencies]` parsing, so prose mentions
such as the W-141 note in `bitty-rich/Cargo.toml` never read as edges).

Results on `03bfda5c`:

- `cargo tree -p bitty-terminal -e normal` matching
  test/validation/extension names returns exactly one hit: `bitty-storage`
  (the W-146 seam).
- `cargo tree -i bitty-storage -e normal`: sole consumer is `bitty-terminal`.
- `cargo tree -i bitty-test-support -e normal` and
  `-i bitty-test-vm -e normal`: no consumers (each returns only itself) —
  both are `[dev-dependencies]`-only (bitty-pty, bitty-rich, bitty-runtime,
  bitty-terminal test targets).
- `cargo tree -i bitty-compat-lab` / `-i bitty-perf`: package ID matches
  nothing — both suites are out of the workspace (W-105, #1684), pinned
  externally via `validation-pins.env`.
- Source level: all 13 `bitty_storage::` references in `crates/*/src` live in
  exactly one file, `crates/bitty-terminal/src/storage_backends.rs` (the
  composition-root seam implementing the Core-owned `SessionFileBackend` /
  `KvCommitBackend` traits). Core-never-imports-extension holds.
- Workspace membership: 16 crates; `bitty-compat-lab` / `bitty-perf` absent.

All eight new tests pin these properties and pass.

## 3. Regression evidence (full workspace gate)

`just test` (cargo-nextest workspace + doctests + `harness=false` platform
entry points + external bitty-perf benches + dev-tools CLI slice) was run on
the gate revision with
`CARGO_TARGET_DIR=$BITTY_WORKSPACE/.targets/ctx-0940-verify`
(all scratch kept out of every repo checkout; cleaned at task close):

- cargo-nextest workspace: **5353 passed, 0 failed, 1 skipped**
  (1 leaky noted by the runner; no failures).
- Doctests (`cargo test --workspace --doc`): pass, 0 failed (4 passed,
  1 ignored across crates).
- Platform entry points (`headless_run`, `winit_window`): pass.
- External bitty-perf suite at the pinned rev (`9cf6c91` per
  `validation-pins.env`, executed from a read-only `git archive` extraction
  under `.targets/` because the workspace `bitty-perf` checkout is still
  metadata-only `main`): lib unit tests **52/52 pass**; all 14 bench
  harnesses compile and execute with no test failures. (Bench _verdict_
  prints such as PB-6/PB-3-reclaim ABOVE_BUDGET on this headless box are
  environmental measurements, not gate failures; the suite's own assertions
  pass.)
- dev-tools slices: `cli_dev` **27 passed**; `bin bitty dev::` **22 passed**.
- New W-147 module `verify_w147_small_core_graph`: **8/8 pass**.
- Pre-checks green: `cargo fmt --check` clean,
  `cargo clippy -p bitty-terminal --all-targets` clean, `actionlint` clean.
- `just ci-local` (Quality gates via act, `bitty-act` image, per-branch cache):
  **🏁 Job succeeded** — all steps green on the gate revision including the
  new test module and evidence file.

## 4. Docs sync (satisfied with evidence — stale pages fixed and merged)

Checked `bitty-terminal-docs` and `bitty-docs` checkouts (read-only) against
the landed removals. The three stale pages identified at gate time were fixed
by verification-only docs PRs in the owning repos and are now merged
(verified read-only via `gh api .../pulls/<n>` showing `merged: true`):

- bitty-terminal-docs#191 — `architecture/core-boundaries.md:246` transitional
  workspaceline claim → fixed by bitty-terminal-docs#193, merge commit
  `570d82cd662189a0751f76b0a77709a7d9ceefa6` (Closes #191).
- bitty-terminal-docs#192 — `development/release-mechanics.md` workspace-member
  roster → fixed by bitty-terminal-docs#194, merge commit
  `b4f79b6cdca0d0a759ef397351ba224a2e3dff62` (Closes #192).
- bitty-docs#435 — `docs/project/repository-map.md` (+ `project-state.json`
  evidence strings) 18-crate count → fixed by bitty-docs#436, merge commit
  `b511fee176a44abc301d35cc24842c71bb17cda6` (Closes #435).

Spot-checked clean (no issue filed): `architecture/overview.md` (target-
architecture framing; "draws no chrome" matches the end-state),
`specifications/search-selection-contract.md` (mechanism language consistent
with the retained host ops; policy retirement is recorded in W-144).

## 5. Ownership table (every retired surface → owner → evidence)

| Retired surface                       | Owner (plugin repo or Core mechanism)                                                                    | Evidence PR                                                                                                                                                      |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Execution supervisor                  | `bitty-execution` repo                                                                                   | bitty#1638 (CTX-0933, W-140)                                                                                                                                     |
| Graphics decode mechanics             | `bitty-graphics` repo                                                                                    | bitty#1639 (W-141)                                                                                                                                               |
| A11y adapter                          | `bitty-a11y` repo (Core had no adapter; verified no-op)                                                  | CTX-0935 (W-142)                                                                                                                                                 |
| Search/copy-mode policy               | `search`, `copy-mode`, `history` plugins; Core keeps capability-gated host ops (W-143) + RFC-0004 family | bitty#1640 (W-143a-c), #1673 (RFC-0004 host, CTX-0955), #1679 (W-144 deletion, CTX-0937); SDK #144; plugin CTX-0004s (search#9, history#9, copy-mode#9)          |
| Platform auxiliaries (blur/URL spawn) | Platform-service contract; permission enforcement retained in Core                                       | bitty#1645 (CTX-0938, W-145)                                                                                                                                     |
| Storage durable-commit seam           | `bitty-storage` repo; Core keeps capture/validation/gates via Core-owned traits                          | bitty#1646 (CTX-0939, W-146); storage #7/#8/#9                                                                                                                   |
| Compat-lab / perf suites              | Standalone `bitty-compat-lab` / `bitty-perf` repos, pinned via `validation-pins.env`                     | bitty#1684 (CTX-0931, W-105)                                                                                                                                     |
| Bar presentation + tab strip          | `bar` plugin (waybar-class; statusline/workspaceline superseded)                                         | bitty#1677 (c246e52, bar deletion), #1680 (34b6d83, tab-strip), #1671/#1664 (host gaps); bar onboarded via plugins#70; statusline archived (registry plugins#69) |
| Beacon policy                         | `beacon` plugin (Core mechanism-only; verified no-op)                                                    | CTX-0928 (W-102)                                                                                                                                                 |
| Composer editing UX                   | `composer` plugin; retained Core fallback + safe-mode identity stay until live-host proof                | bitty#1661/#1662 (CTX-0929 S-1b/S-5/S-7); residual E-CUT-1/2/4 + G1 tracked on bitty#1682 (DEC-W103-5 holds)                                                     |

Known residual (not a regression): composer engine deletion waits on
live-host proof (bitty#1682 open); compat-suite re-pin to merge commits waits
on suite PR merges (recorded in `validation-pins.env` header).

## 6. No private first-party bypass (retained Core paths audit)

- `search_host` (bitty-runtime): every op returns `Result<_, HostOpError>`
  (`Denied`/`Stale`/`Unavailable`); clipboard writes gated on the existing
  `clipboard.write` grant; cross-view use denies; snapshots returned to the
  caller only, never published on the Event Bus. No bypass.
- `HistoryGate` (bitty-plugin-host `history_read.rs`): per-plugin per-source
  scoped grants, no wildcards, grant-scope intersection, rate bounds, new
  `history.*` family beside (never inside) `terminal.*`. No bypass.
- Session restore: opt-in (`session.restore_on_startup`, default false);
  `args.safe` short-circuits at the call site (main.rs:719) AND inside
  `restore_session_on_startup` → `SkippedSafeMode` (session.rs:1193); unit
  tests `safe_mode_never_reads_session_state` /
  `safe_mode_does_not_apply_a_valid_store` green. No bypass.
- Composer fallback (`composer_owner.rs`): pure `decide_composer_owner`
  reads only lifecycle state + grant snapshot; safe mode wins without
  consulting a VM; plugin owns only when `Active` with every required
  capability (`ui.overlay.focus`, `terminal.input.submit`, `process.editor`);
  version mismatch fails closed (no partial activation). Cutover/rollback
  drill tests green. No bypass.

## 7. Follow-ups filed (tracked, out of gate scope)

- bitty-terminal-docs#191, #192; bitty-docs#435 (stale docs, §4 — now fixed
  and merged: terminal-docs #193 `570d82c` + #194 `b4f79b6`, bitty-docs #436
  `b511fee`).
- Pre-existing residuals restated, not re-filed: composer live-host proof +
  engine deletion (bitty#1682), compat-suite merge-commit re-pin
  (`validation-pins.env`), extension-repo metadata sync notes from the
  CTX-0004 verifications.

## 8. Definition-of-done checklist

- [x] Safe startup proven (policy + binary trace + link graph)
- [x] Production-edge graph proven (cargo tree + 8 automatable tests)
- [x] Full `just test` green with pass counts (§3)
- [x] `just ci-local` Quality gates green (repo-mandated pre-push)
- [x] Docs sync satisfied with evidence: stale pages fixed and merged
      (terminal-docs #193 `570d82c` + #194 `b4f79b6`, bitty-docs #436 `b511fee`)
- [x] Ownership table complete with evidence PRs
- [x] Bypass audit clean on the four retained paths
- [ ] PR opened (base main, chore/P1/area:architecture, milestone v0.1.0,
      Closes bitty#1621), NOT merged; carryctx notes recorded
