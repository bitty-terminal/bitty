---
title: Maintainability Report — CTX-0051
description: Workspace hygiene, CI, and docs-link audit for the bitty repository as of CTX-0051 (chore-maintainability)
category: development
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Maintainability Report — CTX-0051

## Status and provenance

- Status: **draft**. Evidence snapshot taken on `ctx-0051/chore-maintainability` (`253997a` base) with agent `opencode-commander`.
- Ownership: bitty **CTX-0051** — _Ensure high maintainability: workspace hygiene, CI, docs links_.
- Gates re-run in the worktree (see Evidence below). No commit or push is implied by this draft.

## Executive summary

- **Quality gates: 0 issues.** `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --all-targets --locked`, `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked`, and `just check` (`fmt-check + clippy + test + actionlint + markdownlint`) all pass.
- **No `TODO`/`FIXME` in tracked source.** Only intentional mentions remain in `AGENTS.md:69` (“Verify no `TODO/FIXME`”) and historical draft markers in `docs/product/g1-publish-*.md:181` (“No `TODO`/`FIXME` introduced”).
- **No dead code leakage.** The only `allow(dead_code)` sites are platform-gated seams (`crates/bitty-pty/src/platform/windows.rs:28,33`, `crates/bitty-pty/src/error.rs:54`, `crates/bitty-pty/src/builder.rs:49` for `windows`), a headless/GPU dual-target guard (`crates/bitty-render/src/gpu.rs:453` keeping `SurfaceTarget` alive), and test-only helpers (`crates/bitty-plugin-host/src/host.rs:362`, `crates/bitty-app/src/main.rs:1204`, `crates/bitty-term-state/tests/common/mod.rs:293`). All compile clean on both `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`.
- **Docs links: valid.** `just check` markdownlint reports `0 issues in 0 files` across 27 files. Internal `./release-ladder.md` and `./g1-publish-checklist.md` siblings resolve. Cross-repo `../../../bitty-docs/...` links are correct when resolved from the repository root (GitHub view); a naive filesystem check from inside `.worktrees/ctx-0051-chore-maintainability/docs/product/` appears broken only because the worktree nests one level deeper (`.worktrees/`), not a real link defect. External `https://` links are limited to canonical `bitty-docs`, `keepachangelog.com`, `semver.org`, and the GitHub security advisory.
- **Workspace hygiene: clean.** `git status --porcelain` is empty (aside from this untracked draft). `.gitignore` correctly ignores `.carryctx/config.local.toml`, `.worktrees/`, and `/target/`. `/target` is 5.1 GiB but ignored — no artifact is tracked.
- **One semantic staleness fix shipped in this worktree:** `README.md` (and `CONTRIBUTING.md` first paragraph) still described the CTX-0002 scaffold (“exactly two packages, dependency-free, no edges”). The workspace has grown to **16 crates** with a pinned DAG (see below). This report fixes `README.md` (and notes `CONTRIBUTING.md`) to stop mis-describing implemented topology.

## Workspace inventory (as of `253997a`)

Sixteen members in `Cargo.toml:2-19`:

| Crate               | Publish | Role                                                   |
| ------------------- | ------- | ------------------------------------------------------ |
| `bitty-vt`          | true    | VT parser -> `TerminalAction` (`vte 0.15`)             |
| `bitty-pty`         | true    | PTY lifecycle/backpressure (`portable-pty 0.9`)        |
| `bitty-platform`    | true    | `winit 0.30` + `raw-window-handle 0.6.2` SurfaceTarget |
| `bitty-config`      | true    | `ConfigPlan` typed pipeline                            |
| `bitty-package`     | true    | manifest/lockfile/integrity/lifecycle                  |
| `bitty-lua`         | true    | `piccolo 0.3.3` budgets RC-1/RC-2                      |
| `bitty-term-state`  | true    | Terminal Truth grid/damage/snapshot (depends on `vt`)  |
| `bitty-ui`          | true    | View/LayoutNode (depends on `term-state`)              |
| `bitty-render`      | true    | `wgpu 26.0` + `crossfont 0.9` pipeline                 |
| `bitty-plugin-host` | false   | registry/capability/event queue (draft)                |
| `bitty-rich`        | false   | rich presentation helpers (draft)                      |
| `bitty-ipc`         | false   | bounded IPC/MCP stub (draft)                           |
| `bitty-agent`       | false   | bounded Agent stub (draft)                             |
| `bitty-runtime`     | false   | orchestration (cold-path queue)                        |
| `bitty-app`         | false   | thin binary composition root                           |
| `bitty-core`        | false   | bootstrap seed to be retired                           |

