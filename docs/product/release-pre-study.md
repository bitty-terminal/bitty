---
title: Release Pre-Study — Crates, Binary Preview, Version Pinning, Ladder v0.1
description: Research for CTX-0115 gathering crates.io leaf dry-run, binary preview nightly, version pinning, and release ladder v0.1 evidence at 0.0.1 without publish or binary release
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Release Pre-Study — Crates, Binary Preview, Version Pinning, Ladder v0.1

## Status and provenance

- Status: **draft**. Research artifact only. No `cargo publish` was executed
  beyond `--dry-run`, no `CARGO_REGISTRY_TOKEN` was used, and no GitHub
  Release binary was published. This study gathers evidence for independent
  review before any real crates.io upload or binary preview.
- Ownership: bitty **CTX-0115** — branch `carryctx/ctx-0115` at `8c88f3a`
  (base `8c88f3a` — CTX-0114 compat matrix, prior `1d9eb6a` stability, `b3c72c9` mail-panel).
  Worktree: `.worktrees/ctx-0115` under `bitty/.worktrees/ctx-0115`.
  Agent: `core-implementer-0115`, session `01M1EMVK`.
- Task: `Research release preparation (crates and binary)` — verify leaf
  dry-run, sketch binary preview nightly, audit version pinning, and map
  `v0.1` ladder. Keep **docs only**, no actual publish, no binary release.
  Gates: `just check`, `cargo audit`, no publish.
- Priority: **P2** | Area: **release** | Labels: `chore,area:release,P2` |
  Milestone: **v0.1.0** | RFC: **OQ-001** | Task: **CTX-0115** / **REL-001** |
  Closes: `Closes #<issue>` (PR body must carry the owning Issue number per `AGENTS.md` hygiene).
- Companion records:
  - **CTX-0043** `chore(crate): prepare workspace for crates.io v0.1.0` — PR #74 (`168493a`) —
    set `workspace.package.version 0.0.0 -> 0.1.0`, added workspace
    `description`/`license`/`repository`/`keywords`/`categories`, per-crate
    `publish` flags and `version = "0.1.0"` pins on publishable edges (now `0.0.1` via CTX-0049).
  - **CTX-0044** `docs(product): add release ladder v0.1-v1.0` — PR #75 (`5a3322a`/`ff9715d` merged at `9eec31b`) —
    crate publish order Groups 1-4 and version mapping.
  - **CTX-0045** `docs(product): add G1 publish checklist` — PR #76 (`83fd63b`) — six leaves dry-run at `168493a`.
  - **CTX-0047/0052/0062/0066** `docs(product): publish G1 logs` — at `9eec31b`/`bbbdc1c`/`ffd3eee`/`c465888` —
    re-verified leaves at `0.0.1` with `808` soak tests and `act -n` gate at `c465888`.
  - **CTX-0049** `chore(version): adjust workspace to 0.0.1 (earliest)` — PR #77 (`956926c`) —
    earliest publish `0.0.1`, deferring `0.1.0` until plugins etc. are more complete.
  - **CTX-0050** `feat(runtime): add v0.1 minimal terminal slice` — PR #78 (`9eec31b`) —
    headless shell echo / resize / backpressure proof (708 tests at that head).
