---
title: G1 Publish Checklist — Leaf Crates Dry-Run
description: Draft checklist verifying workspace 0.1.0, publish flags, and cargo publish dry-run for G1 leaf crates (vt, pty, platform, config, package, lua) with publish order and gates
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# G1 Publish Checklist — Leaf Crates Dry-Run

## Status and provenance

- Status: **draft**. Preparation artifact only. No `cargo publish` was
  executed, no credentials were used, and no crates.io upload occurred.
  This checklist records pre-publish verification for independent review
  before any actual publish.
- Ownership: bitty **CTX-0045** — branch `ctx-0045/publish-g1`.
- Companion records:
  - **CTX-0043** `chore(crate): prepare workspace for crates.io v0.1.0`
    — merged as `168493a` / PR #74 — set `workspace.package.version`
    `0.0.0 -> 0.1.0`, added workspace `description`/`license`/`repository`
    /`keywords`/`categories`, set per-crate `publish` flags and `description`,
    and added `version = "0.1.0"` pins on publishable internal `path` deps.
  - **CTX-0044** `docs(product): add release ladder v0.1-v1.0 and publish
order` — PR #75 (`ctx-0044/docs-release-ladder`, commit `5a3322a`) —
    **pending** (OPEN, MERGEABLE) at the time of this checklist. CTX-0044
    is **not blocking** the G1 leaf dry-run: leaves have no internal
    workspace dependency with `version = "0.1.0"` and verify independently.
    This checklist overlays the concrete Group 1 dry-run evidence on that
    ladder without duplicating or re-approving it.