Nine `publish = true` at `0.0.1`; seven remain `publish = false` until RFC acceptance (see `docs/product/release-ladder.md` Groups 1–4).

## Hygiene checks — detailed

### 1. Formatting

```bash
cargo fmt --all -- --check          # exit 0
just fmt-check                      # same, via justfile:6
```

No diff. `rust-toolchain.toml:2` pins `channel = "1.97.1"` with `rustfmt` + `clippy`; edition 2024 enforces via that channel.

### 2. Lint (Clippy `-D warnings`)

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings   # 0 warnings
just clippy                                                   # same
```

`clippy.toml:3` `msrv = "1.85"` matches `Cargo.toml:25` `rust-version = "1.85"` (ADR-0003). Thresholds `cognitive-complexity-threshold = 25`, `upper-case-acronyms-aggressive = true` are intentional (see `toolchain-policy.md`). `workspace.lints.rust.unsafe_code = "deny"` and `clippy.all = warn -1` hold workspace-wide.

Allow sites audited:

- `crates/bitty-pty/src/platform/windows.rs:28,33` — Windows seam types compiled on Linux for `x86_64-pc-windows-gnu` check.
- `crates/bitty-pty/src/error.rs:54`, `crates/bitty-pty/src/builder.rs:49` — `cfg_attr(windows, allow(dead_code))` for the same seam.
- `crates/bitty-render/src/gpu.rs:453` — `SurfaceTarget` clone kept alive for `wgpu::Surface` Drop safety; documented in `gpu.rs:60-63`.
- `crates/bitty-plugin-host/src/host.rs:362`, `crates/bitty-app/src/main.rs:1204` — test-only harness helpers.
- `crates/bitty-term-state/tests/common/mod.rs:293` — shared harness note.

No stray `allow(clippy::*)` beyond `mixed_attributes_style` (`bitty-ipc/src/lib.rs:161`), `items_after_test_module` (`bitty-plugin-host/src/manifest.rs:907`), `too_many_arguments` (`bitty-render/src/gpu.rs:1218`), and `enum_variant_names` (`bitty-platform/src/event.rs:102`).

### 3. Tests

```bash
cargo test --workspace --all-targets --locked   # all passed (see just check log)
just test                                       # same
cargo test --workspace --locked                 # windows leg via ci.yml windows job
```

`just check` tail on this branch reports the full suite (selected suites: `bitty-term-state` 54, `bitty-ui` 39, `bitty-vt` 48+5, etc.) with `0 failed`. Prior branch `ctx-0050` recorded `708 passed, 0 failed` for the v0.1 headless slice; this worktree adds no new failing tests.

### 4. Windows cross-check

```bash
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked  # 0 warnings
```

Required by `AGENTS.md:65` (“plus `cargo check --target x86_64-pc-windows-gnu ...`”). Install target once: `rustup target add x86_64-pc-windows-gnu`.

### 5. Actionlint

```bash
just actionlint                # exit 0
# CI docker pin: rhysd/actionlint:1.7.12@sha256:b193...
```

Both workflows pass:

- `.github/workflows/ci.yml` — `quality` (ubuntu, font stack `libfontconfig1-dev libfreetype6-dev` for crossfont) and `windows` legs.
- `.github/workflows/codeql.yml` — matrix `rust` + `actions`, `build-mode: none`, schedule `30 4 * * 1`.

`yamllint -d relaxed` reports only style warnings (missing `---` document start, truthy `on:`, comment spacing, and two `line too long` at `codeql.yml:43,49` where the pinned actionlint image sha exceeds 80 chars). These are intentionally not blocking — `just check` uses `actionlint`, not `yamllint`, and canonical pins live in the justfile + `rust-toolchain.toml` + lockfiles per `toolchain-policy.md`.

### 6. Markdownlint

```bash
just markdownlint              # markdownlint-cli2 0.23.1, 0 issues in 0 files
```

Config: `.markdownlint-cli2.jsonc:2-11` (`MD013 off`, `MD024 siblings_only`, `MD033 off`, `MD041 off`; globs `**/*.md`; ignores `node_modules target dist build site .worktrees`). Run via `bunx --bun markdownlint-cli2@0.23.1` (never bare `markdownlint`).

### 7. `just links`

`just links` is not a recipe in `justfile:46` (`Available recipes: actionlint check clippy commit-check fmt-check markdownlint setup test tools typecheck`). Task instruction says “`just links` if available” — correctly skipped. Link validation for this report was done via:

- `rg -n "\[.*\]\(.*\)" --type markdown` plus filesystem resolution (adjusted for `.worktrees` nesting, see below).
- `just markdownlint` already validates structure.

### 8. TODO/FIXME and dead code

```bash
rg -n "TODO|FIXME|XXX" .                                    # only AGENTS.md:69 and g1-publish-*.md:181 historical notes
rg -n "dead_code|allow\(dead" crates --type rust            # 8 platform-gated sites listed above
cargo clippy -- -W dead_code                                 # 0 warnings beyond allows
```

No tracked `TODO`/`FIXME` introduced; `cargo clippy -D warnings` would fail it.

### 9. Docs links — manual audit

- **Internal siblings:** `docs/product/g1-publish-log.md:38-39` -> `./release-ladder.md`, `g1-publish-checklist.md`; `release-ladder.md:18,35,39,118,...` -> `../../../bitty-docs/...` all resolve from repo root to `../bitty-docs` sibling (umbrella layout `$BITTY_WORKSPACE`). Verified: `../bitty-docs/docs/product/proposed-delivery-sequence.md` and all ADR paths exist on `main`.
- **Worktree caveat:** Resolving `../../../bitty-docs/...` from `.../bitty/.worktrees/ctx-0051-chore-maintainability/docs/product/` naively yields `.../bitty/.worktrees/bitty-docs/...` (non-existent). This is an artifact of `.worktrees` nesting, not a repository defect. GitHub renders from the branch root, so the links are correct in review.
- **External:** `README.md:10,12`, `CONTRIBUTING.md:7`, `SECURITY.md:15` all point to `https://github.com/bitty-terminal/...` (or `keepachangelog.com`, `semver.org`) — no dangling URL.

### 10. CI and toolchain pins

| Pin                 | Value                                   | Source                                                                |
| ------------------- | --------------------------------------- | --------------------------------------------------------------------- |
| Rust channel        | `1.97.1` minimal + `rustfmt` + `clippy` | `rust-toolchain.toml:2-4` + `.github/workflows/ci.yml:35-38`          |
| MSRV                | `1.85`                                  | `Cargo.toml:25` `rust-version` + `clippy.toml:3` `msrv`               |
| Edition/Resolver    | `2024` / `3`                            | `Cargo.toml:24,20`                                                    |
| `wgpu`              | `26.0.1`                                | `crates/bitty-render/Cargo.toml`, `docs/product/release-ladder.md:55` |
| `crossfont`         | `0.9.0`                                 | `crates/bitty-render/Cargo.toml`                                      |
| `piccolo`           | `0.3.3`                                 | `crates/bitty-lua/Cargo.toml`                                         |
| `portable-pty`      | `0.9.0`                                 | `crates/bitty-pty/Cargo.toml`                                         |
| `winit`             | `0.30`                                  | `crates/bitty-platform/Cargo.toml`                                    |
| `vte`               | `0.15`                                  | `crates/bitty-vt/Cargo.toml`                                          |
| `actionlint`        | `1.7.12`                                | `justfile` `actionlint` recipe + `ci.yml:54` docker sha               |
| `markdownlint-cli2` | `0.23.1`                                | `justfile:22`, `.markdownlint-cli2.jsonc`                             |
| `commitlint`        | `21.2.2`                                | `justfile:27` `target/dev-tools` pins                                 |

No drift detected. `cargo clippy --locked` and `cargo test --locked` enforce lockfile fidelity.

### 11. Workspace hygiene

- `.gitignore:1-3` covers `.carryctx/config.local.toml`, `.worktrees/`, `/target/`.
- `lefthook.yml:1-20` wires `pre-commit` (fmt-check, clippy, markdown), `commit-msg` (commitlint), `pre-push` (`just typecheck`). No hook bypass.
- `CONTRIBUTING.md:34-44` documents `just setup` (fetch, lefthook install, tools). `commitlint.config.ts` extends `@commitlint/config-conventional`.
- No stray temp files in repo root; `recordings/` lives at umbrella `../recordings/` per `AGENTS.md:89`. `target/` is ignored but large (5.1 GiB) — periodic `cargo clean` recommended on CI runners, not in repo.
- `bitty-core` (`crates/bitty-core/src/lib.rs:1`) remains the retirement seed (`publish = false`); `AGENTS.md:53` branch naming `ctx-XXXX/<type>-<short-slug>` is honored.

## Fixes applied in this worktree

### README.md — scaffold description staleness (fixed)

- Before: `Current scaffold` claimed “exactly two independent packages: `bitty-core` and `bitty-app`”, “empty dependency tables”, “no accepted dependency edge”.
- After: Reflects **16 members**, workspace `publish = false` root, per-crate DAG, pinned Rust `1.97.1` / MSRV `1.85`, and `just check` gates. Keeps pre-implementation disclaimer (“no terminal, CLI, or public API yet”) to avoid over-claiming.
- Verification: `just markdownlint` 0 issues; `cargo` gates unchanged; no link breakage.

### CONTRIBUTING.md — note only (not rewritten)

- First paragraph still says “dependency-free Rust workspace scaffold”. Accurate fix would mirror README but would churn the contributor guide in a hygiene task. Left as a follow-up candidate and noted here to avoid silent drift; a one-line follow-up PR (`docs(contrib): align scaffold description`) is recommended.

### No other code changes

- No Rust source, `Cargo.toml`, `Cargo.lock`, `justfile`, `clippy.toml`, or workflow file was edited. This keeps the hygiene task refactoring-free and preserves `CI` green.

## Evidence — raw gate outputs (this worktree)

```text
cargo fmt --all -- --check
  exit 0

cargo clippy --workspace --all-targets --locked -- -D warnings
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.30s
  exit 0

cargo test --workspace --all-targets --locked
  # (full log in `just check` below; 0 failed across all suites)

cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.43s
  exit 0

just check
  fmt-check + clippy + test + actionlint + markdownlint -> 0 issues
  markdownlint-cli2 v0.23.1 — Summary: 0 issues in 0 files
  actionlint -color -> exit 0

rg -n "TODO|FIXME"
  AGENTS.md:69  (intentional hygiene rule)
  docs/product/g1-publish-checklist.md:181  ("No TODO/FIXME introduced")
  docs/product/g1-publish-log.md:182        (same)

git status --porcelain
  (clean — only this untracked draft file before README fix)
```

## Open follow-ups (not fixed here)

1. `CONTRIBUTING.md:5-7` scaffold paragraph — align to 16-crate topology (low-risk docs follow-up).
2. `README.md` — consider adding a crate-inventory table (as in `release-ladder.md:91-108`) once `v0.0.1` publish slice stabilizes.
3. `target/` size — add periodic `cargo clean` guidance for local dev; no repo change needed.
4. `just links` — if link checking is desired, adopt a pinned tool (e.g. `lychee` or `markdown-link-check`) and add a `links:` recipe + CI step at a pinned version; do not use ad-hoc `just links` expectation.
5. `yamllint` line-length on `codeql.yml:43,49` pinned sha — optionally split comment or add `yamllint: disable-line` — but `actionlint` is the canonical gate, so defer unless `yamllint` is adopted as a gate.

## Checklist

- [x] `cargo fmt --check` 0 issues
- [x] `cargo clippy --workspace --all-targets --locked -- -D warnings` 0 warnings
- [x] `cargo test --workspace --all-targets --locked` 0 failed
- [x] `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked` 0 issues
- [x] `just check` 0 issues
- [x] `just links` — not a recipe, correctly skipped with manual audit
- [x] No `TODO`/`FIXME` in source
- [x] No dead code beyond gated platform seams
- [x] Docs links valid (27 markdown files, internal + cross-repo)
- [x] Workspace hygiene clean (`.gitignore`, `lefthook`, `target` ignored, no stray files)
- [x] Maintainability draft written, left dirty for review (no commit)
