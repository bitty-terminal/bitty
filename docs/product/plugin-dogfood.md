---
title: Plugin Dogfood — First-Party Bundled-Disabled Set (CTX-0096)
description: Verified dogfood of the public Plugin API via the five v1 bundled-disabled first-party plugins (shell-integration, tabs, statusline, palette, project) with manifest/capability/lifecycle parity, safe-mode, Terminal Truth, and bounded cold-path evidence
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Plugin Dogfood — First-Party Bundled-Disabled Set (CTX-0096)

## Status and provenance

- Status: **draft**. Evidence for the `v1` bundled-disabled set per the
  [Default Distribution RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/default-distribution-rfc.md)
  (`OQ-002`, accepted 2026-08-29) and the
  [Plugin Platform RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/plugin-platform-rfc.md)
  (`OQ-011`, `OQ-012`, `OQ-013`, accepted 2026-08-27). This document records
  **implemented evidence only**; it does not claim shipped, stable, or
  compatibility-guaranteed behavior beyond the reviewed code and tests in this
  worktree. Candidate contracts remain `Proposed`/`draft` until independent
  review per the new RFC lifecycle
  (`Draft -> experimental review evidence -> Accepted -> normative`).

- Ownership: bitty **CTX-0096** — _Dogfood Plugin API with first-party plugins_.
  - Priority: P2 | Area: extensibility/dogfooding | Labels: feat,area:plugin,P2 | Milestone: v0.1.0 | RFC: OQ-011, OQ-012, OQ-013 | Task: CTX-0096
- Scope: the **exact** accepted `v1` bundled-disabled set
  (`shell-integration`, `tabs`, `statusline`, `palette`, `project`) kept
  **bundled != enabled**, verified via the public `PluginManifest` /
  `CapabilityId` / `PluginHost` / `Runtime` surface with no private channel.
  `splits` and `search` remain **future candidates** (not implemented).
  `Panel Runtime`, `Browser`, `Agent`, `marketplace`, `daemon`, and `remote UI`
  are **explicitly out of scope** for this task and are not described as
  implemented.

- Worktree: `.worktrees/ctx-0096`, branch `carryctx/ctx-0096`, base `c0aadd2`
  (CTX-0095 vertical slice). Agent `core-implementer-0096`.

- Authority: every `v0.1` gate remains `status: draft` until independent
  review per `open-questions.md`. No open question is closed here. This doc
  does not add roadmap promises, does not claim Panel/Runtime/Browser/Agent
  behavior, and does not weaken normative security controls in
  `bitty-docs/docs/security/`.

## Contracts reconciled

- **Plugin Platform RFC** (`OQ-011`, `OQ-012`, `OQ-013`): capability
  deny-by-default, closed grammar, no wildcards, `fs.read:PARAM` /
  `fs.write:PARAM` with path globs, grants hash-bound to `manifest_hash`,
  per-subscription bounded queues with `DropOldest` (accepted `v1` default,
  `OQ-013` closed), per-plugin `1024` events / `256 KiB` and global
  `8192` events / `2 MiB` (`OQ-014` candidate values, enforced headlessly),
  `EventKind` closed set (four interception points), fail-open veto-wins.

- **Terminal State RFC**: only `Action` writes `State`. Plugins observe
  committed state only via bounded side queue `Snapshot`/`HostObservation`
  (never grid internals), never on the `PTY -> VT Parser -> Action -> State`
  hot path.

- **Isolation RFC** `R-007`: per-plugin logical isolation (one `(PluginId,
generation)` host view), per-plugin budget attribution
  (`total_dropped` / `dropped_per_queue` / `publish_count` / side-queue
  `dropped`), host-visible for `bitty plugin doctor`. No `unsafe`, no VM
  coupling in this slice (host is pure data + validation, `forbid(unsafe)`).

- **Capability RFC** `R-006`: deny-by-default, restricted stdlib (host owns
  closed `CapabilityId` set; unknown identifiers fail validation).

