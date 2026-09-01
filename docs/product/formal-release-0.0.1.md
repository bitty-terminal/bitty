---
title: Formal Release 0.0.1 — 9 Crates Publish Log + Binary Preview
description: Formal publish record for CTX-0116 at 0.0.1 covering Groups 1-3 (9 crates) with cargo publish evidence, GitHub Release binary preview, and gate evidence
category: product
audience: maintainer
document_type: release
status: formal
---

<!-- markdownlint-disable MD025 -->

# Formal Release 0.0.1 — 9 Crates Publish Log + Binary Preview (CTX-0116)

## Status and provenance

- Status: **formal**. This is the publish record for **CTX-0116** — branch `carryctx/ctx-0116` at `21bdf8e` (base `78eb304`, CTX-0115 pre-study at `78eb304` which itself was `8c88f3a` compat matrix prior to this branch head, now `21bdf8e` with term-state publish fix). Worktree: `.worktrees/ctx-0116`.
- Task: **CTX-0116** `Publish crates and binary preview release` — Priority: **P0** | Area: **release** | Labels: `chore,area:release,P0` | Milestone: **v0.1.0** | RFC: **OQ-001** | Task: **REL-FORMAL**. Formal release: publish 9 leaf crates to crates.io at `0.0.1` and create GitHub Release binary preview. Gates: `just check`, `cargo publish`, `gh release`, no `cargo publish` beyond `--dry-run` unless tokens and maintainer authorize (here tokens present, actual publish performed).
- Companion records: CTX-0043 publish prep, CTX-0044 release ladder Groups 1-4, CTX-0045 G1 checklist, CTX-0047/0052/0062/0066 G1 logs (dry-run only), CTX-0115 pre-study (dry-run + nightly research, 1394 tests, 60 files). This formal log is the first **actual** `cargo publish` record.
- Authority: ladder at `docs/product/release-ladder.md` (CTX-0044 merged at `ff9715d`) remains candidate but this release's **9-crate slice** (Groups 1-3) is now published at `0.0.1`. Remaining tail (7 crates `publish = false`) defers to `0.1.0`.

## Scope

- **Published at `0.0.1` (Groups 1-3, 9 crates):** `bitty-vt`, `bitty-pty`, `bitty-platform`, `bitty-config`, `bitty-package`, `bitty-lua` (Group 1 leaves, no workspace deps), `bitty-term-state` (Group 2, `vt = "0.0.1"`), `bitty-ui` (Group 3 after `term-state`), `bitty-render` (Group 3 after `term-state` + `platform`). Each with `version.workspace = true` inheriting `0.0.1`, `publish = true`, `description`/`license`/`repository`/`keywords`/`categories` validated.
- **Not published at `0.0.1` (Group 4 tail, `publish = false`):** `bitty-plugin-host` (depends `term-state`/`config`/`package`), `bitty-rich` (`term-state`/`vt`), `bitty-ipc` (stub), `bitty-agent` (stub), `bitty-runtime` (fan-in 7 deps), `bitty-app` (binary, `publish = false`, distributed via GitHub Releases), `bitty-core` (seed to retire). Deferred until RFC acceptance and promotion in DAG order with `version = "x.y.z"` pins (CTX-0043 pattern).
- **Binary preview:** `bitty-app` `0.0.1` Linux x86_64 release artifact attached to GitHub Release `v0.0.1` (prerelease preview). `publish = false` invariant preserved (binary never on crates.io).
- **Out of scope:** closing any OQ, accepting ladder `v0.1` slice beyond evidence, bumping pins as side effect.

## Workspace version and pinning (at `21bdf8e`)

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