- Authority: if `release-ladder.md` is revised, reconcile this study with that
  revision. Closing any open question still requires its RFC/ADR per the
  open-question register. No roadmap promise is admitted here; version numbers
  are maturity labels per
  [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
  and the topology in
  [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).

## Scope and non-goals

- **In scope for this pre-study (docs only):**
  - Audit workspace version pinning at `0.0.1` (workspace + per-crate + `Cargo.lock` +
    internal `version = "0.0.1"` pins) and toolchain pins.
  - Re-verify **G1 leaves** `cargo publish --dry-run --allow-dirty` (six crates:
    `vt`, `pty`, `platform`, `config`, `package`, `lua`) and record Group 2/3
    expected ordering failures.
  - Research **binary preview nightly** for `bitty-app` (binary never on crates.io)
    as a GitHub Releases nightly channel — workflow shape, tag/artifact naming,
    platform matrix, retention, provenance — without publishing a binary.
  - Map **release ladder `v0.1`** (candidate `v0.1` minimal terminal slice) to
    workspace crate focus and verification gates at this base.
- **Out of scope / explicitly not executed in this task:**
  - Any real `cargo publish` (with or without `--allow-dirty`), any `cargo publish`
    from a clean checkout, any `CARGO_REGISTRY_TOKEN` use, any crates.io index wait.
  - Any real binary release, GitHub Release creation, tag push, artifact upload,
    or `cargo build --release` publish. `bitty-app` stays `publish = false`.
  - Closing OQ-001 (or any OQ) — budgets remain normative in the
    [Performance Budget RFC](../../../bitty-docs/docs/specifications/performance-budget-rfc.md).

## Inventory and publish flags (as of `8c88f3a`)

Sixteen members in `bitty/Cargo.toml:2-19` (edition 2024, resolver 3):

| Crate               | Publish | Internal workspace `version = "0.0.1"` deps                          | Role at `8c88f3a`                                      |
| ------------------- | ------- | -------------------------------------------------------------------- | ------------------------------------------------------ |
| `bitty-vt`          | `true`  | none                                                                 | VT parser -> `TerminalAction` (`vte 0.15`)             |
| `bitty-pty`         | `true`  | none                                                                 | PTY lifecycle/backpressure (`portable-pty 0.9`)        |
| `bitty-platform`    | `true`  | none                                                                 | `winit 0.30` + `raw-window-handle 0.6.2` SurfaceTarget |
| `bitty-config`      | `true`  | none                                                                 | `ConfigPlan` typed pipeline                            |
| `bitty-package`     | `true`  | none                                                                 | manifest/lockfile/integrity/lifecycle                  |
| `bitty-lua`         | `true`  | none (`piccolo 0.3.3` external only)                                 | deterministic VM budgets RC-1/RC-2                     |
| `bitty-term-state`  | `true`  | `bitty-vt = "0.0.1"`                                                 | Terminal Truth grid/damage/snapshot                    |
| `bitty-ui`          | `true`  | `bitty-term-state = "0.0.1"`                                         | View/LayoutNode primitives                             |
| `bitty-render`      | `true`  | `bitty-term-state = "0.0.1"`, `bitty-platform = "0.0.1"`             | `wgpu 26.0` + `crossfont 0.9` pipeline                 |
| `bitty-plugin-host` | `false` | `term-state`, `config`, `package`                                    | registry/capability/event queue (draft)                |
| `bitty-rich`        | `false` | `term-state`, `vt`                                                   | rich presentation helpers (draft)                      |
| `bitty-ipc`         | `false` | none                                                                 | bounded IPC/MCP stub (draft)                           |
| `bitty-agent`       | `false` | none                                                                 | bounded Agent stub (draft)                             |
| `bitty-runtime`     | `false` | `vt`, `term-state`, `pty`, `render`, `platform`, `ui`, `plugin-host` | orchestration (cold-path queue)                        |
| `bitty-app`         | `false` | `platform`, `runtime`                                                | thin binary composition root — never published         |
| `bitty-core`        | `false` | none                                                                 | bootstrap seed to be retired                           |

Nine `publish = true` at `0.0.1` (six G1 leaves + `term-state`/`ui`/`render`);
seven remain `publish = false` until RFC acceptance. Verified via
`grep -R publish Cargo.toml crates/*/Cargo.toml` on this worktree at `8c88f3a`.

Binary crates: only `bitty-app` is a binary (`crates/bitty-app/src/main.rs`
composition root, `publish = false`). `bitty-compat-lab` and `bitty-perf` are
publish-false harness owners, not release artifacts.

## Version pinning (audit at `8c88f3a`)

### Workspace version

```toml
[workspace.package]
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
publish = false
description = "Bitty terminal workspace"
license = "MIT OR Apache-2.0"
repository = "https://github.com/bitty-terminal/bitty"
keywords = ["terminal", "emulator", "pty", "vt", "bitty"]
categories = ["command-line-utilities", "emulators"]
```

- `Cargo.toml:22` `version = "0.0.1"` — inherited from CTX-0049 (`956926c`),
  retained through soak (CTX-0067 `d4af44e`), AGENTS `act -n` gate (CTX-0074 `c465888`),
  compat matrix (CTX-0114 `8c88f3a`). No drift at this head.
- `[workspace.package]` carries `edition = "2024"`, `rust-version = "1.85"`,
  `publish = false` at workspace root, plus `description`/`license`/`repository`/
  `keywords`/`categories` — CTX-0043 metadata retained, corrected in CTX-0049 to `0.0.1`.

### Per-crate `version.workspace` and `publish` metadata

- Every crate sets `version.workspace = true`, inheriting `0.0.1`.
  `Cargo.lock:4` shows `version = "0.0.1"` for `bitty-app` and all workspace members
  (344 crate dependencies scanned by `cargo audit` at this head).
- Publishable crates carry their own `description`/`license = "MIT OR Apache-2.0"`/
  `repository = "https://github.com/bitty-terminal/bitty"` and `keywords`/`categories`
  (e.g. `bitty-vt` `parsing`/`text-processing`, `bitty-pty` `portable-pty 0.9`,
  `bitty-platform` `winit 0.30`/`raw-window-handle 0.6.2`, `bitty-lua` `piccolo 0.3.3`).
  Draft crates keep `publish = false` but still carry `description`/`license`/`repository`
  for consistency.

### Internal `path` edges with `version = "0.0.1"` pins

G1 leaves have **no** internal `version` edge and verify independently.
Second-order crates correctly pin their prerequisites so `cargo publish`
requires the dependency already on crates.io at `^0.0.1`:

- `bitty-term-state` -> `bitty-vt = "0.0.1"` (`crates/bitty-term-state/Cargo.toml:14`)
- `bitty-ui` -> `bitty-term-state = "0.0.1"` (`crates/bitty-ui/Cargo.toml:12`)
- `bitty-render` -> `bitty-term-state = "0.0.1"` + `bitty-platform = "0.0.1"`
  (`crates/bitty-render/Cargo.toml:16-17`) plus dev-only `bitty-vt = "0.0.1"`
  (`crates/bitty-render/Cargo.toml:26`) — does **not** impose ordering beyond
  `term-state`/`platform` per ladder Group 3 note.
- `bitty-compat-lab` test harness also pins `vt` + `term-state` at `0.0.1`
  dev edges (`crates/bitty-compat-lab/Cargo.toml`).

Tail crates that remain `publish = false` keep plain `path` edges without
`version` pins (e.g. `bitty-runtime` depends on `vt`/`term-state`/`pty`/`render`/
`platform`/`ui`/`plugin-host` via `path` only, `bitty-app` on `platform`/`runtime`
via `path` only) — they are validated via workspace headless tests, not via a
crates.io `runtime`/`app` release. When promotion occurs, the CTX-0043 pattern
adds `version = "x.y.z"` pins in DAG order (host before runtime, runtime before app if ever published).

### Lockfile fidelity

- `cargo test --workspace --all-targets --locked`, `cargo clippy --locked`,
  and `cargo check --locked` all enforce the published `Cargo.lock` at `8c88f3a`.
  No `--offline` drift or `version = "0.0.1"` mismatch was observed.
- `cargo deny check` advisory line (supply-chain job) and `cargo audit` both scan
  `Cargo.lock` (344 crates) — see Verification gates below.

## Crate leaf dry-run (G1 at `8c88f3a`)

### Why dry-run only and why `--allow-dirty`

The draft `docs/product/release-pre-study.md` itself is an untracked/dirty
delta in this worktree (task directs docs-only, leave worktree dirty for review;
real `cargo publish` must run from a **clean** checkout at tag `v0.0.1` without
`--allow-dirty`). Real publish after review must be from a clean `main` at the
merge commit of CTX-0044 + CTX-0045 + this study, without `--allow-dirty`
(see Future real-publish procedure). The packaged output still verifies from a
clean tarball copy (`target/package/<crate>-0.0.1`).

### Commands (task-specified order, Group 1 unordered in DAG)

```bash
cargo publish --dry-run -p bitty-vt --allow-dirty
cargo publish --dry-run -p bitty-pty --allow-dirty
cargo publish --dry-run -p bitty-platform --allow-dirty
cargo publish --dry-run -p bitty-config --allow-dirty
cargo publish --dry-run -p bitty-package --allow-dirty
cargo publish --dry-run -p bitty-lua --allow-dirty
```

Run independently; each leaf verifies from the packaged tarball with
`Finished dev profile` and `warning: aborting upload due to dry run` (expected
for `--dry-run`). Executed on this worktree at `8c88f3a`, toolchain `1.97.1`.

### Results table (at `8c88f3a`, `ctx-0115`, `rust-toolchain.toml` `1.97.1`)

| #   | Crate            | Version | `cargo publish --dry-run --allow-dirty` | Packaged                                  | Verified build                                                                                                                         |
| --- | ---------------- | ------- | --------------------------------------- | ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `bitty-vt`       | `0.0.1` | **PASS**                                | 24 files, 118.4 KiB (27.7 KiB compressed) | `Compiling bitty-vt ... Finished dev` — `vte 0.15`                                                                                     |
| 2   | `bitty-pty`      | `0.0.1` | **PASS**                                | 14 files, 75.1 KiB (21.9 KiB compressed)  | `Compiling bitty-pty ... Finished dev` — `portable-pty 0.9` (4.94 s)                                                                   |
| 3   | `bitty-platform` | `0.0.1` | **PASS**                                | 17 files, 187.6 KiB (48.5 KiB compressed) | `Compiling bitty-platform ... Finished dev` — `winit 0.30`, `raw-window-handle 0.6.2` (1.62 s verify, full winit recompile in sandbox) |
| 4   | `bitty-config`   | `0.0.1` | **PASS**                                | 13 files, 120.7 KiB (24.9 KiB compressed) | `Compiling bitty-config ... Finished dev` (2.35 s)                                                                                     |
| 5   | `bitty-package`  | `0.0.1` | **PASS**                                | 18 files, 269.7 KiB (53.5 KiB compressed) | `Compiling bitty-package ... Finished dev` (3.01 s)                                                                                    |
| 6   | `bitty-lua`      | `0.0.1` | **PASS**                                | 6 files, 60.8 KiB (14.4 KiB compressed)   | `Compiling bitty-lua ... Finished dev` — `piccolo 0.3.3` (8.02 s)                                                                      |

All six: **dry-run PASS**. No metadata errors, no missing `description`/
`license`/`repository`, no unpublished internal path dep errors (expected only
for Groups 2-3), no `publish = false` block. Warnings are only the intentional
`aborting upload due to dry run`.

Raw excerpt (`bitty-vt` representative; all 6 analogous):

```text
Updating crates.io index
Packaging bitty-vt v0.0.1 (.../crates/bitty-vt)
Updating crates.io index
Packaged 24 files, 118.4KiB (27.7KiB compressed)
Verifying bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
 Compiling bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.81s
 Uploading bitty-vt v0.0.1 (.../crates/bitty-vt)
warning: aborting upload due to dry run
```

Sizes vs historical: `vt` 118.4 KiB (was 97.8 at `bbbdc1c`/`168493a`),
`pty` 75.1 (was 66.1), `platform` 187.6 (was 124.4 → 174.9 at `ffd3eee` → 187.6 now
with window/IME/monitor additions since CTX-0051), `package` 269.7 (was 153.8 →
now with resolver + hostile tests), `lua` 60.8 (was 60.4), `config` stable
120.7. Growth is tracked, not a publish blocker — verify remains PASS.

### Group 2/3 ordering confirmation (expected failures, not defects)

Group 2/3 correctly fail on missing crates.io index until their prerequisites are
indexed. Verified on this worktree:

- `cargo publish --dry-run -p bitty-term-state --allow-dirty` → `error: no matching package named bitty-vt found` (requires `vt` at `^0.0.1`).
- `cargo publish --dry-run -p bitty-render --allow-dirty` → `error: no matching package named bitty-platform found` (requires `platform` + `term-state`).

This confirms the DAG ordering documented in `release-ladder.md` Groups 2-3, not a
metadata defect. Re-verify with `--dry-run -p bitty-term-state` once Group 1 is
indexed, then `bitty-ui`/`bitty-render` in parallel after `term-state` is indexed.

### Future real-publish procedure (not executed here)

> Do not run `cargo publish` in this task. This study is intentionally left
> dirty; no credentials were configured; verification was `--dry-run` only.

When this study and the ladder/checklists are reviewed and merged, the
maintainer sequence — **from a clean checkout of `main` at tag `v0.0.1` without
`--allow-dirty`** — is:

```bash
# from a clean checkout of main at the merge commit (tag v0.0.1), no --allow-dirty
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
`cargo check --target x86_64-pc-windows-gnu` and `cargo test --workspace --all-targets --locked`
and `just check` green on that commit, plus a fresh `--dry-run` for the next
crate and `act -n` DRYRUN. Do not use `--allow-dirty` for the real publish;
ensure `git status` clean and tag `v0.0.1` (deferring `0.1.0`). CI `Quality gates` +
`CodeQL` + `Supply chain` + `Windows` must be green before push per `AGENTS.md`.

## Binary preview nightly (research, no release in this task)

### Binary identity

- **`bitty-app` is never published to crates.io.** `crates/bitty-app/Cargo.toml:9`
  sets `publish = false` with `version.workspace = true` (`0.0.1`), thin
  composition root depending only on `bitty-platform` + `bitty-runtime` +
  `bitty-render` (plus `pollster 0.3` for GPU await). No workspace edge allows
  a crates.io binary — distribution is via GitHub Releases only.
- Binary version at `8c88f3a` is `0.0.1` (workspace version). Preview builds
  before `0.0.1` tag use the form `v0.0.1-preview.N+<sha>` or the nightly
  channel `nightly-YYYYMMDD+<sha>` — see Tag and artifact naming below.

### Why nightly is not crates.io

Crates.io is for library crates (`publish = true` above); the binary is
installed via `cargo install bitty-app` only from source or via pre-built
archives attached to a GitHub Release. A nightly preview therefore lives as a
**GitHub Releases pre-release** with workflow-generated archives, not as a
crates.io publish. This preserves the `publish = false` invariant and avoids
crates.io version churn for every nightly.

### Nightly trigger and workflow sketch (proposed, not implemented here)

The study proposes the following shape for a future
`.github/workflows/nightly.yml` (not added in this docs-only PR; no release
was created). The sketch reuses the existing `ci.yml` justfile gates and the
pinned toolchain matrix from
[toolchain-policy.md](../../../bitty-docs/docs/development/toolchain-policy.md):

```yaml
name: nightly-preview
on:
  schedule:
    - cron: "0 2 * * *"        # 02:00 UTC nightly
  workflow_dispatch:           # manual run with optional sha input
  push:
    tags: ["nightly-*", "v0.*-preview.*"]  # tag-triggered preview on demand
permissions:
  contents: write              # create pre-release + upload assets
concurrency:
  group: nightly-${{ github.ref }}
  cancel-in-progress: true
jobs:
  build:
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            archive: bitty-nightly-linux-x64.tar.gz
          - os: macos-14
            target: aarch64-apple-darwin
            archive: bitty-nightly-macos-arm64.tar.gz
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            archive: bitty-nightly-windows-x64.zip
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - run: rustup toolchain install 1.97.1 --profile minimal
      - run: cargo build -p bitty-app --release --locked --target ${{ matrix.target }}
      - run: strip / codesign / notarize per platform (future)
      - uses: actions/upload-artifact@v4
        with: { name: ${{ matrix.archive }}, path: target/${{ matrix.target }}/release/bitty* }
  release:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
      - run: gh release create nightly-$(date +%Y%m%d)+$(git rev-parse --short HEAD)
             --prerelease --title "nightly $(date +%Y%m%d) ($(git rev-parse --short HEAD))"
             --generate-notes bitty-nightly-*.tar.gz bitty-nightly-*.zip
```

- **Gates before artifact assembly:** `cargo fmt --check` + `cargo clippy -D warnings`
  - `cargo test --workspace --all-targets --locked` + `cargo check --target x86_64-pc-windows-gnu`
  - `actionlint` + `markdownlint` + `act -n` must be green — the same gates as this pre-study.
    Fail the nightly build if any gate fails; never publish a nightly from a red `main`.
- **Cargo audit/deny in nightly:** include the `Supply chain (deny/audit)` legs
  (`cargo deny check` + `cargo audit --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0192`)
  as prerequisites — see Verification gates below.
- **Install font stack** on Linux (`libfontconfig1-dev libfreetype6-dev`) before
  `cargo build` — same as `ci.yml: quality` leg for `crossfont`/`bitty-render`.

No `.github/workflows/nightly.yml` was added in this PR; no `gh release create`
was executed; no binary was built with `cargo build --release` for distribution
in this task. The workflow above is the reviewable proposal.

### Tag and artifact naming (proposed)

- **Nightly channel:** `nightly-YYYYMMDD+<sha7>` (e.g. `nightly-20260901+8c88f3a`).
  Tag points at the `main` head that passed gates. Assets are attached to that
  pre-release and overwritten per the retention policy (keep latest N nightlies,
  prune older).
- **Preview channel (near tag):** `v0.0.1-preview.N+<sha>` (N increments per
  preview after `0.0.1` tag candidate). Follows semver pre-release syntax; no
  crates.io version is bumped — it is a GitHub Release tag only.
- **Archives:**
  - `bitty-nightly-linux-x64.tar.gz` — `x86_64-unknown-linux-gnu`, `bitty` binary +
    `LICENSE` + `README.md` + `CHANGELOG.md` slice.
  - `bitty-nightly-macos-arm64.tar.gz` — `aarch64-apple-darwin`, same bundle.
  - `bitty-nightly-windows-x64.zip` — `x86_64-pc-windows-msvc`, `bitty.exe` + `LICENSE`.
  - Optional `-debug` archives with `*.dSYM`/`*.pdb` for crash triage (future).
- **Checksums and provenance:** each archive ships `SHA256SUMS` and a
  `provenance.json` (commit sha, workflow run id, toolchain `1.97.1`, `Cargo.lock`
  hash). Future SLSA/rekor attestation is a candidate post-`v0.1` (not in this study).

### Platform matrix and signing (proposed)

| Platform     | Target                     | Runner           | Signing/notarization                                                                                                        | Runtime deps                                                                    |
| ------------ | -------------------------- | ---------------- | --------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-latest`  | none (tarball + SHA256)                                                                                                     | `glibc` + Wayland/X11 runtime already tested in `ci.yml` linux-x11/wayland legs |
| macOS ARM64  | `aarch64-apple-darwin`     | `macos-14`       | Apple Developer ID + notarization (future, requires secrets) — until then unsigned tarball with `com.apple.quarantine` note | no extra                                                                        |
| Windows x64  | `x86_64-pc-windows-msvc`   | `windows-latest` | Authenticode via `azure/trusted-signing` (future) — until then unsigned zip                                                 | `VCRedist` no extra                                                             |

The `ci.yml` matrix at `8c88f3a` already covers Linux X11/Wayland, MSRV, macOS ARM64,
Windows — nightly reuses those runners for artifact builds.

### Retention and promotion

- **Nightly retention:** keep last 14 nightlies as pre-releases; prune older
  via `gh release delete` + tag delete. Nightlies are **not** `latest` and do
  not set `isLatest` — only a real `v0.0.1` release will.
- **Promotion to preview/stable:** a nightly that passes extended soak
  (`perf-evidence.md` budget, `soak-0.0.1.md` scenarios, `manual-smoke.md`
  checklist, compat matrix CTX-0114) may be promoted to
  `v0.0.1-preview.N` or `v0.0.1` by retagging the same sha after independent
  review — no rebuild with different sources.
- **No publish in this task:** the study leaves no tag, no release, no
  artifact, no `cargo build --release` output in the repo root, and no workflow
  file change. The PR diff is docs only.

## Release ladder v0.1

### Candidate ladder and provenance

The candidate ladder is canonical in
[`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
(ChatGPT share `6a8dae4b-2aec-83ea-9174-03abc1f81531`, English rendering) and
mirrored in `bitty/docs/product/release-ladder.md` (CTX-0044 draft, `status: draft`,
updated CTX-0049/CTX-0050). Version numbers are **architecture-maturity labels**,
not calendar promises. No open question is closed here.

| Version  | Candidate scope (from proposed-delivery-sequence)          | Workspace crate focus for that slice                                                                                                                                  | Gate sketch                                                                                            |
| -------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `v0.0.x` | Architecture and protocol prototypes                       | `bitty-core` seed only; `publish = false`                                                                                                                             | prototype                                                                                              |
| `v0.1`   | A shell runs correctly in a minimal terminal slice         | Minimal Correct Terminal: `vt` + `pty` + `term-state` + `platform` + `config` + `render` + `ui` + `runtime` + `app`; `package`/`lua` leaves ready but not on hot path | shell echo + resize + backpressure headless tests; `cargo check`; `cargo publish --dry-run` leaf batch |
| `v0.2`   | VT and TUI compatibility work                              | `vt`/`term-state` parser-to-action-to-state fidelity, compatibility matrix (OQ-004)                                                                                   | differential tests, fuzz, Neovim/tmux corpora                                                          |
| `v0.3`   | GPU rendering, fonts, performance, graphics protocols      | `render` (`wgpu 26.0`, `crossfont 0.9`) + `platform` SurfaceTarget; `sw-fallback` opt-in                                                                              | render snapshots, GPU present headless, perf budget PB-1/PB-2                                          |
| `v0.4`   | Lua configuration system                                   | `config` (`ConfigPlan`) + `lua` (`piccolo 0.3.3` RC-1/RC-2)                                                                                                           | config merge/reload/trust + Lua Fuel/wall + 32 MiB measurement                                         |
| `v0.5`   | Plugin API                                                 | `plugin-host` capability/event lifecycle (OQ-011/012/013)                                                                                                             | capability grant/revocation, bounded EventQueue                                                        |
| `v0.6`   | Plugin manager and lazy loading                            | `package` lifecycle + manager overlay on host                                                                                                                         | activation/rollback, lazy-load budgets                                                                 |
| `v0.7`   | DevTools and the debug protocol                            | `runtime` instrumentation seam (no dedicated `bitty-debug` yet)                                                                                                       | versioned surface + inspector                                                                          |
| `v0.8`   | Rich presentation, Markdown stress, shell integration      | `rich` (OQ-008/015/016) rich blocks, hyperlinks, images                                                                                                               | rich-block scene/zone, image limit/budget                                                              |
| `v0.9`   | IPC, `bitty ctl`, MCP adapter, and stabilization           | `ipc` + `agent` bounded framing/scopes (OQ-018)                                                                                                                       | framed 256 KiB, peer-credential auth, rate-limit RC-9/RC-10                                            |
| `v1.0`   | Stabilized plugin, configuration, command, debug contracts | All above under semver surfaces; Tier 1 platforms per ADR-0002                                                                                                        | compatibility matrix + security P0 gates + versioned APIs v1                                           |

Non-goals and daemon staging remain as in `proposed-delivery-sequence.md`:
`bittyd` and remote UI are **post-v1.0** candidates (OQ-020, ADR-0008), not part
of this ladder.

### `v0.1` slice — workspace focus and current evidence at `8c88f3a`

`v0.1` is the earliest publish slice at `0.0.1` (CTX-0049; was `0.1.0` in
CTX-0043) — deferring `0.1.0` until plugins etc. are more complete.

- **Crate focus:** `vt` + `pty` + `term-state` + `platform` + `config` + `render`
  - `ui` + `runtime` + `app` headless, with `package`/`lua` leaves included at
    `0.0.1` for completeness though not on the hot path of the minimal shell
    (see Workspace version mapping in `release-ladder.md`).
- **Publish slice at `0.0.1`:** Groups 1-3 only (`vt`, `pty`, `platform`, `config`,
  `package`, `lua`, `term-state`, `ui`, `render` at `0.0.1`). Tail remains
  `publish = false` at `0.0.1` (deferring `0.1.0`) — see Group 4.
- **Headless evidence (CTX-0050 branch `ctx-0050/feat-minimal-terminal`, retained):**
  `crates/bitty-runtime/tests/v01_minimal_terminal.rs` proves
  shell echo deterministic replay (`PTY bytes -> VT Parser -> TerminalAction -> State ->
Snapshot + Damage -> GridRenderer -> Surface::headless_present`),
  `handle_resize(800×600 -> 100×37 cells)` full redraw, and backpressure bounded
  `MAX_BUFFERED_BYTES = 8 KiB × 16 = 128 KiB` with `DropOldest` queues. Existing
  `bitty-vt` replay fixtures (`shell_session`/`escape_storm`/`fullscreen_app`/`osc_sweep`,
  `seeds/*.bin` 14 seeds) and `handle_pty_bytes_flow_reaches_render` cover the parser leg.
- **Current status in this worktree:** `v0.1` share remains **candidate** until an
  ADR/RFC with independent review accepts the slice. This study does not close
  any OQ; it records the mapping and the gate sketch for review.

### Future increments

- `0.0.2`/`0.0.x` for `0.0.1` patches, `0.1.0` for the deferred plugin-complete
  slice (when `plugin-host`/`rich`/`ipc`/`agent`/`runtime` promotion lands in
  DAG order with `version = "x.y.z"` pins, CTX-0043 pattern), `0.2.0` for VT/TUI,
  `0.3.0` for GPU etc., per Workspace version mapping in `release-ladder.md`.
- The `1.0` bump requires stabilization gates per
  [v1.0 criteria in proposed-delivery-sequence](../../../bitty-docs/docs/product/proposed-delivery-sequence.md#candidate-v10-criteria)
  and the security overview/threat-model normative controls.

## Toolchain and version pinning matrix

Pinned per `justfile`, `rust-toolchain.toml`, `Cargo.lock`, `deny.toml`
(per `toolchain-policy.md` — single source of truth, no side-effect bumps):

| Pin                         | Value                                                                                                     | Source                                                                                       |
| --------------------------- | --------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Rust channel                | `1.97.1` minimal + `rustfmt` + `clippy`                                                                   | `rust-toolchain.toml:2-4` + `.github/workflows/ci.yml:35-38`                                 |
| MSRV                        | `1.85` (edition 2024)                                                                                     | `Cargo.toml:25` `rust-version` + `clippy.toml:3` `msrv` + `ci.yml: msrv` leg                 |
| Edition / Resolver          | `2024` / `3`                                                                                              | `Cargo.toml:24,20`                                                                           |
| `workspace.package.version` | `0.0.1` (deferring `0.1.0`)                                                                               | `Cargo.toml:22`                                                                              |
| `wgpu`                      | `26.0` (26.0.1 in lock)                                                                                   | `crates/bitty-render/Cargo.toml:20`                                                          |
| `crossfont`                 | `0.9`                                                                                                     | `crates/bitty-render/Cargo.toml:21`                                                          |
| `piccolo`                   | `0.3.3`                                                                                                   | `crates/bitty-lua/Cargo.toml:19`, `g1-publish-log.md:54`                                     |
| `portable-pty`              | `0.9.0`                                                                                                   | `crates/bitty-pty/Cargo.toml:18`                                                             |
| `winit`                     | `0.30`                                                                                                    | `crates/bitty-platform/Cargo.toml:17`                                                        |
| `raw-window-handle`         | `0.6.2`                                                                                                   | `crates/bitty-platform/Cargo.toml:18`                                                        |
| `vte`                       | `0.15`                                                                                                    | `crates/bitty-vt/Cargo.toml:18`                                                              |
| `actionlint`                | `1.7.12`                                                                                                  | `justfile: actionlint` recipe + `ci.yml:54` docker `rhysd/actionlint:1.7.12@sha256:b1934...` |
| `markdownlint-cli2`         | `0.23.1` (markdownlint `0.41.1`)                                                                          | `justfile:22`, `.markdownlint-cli2.jsonc:2`                                                  |
| `commitlint`                | `21.2.2`                                                                                                  | `justfile:27` `target/dev-tools` pins                                                        |
| `cargo-deny`                | `0.20.2`                                                                                                  | `ci.yml: supply-chain` `cargo install cargo-deny --version 0.20.2`                           |
| `cargo-audit`               | `0.22.2`                                                                                                  | `ci.yml: supply-chain` `cargo install cargo-audit --version 0.22.2`                          |
| `deny.toml` ignore          | `RUSTSEC-2024-0436` (`paste 1.0.15` unmaintained), `RUSTSEC-2026-0192` (`ttf-parser 0.25.1` unmaintained) | `deny.toml:8-11` — tracked, ignored until upstream replacement lands                         |

No drift detected at `8c88f3a`; `cargo --locked` and `cargo deny --locked` enforce
lockfile fidelity. Do not bump pins as a side effect of this docs-only task;
report drift instead.

## Verification gates (must PASS; no publish in this task)

Re-verified at `8c88f3a` (`ctx-0115`) after writing this draft (worktree
intentionally dirty with this doc; `cargo publish --dry-run` used `--allow-dirty`;
all other gates run without `--allow-dirty`):

| Gate                         | Command                                                                                             | Result at `8c88f3a` (2026-09-01, `ctx-0115`, toolchain `1.97.1`)                                                                                                                                                                                                   |
| ---------------------------- | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `cargo check`                | `cargo check --workspace --all-targets --locked`                                                    | **PASS** — `Finished dev profile`                                                                                                                                                                                                                                  |
| `cargo check` (Windows)      | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked`                     | **PASS** — `Finished dev profile` in 16.99 s (cross-checked `x86_64-pc-windows-gnu`)                                                                                                                                                                               |
| `just check`                 | `just check` (`fmt-check` + `clippy -D warnings` + `test --locked` + `actionlint` + `markdownlint`) | **PASS** — 0 issues                                                                                                                                                                                                                                                |
| └ `cargo fmt --check`        | via `just fmt-check`                                                                                | PASS — no diff                                                                                                                                                                                                                                                     |
| └ `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings`                                    | PASS — 0 warnings                                                                                                                                                                                                                                                  |
| └ `cargo test`               | `cargo test --workspace --all-targets --locked`                                                     | PASS — **1394 passed**, 0 failed (full suite; `bitty-vt` 57, `bitty-term-state` 61+4+3, `bitty-ui` 59, `bitty-pty` 8, `bitty-render` 4, `bitty-runtime` soak/v01 incl., `compat-lab` 1, etc.; prior 808 at `c465888` with CTX-0067 soak 90 s wall budget retained) |
| └ `actionlint`               | `actionlint -color`                                                                                 | PASS                                                                                                                                                                                                                                                               |
| └ `markdownlint`             | `bunx --bun markdownlint-cli2@0.23.1` (`**/*.md`)                                                   | PASS — `0 issues in 0 files` — 59 files with this draft (was 59 prior inclusive)                                                                                                                                                                                   |
| └ `act -n`                   | `act -n` (workflow syntax dry-run for `ci.yml`/`codeql.yml`)                                        | **PASS** — DRYRUN Quality gates, MSRV, Windows, Supply chain, CodeQL rust/actions — syntax only, not runtime                                                                                                                                                       |
| publish metadata             | `cargo publish --dry-run --allow-dirty` per G1 leaf                                                 | **PASS all 6** — see leaf table (re-verified at `8c88f3a` with `--allow-dirty`)                                                                                                                                                                                    |
| `cargo audit`                | `cargo audit` / `cargo audit --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0192`                 | **PASS** — `Loaded 1235 advisories`, 344 crates scanned, 2 allowed warnings (`paste` RUSTSEC-2024-0436, `ttf-parser` RUSTSEC-2026-0192) per `deny.toml`                                                                                                            |
| `cargo deny`                 | `cargo deny check`                                                                                  | **PASS** — `advisories ok, bans ok, licenses ok, sources ok`                                                                                                                                                                                                       |
| version/publish flag drift   | `grep` of `Cargo.toml` workspace + per-crate                                                        | PASS — `workspace.package.version 0.0.1`, G1 leaves `publish = true`, Group 2/3 pins present                                                                                                                                                                       |

No `TODO`/`FIXME` introduced; frontmatter `status: draft` retained.
Full `just check` + `cargo check --target x86_64-pc-windows-gnu` + `act -n` +
`cargo audit` + `cargo deny check` log is retained in the task checkpoint;
the tail above is the auditable summary. No formatting, lint, test, or markdown
failures were introduced by this draft.

## Security and release blockers

- Normative security requirements in `bitty-docs/docs/security/` override this
  draft. P0 controls (parser limits, VM isolation per RC-1/RC-2, capability
  deny-by-default, restricted stdlib, bounded framing 256 KiB, paste inspection,
  OSC policy, fuzz, package lock) remain **release blockers** and are not
  weakened by this ladder — see
  [Security Overview](../../../bitty-docs/docs/security/overview.md) and
  [Threat Model](../../../bitty-docs/docs/security/threat-model.md).
- Plugin, Agent, MCP, IPC, clipboard, filesystem, process, network, URL, and
  debug changes require focused security review (`AGENTS.md: Security`).
- This study adds **no** temporary bypass, ambient authority, unbounded
  parser/resource path, native in-process plugin, or allow-all capability.
- `RUSTSEC-2024-0436`/`RUSTSEC-2026-0192` are tracked as `allow` in `deny.toml`
  until upstream replacement lands; `cargo audit` is green only with those
  ignores and is recorded honestly here — not a security downgrade.

## Cross-reference and maintenance

- Release ladder and Group 2/3/4 detail:
  [`release-ladder.md`](./release-ladder.md) (CTX-0044, merged at `ff9715d`).
  This study is a companion to [`g1-publish-checklist.md`](./g1-publish-checklist.md)
  and [`g1-publish-log.md`](./g1-publish-log.md) (CTX-0045/0047); do not cite it
  without that ladder.
- Candidate spine provenance:
  [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
  (ChatGPT share `6a8dae4b-2aec-83ea-9174-03abc1f81531`, English rendering).
- Workspace topology DAG: [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
- Platform and compatibility bars: [ADR-0002](../../../bitty-docs/docs/decisions/adrs/ADR-0002-platform-support-tiers.md),
  compatibility milestone RFC; evidence at
  [`compat-matrix.md`](./compat-matrix.md) (CTX-0114) and [`compat-lab.md`](./compat-lab.md).
- Security gates for `v1.0` remain normative in
  [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and
  [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md);
  this ladder does not weaken them.
- Performance budgets (OQ-001) are normative in
  [Performance Budget RFC](../../../bitty-docs/docs/specifications/performance-budget-rfc.md);
  nightly preview promotion must respect PB-1/PB-2 (`startup`, `latency`,
  `idle`, `memory`) budgets there.
- When a version slice is accepted via ADR/RFC, bump
  `workspace.package.version`, flip tail `publish` flags in DAG order, and
  add `version = "x.y.z"` pins on newly publishable edges (CTX-0043 pattern),
  then add a fresh dry-run column here and archive the prior study date.
- Toolchain pin single source: `justfile`, `rust-toolchain.toml`, `Cargo.lock`
  (`bun.lock`/`Cargo.lock` per repo). Do not bump pins as a side effect of an
  unrelated task; report drift instead — see
  [toolchain-policy.md](../../../bitty-docs/docs/development/toolchain-policy.md).

## Revision history

- `2026-09-01` CTX-0115 `carryctx/ctx-0115` at `8c88f3a` — **pre-study** created;
  workspace `0.0.1` retained (no drift, verified `just check` + `cargo audit`);
  publish flags `vt/pty/platform/config/package/lua = true` retained; publish order
  `vt -> pty -> platform -> config -> package -> lua` confirmed per `release-ladder.md`
  and `deny.toml` supply-chain; all 6 G1 leaves `cargo publish --dry-run --allow-dirty`
  **PASS** (vt 118.4 KiB (27.7), pty 75.1 KiB (21.9), platform 187.6 KiB (48.5),
  config 120.7 KiB (24.9), package 269.7 KiB (53.5), lua 60.8 KiB (14.4);
  each `Finished dev` + `aborting upload due to dry run` — re-verified at `8c88f3a`);
  Group 2 `term-state` and Group 3 `render`/`ui` correctly fail on missing index
  (ordering proof); binary preview nightly researched as GitHub Releases pre-release
  `nightly-YYYYMMDD+sha` / `v0.0.1-preview.N+sha` with matrix linux-x64 / macos-arm64 /
  windows-x64, retention 14, provenance `SHA256SUMS` + `provenance.json` — **no
  binary published, no tag pushed, no workflow added in this docs-only PR**;
  `v0.1` ladder mapped (candidate `v0.1` minimal terminal `vt`/`pty`/`term-state`/
  `platform`/`config`/`render`/`ui`/`runtime`/`app` headless at `0.0.1`, deferring
  `0.1.0`); verification gates **PASS**: `cargo fmt --check` PASS, `cargo clippy -D warnings`
  PASS (0 warnings), `cargo test --workspace --all-targets --locked` **1394 passed**
  (prior 808 at `c465888`), `cargo check --target x86_64-pc-windows-gnu` PASS,
  `actionlint` PASS, `markdownlint` PASS (59 files), `act -n` DRYRUN PASS
  (Quality gates + CodeQL + MSRV + Windows + Linux/Supply chain), `cargo audit`
  PASS (1235 advisories, 344 crates, 2 allowed `paste`/`ttf-parser`), `cargo deny`
  PASS (advisories/bans/licenses/sources ok); **no `cargo publish` executed beyond
  --dry-run with --allow-dirty and intentionally dirty worktree; actual publish
  will be from clean `main` at tag `v0.0.1` without --allow-dirty in order
  `vt -> pty -> platform -> config -> package -> lua`, waiting for index propagation
  before G2/G3** — docs only.
- `2026-08-29` CTX-0066 `ctx-0066/publish-g1` at `c465888` — finalized for `0.0.1` at
  `c465888` (soak CTX-0067 at `d4af44e`, AGENTS `act -n` gate at `c465888`); six leaves
  dry-run PASS; `just check` PASS (808 tests, 29 files, `act -n`).
- `2026-08-29` CTX-0062 `ctx-0062/publish-g1-final` at `ffd3eee` — re-verified `0.0.1`;
  six leaves PASS; `just check` PASS (801 tests).
- `2026-08-28` CTX-0052/0047 at `bbbdc1c`/`9eec31b` — draft logs, six leaves PASS
  (vt 97.8, pty 66.1, platform 124.4, config 120.7, package 153.8, lua 60.4);
  `just check` PASS (708 tests).
- `2026-08-28` CTX-0045 at `168493a` — draft checklist created; six leaves PASS;
  `cargo check` PASS; `just check` PASS.
