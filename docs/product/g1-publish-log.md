---
title: G1 Publish Log — Leaf Crates Dry-Run (CTX-0047)
description: Draft dry-run log for G1 leaf crates (vt, pty, platform, config, package, lua) at 0.0.1 with cargo publish --dry-run evidence, publish order, and gates; no actual crates.io upload
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# G1 Publish Log — Leaf Crates Dry-Run (CTX-0047)

## Status and provenance

- Status: **draft**. Preparation artifact only. No `cargo publish` was
  executed, no credentials were used, and no crates.io upload occurred.
  This log records pre-publish **dry-run** verification for
  `CTX-0047` on branch `ctx-0047/publish-g1-actual`.
- Ownership: bitty **CTX-0047** — branch `ctx-0047/publish-g1-actual`.
- Task: `Publish G1 leaves to crates.io (actual) — dry-run preparation only`
  — verify `0.0.1` and `cargo publish --dry-run` for G1 leaves
  `vt`, `pty`, `platform`, `config`, `package`, `lua` in documented order.
- Companion records:
  - **CTX-0045** `docs(product): add G1 publish checklist`
    — PR #76 (`83fd63b`) — draft checklist with full G1 dry-run evidence at
    `168493a`, pending independent review.
  - **CTX-0044** `docs(product): add release ladder v0.1-v1.0 and publish
order` — PR #75 (`ff9715d`) — **merged** to `main` at `9eec31b`.
  - **CTX-0043** `chore(crate): prepare workspace for crates.io v0.1.0`
    — PR #74 (`168493a`) — set workspace `0.0.0 -> 0.1.0` (now `0.0.1` via
    CTX-0049), workspace metadata, per-crate `publish` flags.
  - **CTX-0049** `chore(crate): adjust workspace to 0.0.1 earliest, defer
0.1.0` — PR #77 (`956926c`) — earliest publish `0.0.1`.
  - **CTX-0050** `feat(runtime): add v0.1 minimal terminal slice` — PR #78
    (`9eec31b`) — current `HEAD`.
