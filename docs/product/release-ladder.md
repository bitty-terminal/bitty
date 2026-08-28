---
title: Release Ladder v0.1-v1.0
description: Draft v0.1-v1.0 release ladder and crate publish order for the bitty workspace linking CTX-0043 publish prep and the candidate version ladder from proposed-delivery-sequence
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Release Ladder v0.1-v1.0

## Status and provenance

- Status: **draft**. This ladder is a planning companion to the candidate
  version-maturity ladder recorded in
  [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
  (source: second historical ChatGPT share
  `6a8dae4b-2aec-83ea-9174-03abc1f81531`; English rendering, not reproduced).
  Nothing here is accepted direction, a roadmap commitment, or authorization
  to publish — it is a reviewable proposal awaiting independent review.
- Ownership: bitty CTX-0044 (updated CTX-0049, CTX-0050). Companion
  implementation is bitty CTX-0043 `chore(crate): prepare workspace for crates.io v0.1.0`
  — branch `ctx-0043/chore-crate-publish`, PR #74 — which set
  `workspace.package.version 0.0.0 -> 0.1.0`, added
  `description`/`license`/`repository`/`keywords`/`categories` workspace
  metadata, and set per-crate `publish` flags plus `description` and
  versioned `path` deps for publishable crates. CTX-0049
  `chore(version): adjust workspace to 0.0.1 (earliest)` adjusts the
  earliest publish to `0.0.1` and defers `0.1.0` until plugins etc. are
  more complete (see Workspace version mapping).
- Authority: if an ADR/RFC accepts part of this ladder, update that artifact
  and this record together. No open question is closed here.
- Relationship: the [Roadmap index](../../../bitty-docs/docs/roadmap/README.md)
  admits items only with accepted requirements, dependencies, owners, and
  success evidence. This draft does not satisfy that bar and must not be cited
  as a dated release promise. The
  [Proposed Delivery Sequence](../../../bitty-docs/docs/product/proposed-delivery-sequence.md)
  remains the provenance record; this ladder overlays a concrete **crate
  publish order** and **version mapping** without weakening normative
  security controls.

## Version ladder v0.1-v1.0 (candidate mapping)

The candidate ladder from `proposed-delivery-sequence.md` is retained verbatim
for traceability, now mapped to workspace crates and gate evidence. Version
numbers are **architecture-maturity labels**, not calendar promises.

| Version | Candidate scope (from proposed-delivery-sequence)          | Workspace crate focus for that slice                                                                                                                                          | Gate sketch                                                                                            |
| ------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| v0.0.x  | Architecture and protocol prototypes                       | `bitty-core` seed only; `publish = false`                                                                                                                                     | prototype                                                                                              |
| v0.1    | A shell runs correctly in a minimal terminal slice         | Minimal Correct Terminal: `vt` + `pty` + `term-state` + `platform` + `config` + `render` + `ui` + `runtime` + `app`; `package`/`lua` leaves ready but not wired into hot path | shell echo + resize + backpressure headless tests; `cargo check`; `cargo publish --dry-run` leaf batch |
| v0.2    | VT and TUI compatibility work                              | `bitty-vt`/`bitty-term-state` parser-to-action-to-state fidelity, compatibility matrix (OQ-004)                                                                               | differential tests, fuzz, Neovim/tmux TUI corpora                                                      |
| v0.3    | GPU rendering, fonts, performance, graphics protocols      | `bitty-render` (`wgpu 26.0`, `crossfont 0.9`) + `bitty-platform` `SurfaceTarget` seam; `sw-fallback` opt-in                                                                   | render snapshot tests, GPU present path headless, perf budget PB-1/PB-2                                |
| v0.4    | Lua configuration system                                   | `bitty-config` (`ConfigPlan`) + `bitty-lua` (`piccolo 0.3.3` RC-1/RC-2)                                                                                                       | config merge/reload/trust tests; Lua Fuel/wall + 32 MiB measurement                                    |
| v0.5    | Plugin API                                                 | `bitty-plugin-host` capability/event lifecycle (OQ-011/012/013)                                                                                                               | capability grant/revocation, bounded `EventQueue` tests                                                |
| v0.6    | Plugin manager and lazy loading                            | `bitty-package` lifecycle + manager overlay on host                                                                                                                           | activation/rollback, lazy-load budgets                                                                 |
| v0.7    | DevTools and the debug protocol                            | `bitty-runtime` instrumentation seam (no dedicated `bitty-debug` yet)                                                                                                         | debug protocol versioned surface + inspector                                                           |
| v0.8    | Rich presentation, Markdown stress, shell integration      | `bitty-rich` (OQ-008/015/016) rich blocks, hyperlinks, images                                                                                                                 | rich-block scene/zone tests, image limit/budget tests                                                  |
| v0.9    | IPC, `bitty ctl`, MCP adapter, and stabilization           | `bitty-ipc` + `bitty-agent` bounded framing/scopes (OQ-018)                                                                                                                   | framed 256 KiB, peer-credential auth, rate-limit RC-9/RC-10                                            |
| v1.0    | Stabilized plugin, configuration, command, debug contracts | All above under semver-compatible surfaces; Tier 1 platforms per ADR-0002                                                                                                     | compatibility matrix + security P0 gates + versioned APIs v1                                           |

Non-goals and daemon staging remain as in
`proposed-delivery-sequence.md`: `bittyd` and remote UI are **post-v1.0**
candidates (OQ-020), not part of this ladder.

## Workspace version mapping

- `workspace.package.version = "0.0.1"` (CTX-0049; was `"0.1.0"` in
  CTX-0043). The earliest publish slice (minimal Correct Terminal,
  `v0.1` ladder row) publishes at `0.0.1` when that slice lands;
  `0.1.0` is deferred until plugins etc. are more complete (see Group 4
  tail). Earlier tags remain `0.0.x` prototypes before `0.0.1` with
  `publish = false` at the workspace root.
- Per-crate `version.workspace = true` inherits `0.0.1`; publishable crates
  additionally pin internal `path` edges with `version = "0.0.1"` so
  `cargo publish` requires the dependency already on crates.io at `^0.0.1`.
  Draft crates keep `publish = false` but still carry `description`/`license`
  /`repository` for consistency and are **not** published at `0.0.1`.
- Future increments follow semver within the ladder: `0.0.2`/`0.0.x` for
  `0.0.1` patches, `0.1.0` for the deferred plugin-complete slice,
  `0.2.0` for VT/TUI, `0.3.0` for GPU, etc., with patch bumps for fixes.
  The `1.0` bump requires stabilization gates per
  [v1.0 criteria in proposed-delivery-sequence](../../../bitty-docs/docs/product/proposed-delivery-sequence.md#candidate-v10-criteria).

## Crate inventory (as of CTX-0043 head `7b215a2` / `3bfe386` base)

Sixteen members in `bitty/Cargo.toml`; `publish` flags added in CTX-0043:

| Crate               | Publish | Workspace deps                                                       | Role (per ADR-0003)                                                       |
| ------------------- | ------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `bitty-vt`          | true    | none                                                                 | VT parser -> `TerminalAction` (`vte 0.15`)                                |
| `bitty-pty`         | true    | none                                                                 | PTY lifecycle/backpressure (`portable-pty 0.9`)                           |
| `bitty-platform`    | true    | none                                                                 | winit 0.30 + `raw-window-handle 0.6.2` SurfaceTarget                      |
| `bitty-config`      | true    | none                                                                 | `ConfigPlan` typed pipeline                                               |
| `bitty-package`     | true    | none                                                                 | manifest/lockfile/integrity/lifecycle (OQ-021 accepted; signatures draft) |
| `bitty-lua`         | true    | none                                                                 | `piccolo 0.3.3` deterministic VM budgets RC-1/RC-2                        |
| `bitty-term-state`  | true    | `bitty-vt`                                                           | Terminal Truth grid/damage/snapshot                                       |
| `bitty-ui`          | true    | `bitty-term-state`                                                   | View/LayoutNode primitives                                                |
| `bitty-render`      | true    | `bitty-term-state`, `bitty-platform`                                 | wgpu 26.0 + crossfont 0.9 snapshot pipeline                               |
| `bitty-plugin-host` | false   | `bitty-term-state`, `bitty-config`, `bitty-package`                  | Plugin registry/capability/event queue (draft, OQ-014)                    |
| `bitty-rich`        | false   | `bitty-term-state`, `bitty-vt`                                       | Rich presentation helpers (draft, OQ-015/016)                             |
| `bitty-ipc`         | false   | none                                                                 | Bounded IPC/MCP stub (draft, OQ-018)                                      |
| `bitty-agent`       | false   | none                                                                 | Bounded Agent stub (draft, OQ-018/019)                                    |
| `bitty-runtime`     | false   | `vt`, `term-state`, `pty`, `render`, `platform`, `ui`, `plugin-host` | Orchestration (cold-path queue)                                           |
| `bitty-app`         | false   | `platform`, `runtime`                                                | Thin binary composition root                                              |
| `bitty-core`        | false   | none                                                                 | Bootstrap seed to be retired                                              |

Nine publishable at `0.0.1` (first six rows above (vt, pty, platform, config, package, lua) plus `term-state`/`ui`/`render`):
`vt`, `pty`, `platform`, `config`, `package`, `lua`, `term-state`, `ui`, `render`.
Seven remain `publish = false` until their RFC is accepted and they are wired
into the publishable set in order dependency order.

## Concrete publish order (leaf -> core -> branch -> tail)

The order is forced by the DAG in
[ADR 0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md)
and the `version = "0.0.1"` pins (CTX-0043 at `0.1.0`, adjusted to `0.0.1`
in CTX-0049). Publish groups are
sequential; within a group crates are unordered (independent).

### Group 1 — Leaves (`publish = true`, no workspace deps)

No internal path dependency with `version = "0.0.1"`; each can be
`cargo publish --dry-run` verified independently. CTX-0043 verified:

- `bitty-vt` — `cargo publish --dry-run --allow-dirty` Packaging+Verifying PASS
- `bitty-pty` — PASS
- `bitty-platform` — PASS (headless tests under `gui-tests` feature gated)
- `bitty-config` — PASS
- `bitty-package` — PASS
- `bitty-lua` — PASS (`piccolo 0.3.3`)

Publish these first, in any order, waiting for crates.io index propagation
between groups.

### Group 2 — Terminal Truth

- `bitty-term-state` — depends only on `bitty-vt = "0.0.1"`. Publish after
  `vt` is on crates.io. CTX-0043 dry-run correctly reported missing index
  for this ordering reason (metadata valid, `version` pin present).

### Group 3 — Presentation branch (parallel after Group 2)

- `bitty-ui` — depends on `bitty-term-state = "0.0.1"` — publish after Group 2.
- `bitty-render` — depends on `bitty-term-state = "0.0.1"` and
  `bitty-platform = "0.0.1"` — publish after Groups 1+2. Dev-edge `bitty-vt`
  is `dev-dependencies` only and does not impose publish ordering beyond
  `term-state`/`platform`.

`ui` and `render` have no edge between them and may publish concurrently once
their prerequisites are indexed. Together with Groups 1-2 they constitute the
earliest shell publish slice (ladder `v0.1` row): `vt`, `pty`, `platform`,
`config`, `package`, `lua`, `term-state`, `ui`, `render` at `0.0.1`.
(`config`/`package`/`lua` are leaves included at `0.0.1` for completeness
though not on the hot path of the minimal shell; `0.1.0` is deferred until
plugins etc. are more complete.)

### Group 4 — Later draft tail (`publish = false` today)

Deferred past `0.0.1` (and `0.1.0` deferred until plugins etc. are more
complete) until RFC acceptance and explicit wiring into the graph. All retain
`publish = false` with `description`/`license`/`repository` for consistency;
none is `cargo publish`ed at `0.0.1`:

- `bitty-plugin-host` — draft; depends on `term-state`, `config`, `package`.
  Publish only after Groups 1-3 are at the target version and OQ-014 is
  accepted; otherwise cycle/gate risk.
- `bitty-rich` — draft rich-content sibling of `term-state` (OQ-008/015/016).
- `bitty-ipc` / `bitty-agent` — draft bounded stubs (OQ-018/019); later `0.9`
  slice.
- `bitty-runtime` — orchestrator fan-in; publishes only when its seven
  dependencies are already published at the same version line. Kept
  `publish = false` at `0.0.1` — the `v0.1` row shell is validated via
  headless
  integration tests in the workspace, not via a crates.io `runtime` release.
- `bitty-app` — binary; never published (`publish = false`).
- `bitty-core` — seed to be retired; never published.

Future revision of this ladder will promote tail crates to `publish = true`
in DAG order (host before runtime, runtime before app if ever published) and
align with the `v0.5`/`v0.6`/`v0.8`/`v0.9` slices.

### Verification gates (from CTX-0043)

- `cargo check --workspace --all-targets --locked` PASS (CTX-0043 head).
- `cargo publish --dry-run --allow-dirty` per crate: six leaves PASS;
  dependent crates correctly fail on missing index (expected) with valid
  metadata and `version` pin. This confirms ordering, not metadata defects.
- `just check` equivalent: `cargo fmt --check` PASS,
  `cargo clippy --workspace --all-targets --locked -- -D warnings` PASS,
  `cargo test --workspace --all-targets --locked` PASS,
  `actionlint` PASS, `markdownlint` PASS.

### v0.1 slice evidence (CTX-0050 — `ctx-0050/feat-minimal-terminal`) — implemented, draft

CTX-0050 implements the `v0.1` row — Minimal Correct Terminal
(`vt` + `pty` + `term-state` + `platform` + `config` + `render` + `ui` + `runtime` + `app`;
`package`/`lua` leaves ready but not on the hot path per CTX-0049) — as a
**headless, deterministic, bounded** slice. Status remains **draft** until
independent review; this section records evidence, not acceptance.

- **Shell echo (headless, deterministic replay):**
  `crates/bitty-runtime/tests/v01_minimal_terminal.rs::v01_shell_echo_headless_and_deterministic_replay`
  feeds the same synthetic shell byte stream (`"bitty"` + SGR + OSC title + BEL)
  as one chunk, byte-by-byte, and split mid-escape; each path yields identical
  `Snapshot` text/generation/title and identical `PresentStats` fills/glyphs and
  bit-identical `headless_rgba`. The stream exercises
  `PTY bytes -> VT Parser -> TerminalAction -> State -> Snapshot + Damage -> GridRenderer -> Surface::headless_present`
  without window, GPU, or filesystem. Existing
  `bitty-vt` replay tests (`parser::tests::action_stream_identical_across_chunkings`,
  `tests/replay.rs` fixtures `shell_session`, `escape_storm`) and
  `bitty-runtime::tests::handle_pty_bytes_flow_reaches_render`
  provide the parser/state leg of the same contract.
- **Resize (headless, honest):**
  `v01_resize_headless_reconfigures_surface_and_reflows_layout_deterministically`
  proves `Runtime::handle_resize(PhysicalSize::new(800, 600))` recomputes the
  logical grid from `RuntimeConfig::grid_from_pixels` (800×600 → 100×37 cells at
  8×16), reconfigures the `Surface::headless` extent, reflows `LayoutNode` leaf
  allocations (horizontal split 100 → 50+50), and forces a full redraw; a
  zero-sized resize is correctly skipped per
  `bitty_platform::map_resize_to_surface_extent` (minimized/occluded contract).
  Covered also by `bitty-runtime::tests::handle_resize_reconfigures_surface_and_keeps_grid_pending_full_redraw`,
  `zero_resize_is_skipped_honestly`, `handle_resize_updates_container_and_reflows`
  and `bitty-app` `handle_resize` path via `PlatformEvent`.
- **Backpressure (bounded, no growth):**
  `v01_backpressure_bounded_no_growth` asserts the hard bound
  `MAX_BUFFERED_BYTES = READ_CHUNK_SIZE (8 KiB) × CHANNEL_CAPACITY_CHUNKS (16) = 128 KiB`
  and drives the runtime's bounded queues headlessly:
  `ColdQueue` capacity 2 with 5 title OSCs → `len == 2`, `dropped >= 3`;
  `PluginHost` side queue capacity 2 with 5 titles → `len == 2`, `dropped == 3`,
  `DropOldest` keeps the newest two. The PTY pump invariant
  (`bitty-pty::reader::tests::pump_respects_channel_bound_with_idle_consumer`:
  blocked `send` → channel at `CAPACITY + 1` chunks max → kernel PTY buffer → child
  blocks, no loss, no growth) is the same contract the runtime inherits via `bitty-pty`.
  No unbounded allocation path exists on the hot PTY → VT → State leg.
- **Deterministic replay (extra):** byte-identity across 1-byte and mid-escape splits
  above, plus layout determinism (`layout_allocations` identical across two runtimes
  with same tree+container) proven in `v01_resize_*` and in existing
  `deterministic_layout_same_tree_same_container`, `tick_with_split_composites_both_leaves_headlessly`
  (renders split/stack/overlay deterministically via `HeadlessRasterizer`).
- **Gates on this branch:**
  `cargo check --workspace --all-targets --locked` PASS,
  `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked` PASS,
  `cargo test --workspace --all-targets --locked` **708 passed, 0 failed** (704 prior + 4 new v0.1 proofs),
  `cargo clippy --workspace --all-targets --locked -- -D warnings` 0 warnings,
  `cargo fmt --all -- --check` clean,
  `just check` (fmt-check + clippy + test + actionlint + markdownlint) **0 issues**,
  `actionlint` 0, `markdownlint` 0.
- **App headless smoke:** `bitty-app::run_headless_smoke` and
  `headless_smoke_is_total_without_display_or_gpu` feed synthetic bytes,
  tick via `Surface::headless_present`, and prove the same `bitty-runtime` path
  from the binary composition root (`--headless` / `BITTY_HEADLESS=1` /
  display-unavailable fallback), including split/stack/overlay layout proofs
  deterministically.

No open question is closed by this branch; `v0.1` remains candidate until an
ADR/RFC with independent review accepts the slice. The next `0.0.2` patch line
and deferred `0.1.0` gating remain as described in Workspace version mapping.

## Cross-reference and maintenance

- Candidate spine and early-deferral list: canonical in
  [Proposed Delivery Sequence](../../../bitty-docs/docs/product/proposed-delivery-sequence.md#candidate-build-order-spine).
- Compatibility and platform bars: [ADR-0002](../../../bitty-docs/docs/decisions/adrs/ADR-0002-platform-support-tiers.md),
  [Compatibility Milestone RFC](../../../bitty-docs/docs/specifications/compatibility-milestone-rfc.md).
- Security gates for `v1.0` (parser limits, VM isolation, capabilities,
  paste protection, OSC policy, fuzz, package lock): normative in
  [Security Overview](../../../bitty-docs/docs/security/overview.md) and
  [Threat Model](../../../bitty-docs/docs/security/threat-model.md);
  this ladder does not weaken them.
- Maintain this file alongside `proposed-delivery-sequence.md`: when a
  version slice is accepted via ADR/RFC, bump `workspace.package.version`,
  flip tail `publish` flags in DAG order, and add `version = "x.y.z"` pins
  on the newly publishable edges (CTX-0043 pattern).

## Acceptance note (for proposed-delivery-sequence)

This ladder overlays the candidate build-order spine with a concrete
publishing sequence without accepting it. `proposed-delivery-sequence.md`
retains provenance and candidate status; this file adds the implementable
earliest slice at `0.0.1` (deferring `0.1.0` until plugins etc. are more
complete) and the forward ordering for `0.2`-`1.0`. Closing any
register item still requires its RFC/ADR with independent review per the
[open-question register](../../../bitty-docs/docs/decisions/open-questions.md).
Updated 2026-08-27 via CTX-0044 (`ctx-0044/docs-release-ladder`) on top of
CTX-0043 `7b215a2`; updated 2026-08-28 via CTX-0049
(`ctx-0049/chore-version-0-0-1`) adjusting earliest to `0.0.1` and deferring
`0.1.0`; updated 2026-08-28 via CTX-0050 (`ctx-0050/feat-minimal-terminal`)
adding headless `v0.1` slice evidence (shell echo deterministic replay,
resize, backpressure bounded, 708 tests, `just check` 0 issues) and a new
`crates/bitty-runtime/tests/v01_minimal_terminal.rs` integration suite —
still `status: draft`.