- **Vertical slice evidence `c0aadd2`**: single-window truth
  (`State` -> `Snapshot + Damage` -> `GridRenderer` -> `Surface::headless`
  deterministic RGBA) preserved without regression; layout and focus remain
  headless-testable.

Existing seams inspected via `ctxctl`:

- `crates/bitty-plugin-host/src/host.rs`, `registry.rs`, `grant.rs`,
  `capability.rs`, `event.rs`, `manifest.rs` — bounded, headless, `forbid(unsafe)`.
- `crates/bitty-config/src/types.rs`, `plan.rs`, `merge.rs` — `PluginSpec`
  (`id`, `enabled`) with scalar-replace merge, default empty set.
- `crates/bitty-runtime/src/runtime.rs`, `queue.rs` — cold-path `ColdQueue`
  `DropOldest` and `PluginHost` side queue bridging (ADR-0003 rule 4).

## Implementation (public API only)

All five bundled plugins are **plain `PluginManifest` values** built from the
same public types a third-party `xuepoo.*` manifest would use:

```rust
use bitty_plugin_host::{CapabilityId, PluginId, PluginManifest, QualifiedName};

let m = PluginManifest {
    identity: PluginIdentity {
        id: PluginId::new("bitty-terminal.shell-integration").unwrap(),
        name: "Shell Integration".into(),
        version: "0.1.0".into(),
        description: "OSC 7/133 semantic zones, cwd/title propagation".into(),
        license: Some("MIT".into()),
    },
    compat: Compat { bitty: Some(">=0.1,<1.0".into()), plugin_api: Some("^1.0".into()) },
    capabilities: {
        let mut c = CapabilityRequests::default();
        c.ids.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
        c
    },
    lazy: LazyTriggers {
        events: vec!["terminal.cwd-changed".into(), "terminal.title-changed".into()],
        ..Default::default()
    },
    ..Default::default()
};
```

No host-private import, no ambient authority, no `allow-all` capability. The
host path for every plugin is identical:
`declare -> resolve -> register -> (grant + hash check) -> activate`
(`GrantRecord` bound to `manifest_hash()`), then `subscribe` / `publish` /
`drain` via the bounded `EventPipeline` and `SideQueue<HostObservation>`.

### Catalog for `v1` — bundled, disabled

| Plugin ID                          | Policy                                                             | Core mechanism                                         | Capability sketch                                                                             | Lazy triggers                                                                                                          |
| ---------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------ | --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `bitty-terminal.shell-integration` | OSC 7/133 zones, cwd/title propagation, fail-closed fallback       | VT parser OSC 7/133, `ImageStore` anchor fallback      | `terminal.semantic-read`                                                                      | `terminal.cwd-changed`, `terminal.title-changed`, `terminal.bell`                                                      |
| `bitty-terminal.tabs`              | tab commands, tabline presentation, ordering, closing policy       | `LayoutNode` split primitives, tabline exclusive claim | `ui.rich` + claim `tabline`                                                                   | commands `bitty-terminal.tabs:new`, `close`, `next`; events `terminal.title-changed`, `focus.changed`; claim `tabline` |
| `bitty-terminal.statusline`        | cwd, mode, Git/task presentation, status-component composition     | statusline slot composition, semantic snapshot         | `terminal.semantic-read`, `ui.rich`                                                           | `terminal.cwd-changed`, `terminal.title-changed`                                                                       |
| `bitty-terminal.palette`           | command palette and picker via overlay, declarative list/text only | overlay slot, command registry, declarative primitives | `ui.overlay`                                                                                  | command `bitty-terminal.palette:toggle`; event `focus.changed`                                                         |
| `bitty-terminal.project`           | project discovery and session presentation                         | constrained project discovery                          | `terminal.semantic-read` + `fs.read:~/projects/**` (`FilesystemRequest` read `~/projects/**`) | commands `bitty-terminal.project:open`, `switch`; event `terminal.cwd-changed`                                         |