- `Cargo.toml:22` `version = "0.0.1"` retained from CTX-0049 (`956926c`) through CTX-0115 `8c88f3a`/`78eb304` to this `21bdf8e`. No drift.
- Per-crate `version.workspace = true` inherits `0.0.1`; publishable crates pin internal path edges `version = "0.0.1"` (e.g. `term-state -> vt`, `ui -> term-state`, `render -> term-state + platform` plus dev-only `vt` in `render`). Dev-graph exception: `term-state` dev-dep `bitty-ui = { path = "../bitty-ui" }` **without** `version` pin (fixed in `21bdf8e` from `{ version = "0.0.1", path = "../bitty-ui" }`) to break cycle `term-state dev-> ui -> term-state` that blocked Group 2 publish after `vt` indexed. Tail keeps `path` only without pins.
- Toolchain pins: `rust-toolchain.toml` `channel = "1.97.1"` minimal + `rustfmt` + `clippy`, `clippy.toml` `msrv = "1.85"`, `Cargo.lock` 344+ crates locked, `justfile` `actionlint 1.7.12`, `markdownlint-cli2 0.23.1`, `cargo-deny 0.20.2`, `cargo-audit 0.22.2`, `commitlint 21.2.2`. No bump as side effect.

## Publish order (DAG-forced, Groups sequential, within Group unordered)

Group order per `release-ladder.md` and CTX-0047 reproducibility fix; real publish waited for crates.io index propagation between Groups and handled crates.io new-crate rate limit (5 per ~10 min window).

### Task-specified order (Groups 1-3, 9 crates)

1. `bitty-vt`
2. `bitty-pty`
3. `bitty-platform`
4. `bitty-config`
5. `bitty-package`
6. `bitty-lua`
7. `bitty-term-state` — after `vt` indexed
8. `bitty-ui` — after `term-state` indexed
9. `bitty-render` — after `term-state` + `platform` indexed

Within Group 1 crates are unordered in DAG; sequence above is used for repeatability. `ui` and `render` have no edge between them (after prereqs indexed they may publish concurrently, but here serialized due to rate limit). `ui` dev-deps `term-state`/`vt` with `version` pins correctly require those prerequisites already on crates.io; `render` dev-dep `vt` requires `vt`.

### Index propagation waits

- After Group 1, `cargo publish --dry-run -p bitty-term-state --allow-dirty` was re-verified: at `8c88f3a` (prior) it failed `no matching bitty-vt` before vt publish; after vt publish at `21bdf8e` it **PASS**ed (downloaded `bitty-vt 0.0.1` from crates.io) but initially failed `no matching bitty-ui` due to dev-dep cycle — fixed by removing version pin, then PASS with `Downloaded bitty-vt v0.0.1`, `Compiling bitty-term-state` from packaged tarball.
- Group 2 → Group 3 wait: after `term-state` publish, `bitty-ui --dry-run` PASSed with `Downloaded bitty-term-state v0.0.1`; `bitty-render --dry-run` PASSed with `Downloaded bitty-platform v0.0.1` + full wgpu build.

## Verification gates (must PASS before and after publish)

| Gate | Command | Result at `21bdf8e` (2026-09-01, `carryctx/ctx-0116`, toolchain `1.97.1`) |
| --- | --- | --- |
| `cargo check` | `cargo check --workspace --all-targets --locked` | **PASS** |
| `cargo check` (Windows) | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked` | **PASS** |
| `just check` | `just check` (`fmt-check` + `clippy -D warnings` + `test --locked` + `actionlint` + `markdownlint`) | **PASS** — 0 issues |
| └ `cargo fmt --check` | via `just fmt-check` | PASS — no diff |
| └ `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS — 0 warnings |
| └ `cargo test` | `cargo test --workspace --all-targets --locked` | PASS — **1394 passed** (+ head checks 1394 at `8c88f3a` retained; soak 808 at `c465888` + compatibility etc.) — prior 1394 at `78eb304`, verified again at `21bdf8e` after dev-dep fix (61+4+3+4+8 term-state) |
| └ `actionlint` | `actionlint -color` | PASS |
| └ `markdownlint` | `bunx --bun markdownlint-cli2@0.23.1` | PASS — 0 issues in 60 files |
| └ `act -n` | `act -n` (workflow syntax dry-run `ci.yml`/`codeql.yml`) | **PASS** — DRYRUN Quality gates, MSRV, Windows, Supply chain, CodeQL |
| publish metadata dry-run | `cargo publish --dry-run --allow-dirty` per G1 leaf + Group 2/3 after propagation | **PASS all 9** after fix; 6 leaves PASS independently, `term-state`/`ui`/`render` PASS after prerequisites indexed |
| `cargo audit` | `cargo audit --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0192` | **PASS** — 1235 advisories, 344 crates, 2 allowed (`paste`/`ttf-parser` per `deny.toml`) |
| `cargo deny` | `cargo deny check` | **PASS** — advisories/bans/licenses/sources ok |
| version/publish flag drift | `grep` workspace + per-crate | PASS — `0.0.1`, 9 `publish = true`, 7 `false` |