- Provenance: builds on the candidate `v0.1` shell slice and publish-order
  DAG documented in `docs/product/release-ladder.md` (CTX-0044) and the
  canonical workspace topology in
  [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
  No roadmap promise is admitted here; version numbers are maturity labels.
- Authority: if CTX-0044 is revised before merge, reconcile the order and
  gate table below with that revision. Closing any open question still
  requires its RFC/ADR per the open-question register.

## Scope

- **G1 leaves** (this checklist): the six crates with `publish = true` and
  no workspace `version = "0.1.0"` path edge — `bitty-vt`, `bitty-pty`,
  `bitty-platform`, `bitty-config`, `bitty-package`, `bitty-lua`. Each can
  be verified with `cargo publish --dry-run --allow-dirty` without waiting
  for crates.io index propagation beyond external crates.
- Out of scope for G1: `bitty-term-state` (Group 2, depends on `vt`),
  `bitty-ui`/`bitty-render` (Group 3, after `term-state` + `platform`),
  and the draft tail (`plugin-host`, `rich`, `ipc`, `agent`, `runtime`,
  `app`, `core`) which remain `publish = false` at `0.1.0`. Their order
  and gates are documented in the release ladder, not re-verified here.

## Pre-publish workspace verification

### Workspace version

- `Cargo.toml` `[workspace.package] version = "0.1.0"` — verified in
  `ctx-0045/publish-g1` at `168493a` (inherited from CTX-0043). No drift.
- `[workspace.package]` carries `edition = "2024"`,
  `rust-version = "1.85"`, `publish = false` at the workspace root,
  plus `description`, `license = "MIT OR Apache-2.0"`,
  `repository = "https://github.com/bitty-terminal/bitty"`,
  `keywords`/`categories` — CTX-0043 metadata retained.

```toml
[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
publish = false
```

- Per-crate `version.workspace = true` inherits `0.1.0`; G1 leaves set
  `publish = true` with their own `description`/`license`/`repository`
  and no internal `path` dependency requiring `version = "0.1.0"`.

### Publish flags (as of `168493a`, verified `2026-08-28`)

| Crate               | `publish` | Internal workspace deps                                              | Expected at `0.1.0` |
| ------------------- | --------- | -------------------------------------------------------------------- | ------------------- |
| `bitty-vt`          | `true`    | none                                                                 | publish in G1       |
| `bitty-pty`         | `true`    | none                                                                 | publish in G1       |
| `bitty-platform`    | `true`    | none                                                                 | publish in G1       |
| `bitty-config`      | `true`    | none                                                                 | publish in G1       |
| `bitty-package`     | `true`    | none                                                                 | publish in G1       |
| `bitty-lua`         | `true`    | none (`piccolo = "0.3.3"` external only)                             | publish in G1       |
| `bitty-term-state`  | `true`    | `bitty-vt = "0.1.0"`                                                 | Group 2             |
| `bitty-ui`          | `true`    | `bitty-term-state = "0.1.0"`                                         | Group 3             |
| `bitty-render`      | `true`    | `bitty-term-state = "0.1.0"`, `bitty-platform = "0.1.0"`             | Group 3             |
| `bitty-plugin-host` | `false`   | `term-state`, `config`, `package`                                    | tail                |
| `bitty-rich`        | `false`   | `term-state`, `vt`                                                   | tail                |
| `bitty-ipc`         | `false`   | none                                                                 | tail                |
| `bitty-agent`       | `false`   | none                                                                 | tail                |
| `bitty-runtime`     | `false`   | `vt`, `term-state`, `pty`, `render`, `platform`, `ui`, `plugin-host` | tail                |
| `bitty-app`         | `false`   | `platform`, `runtime`                                                | binary, never       |
| `bitty-core`        | `false`   | none                                                                 | seed to retire      |

### CTX-0044 pending check

- `gh pr view 75 --repo bitty-terminal/bitty --json state,mergeable` on
  `2026-08-28` returned `state: OPEN`, `mergeable: MERGEABLE`.
  Title: `docs(product): add release ladder v0.1-v1.0 and publish order
(CTX-0044)`. Base `main` is `168493a` (CTX-0043 merged); CTX-0044 worktree
  head is `5a3322a` adding `docs/product/release-ladder.md`.
- Conclusion: pending review is expected and does **not** block G1 leaf
  `cargo publish --dry-run` — leaves verify independently and were
  previously verified in CTX-0043 the same way. This checklist records
  fresh dry-runs on the `ctx-0045/publish-g1` worktree at `168493a`.

## Publish order

### Task-specified G1 order (within Group 1)

Group 1 crates are unordered with respect to each other (DAG has no edge
between them). For repeatability, CTX-0045 fixes the invocation sequence
to the order stated in its task title and description:

1. `bitty-vt`
2. `bitty-pty`
3. `bitty-platform`
4. `bitty-config`
5. `bitty-package`
6. `bitty-lua`

Within a real crates.io publish, these six may be published in any order;
the sequence above is used for this checklist so reviewers see a single
reproducible path. No index propagation wait is required between them for
dry-run; for a real publish, allow index propagation before starting
Group 2.

### Full ladder context (for traceability — see `release-ladder.md`)

- **Group 1 — Leaves** (`publish = true`, no workspace `version` edge):
  `vt`, `pty`, `platform`, `config`, `package`, `lua` — this checklist.
- **Group 2 — Terminal Truth**: `bitty-term-state` (depends on `vt`).
  Publish only after `vt` is on crates.io. CTX-0043 dry-run correctly
  reported missing index for this group; re-verify with
  `cargo publish --dry-run -p bitty-term-state --allow-dirty` once Group 1
  is indexed (expected to PASS after `vt` is published).
- **Group 3 — Presentation branch** (parallel after Group 2):
  `bitty-ui` (after `term-state`) and `bitty-render` (after `term-state` +
  `platform`). May publish concurrently once prerequisites are indexed.
  `bitty-vt` dev-dependency in `bitty-render` does not impose ordering.
- **Group 4 — Tail** (`publish = false` at `0.1.0`): `plugin-host`, `rich`,
  `ipc`, `agent`, `runtime`, `app`, `core`. Not publishable at `0.1.0`;
  promotion requires RFC acceptance and DAG-order `publish` flips plus
  `version = "x.y.z"` pins (CTX-0043 pattern). `runtime`/`app` are validated
  via workspace headless integration tests, not via a crates.io release.

## Verification gates (must PASS before any real publish)

| Gate                         | Command                                                                                             | Result on `ctx-0045/publish-g1` at `168493a` (2026-08-28)                                                                  |
| ---------------------------- | --------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `cargo check`                | `cargo check --workspace --all-targets --locked`                                                    | **PASS** — `Finished dev profile` in 0.07s (full run 19.18s for `--workspace --all-targets` without `--locked`); no errors |
| `just check`                 | `just check` (`fmt-check` + `clippy -D warnings` + `test --locked` + `actionlint` + `markdownlint`) | **PASS** — 0 issues                                                                                                        |
| └ `cargo fmt --check`        | via `just fmt-check`                                                                                | PASS — no diff                                                                                                             |
| └ `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings`                                    | PASS — 0 warnings                                                                                                          |
| └ `cargo test`               | `cargo test --workspace --all-targets --locked`                                                     | PASS — all suites ok (vt 48, term-state 54+3+4+4, ui 39, platform/platform-adjacent, etc.) — see log below                 |
| └ `actionlint`               | `actionlint -color`                                                                                 | PASS                                                                                                                       |
| └ `markdownlint`             | `bunx --bun markdownlint-cli2@0.23.1` (`**/*.md`)                                                   | PASS — `0 issues in 0 files` — now covers 25 files including this draft                                                    |
| publish metadata             | `cargo publish --dry-run --allow-dirty` per G1 leaf                                                 | **PASS all 6** — see dry-run table                                                                                         |
| version/publish flag drift   | `grep` of `Cargo.toml` workspace + per-crate                                                        | PASS — `workspace.package.version 0.1.0`, G1 leaves `publish = true`, rest as table above                                  |

`just check` evidence (tail):

```text
markdownlint-cli2 v0.23.1 (markdownlint v0.41.1)
Finding: **/*.md !node_modules !target !dist !build !site !.worktrees
Linting: 24 files  -> 25 files with this draft
Summary: 0 issues in 0 files
actionlint -color  -> PASS
cargo test --workspace --all-targets --locked  -> all PASS (see excerpt)
cargo fmt --all -- --check -> PASS
cargo clippy --workspace --all-targets --locked -- -D warnings -> PASS
cargo check --workspace --all-targets --locked -> PASS
```

No `TODO`/`FIXME` introduced; frontmatter `status: draft` retained.

## G1 leaf `cargo publish --dry-run` results

Executed in worktree `/mnt/data/Workspace/Projects/bitty-terminal/bitty/.worktrees/ctx-0045-publish-g1`
at `168493a`, toolchain `1.97.1` / `rust-toolchain.toml`, with `--allow-dirty`
so the untracked draft checklist does not block verification. Mirrors the
CTX-0043 dry-run pattern and is skipped forcredentialled `cargo publish`.

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
(`target/package/<crate>-0.1.0`) with `Finished dev profile` and
`warning: aborting upload due to dry run` (expected for `--dry-run`).

### Results table

| #   | Crate            | Version | `cargo publish --dry-run --allow-dirty` | Packaged                                  | Verified build                                                                        |
| --- | ---------------- | ------- | --------------------------------------- | ----------------------------------------- | ------------------------------------------------------------------------------------- |
| 1   | `bitty-vt`       | `0.1.0` | **PASS**                                | 23 files, 97.8 KiB (22.5 KiB compressed)  | `Compiling bitty-vt ... Finished dev` — `vte 0.15`                                    |
| 2   | `bitty-pty`      | `0.1.0` | **PASS**                                | 14 files, 66.1 KiB (19.5 KiB compressed)  | `Compiling bitty-pty ... Finished dev` — `portable-pty 0.9`                           |
| 3   | `bitty-platform` | `0.1.0` | **PASS**                                | 12 files, 124.4 KiB (33.6 KiB compressed) | `Compiling bitty-platform ... Finished dev` — `winit 0.30`, `raw-window-handle 0.6.2` |
| 4   | `bitty-config`   | `0.1.0` | **PASS**                                | 13 files, 120.7 KiB (24.9 KiB compressed) | `Compiling bitty-config ... Finished dev`                                             |
| 5   | `bitty-package`  | `0.1.0` | **PASS**                                | 13 files, 153.8 KiB (33.8 KiB compressed) | `Compiling bitty-package ... Finished dev`                                            |
| 6   | `bitty-lua`      | `0.1.0` | **PASS**                                | 6 files, 58.1 KiB (14.0 KiB compressed)   | `Compiling bitty-lua ... Finished dev` — `piccolo 0.3.3`                              |

All six: **dry-run PASS**. No metadata errors, no missing `description`/
`license`/`repository`, no unpublished internal path dep errors (expected
only for Groups 2-3), no `publish = false` block. Warnings are only the
intentional `aborting upload due to dry run`.

Raw log excerpt (`bitty-vt` representative; all 6 analogous):

```text
Updating crates.io index
Packaging bitty-vt v0.1.0 (.../crates/bitty-vt)
Updating crates.io index
Packaged 23 files, 97.8KiB (22.5KiB compressed)
Verifying bitty-vt v0.1.0 (.../target/package/bitty-vt-0.1.0)
 Compiling arrayvec v0.7.8
 Compiling memchr v2.8.3
 Compiling vte v0.15.0
 Compiling bitty-vt v0.1.0 (.../target/package/bitty-vt-0.1.0)
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.00s
 Uploading bitty-vt v0.1.0 (.../crates/bitty-vt)
warning: aborting upload due to dry run
```

Equivalent `Finished` lines were observed for `bitty-pty` (5.96 s), `bitty-platform`
(19.21 s, full winit/Wayland recompilation in the verification sandbox),
`bitty-config` (2.25 s), `bitty-package` (2.26 s), `bitty-lua` (7.79 s,
`piccolo 0.3.3` recompilation).

### Why `--allow-dirty` is used

The draft `docs/product/g1-publish-checklist.md` itself is untracked in this
worktree (the task directs to leave the worktree dirty and not commit). Real
`cargo publish` after final review must be run from a **clean** tree on the
merge commit of CTX-0044 + CTX-0045 without `--allow-dirty`; the flag here only
allows the checklist to coexist with the verification. The packaged output
still verifies from a clean tarball copy.

## Future real-publish procedure (not executed here)

> Do not run `cargo publish` in this task. The checklist is intentionally
> left dirty; no credentials are configured in this environment.

When the ladder (CTX-0044) and this checklist (CTX-0045) are both reviewed
and merged, the maintainer sequence is:

```bash
# from a clean checkout of main at the merge commit (CTX-0043 + CTX-0044 + CTX-0045)
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
is clean and the tag is `0.1.0`. CI `Quality gates` + `CodeQL` must be green
before push per `AGENTS.md`.

## Cross-reference and maintenance

- Release ladder and Group 2/3/4 detail:
  [`release-ladder.md`](./release-ladder.md) (CTX-0044 draft, PR #75).
  This checklist is a companion; do not cite it without that ladder.
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
  then add a fresh dry-run column here and archive the prior checklist date.

## Revision history

- `2026-08-28` CTX-0045 `ctx-0045/publish-g1` at `168493a` — draft checklist
  created; workspace `0.1.0` + publish flags verified; CTX-0044 PR #75
  confirmed OPEN/MERGEABLE and non-blocking; all 6 G1 leaves dry-run PASS;
  `cargo check --workspace --all-targets` PASS; `just check` PASS.