Accepted `v1` rule preserved: **bundled does not mean enabled**. A fresh
install with no user configuration (`EffectiveConfig::default` has empty
`plugins`) starts core only, identical to `bitty --safe`. Enabling is an
explicit user action:

```lua
-- init.lua -> ConfigPlan -> merge -> EffectiveConfig
plugins = {
  ["bitty-terminal.tabs"] = { enabled = true }, -- explicit opt-in, consent UX, permission-diff gate on capability increase
}
```

Project-scoped or workspace configuration may **narrow** the enabled set but
may never add a capability the user has not granted (workspace narrowing
`apply_workspace_narrowing` rejects additions). Unknown capabilities fail
validation; no wildcards, no `*`.

### Future candidates not implemented in this task

- `splits` and `search` — explicitly deferred as future candidates per the
  task scope; no manifest, no code, no docs claim them as implemented.
- `Panel Runtime`, `Browser`, `Agent`, `marketplace`, `daemon`, `remote UI` —
  explicitly out of scope; this doc and the code do not describe them as
  implemented and do not add a Panel provider, WebView, or Event Bus.

## Verification — bounded, headless, deterministic

All checks are headless, `forbid(unsafe)`, and run on both Linux and
`windows-latest` (`cargo check --target x86_64-pc-windows-gnu`).

### Bundled catalog tests (`crates/bitty-plugin-host`)

```bash
cargo test -p bitty-plugin-host --test bundled_dogfood
```

- `bundled_manifests_are_five_and_validate` — exactly five manifests,
  each `validate()` passes, `MANIFEST_MAX_BYTES` respected, sorted ids match
  the `v1` set, `is_bundled` correct.
- `bundled_plugins_load_via_public_api_with_grant_checks` — each manifest
  goes `declare -> resolve -> register`; `activate` fail-closed without grant,
  succeeds after `GrantRecord::granted` with exact `manifest_hash`; registry
  ends `Activated` with five entries.
- `bundled_parity_with_third_party_same_manifest_shape` — `xuepoo.shell-mirror`
  with identical `capabilities`/`lazy`/`compat` has identical validation and
  grant lifecycle as `bitty-terminal.shell-integration` (no private channel).
- `bundled_plugins_are_observation_only_and_use_bounded_side_queue` —
  flood side queue `16 -> 10` pushes: oldest dropped, newest survive
  (`DropOldest`), `len == 4`, `dropped == 6`; per-sub pipeline
  `TerminalBell` flood `80` into `64` cap -> `queued <= 64`,
  `invariant_queue_bounds` and `invariant_global_bounds` hold; coalescable
  `title-changed` collapses as specified.
- `default_disabled_safe_mode_leaves_host_functional` — empty host has zero
  registry/side/pipeline queues; `safe_mode` rejects all five
  `bitty-terminal.*` (treated as non-`bitty.` builtin, parity with
  third-party) without panic; `bitty.core` would still be allowed under the
  candidate built-in namespace; host remains usable and bounded.
- `grant_revocation_and_hash_binding_for_bundled` — `fs.read:~/projects/**`
  granted, then single-capability `revoke` detaches at next boundary;
  version bump changes `manifest_hash` and the stale grant no longer matches
  (`is_granted` false, deny-by-default).
- `no_hot_path_coupling_via_public_api_only` — bundled uses only public
  `PluginManifest`/`CapabilityId`/`QualifiedName`/`FilesystemRequest`; no
  `wgpu`/`winit`/`Window`/`Surface`/`PtyWriter` leak; `PluginHost` is
  headless-constructible.

### Runtime integration (`crates/bitty-runtime`)

```bash
cargo test -p bitty-runtime --test bundled_dogfood_runtime
```