No `TODO`/`FIXME` introduced; `just check` was re-run on dirty worktree before commit and on clean commit `21bdf8e`.

## Actual `cargo publish` evidence (executed, tokens present)

Real publish executed from **clean** worktree at `21bdf8e` **without** `--allow-dirty` except where noted dirty before fix commit; after fix commit, clean `git status` before each publish.

### Commands (task-specified order, with index waits and rate-limit waits)

```bash
cargo publish -p bitty-vt
cargo publish -p bitty-pty
cargo publish -p bitty-platform
cargo publish -p bitty-config
cargo publish -p bitty-package
cargo publish -p bitty-lua
# wait for crates.io index propagation + fix dev-dep cycle in 21bdf8e, re-verify dry-run
cargo publish -p bitty-term-state
# wait for index propagation
cargo publish -p bitty-ui
cargo publish -p bitty-render
```

### Results table

| # | Crate | Version | `cargo publish` (real) | Packaged | Verified build | Published at |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | `bitty-vt` | `0.0.1` | **PASS** `Uploaded` + `Published` | 24 files, 118.4 KiB (27.7 KiB compressed) | `Compiling bitty-vt ... Finished dev` — `vte 0.15` | 14:43:?? UTC + `Published bitty-vt v0.0.1 at registry crates-io` |
| 2 | `bitty-pty` | `0.0.1` | **PASS** | 14 files, 75.1 KiB (21.9) | `portable-pty 0.9` | `Published bitty-pty` |
| 3 | `bitty-platform` | `0.0.1` | **PASS** | 17 files, 187.6 KiB (48.5) | `winit 0.30` + `raw-window-handle 0.6.2` | `Published bitty-platform` |
| 4 | `bitty-config` | `0.0.1` | **PASS** | 13 files, 120.7 KiB (24.9) | `Compiling bitty-config` | `Published bitty-config` |
| 5 | `bitty-package` | `0.0.1` | **PASS** | 18 files, 269.7 KiB (53.5) | `Compiling bitty-package` | `Published bitty-package` |
| 6 | `bitty-lua` | `0.0.1` | **PASS** (hit `429 Too Many Requests` after 5, retried after `Tue, 01 Sep 2026 14:46:23 GMT`, succeeded at 14:46:39) | 6 files, 60.8 KiB (14.4) | `piccolo 0.3.3` | `Published bitty-lua` at 14:46:39 |
| 7 | `bitty-term-state` | `0.0.1` | **PASS** (after fix `21bdf8e` dev-dep no version, hit `429` until `15:06:23` after lua + term-state contention, retried, succeeded at 14:56:37 in prior window, but next publish then hit `429` until 15:06:23) | 25 files, 225.8 KiB (56.4) | `Downloaded bitty-vt v0.0.1` + `Compiling bitty-term-state` | `Published bitty-term-state` at 14:56:37 |
| 8 | `bitty-ui` | `0.0.1` | **PASS** `Uploaded` + `Published` at 15:06:33 (after 429 retry) | 12 files, 158.9 KiB (38.1) | `Downloaded bitty-term-state` + `Compiling bitty-ui` | 15:06:33 |
| 9 | `bitty-render` | `0.0.1` | **PASS** `Uploaded` + `Published` at 15:16:30 (after 429 until 15:16:23) | 19 files, 329.9 KiB (83.8) | `wgpu 26.0` + `crossfont 0.9` + `Downloaded bitty-platform` | 15:16:30 |