- Provenance: builds on the publish-order DAG in
  [`release-ladder.md`](./release-ladder.md) (CTX-0044) and
  [`g1-publish-checklist.md`](./g1-publish-checklist.md) (CTX-0045), and the
  workspace topology in
  [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
  No roadmap promise is admitted; version numbers are maturity labels.
- Authority: if `release-ladder.md` is revised, reconcile the order and gate
  table below with that revision. Closing any open question still requires
  its RFC/ADR per the open-question register.

## Scope

- **G1 leaves** (this log): six crates with `publish = true` and no
  workspace `version = "0.0.1"` path edge — `bitty-vt`, `bitty-pty`,
  `bitty-platform`, `bitty-config`, `bitty-package`, `bitty-lua`.
  Each is verified with `cargo publish --dry-run --allow-dirty` without
  waiting for crates.io index propagation.
- Out of scope for G1: `bitty-term-state` (Group 2, depends on `vt`),
  `bitty-ui`/`bitty-render` (Group 3, after `term-state` + `platform`),
  and the draft tail (`plugin-host`, `rich`, `ipc`, `agent`, `runtime`,
  `app`, `core`) which remain `publish = false` at `0.0.1` (deferring
  `0.1.0`). Their order and gates are documented in the release ladder,
  not re-verified here.
- This log is **not** a publish record. A real publish log would be written
  from a clean checkout after maintainer `cargo publish` with credentials
  and index propagation waits.

## Workspace verification at log time

### Head and toolchain

- Worktree: `/mnt/data/Workspace/Projects/bitty-terminal/bitty/.worktrees/ctx-0052-publish-g1`
- Branch: `ctx-0052/publish-g1` — `HEAD bbbdc1c` (`main` at
  `bbbdc1c`; historical `ctx-0047/publish-g1-actual` at `9eec31b` — see Revision history).
- Toolchain: `rust-toolchain.toml` `channel = "1.97.1"` — `cargo 1.97.1`
  (`c980f4866 2026-06-30`), `rustc 1.97.1` (`8bab26f4f 2026-07-14`).
- `git status` was **clean** before this draft log; the log itself is
  intentionally left **dirty** (untracked) per the task — real publish
  must run from a clean tree without `--allow-dirty`.

### Workspace version

- `Cargo.toml` `[workspace.package] version = "0.0.1"` — inherited from
  CTX-0049 at `956926c`, retained at `9eec31b`. No drift.
- `[workspace.package]` carries `edition = "2024"`,
  `rust-version = "1.85"`, `publish = false` at the workspace root,
  plus `description = "Bitty terminal workspace"`,
  `license = "MIT OR Apache-2.0"`,
  `repository = "https://github.com/bitty-terminal/bitty"`,
  `keywords`/`categories` — CTX-0043 metadata retained.

```toml
[workspace.package]
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
publish = false
```

- Per-crate `version.workspace = true` inherits `0.0.1`; G1 leaves set
  `publish = true` with their own `description`/`license`/`repository`
  and no internal `path` dependency requiring `version = "0.0.1"`.

### Publish flags (verified `2026-08-28` at `bbbdc1c`; historical `9eec31b`)

| Crate               | `publish` | Internal workspace deps                                              | Expected at `0.0.1` |
| ------------------- | --------- | -------------------------------------------------------------------- | ------------------- |
| `bitty-vt`          | `true`    | none                                                                 | publish in G1       |
| `bitty-pty`         | `true`    | none                                                                 | publish in G1       |
| `bitty-platform`    | `true`    | none                                                                 | publish in G1       |
| `bitty-config`      | `true`    | none                                                                 | publish in G1       |
| `bitty-package`     | `true`    | none                                                                 | publish in G1       |
| `bitty-lua`         | `true`    | none (`piccolo = "0.3.3"` external only)                             | publish in G1       |
| `bitty-term-state`  | `true`    | `bitty-vt = "0.0.1"`                                                 | Group 2             |
| `bitty-ui`          | `true`    | `bitty-term-state = "0.0.1"`                                         | Group 3             |
| `bitty-render`      | `true`    | `bitty-term-state = "0.0.1"`, `bitty-platform = "0.0.1"`             | Group 3             |
| `bitty-plugin-host` | `false`   | `term-state`, `config`, `package`                                    | tail                |
| `bitty-rich`        | `false`   | `term-state`, `vt`                                                   | tail                |
| `bitty-ipc`         | `false`   | none                                                                 | tail                |
| `bitty-agent`       | `false`   | none                                                                 | tail                |
| `bitty-runtime`     | `false`   | `vt`, `term-state`, `pty`, `render`, `platform`, `ui`, `plugin-host` | tail                |
| `bitty-app`         | `false`   | `platform`, `runtime`                                                | binary, never       |
| `bitty-core`        | `false`   | none                                                                 | seed to retire      |

Verified via `grep -R publish Cargo.toml crates/*/Cargo.toml` and
per-crate `Cargo.toml` inspection on this worktree.

## Publish order

### Task-specified G1 order (within Group 1)

Group 1 crates are unordered with respect to each other (DAG has no edge
between them). For repeatability, CTX-0047 fixes the invocation sequence
to the order stated in its task description:

1. `bitty-vt`
2. `bitty-pty`
3. `bitty-platform`
4. `bitty-config`
5. `bitty-package`
6. `bitty-lua`

Within a real crates.io publish, these six may be published in any order;
the sequence above is used for this log so reviewers see a single
reproducible path. No index propagation wait is required between them for
dry-run; for a real publish, allow index propagation before starting Group 2.

### Full ladder context (traceability — see `release-ladder.md`)

- **Group 1 — Leaves** (`publish = true`, no workspace `version` edge):
  `vt`, `pty`, `platform`, `config`, `package`, `lua` — this log.
- **Group 2 — Terminal Truth**: `bitty-term-state` (depends on `vt`).
  Publish only after `vt` is on crates.io. CTX-0043/CTX-0045 dry-runs
  correctly reported missing index for this group; re-verify with
  `cargo publish --dry-run -p bitty-term-state --allow-dirty` once Group 1
  is indexed (expected to PASS after `vt` is published).
- **Group 3 — Presentation branch** (parallel after Group 2):
  `bitty-ui` (after `term-state`) and `bitty-render` (after `term-state` +
  `platform`). May publish concurrently once prerequisites are indexed.
  `bitty-vt` dev-dependency in `bitty-render` does not impose ordering.
- **Group 4 — Tail** (`publish = false` at `0.0.1`): `plugin-host`, `rich`,
  `ipc`, `agent`, `runtime`, `app`, `core`. Not publishable at `0.0.1`
  (deferring `0.1.0`); promotion requires RFC acceptance and DAG-order
  `publish` flips plus `version = "x.y.z"` pins (CTX-0043 pattern).
  `runtime`/`app` are validated via workspace headless integration tests,
  not via a crates.io release.

## Verification gates (must PASS before any real publish)

| Gate                         | Command                                                                                             | Result on `ctx-0052/publish-g1` at `bbbdc1c` (2026-08-28; historical `ctx-0047/publish-g1-actual` at `9eec31b`) |
| ---------------------------- | --------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `cargo check`                | `cargo check --workspace --all-targets --locked`                                                    | **PASS** — `Finished dev profile` (re-run via `just check` clippy/typecheck leg)                                |
| `just check`                 | `just check` (`fmt-check` + `clippy -D warnings` + `test --locked` + `actionlint` + `markdownlint`) | **PASS** — 0 issues (see evidence below)                                                                        |
| └ `cargo fmt --check`        | via `just fmt-check`                                                                                | PASS — no diff                                                                                                  |
| └ `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings`                                    | PASS — 0 warnings                                                                                               |
| └ `cargo test`               | `cargo test --workspace --all-targets --locked`                                                     | PASS — 708 passed, 0 failed (CTX-0050 baseline; see `just check` tail)                                          |
| └ `actionlint`               | `actionlint -color`                                                                                 | PASS                                                                                                            |
| └ `markdownlint`             | `bunx --bun markdownlint-cli2@0.23.1` (`**/*.md`)                                                   | PASS — 0 issues in 28 files (now includes this draft; historical 27 at `9eec31b`)                               |
| publish metadata             | `cargo publish --dry-run --allow-dirty` per G1 leaf                                                 | **PASS all 6** — see dry-run table                                                                              |
| version/publish flag drift   | `grep` of `Cargo.toml` workspace + per-crate                                                        | PASS — `workspace.package.version 0.0.1`, G1 leaves `publish = true`                                            |

`just check` is the normative gate per `justfile`; it was executed on this
worktree after writing this draft log (worktree intentionally dirty). Raw
tail is captured below.

No `TODO`/`FIXME` introduced; frontmatter `status: draft` retained.

## G1 leaf `cargo publish --dry-run` results

Executed in worktree
`/mnt/data/Workspace/Projects/bitty-terminal/bitty/.worktrees/ctx-0052-publish-g1`
at `bbbdc1c` (historical `ctx-0047/publish-g1-actual` at `9eec31b`), toolchain `1.97.1`, with `--allow-dirty` so the untracked draft
log does not block verification. Mirrors the CTX-0043/CTX-0045 dry-run pattern;
`--allow-dirty` is **not** used for a real `cargo publish`.

### Commands (task-specified order)

```bash
cargo publish --dry-run -p bitty-vt --allow-dirty
cargo publish --dry-run -p bitty-pty --allow-dirty
cargo publish --dry-run -p bitty-platform --allow-dirty
cargo publish --dry-run -p bitty-config --allow-dirty
cargo publish --dry-run -p bitty-package --allow-dirty
cargo publish --dry-run -p bitty-lua --allow-dirty
```

Run independently; each leaf verifies from the packaged tarball
(`target/package/<crate>-0.0.1`) with `Finished dev profile` and
`warning: aborting upload due to dry run` (expected for `--dry-run`).

### Results table

| #   | Crate            | Version | `cargo publish --dry-run --allow-dirty` | Packaged                                  | Verified build                                                                        |
| --- | ---------------- | ------- | --------------------------------------- | ----------------------------------------- | ------------------------------------------------------------------------------------- |
| 1   | `bitty-vt`       | `0.0.1` | **PASS**                                | 23 files, 97.8 KiB (22.5 KiB compressed)  | `Compiling bitty-vt ... Finished dev` — `vte 0.15`                                    |
| 2   | `bitty-pty`      | `0.0.1` | **PASS**                                | 14 files, 66.1 KiB (19.5 KiB compressed)  | `Compiling bitty-pty ... Finished dev` — `portable-pty 0.9`                           |
| 3   | `bitty-platform` | `0.0.1` | **PASS**                                | 12 files, 124.4 KiB (33.6 KiB compressed) | `Compiling bitty-platform ... Finished dev` — `winit 0.30`, `raw-window-handle 0.6.2` |
| 4   | `bitty-config`   | `0.0.1` | **PASS**                                | 13 files, 120.7 KiB (24.9 KiB compressed) | `Compiling bitty-config ... Finished dev`                                             |
| 5   | `bitty-package`  | `0.0.1` | **PASS**                                | 13 files, 153.8 KiB (33.8 KiB compressed) | `Compiling bitty-package ... Finished dev`                                            |
| 6   | `bitty-lua`      | `0.0.1` | **PASS**                                | 6 files, 60.4 KiB (14.3 KiB compressed)   | `Compiling bitty-lua ... Finished dev` — `piccolo 0.3.3`                              |

All six: **dry-run PASS**. No metadata errors, no missing `description`/
`license`/`repository`, no unpublished internal path dep errors (expected
only for Groups 2-3), no `publish = false` block. Warnings are only the
intentional `aborting upload due to dry run`.

Raw log excerpt (`bitty-vt` representative; all 6 analogous on this worktree):

```text
Updating crates.io index
Packaging bitty-vt v0.0.1 (.../crates/bitty-vt)
Updating crates.io index
Packaged 23 files, 97.8KiB (22.5KiB compressed)
Verifying bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
 Compiling bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.82s
 Uploading bitty-vt v0.0.1 (.../crates/bitty-vt)
warning: aborting upload due to dry run
```

Equivalent `Finished` lines were observed for `bitty-pty` (5.33 s),
`bitty-platform` (55.11 s, full winit/Wayland recompilation in the
verification sandbox), `bitty-config` (4.32 s), `bitty-package` (4.40 s),
`bitty-lua` (17.63 s, `piccolo 0.3.3` recompilation) on this worktree
(toolchain `1.97.1`, `--allow-dirty`).

### Why `--allow-dirty` is used

The draft `docs/product/g1-publish-log.md` itself is untracked in this
worktree (the task directs to leave the worktree dirty and not commit until
review). Real `cargo publish` after final review must be run from a **clean**
tree on the merge commit without `--allow-dirty`; the flag here only allows
the checklist/log to coexist with the verification. The packaged output still
verifies from a clean tarball copy.

## `just check` evidence (2026-08-28, `ctx-0052/publish-g1` at `bbbdc1c`; historical `ctx-0047/publish-g1-actual` at `9eec31b`)

Re-verified at `bbbdc1c` (`ctx-0052/publish-g1`) after historical `ctx-0047/publish-g1-actual` at `9eec31b`. Executed after writing this draft log (worktree dirty, `--allow-dirty` for publish only; `just check` itself does not use `--allow-dirty`):

```text
cargo fmt --all -- --check -> PASS
cargo clippy --workspace --all-targets --locked -- -D warnings -> PASS (0 warnings)
cargo test --workspace --all-targets --locked -> PASS (708 passed, 0 failed; 704 prior + 4 new v0.1 proofs)
actionlint -color -> PASS
bunx --bun markdownlint-cli2@0.23.1 -> PASS (0 issues in 28 files, including this draft)
```

Full `just check` log is retained in the task checkpoint; the tail above is
the auditable summary. No formatting, lint, test, or markdown failures were
introduced by this draft log. The file count increased from 27 (CTX-0047 at `9eec31b`) to 28 (CTX-0052 at `bbbdc1c`; CTX-0051 added `docs/development/maintainability.md`).

## Future real-publish procedure (not executed here)

> Do not run `cargo publish` in this task. The checklist/log is intentionally
> left dirty; no `cargo publish` executed; verification was --dry-run only (token present but not used).

When the ladder (CTX-0044, merged) and the G1 evidence (CTX-0045 checklist +
this CTX-0047 log) are reviewed, the maintainer sequence is:

```bash
# from a clean checkout of main at the merge commit
cargo publish -p bitty-vt
cargo publish -p bitty-pty
cargo publish -p bitty-platform
cargo publish -p bitty-config
cargo publish -p bitty-package
cargo publish -p bitty-lua
# wait for crates.io index propagation, then:
cargo publish -p bitty-term-state   # Group 2 (after vt indexed)
cargo publish -p bitty-ui           # Group 3 (after term-state)
cargo publish -p bitty-render       # Group 3 (after term-state + platform)
```

Each step requires `cargo check --workspace --all-targets --locked` and
`just check` green on that commit, plus a fresh `--dry-run` for the next
crate. Do not use `--allow-dirty` for the real publish; ensure `git status`
is clean and the tag is `0.0.1` (deferring `0.1.0`). CI `Quality gates` +
`CodeQL` must be green before push per `AGENTS.md`.

No `cargo publish` executed in CTX-0047/CTX-0052; verification was --dry-run only (token present but not used); all
verifications were `cargo publish --dry-run --allow-dirty`.

## Next steps — actual `cargo publish` (after P0 review and `0.0.1` tag)

- **Next step (not in this PR):** actual `cargo publish` for G1 leaves in
  documented order `bitty-vt` → `bitty-pty` → `bitty-platform` →
  `bitty-config` → `bitty-package` → `bitty-lua` will be executed **after**
  **P0 review** (CTX-0053, prior CTX-0048, OQ-008/014/015/016/018/019/030/031/032)
  and the `0.0.1` tag on `main`, from a clean checkout without `--allow-dirty`.
  CTX-0052 re-verifies this order and confirms no `cargo publish` is executed
  without credentials; dry-run only.
- Order is fixed for repeatability per CTX-0047/CTX-0052 (Group 1 has no DAG
  edges; any order is valid for crates.io, but this sequence is the auditable
  path). `release-ladder.md` Groups 1→2→3 remain correct: Group 1 leaves
  (no internal `version = "0.0.1"` edge), Group 2 `term-state` after `vt`,
  Group 3 `ui`/`render` after `term-state` (+ `platform` for render).
- Gates before publish: `cargo check --workspace --all-targets --locked`
  PASS, `just check` PASS (28 files, 708 tests), fresh
  `cargo publish --dry-run` PASS for the next crate, and CI `Quality gates` +
  `CodeQL` green. CTX-0052 verified all three at `bbbdc1c` (see Revision
  history).
- Index propagation: allow crates.io index to settle before Group 2
  (`bitty-term-state` after `vt`); Group 3 (`bitty-ui`, `bitty-render`)
  after Group 2 is indexed. Tail crates remain `publish = false` at
  `0.0.1` (deferring `0.1.0`). No `--allow-dirty` for real publish; ensure
  `git status` clean and `Cargo.toml` `0.0.1` before tagging.

## Cross-reference and maintenance

- Release ladder and Group 2/3/4 detail:
  [`release-ladder.md`](./release-ladder.md) (CTX-0044, merged at `ff9715d`).
  This log is a companion to [`g1-publish-checklist.md`](./g1-publish-checklist.md)
  (CTX-0045); do not cite it without that ladder.
- Candidate spine provenance:
  [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
  (ChatGPT share `6a8dae4b-2aec-83ea-9174-03abc1f81531`, English rendering).
- Workspace topology DAG: [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
- Platform and compatibility bars: [ADR-0002](../../../bitty-docs/docs/decisions/adrs/ADR-0002-platform-support-tiers.md),
  compatibility milestone RFC.
- Security gates for `v1.0` remain normative in
  [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and
  [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md);
  this ladder does not weaken them.
- When a version slice is accepted via ADR/RFC, bump
  `workspace.package.version`, flip tail `publish` flags in DAG order, and
  add `version = "x.y.z"` pins on newly publishable edges (CTX-0043 pattern),
  then add a fresh dry-run column here and archive the prior log date.

## Revision history

- `2026-08-28` CTX-0052 `ctx-0052/publish-g1` at `bbbdc1c` — **re-verified** for
  `0.0.1`; workspace `0.0.1` verified (`Cargo.toml` + `version.workspace`);
  publish flags `vt/pty/platform/config/package/lua = true` retained; publish
  order `vt` → `pty` → `platform` → `config` → `package` → `lua` confirmed
  correct per `release-ladder.md` (Group 1 leaves unordered, G2 after `vt`,
  G3 after `term-state`); all 6 G1 leaves `cargo publish --dry-run
--allow-dirty` **PASS** (vt 97.8 KiB, pty 66.1 KiB, platform 124.4 KiB,
  config 120.7 KiB, package 153.8 KiB, lua 60.4 KiB, each `Finished dev` +
  `aborting upload due to dry run`); `just check` **PASS** (fmt, clippy 0
  warnings, 708 tests, actionlint, markdownlint 28 files); `cargo check`
  PASS; **no `cargo publish` executed; verification was --dry-run only (token present but not used)**; actual
  publish deferred **after P0 (CTX-0053)** and `0.0.1` tag from clean tree
  without `--allow-dirty`; worktree left **dirty** per task; companion
  `release-ladder.md` and `g1-publish-checklist.md` retained.
- `2026-08-28` CTX-0047 `ctx-0047/publish-g1-actual` — **finalized** for
  `0.0.1`; added Next steps: actual `cargo publish` order `vt` → `pty` →
  `platform` → `config` → `package` → `lua` will be done **after P0 review**
  (CTX-0048, OQ-008/014/015/016/018/019/030/031/032) and `0.0.1` tag on `main`,
  from clean checkout without `--allow-dirty`; no `cargo publish` executed
  in this PR, no credentials; `just check` re-verified PASS (27 files),
  6 leaves `cargo publish --dry-run --allow-dirty` PASS retained.
- `2026-08-28` CTX-0047 `ctx-0047/publish-g1-actual` at `9eec31b` — draft log
  created; workspace `0.0.1` verified; all 6 G1 leaves `cargo publish
--dry-run --allow-dirty` PASS (vt 97.8 KiB, pty 66.1 KiB, platform 124.4 KiB,
  config 120.7 KiB, package 153.8 KiB, lua 60.4 KiB); `just check` PASS
  (fmt, clippy 0 warnings, 708 tests, actionlint, markdownlint 27 files);
  no `cargo publish` executed, no credentials used, worktree left dirty per
  task; companion CTX-0045 checklist retained.
- `2026-08-28` CTX-0045 `ctx-0045/publish-g1` at `168493a` — draft checklist
  created; workspace `0.1.0` (now `0.0.1`) + publish flags verified; CTX-0044
  confirmed OPEN/MERGEABLE and non-blocking; all 6 G1 leaves dry-run PASS;
  `cargo check --workspace --all-targets` PASS; `just check` PASS.