- `default_disabled_zero_plugins_and_tick_still_presents` — `EffectiveConfig::default`
  has empty `plugins`; `Runtime::with_defaults()` has zero `PluginHost`
  registry entries and `plugin_side_len == 0`; `tick` presents the pending
  full redraw via `Surface::headless` (deterministic RGBA) and `handle_pty_bytes`
  still produces `cold_queue` and side-queue observations.
- `bundled_plugins_load_via_public_api_through_runtime` — each bundled manifest
  via `Runtime::register_plugin` / `activate_plugin` (which delegate to the
  same `PluginHost` path) with grant checks; `subscribe_plugin_event` and
  `publish`/`drain` via bounded pipeline (`32`/`8 KiB` batch).
- `safe_mode_compatibility_bundled_disabled_and_runtime_still_ticks` —
  `set_plugin_safe_mode(true)` rejects all five bundled declares; `tick`
  still presents; disabling safe mode re-allows registration.
- `config_driven_enable_respects_default_disabled_and_public_api` —
  `EffectiveConfig { plugins: [PluginSpec { id, enabled: true }] }` loads
  exactly the named bundled manifest; `enabled: false` loads zero; both via
  `bundled_manifest_for` and `register_plugin`.
- `terminal_truth_protected_plugin_observes_snapshot_not_grid_mutation` —
  `handle_pty_bytes(b"\x1b]0;side-observation-test\x07")` generates a
  `HostObservation::TitleChanged` in the bounded side queue; `State::generation`
  advances; `Snapshot` retains the title; no plugin writes `State`.
- `bounded_cold_path_drop_oldest_and_attributable` — `DropOldest` per-sub
  `64` / per-plugin `1024`+`256 KiB` / global `8192`+`2 MiB`; flood of
  non-coalescable `TerminalBell` yields `plugin_total_dropped > 0` and
  `invariant_queue_bounds`/`invariant_global_bounds` hold; side queue flood
  beyond `16` yields `plugin_side_dropped > 0` and `plugin_side_len == capacity`;
  `plugin_dropped_per_queue` is per-queue attributed for `bitty plugin doctor`.
- `no_panel_runtime_browser_agent_marketplace_smuggled` — catalog is exactly
  the five `v1` ids, contains no `splits`/`search` and no
  `browser`/`agent`/`panel` substrings.