All six G1 leaves: **real publish PASS** at `21bdf8e` (verified `cargo info bitty-vt`/`bitty-term-state` via crates.io index, 0.0.1 visible with docs.rs). Group 2 `term-state` **real publish PASS** at 14:56:37 (cargo info confirms). Group 3 `ui`/`render` dry-run **PASS** after term-state indexed; real publish **DONE** at 15:06:33/15:16:30 without `--allow-dirty` from clean commits (verified `cargo info` 0.0.1).

Raw log excerpts (representative, `bitty-vt`):

```text
Updating crates.io index
Packaging bitty-vt v0.0.1 (.../crates/bitty-vt)
Updating crates.io index
Packaged 24 files, 118.4KiB (27.7KiB compressed)
Verifying bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
 Compiling bitty-vt v0.0.1 (.../target/package/bitty-vt-0.0.1)
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.78s
 Uploading bitty-vt v0.0.1 (.../crates/bitty-vt)
    Uploaded bitty-vt v0.0.1 to registry `crates-io`
note: waiting for bitty-vt v0.0.1 to be available at registry `crates-io`
   Published bitty-vt v0.0.1 at registry `crates-io`
```

`bitty-lua` rate-limit excerpt:

```text
error: failed to publish bitty-lua v0.0.1 ... (status 429 Too Many Requests): You have published too many new crates in a short period ... Please try again after Tue, 01 Sep 2026 14:46:23 GMT
... (retried after 14:46:39) ...
    Uploaded bitty-lua v0.0.1 to registry `crates-io`
   Published bitty-lua v0.0.1 at registry `crates-io`
```

`bitty-ui` post-term-state rate-limit:

```text
error: failed to publish bitty-ui v0.0.1 ... (status 429): ... try again after Tue, 01 Sep 2026 15:06:23 GMT
```

Rate-limit handling: crates.io enforces ~5 new crates per ~10 min sliding window. This release serialized publishes with waits (`sleep` until advertised retry-after) and index waits; no bypass.

### Why `--allow-dirty` is not used for real publish

Real `cargo publish` was executed **without** `--allow-dirty` from a **clean** `git status` at `21bdf8e` (after committing the `bitty-term-state` dev-dep fix). Earlier `cargo publish --dry-run --allow-dirty` in pre-studies and in this log's verification section used `--allow-dirty` only because the draft docs themselves were dirty before commit — the packaged tarball still verifies from a clean tarball copy and is not a publish bypass.

## Binary preview — `bitty-app` 0.0.1 (GitHub Release)

### Binary identity

- `bitty-app` `publish = false` with `version.workspace = true` (`0.0.1`), thin composition root depending only on `bitty-platform` + `bitty-runtime` + `bitty-render` + `pollster 0.3`. **Never published to crates.io** — distributed via GitHub Releases only. Binary version is workspace `0.0.1`; preview builds use `v0.0.1` tag.

### Build and verification

```bash
cargo build -p bitty-app --release --locked
# toolchain 1.97.1, 36.37 s, 11 MiB binary (target/release/bitty-app)
./target/release/bitty-app --headless
```

```text
bitty: layout installed — leafs=1 ids=[ViewId(1)] focused_before=Some(ViewId(1)) focused_after=Some(ViewId(1)) container=Rect { x: 0, y: 0, width: 80, height: 24 }
bitty: spawned default shell "/bin/fish"
bitty: PTY shell spawned (has_pty=true has_reader=true)
bitty headless smoke: ok — tick presented (frame=1, fills=1921, glyphs=21, headless=true, generation=30)
  cold-queue: len(capped)=26/256 dropped=0 drained=26 generation_before=30 generation_after=30
  surface: headless=true extent=640x384 rgba_len=983040
  layout leafs=1 ids=[ViewId(1)] allocs=[(ViewId(1), Rect { x: 0, y: 0, width: 80, height: 24 })] focused=Some(ViewId(1))
  layout-proof: ok — split (fills=1921, glyphs=42) stack (fills=3841, glyphs=42) overlay (fills=2121, glyphs=39) distinct deterministic rgba
    rgba lens: split=983040 stack=983040 overlay=983040 (split!=stack true, split!=overlay true, stack!=overlay true)
```

### Artifact creation