### Workspace gates (must PASS before commit)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets
just check   # fmt-check + clippy + test + actionlint + markdownlint
act -n       # workflow syntax
```

On this worktree at `c0aadd2` + this task delta:

- `cargo fmt --all -- --check` — PASS
- `cargo clippy --workspace --all-targets --locked -- -D warnings` — PASS (0 warnings)
- `cargo test --workspace --all-targets` — PASS (including the 14 new dogfood tests above, plus the existing vertical-slice and dogfooding suites; idle `tick` remains `None` when no damage)
- `cargo check --workspace --all-targets` — PASS
- `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets` — PASS (headless seams remain window-less)
- `just check` — PASS (0 issues; `actionlint` and `markdownlint-cli2@0.23.1` via `bunx --bun` as pinned in `justfile`)
- `act -n` — PASS (`.github/workflows/ci.yml`, `codeql.yml` syntax)
- Hot-path regression: `v01_minimal_terminal`, `runtime_soft_present`,
  `final_integration`, and `soak` suites still pass with identical
  deterministic replay (`same bytes -> same generation/fills/glyphs -> bit-identical RGBA`).

## Changed files in this task

- `crates/bitty-plugin-host/src/bundled.rs` — new catalog module (public API only, `forbid(unsafe)`, five manifests, helpers `all_bundled_manifests`/`bundled_manifest_for`/`is_bundled`, bounded-string checks, unit tests).
- `crates/bitty-plugin-host/src/lib.rs` — expose `pub mod bundled`.
- `crates/bitty-plugin-host/tests/bundled_dogfood.rs` — new headless dogfood suite (7 tests, parity, bounded, safe-mode, hash binding).
- `crates/bitty-runtime/Cargo.toml` — add `bitty-config` as `dev-dep` for `EffectiveConfig` dogfood tests (no runtime hot-path dep; `Cargo.lock` updated).
- `crates/bitty-runtime/tests/bundled_dogfood_runtime.rs` — new runtime dogfood suite (7 tests, default-disabled via `EffectiveConfig`, safe-mode via `Runtime`, Terminal Truth via side queue, bounded cold path).
- `Cargo.lock` — updated for the above dev-dep.
- This file — new evidence doc, `status: draft`, synchronized only from the test and gate evidence above (no stale claims, no Panel/Browser/Agent/marketplace/daemon claims).

## Unresolved risks and follow-up

- **Built-in namespace prefix**: `PluginHost::declare` safe-mode currently
  checks `starts_with("bitty.")` as the candidate built-in namespace. The
  `v1` bundled set uses `bitty-terminal.*`, which is treated as non-builtin
  and thus rejected in safe mode — this gives parity with third-party
  `xuepoo.*` and matches `bitty --safe` as zero non-core plugins by
  construction. If a future decision promotes `bitty-terminal.*` to a
  built-in namespace, the prefix check and this doc must be revised together
  behind a reviewed change; no silent fix is applied here.

- **Filesystem grant scope**: `bitty-terminal.project` currently requests
  `fs.read:~/projects/**` as a single glob. Per-package resolver policy and
  symlink/device rejection are owned by `bitty-package` and the capability
  gate, not by this dogfood slice. Path-scoped `fs` grants with real-path
  resolution remain as specified; no `fs.write` is requested by any `v1`
  bundled plugin.

- **Queue budgets remain candidate values**: per-sub `64`, per-plugin `1024`
  / `256 KiB`, global `8192` / `2 MiB`, batch `32` / `8 KiB`, side queue
  `128` (runtime) are the accepted `v1` defaults enforced headlessly. Exact
  timeout milliseconds and `RC-1`/`RC-2` instruction/memory enforcement remain
  `OQ-014` candidates; the host's Lua VM (`piccolo`) seam is deferred.

- **Panel ecosystem**: the Panel Extensibility Vision (CTX-0094) remains
  candidate; this task does not add a Panel provider, WebView, or Event Bus.
  Any future Panel dogfood must extend this catalog via a new RFC-governed
  task.

## Cross-reference

- [Plugin Platform RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/plugin-platform-rfc.md) — accepted `v1` API, capability grammar, event pipeline.
- [Default Distribution RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/default-distribution-rfc.md) — bundled-disabled, five disable surfaces, `generation` disposal, `PB-5` `<= 40 MiB`.
- [Plugin Roadmap](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/product/plugin-roadmap.md) — first-party wave and dogfood validation signals (source for the `v1` five).
- [Terminal State RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/terminal-state-rfc.md) — `Action::Print` as sole `State` write path, damage model.
- [Core and Plugin Boundaries](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/architecture/core-boundaries.md) — mechanism/policy split, declarative UI, generation lifecycle.
- [Release Ladder](release-ladder.md) — `v0.1` slice at `c0aadd2` (this task dogfoods that slice; no open question closed).
- [Dogfooding — Minimal Terminal Daily-Driver](dogfooding.md) — `Phase G` checklist (complementary; this doc does not weaken its `0.1` gates).
- Tests: `crates/bitty-plugin-host/tests/bundled_dogfood.rs`, `crates/bitty-runtime/tests/bundled_dogfood_runtime.rs`; manifests: `crates/bitty-plugin-host/src/bundled.rs`.

## Revision history

- `2026-08-31` CTX-0096 `carryctx/ctx-0096` — draft creation: add `bundled.rs` catalog, two dogfood suites (14 tests), `plugin-dogfood.md` evidence, `Cargo.lock` sync; gates `just check` + `act -n` + `cargo test` PASS; worktree dirty until PR.