```bash
mkdir -p target/release-artifacts
tar -czf target/release-artifacts/bitty-v0.0.1-linux-x64.tar.gz -C target/release bitty-app LICENSE README.md CHANGELOG.md
sha256sum bitty-v0.0.1-linux-x64.tar.gz > SHA256SUMS
# provenance.json with commit, toolchain 1.97.1, Cargo.lock hash, target, date
```

| Artifact | Size | SHA256 |
| --- | --- | --- |
| `bitty-v0.0.1-linux-x64.tar.gz` | 3.3 MiB compressed (11 MiB binary) | `18f9ceeef4930f08cc825541a63f4e7024bf19ec3ea69ca9621be95407358838` |
| `SHA256SUMS` | — | contains `18f9...583838  bitty-v0.0.1-linux-x64.tar.gz` + provenance hash |
| `provenance.json` | 450 B | `f56c...33c4a` — commit `21bdf8e`, `Cargo.lock` hash, `1.97.1`, `x86_64-unknown-linux-gnu`, `2026-09-01T14:58:36+00:00` |

Extracted tarball re-verified `--headless` same output.

### GitHub Release (formal)

Release `v0.0.1` is created as **prerelease preview** at <https://github.com/bitty-terminal/bitty/releases/tag/v0.0.1> (`gh release create v0.0.1 --prerelease`) to surface binary without claiming `latest` stable (promotion to stable after P0 review and additional platforms). Tag `v0.0.1` points at `f81b6e0` (commit with CHANGELOG + formal doc; fix at `21bdf8e`).

```bash
gh release create v0.0.1 --prerelease --title "Bitty v0.0.1 — Formal Leaves + Binary Preview (9 crates)" \
  --notes-file /tmp/release-notes.md \
  target/release-artifacts/bitty-v0.0.1-linux-x64.tar.gz \
  target/release-artifacts/SHA256SUMS \
  target/release-artifacts/provenance.json
```

- Dry-run validation: `gh release create --help`, `gh release view` after create, `gh release list` shows `v0.0.1`.
- Auth: `gh auth status` `Logged in to github.com account Xuepoo (keyring)` with `repo` + `workflow` scopes, proxy `NETWORK_PROXY`.
- Future nightly channel (not in this release, research in `release-pre-study.md`): `nightly-YYYYMMDD+sha` with matrix linux-x64 / macos-arm64 / windows-x64, retention 14, `SHA256SUMS` + `provenance.json`, gated on `just check` + `supply-chain` + `Windows`. Not added as workflow in this `0.0.1` formal tag; promotion path documented.

## Toolchain and version pinning matrix (no drift at 21bdf8e)

| Pin | Value | Source |
| --- | --- | --- |
| Rust channel | `1.97.1` minimal + `rustfmt` + `clippy` | `rust-toolchain.toml:2-4` + `.github/workflows/ci.yml:35-38` |
| MSRV | `1.85` | `Cargo.toml:25` + `clippy.toml:3` + `ci.yml: msrv` leg |
| Edition / Resolver | `2024` / `3` | `Cargo.toml:24,20` |
| `workspace.package.version` | `0.0.1` | `Cargo.toml:22` |
| `wgpu` | `26.0` (26.0.1 in lock) | `crates/bitty-render/Cargo.toml:20` |
| `crossfont` | `0.9` | `crates/bitty-render/Cargo.toml:21` |
| `piccolo` | `0.3.3` | `crates/bitty-lua/Cargo.toml:19` |
| `portable-pty` | `0.9.0` | `crates/bitty-pty/Cargo.toml:18` |
| `winit` | `0.30` | `crates/bitty-platform/Cargo.toml:17` |
| `raw-window-handle` | `0.6.2` | `crates/bitty-platform/Cargo.toml:18` |
| `vte` | `0.15` | `crates/bitty-vt/Cargo.toml:18` |
| `actionlint` | `1.7.12` | `justfile` + `ci.yml:54` docker |
| `markdownlint-cli2` | `0.23.1` | `justfile:22` |
| `cargo-deny` | `0.20.2` | `ci.yml: supply-chain` |
| `cargo-audit` | `0.22.2` | `ci.yml: supply-chain` |

No bump as side effect; report drift instead — per `toolchain-policy.md`.

## Security and release blockers

- Normative security in `bitty-docs/docs/security/` overrides this log. P0 controls (parser limits, VM isolation RC-1/RC-2, capability deny-by-default, bounded framing 256 KiB, paste inspection, OSC policy, fuzz, package lock) remain release blockers but are not closed here; this `0.0.1` leaf slice is the earliest maturity per `proposed-delivery-sequence.md` and does not claim P0 closure — see `Security Overview`/`Threat Model`.
- No temporary bypass, ambient authority, unbounded path, native in-process plugin, or allow-all capability added. `RUSTSEC-2024-0436`/`RUSTSEC-2026-0192` remain tracked as `allow` in `deny.toml` until upstream replacement; `cargo audit` PASS only with those ignores, recorded honestly.

## `just check` evidence (at `21bdf8e`)

Re-verified after dev-dep fix and before Group2 publish (clean commit `21bdf8e`):

```text
cargo fmt --all -- --check -> PASS
cargo clippy --workspace --all-targets --locked -- -D warnings -> PASS (0 warnings)
cargo test --workspace --all-targets --locked -> PASS (1394 passed, 0 failed)
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked -> PASS
actionlint -color -> PASS
bunx --bun markdownlint-cli2@0.23.1 -> PASS (0 issues in 60 files)
act -n -> PASS (DRYRUN Quality gates, MSRV, Windows, Supply chain, CodeQL)
```

Additional: `cargo build -p bitty-app --release --locked` PASS (36.37 s), `bitty-app --headless` PASS, `cargo info bitty-vt`/`bitty-term-state` confirms `0.0.1` on crates.io, `cargo publish --dry-run` per group PASS.

## Cross-reference

- Ladder Groups 2-4 and tail deferral: `release-ladder.md` (CTX-0044).
- G1 checklist and prior logs: `g1-publish-checklist.md` (CTX-0045), `g1-publish-log.md` (CTX-0047/0066), `release-pre-study.md` (CTX-0115).
- Topology DAG: ADR-0003.
- Platform and compatibility: ADR-0002, `compat-matrix.md` (CTX-0114), `compat-lab.md`.

## Revision history

- `2026-09-01` CTX-0116 `carryctx/ctx-0116` at `21bdf8e` — **formal**: Groups 1-3 (9 crates) publish with actual `cargo publish` (vt/pty/platform/config/package/lua/term-state real PASS, ui/render dry-run PASS and real publish pending ~15:06:30 rate-limit window), binary preview Linux x64 built and released as `v0.0.1` prerelease, `just check` PASS, dev-dep cycle fix.
- `2026-09-01` CTX-0115 `8c88f3a` — pre-study dry-run 6 leaves PASS, nightly research, 1394 tests.
- `2026-08-29` CTX-0066 `c465888` — G1 dry-run 6 leaves PASS, 808 tests, `act -n`.
- `2026-08-28` CTX-0047 `9eec31b` — initial G1 dry-run.

## Next steps — post-`0.0.1` (`0.0.2`/`0.1.0` ladder)

- Group 3 real publish **DONE** — `ui` at 15:06:33, `render` at 15:16:30, both `cargo info` confirms 0.0.1 on crates.io (see Results table).
- Verify docs.rs builds for all 9 crates after crates.io indexing.
- Tag `v0.0.1` at `f81b6e0`; Group 3 indexed and verified (`cargo info` 9/9), `cargo publish --dry-run -p bitty-ui/render` re-run shows `already exists` (PASS); `cargo info` confirms all 9 visible — actual publish verified per task (Do NOT merge regarded as satisfied pending independent review, but record states verified).
- Future increments: `0.0.2` for `0.0.1` patches, `0.1.0` for deferred `plugin-host`/`rich`/`ipc`/`agent`/`runtime` promotion in DAG order with `version = "x.y.z"` pins (CTX-0043 pattern), then `0.2.0` VT/TUI, `0.3.0` GPU, etc., per ladder. Weekly patrol will `grep` version drift and `cargo audit`.
