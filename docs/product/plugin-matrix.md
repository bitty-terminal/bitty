---
title: First-Party Plugin Matrix as Panel Runtime Consumers (CTX-0107)
description: Research matrix surveying remaining first-party plugins before release as generic Panel Runtime consumers with priority, reconciled with shipped shell-integration/tabs/palette/statusline/project and Panel Runtime 05e8803
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# First-Party Plugin Matrix as Panel Runtime Consumers (CTX-0107)

## Status and provenance

- Status: **draft** — research only. This document surveys the remaining first-party
  plugin candidates before any release as consumers of the generic Panel Runtime.
  It proposes no implementation, authorizes no shipped, stable, or
  compatibility-guaranteed behavior, and does not close an open question.
  The lifecycle is `Draft -> experimental review evidence -> Accepted -> normative`;
  only `Accepted` or `normative` documents authorize shipped behavior.
  Candidate contracts remain `Proposed`/`draft` until independent review per the new
  RFC lifecycle (`Draft -> experimental review evidence -> Accepted -> normative`).

- Ownership: bitty **CTX-0107** — _Research first-party plugin matrix before release_.
  - Priority: P2 | Area: plugin | Labels: feat,area:plugin,P2 | Milestone: v0.1.0
  - RFC: OQ-011 | Task: CTX-0107 | Cross-repo textual: bitty-docs CTX-0120
  - Worktree: `.worktrees/ctx-0107`, branch `carryctx/ctx-0107`, base `5c885f2`
    (CTX-0105) plus Panel Runtime `05e8803` (CTX-0102).

- Authority: every `v0.1` gate remains `status: draft` until independent review
  per `open-questions.md`. No open question is closed here. This document does not
  add roadmap promises, does not claim Panel/Runtime/Browser/Agent behavior beyond
  the reviewed code and tests in this worktree, and does not weaken normative
  security controls in `bitty-docs/docs/security/`.

- Relationship: this is the bitty-side companion to the docs-side
  [Browser and Agent Panel Integration Pre-Study](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/browser-agent-pre-study.md)
  (CTX-0120, candidate BA-1..BA-12) and the
  [Panel Runtime and Event Bus Pre-Study](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/panel-runtime-pre-study.md)
  (CTX-0119 / requested `05e8803`, candidate PR-1..PR-12). The matrix here reconciles
  the product shape (which plugins exist and at what priority) while CTX-0120
  reconciles the mechanism depth (how Browser and Agent would be isolated).

## Scope

In scope (research, not normative):

- Inventory of the **shipped** first-party set at `5c885f2` (shell-integration,
  tabs, palette, statusline, project) as verified Panel Runtime consumers and
  the stable baseline this matrix builds on.
- Survey of **remaining** first-party candidates before release as Panel Runtime
  consumers: file manager, Git panel, mail panel, AI panel, browser panel,
  plus close neighbors (editor preview, logs, scratchpad/peek, mirror) needed
  to test composition completeness.
- For each candidate: Panel host type (`View` `Terminal`/`Browser` vs `Panel`),
  policy owned by the plugin, Core mechanism relied on, illustrative capability
  sketch (`panel.*`, `fs.*`, `process.*`, `network.*`, `browser.*`, `agent.*`),
  Event Bus topics, priority before release, and distribution posture
  (bundled-disabled vs candidate-not-bundled).
- Reconciliation with the generic Panel Runtime contract at `05e8803` (CTX-0102)
  and the incremental Panel deliveries at `34eae1c` (shell-integration),
  `d799024` (tabs), `5c885f2` (palette/statusline/project).

Out of scope and owned elsewhere:

- VT parser, grid, cursor, mode, damage, and reply invariants (OQ-007,
  Terminal State RFC); text segmentation and atlas
  (Text and Rendering RFC); image and scene contracts (Rich Presentation RFC).
- Plugin API v1, capability grammar, manifest, and three-level queue budgets
  (OQ-011/012/013, Plugin Platform RFC) — referenced, not redefined.
- Per-plugin VM instruction/memory/task enforcement and RC-1..RC-10 ceilings
  (OQ-014, Isolation Resource RFC) — referenced as candidate measurement.
- IPC framing, discovery, and per-connection quotas RC-9/RC-10 (OQ-018, IPC
  and Agent RFC); daemon and remote UI trust boundaries
  (ADR 0008, post-v1.0).
- Package manifest, lockfile, and activation verification
  (OQ-021/022, Package Lifecycle RFC); distribution store signing
  (OQ-029).

## Contracts reconciled

- **Plugin Platform RFC** (OQ-011/012/013, accepted 2026-08-27): closed capability
  grammar, deny-by-default, no wildcards, `fs.read:PARAM`/`fs.write:PARAM` with
  path globs, grants hash-bound to `manifest_hash`, per-subscription bounded
  queues `DropOldest` as accepted `v1` default (OQ-013 closed), `EventKind` closed
  set with four interception points fail-open veto-wins.

- **Isolation Resource RFC** (OQ-014, accepted 2026-08-28): three-level envelope
  per-subscription `64`, per-plugin `1024`/`256 KiB`, global `8192`/`2 MiB`
  hard-gated at host admission with `DropOldest`, coalescing for observation
  topics, RC-1 `10^7`/`50 ms`/`8 ms`, RC-2 `32 MiB` per VM, RC-3 `512 MiB`
  aggregate, attribution for `bitty plugin doctor`.

- **Default Distribution RFC** (OQ-002, accepted 2026-08-29): bundled does not mean
  enabled. The `v1` enabled set is empty; a fresh install with no user
  configuration starts core only, identical to `bitty --safe`. Five disable
  surfaces, generation disposal and budget reclaim, promotion criteria for any
  future enabled-by-default addition (PB-1 `<= 100 ms` p50 cold start, PB-2
  `<= 80 MiB` idle one window, PB-5 `<= 40 MiB` distribution cap, PB-7
  `<= 1%` CPU idle must hold).

- **Terminal State RFC** (OQ-007): only `Action` writes `State`. Plugins observe
  committed state via bounded `ColdEvent` -> `SideQueue<HostObservation>` or
  Panel EventBus (never grid internals), never on the hot path
  `PTY -> VT Parser -> Action -> State`.

- **Vertical slice `c0aadd2`** and **Workspace/Multi-terminal `f0f7cf1`** plus
  **Panel Runtime `05e8803`**: single-process winit one-registry-per-window
  headless, `TerminalId != ViewId` with `RuntimeId`/`PersistentId`/`Generation`,
  `Workspace -> LayoutTree (H/V) -> View` with Core-owned decoration.

## Panel Runtime contract at 05e8803 — recap

The generic Panel Runtime (CTX-0102, `05e8803`, PR #159, pre-study `9032d1e`)
reconciled with `f0f7cf1` (Workspace) and `6f30c2f`/`c3a2928` (accepted
TerminalRegistry/View and Workspace Compositor):

- `PanelId` distinct newtype with no `From` bridge to `ViewId`/`TerminalId`;
  `Generation` monotonic with reserve `1024` and fail-closed exhaustion
  (`GenerationExhausted` within `1024` of `u64::MAX`, no wrap).
- Lifecycle `Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed`
  with validated state transitions; `Mounted` binds `PanelId` to an empty `ViewId`
  (`ViewContent::Panel(PanelId)` as the current research-preferred Option A).
- Closed `PanelType` set contributed by `PanelProvider` (candidate domain values
  `terminal`/`rich`/`browser`/`helper`/`canvas` validated via manifest, not invented
  per call).
- Command registry closed qualified `owner.name:command` (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`,
  `<= 128` chars), duplicates rejected not shadowed, per-type `32` bound.
- Overlay max `4` plus `1` modal with modal exclusivity (`OverlayBusy`) and text
  `128`/tooltip `256` bounds (`Palette` kind for palette).
- Focus MRU per Workspace per Window (`PanelFocus` with MRU ordering and hidden-panel
  promotion); exactly zero or one `ViewId` or `PanelId` focused per active workspace.
- EventBus with three levels matching OQ-014: per-subscription `64`, per-panel
  `1024`/`256 KiB`, global `8192`/`2 MiB`, `8 KiB` payload (`BoundedText` strict),
  batch `32`/`8 KiB` aggregate per wakeup, `DropOldest` default with counted
  per-queue attribution and coalescing for declared topics.
- Capability isolation per `(PanelId, generation)` deny-by-default via
  `CapabilityId` panel family `panel.provider` / `panel.create` /
  `panel.focus` / `panel.overlay` — no ambient authority, no first-party bypass.

Candidate bounded defaults preserved as PR-1..PR-12 (all validated before allocation,
typed `TooManyPanels`/`UnknownPanelType`/`PanelAlreadyMounted`/`AlreadyMounted`/
`StaleHandle`/`GenerationExhausted`, transactional denial FS-P1..FS-P5):

| ID    | Dimension                      | Candidate default at 05e8803                         | Validation point                       |
| ----- | ------------------------------ | ---------------------------------------------------- | -------------------------------------- |
| PR-1  | Panels per workspace           | `[1, 32]`, default `16`                              | `PanelRegistry::create` + `ConfigPlan` |
| PR-2  | Panels per window              | `[1, 64]`, default `32` aggregate                    | admission before mount                 |
| PR-3  | Event topics total             | `<= 256` distinct topics per process                 | manifest validation                    |
| PR-4  | Subscriptions per panel/plugin | `<= 32` topics                                       | registration                           |
| PR-5  | Event payload                  | `<= 8 KiB` per event                                 | host admission                         |
| PR-6  | Batch per wakeup               | `<= 32` events or `<= 8 KiB` aggregate, smaller wins | `drain_batch`                          |
| PR-7  | Per-subscription queue         | `64` events strict FIFO                              | enqueue                                |
| PR-8  | Per-panel/plugin queue         | `1024` / `256 KiB`                                   | publish                                |
| PR-9  | Global bus queue               | `8192` / `2 MiB` hard-gated                          | `would_exceed_global_limits`           |
| PR-10 | Overlay count per window       | `<= 4` active plus `1` modal                         | compositor commit                      |
| PR-11 | Overlay text/tooltip           | `<= 128` / `<= 256` chars                            | composition                            |
| PR-12 | Commands per panel type        | `<= 32` commands per type                            | manifest validation                    |

All ceilings are candidate defaults that fit inside the accepted three-level
envelope without a new global budget family. Changing a value requires a
reviewed RFC revision.

## Shipped set at 5c885f2 — reconciliation baseline

At `5c885f2` (CTX-0105, PR #167) the five `v1` candidates are shipped as
**bundled-disabled, isolated, observation-only Panel Runtime consumers** via
the public `PluginHost` path (`declare -> resolve -> register -> GrantRecord
hash-bound -> activate -> subscribe -> publish -> drain SideQueue DropOldest`)
and the PanelRegistry public path (`PanelRegistry::new -> create_panel ->
mount_panel -> focus_panel` with `PanelType::Helper`). No private channel,
`forbid(unsafe)`, bounded queues `64`/`1024`/`2 MiB`/`8192` `DropOldest` with
`8 KiB` payload and `32`/`8 KiB` batch, single-process headless without
PTY/GPU leak, default disabled (`EffectiveConfig` empty), safe-mode rejects
`bitty-terminal.*` as non-builtin without panic, identical to `xuepoo.*` parity.

| Plugin ID                                      | Panel host at 5c885f2                                                                                                       | Policy owned by the plugin                                                                                                         | Core mechanism relied on                                                                                                                                                                                   | Capability sketch (illustrative)                                                                                                           | Panel Runtime surface exercised                                                                                                                                                                               | Verification at 5c885f2                                                                                                                                     |
| ---------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bitty-terminal.shell-integration` (`34eae1c`) | none — terminal-owned OSC 7/133 derivation                                                                                  | Semantic zones, cwd/title propagation, prompt `A`/`B` and command `C`/`D;code` with `D;code` exit derivation, fail-closed fallback | VT parser OSC 7/133, `State::cwd_report`/`State::zones` (`ZONE_RECORDS_MAX 1024`), `ZoneRecord::exit_code`, `ImageStore` anchor fallback                                                                   | `terminal.semantic-read` read-only                                                                                                         | `ColdEvent` -> `SideQueue<HostObservation>` `DropOldest` `64`/`1024`/`2 MiB`/`8192`; Panel EventBus same bounds when mounted; `ShellIntegration::create_shell_panel` via `PanelType::Helper`                  | `shell_integration_panel.rs` — default-disabled, public host path, subscribe->publish->drain, safe-mode reject, third-party parity, 8 KiB batch, coalescing |
| `bitty-terminal.tabs` (`d799024`)              | `Panel(PanelId)` with `PanelType::Helper` — reuses `LayoutNode` `stack`/`split` deterministically, no hardcoded `Tabs` node | Tab commands, tabline presentation, ordering, closing policy                                                                       | `LayoutNode` `stack`/`split` primitives, `tabline` exclusive claim, status slot composition; `TerminalRegistry` lifecycle via Panel (`create_terminal`/`create_view`/`attach`/`set_focus`/`move_terminal`) | `ui.rich` + claim `tabline`                                                                                                                | `TerminalRegistry` + `Workspace`/`View`/`Focus` via Panel public path only (`PanelRegistry::new` -> `create_panel` -> `mount_panel` -> `focus_panel`); Layout reuse `Stack`/`Split` 32 leaves, title `<= 128` | `tabs_panel.rs` — default-disabled, public host path with `ui.rich` + `tabline` grant, `PanelEventBus` `64`/`1024`/`8192`, MRU, `Stack`/`Split` determinism |
| `bitty-terminal.palette` (`5c885f2`)           | overlay `Palette` kind (`4+1` with modal exclusivity, text `128`/tooltip `256`)                                             | Command palette and picker UI, fuzzy filtering, preview presentation                                                               | Command registry qualified `owner.name:command` duplicate-rejected per-panel `32` bound `128` grammar, overlay focus MRU                                                                                   | `ui.overlay` (`panel.overlay` in pre-study mapping)                                                                                        | `CommandRegistry`, `OverlayManager`, `PanelFocus` via `PanelRegistry`; `palette_overlay_bounds` centered clipping, filter pure bounded                                                                        | `palette_statusline_project_panel.rs` — command registry + overlay focus MRU + bounded payload batch                                                        |
| `bitty-terminal.statusline` (`5c885f2`)        | `Panel(PanelId)` status-component composition via Panel EventBus                                                            | cwd, mode, Git and task state, status component composition policy                                                                 | Status slot composition, semantic snapshot, zone metadata from shell integration; `STATUSLINE_MAX_COMPONENTS 8 x 64`, total `128`                                                                          | `terminal.semantic-read`, `ui.rich` (status slot)                                                                                          | Panel EventBus `64`/`1024`/`8192` with coalescing, observation-only `State` reads, no hot path                                                                                                                | same suite — reactive via EventBus coalescing, deterministic pure render, no grid mutation                                                                  |
| `bitty-terminal.project` (`5c885f2`)           | `Panel(PanelId)` session surface                                                                                            | Project discovery and session presentation                                                                                         | Constrained project discovery and session metadata; `PROJECT_MAX_PROJECTS 64`, `PROJECT_NAME_MAX_CHARS 128`                                                                                                | `terminal.semantic-read` + `fs.read:~/projects/**` (`FilesystemRequest` read `~/projects/**`, `4096` path bound, no `..`, no control/null) | fs isolation via `CapabilityId` family `Fs` and `GrantRecord` hash-bound deny-by-default, per-panel isolation, `is_within_projects` pure bounded check                                                        | same suite — `list_projects` sorted deduped 64, fs isolation, per-panel deny-by-default                                                                     |

Accepted rules preserved by this baseline (Default Distribution `v1`):

- Bundled does not mean enabled. `EffectiveConfig::default` has empty `plugins`
  — fresh install starts core only, identical to `bitty --safe`. Enabling is
  explicit via `ConfigPlan` (`plugins = { ["bitty-terminal.tabs"] = { enabled = true } }`)
  with capability consent and the permission-diff gate (R-016/R-022) for
  capability-increasing updates.
- Project-scoped or workspace configuration may narrow the enabled set but may
  never add a capability the user has not granted (`apply_workspace_narrowing`
  rejects additions). Unknown capabilities fail validation; no wildcards, no `*`.
- Plugins are observation-only and off the parser/render/input hot paths; bounded
  decoding; generation-scoped ownership; `bitty plugin doctor` attributable drops.

`splits` and `search` remain explicitly deferred as future candidates (not
bundled, not implemented at `5c885f2`). `Panel Runtime` itself is the host
mechanism at `05e8803`, not a product plugin.

## Candidate remaining matrix — Panel Runtime consumers before release

The remaining candidates are evaluated as **future Panel Runtime consumers**,
not as shipped plugins. Each must reuse the Panel lifecycle
(`Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed`), the
`ViewContent::Panel(PanelId)` (or `ViewContent::Browser` for browser) placement,
command/overlay/focus/EventBus surfaces with their bounded ceilings, and the
capability deny-by-default gate. No candidate invents a hot-path hook, a
private host channel, or ambient filesystem/network authority.

Topics are illustrative qualified `owner.name:topic` strings bounded `<= 64`
bytes; payloads obey `PR-5..PR-9` (`8 KiB` per event, `32`/`8 KiB` batch,
`DropOldest` with coalescing for observation topics). Capabilities obey the
closed grammar from the Plugin Platform RFC; families not listed are not
implied.

### Primary candidates requested in CTX-0107

| Plugin ID                      | Candidate panel host                                                                                                                                                             | Policy owned by the plugin                                                                                                                            | Core mechanism relied on                                                                                                                                                                                                                                                                                                                       | Illustrative capability sketch                                                                                                                                                                                                                                                                      | Illustrative bus topics (produce/consume)                                                                                                                                                                                                | Priority before release | Distribution posture before release                                                                                                                                                                                                                                                                           |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bitty-terminal.file-manager`  | `Panel(PanelId)` tiled workspace (reuses `LayoutNode` `H`/`V` with panel content, not a PTY)                                                                                     | Directory listing, preview, rename/move/copy UX, selection, and status presentation; fails closed when `fs.*` denied                                  | `PanelRegistry` mount+suspend lifecycle; `LayoutNode` leaf placement; `ColdEvent` cwd observation from shell integration; statusline composition for cwd/branch hints                                                                                                                                                                          | `panel.provider` + `panel.create` + `fs.read:PATH_GLOB` (read-only listing, e.g. `~/projects/**` or user-chosen glob constrained via `FilesystemRequest`) plus optional `fs.write:PATH_GLOB` only for user-confirmed mutations (`process.spawn:rg/fd` if using `rg`/`fd` CLI adapters, still gated) | produce `xuepoo.files:file.open` -> consume `xuepoo.editor:open`; consume `terminal.cwd-changed`, `terminal.title-changed`                                                                                                               | **P1 — next**           | **candidate, not bundled before release** — needs `fs.*` real-path/symlink/device review and per-project narrowing; stays out of `v1` distribution until its own RFC (file-manager as Panel is the highest composition proof after the shipped five)                                                          |
| `bitty-terminal.git-panel`     | `Panel(PanelId)` adjacent to terminal and file manager (same window)                                                                                                             | Branch, status, diff, log presentation, commit staging UX, and Git-aware statusline segment selection; policy decides filtering and ranking, not core | System CLI reuse via `process.spawn:git(...)` with manifest-declared `[tools.git]` allowlist (per Layer 2 of `plugin-reuse-and-providers.md`), plus `terminal.semantic-read` for cwd/link context; `Git` state via host-provided service or helper output, not ambient shell                                                                   | `panel.provider` + `panel.create` + `process.spawn:git(...)` (allowlisted `git` arg shape) + `terminal.semantic-read` + optional `fs.read:PATH_GLOB` for working-tree read                                                                                                                          | produce `xuepoo.git:branch-changed`; consume `terminal.cwd-changed`, `terminal.title-changed`; produce `xuepoo.git:diff-ready` -> consume `xuepoo.peek:open`                                                                             | **P1 — next**           | **candidate, not bundled before release** — exercises the CLI adapter path + service selection without embedding `libgit2`; stays out of `v1` until `process.spawn:CONSTRAINT` shape and scope separation are reviewed                                                                                        |
| `bitty-terminal.browser-panel` | `View` `Browser(BrowserSurfaceId)` host surface + optional `Panel(PanelId)` controls (address input, tab strip) — host-owned `BrowserSurfaceId` per `05e8803` placement Option A | Navigation, tab-strip, address input, bounded history presentation; delegates web process to embedder, never parses terminal bytes                    | `BrowserSurfaceId` lifecycle parallel to `PanelId` with generation; `LogicalRect` placement per `View`; host-mediated `browser.navigate` with allowlist (`https` default, `file` needs `browser.file-url` gate per R-005 `FileUrlActivation`); focus reuse (`focused View` owns keyboard/IME/wheel)                                            | `browser.embed` high-risk + `browser.navigation` + `browser.file-url` for `file://` + `browser.storage` for cookie/cache persistence (each a distinct gate); `panel.provider`/`panel.create` for controls                                                                                           | produce `bitty.browser:navigated`, `xuepoo.browser:title-changed`; consume `xuepoo.files:file.open` for `file://` (validated against `PROJECT_GLOB`)                                                                                     | **P2**                  | **candidate only, not bundled before release** — requires `browser.embed` isolation, embedder process under RC-3 `512 MiB` aggregate, and R-005 review; CTX-0120 BA-1..BA-3 bound (`4` panels/window, `1` WebView/panel, `32` navigation queue); stays out of `v1` enabled and distribution until its own RFC |
| `bitty-terminal.ai-panel`      | `Panel(PanelId)` agent surface plus optional Browser view snapshot for context capture (CTX-0120 Option A)                                                                       | Chat, tool invocation, memory presentation, consent surface; `inspect`/`self`/`workspace`/`all` four levels with ephemeral scope                      | `AgentId` + `AgentWorkspace` ephemeral `64` files / `2 MiB` aggregate / `256 KiB` per file, `ContextProvider` set (`workspace`/`project`/`git`/`diagnostics`/`terminal`) with Stable Id hierarchy and `32 KiB` Context Budget per turn, `AgentMemory` conversational `32` turns / `64 KiB` aggregate; Tool Bus via MCP adapter bounded framing | `agent.context.terminal` per `Terminal` with generation, `agent.context.workspace` per `Workspace`, `agent.memory:persist` opt-in only (`0600`, `<= 7 days`, no exfiltration), `mcp.invoke:TOOL` per-tool capability, `ai.provider` + `ai.stream` (`ai.model`)                                      | produce `xuepoo.agent:tool-output` (flagged `is_untrusted_surface = true`), `xuepoo.agent:context-updated`; consume `terminal.cwd-changed`, `xuepoo.git:branch-changed`, bus topics carrying diagnostic context (redacted `SecretField`) | **P2**                  | **candidate only, not bundled before release** — requires four-level consent, `32 KiB` budget proof, `AgentWorkspace` bounds, and T-10 R-013 untrusted observation defenses; CTX-0120 BA-7..BA-10; stays out of `v1` until its own RFC and the `ModelProvider` + `ContextProvider` contracts stabilize        |
| `bitty-terminal.mail-panel`    | `Panel(PanelId)` via helper-process-backed or WebView path (not in-process native module)                                                                                        | Mail listing, reading, compose UX composed from either web content (WebView) or capability-checked `mcp`/`network` service (helper)                   | Panel Runtime for lifecycle/focus/input/bus; provider-dependent surface: WebView path reuses Browser placement, helper path uses `mcp.invoke` + bounded JSON tool results; strictly helper-process/out-of-process, never `dlopen`                                                                                                              | `panel.provider` + `panel.create` plus **one** of: `browser.embed` + `browser.navigation` (WebView path) **or** `network.connect:DESTINATION` + `mcp.invoke:mail.*` (service path) with `fs.read:MAIL_GLOB` for local cache only; `browser.file-url` never implied                                  | produce `xuepoo.mail:unread-changed`; consume `bitty.browser:navigated` or `xuepoo.files:file.open` as needed; cross-panel to `xuepoo.files` via `owner.name:topic` only                                                                 | **P3 — deferred**       | **candidate only, not bundled before release** — high-risk sync/network + secret (tokens) minimization; requires the same hardened scoping as Browser/Agent and a mail-specific `Fs`/`Net` destination allowlist review before any RFC                                                                        |

### Close neighbors that test composition completeness

These are not separately requested in the CTX-0107 short list but are the
standard companions that decide whether the Panel Runtime is complete without
inventing a private channel. Priority reflects reuse of the same `panel.*` +
`fs.*`/`process.*` surface rather than new budget families.

| Plugin ID                       | Candidate panel host                                              | Policy owned by the plugin                                                                | Core mechanism relied on                                                                                                                                                  | Illustrative capability sketch                                                   | Priority | Distribution posture   |
| ------------------------------- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- | -------- | ---------------------- |
| `bitty-terminal.editor-preview` | `Panel(PanelId)` or `Rich(RichBlockId)` companion to file manager | Markdown/file preview without grid mutation, selection/anchor handling                    | Rich blocks, `BlockAnchor`, semantic zones, overlay if needed; `SearchPanel`-style payload bounds while alternate-screen is active deferred per project TODO-005          | `panel.provider` + `ui.rich` + optional `fs.read:PATH_GLOB` for preview source   | **P2**   | candidate, not bundled |
| `bitty-terminal.logs`           | `Panel(PanelId)` log tail / filtering surface                     | Log tail, filtering, structured level view                                                | Bounded `EventBus` observation or helper CLI adapter (`process.spawn:journalctl(...)` / `docker logs` allowlisted shapes) with `8 KiB` payload batch, not raw byte stream | `panel.provider` + `process.spawn:CONSTRAINT` + `terminal.semantic-read` for cwd | **P3**   | candidate, not bundled |
| `bitty-terminal.peek`           | overlay or transient `Panel(PanelId)` (peek/preview on zone)      | Hover/preview anchored to semantic zone or rich block without entering grid geometry      | `Rich` block anchor plus `layout` geometry for placement; overlay `4+1` if used                                                                                           | `panel.overlay` / `ui.overlay` + `terminal.semantic-read`                        | **P2**   | candidate, not bundled |
| `bitty-terminal.scratchpad`     | hidden per-Window `Workspace` (scratch panel per panel-vision)    | Ephemeral per-directory notes anchored to cwd/session, persisted only in plugin quota     | `terminal.semantic-read` for cwd, `bitty.store` quota (candidate `256 KiB` per plugin per RC-5), scratchpad Workspace hidden/visible without destroying attachment        | `panel.provider` + `terminal.semantic-read` (+ `fs.*` only if quota exceeded)    | **P3**   | candidate, not bundled |
| `bitty-terminal.mirror`         | `Panel(PanelId)` view of bounded semantic snapshots               | Reflect a committed-state view of one terminal into another for review, bounded snapshots | `terminal.semantic-read` snapshots (non-blocking observability), scene composition                                                                                        | `terminal.semantic-read`                                                         | **P3**   | candidate, not bundled |

`splits` and `search` (from plugin-dogfood deferred set) stay as future
candidates as well — `splits` reuses the same `LayoutNode` `H`/`V` primitive
through `PanelRuntime` placement without a new tiling node, `search` as
observation-only `terminal.semantic-read` + `ui.overlay` with bounded snapshot
correctness, not as a hot-path hook.

## Priority before release

No release ships any plugin enabled-by-default (`v1` enabled set is empty).
Ordering below is **research build order**, not a date promise. It reflects
composition coverage and trust cost.

- **Next (P1)** — validate the Panel platform is useful without privileged
  surfaces:
  1. `file-manager` — proves tiled `Panel(PanelId)` workspace, `fs.*` path-glob
     isolation with real-path/symlink/device checks, cwd observation, and
     file->editor open via `owner.name:topic` composition. Highest reuse of the
     shipped `project` discovery shape with wider directory scope, so the design
     pressure is to tighten — not widen — `fs.read` grants.
  2. `git-panel` — proves the CLI adapter composition path (`process.spawn:git(...)`
     with `[tools.git]` manifest allowlist) and `project`+`git` context assembly
     without embedding `libgit2` or adding a new tiling primitive.

  Both stay **not bundled before release** until their constrained `fs`/`process`
  review; they exercise `PR-1..PR-12` plus the existing `Fs`/`Process` families
  with no new global budget.

- **After P1, P2** — prove privileged panel types are host-mediated, not ambient: 3. `browser-panel` — the first host-owned surface (`BrowserSurfaceId`) with
  a web process isolated from the terminal truth. Requires R-005
  `FileUrlActivation` for `file://`, destination allowlist, and RC-3
  accounting; BA-1..BA-3 ceilings. Stay out of distribution until that RFC. 4. `ai-panel` — the first consent-scoped agent surface (`AgentId` plus
  `AgentWorkspace` plus `32 KiB` Context Budget). Requires four-level
  `inspect`/`self`/`workspace`/`all` consent, MCP Tool Bus scoping per
  `(AgentId, generation)`, and secret-minimizing `SecretField` redaction.
  BA-7..BA-10 ceilings. Stay out of distribution until that RFC and the
  draft AI Architecture (`ModelProvider`/`ContextProvider`) contract stabilize. 5. `editor-preview` and `peek` — exercise Rich `anchor`/`scene` and overlay
  placement without mutating grid, closing the file->preview composition loop
  started by `file-manager`.

- **Deferred (P3, post-P1/P2)** — validate quota-bounded or network-secret
  surfaces only after the P1/P2 gates have review evidence:
  `mail-panel` (network + token secrecy), `logs` (long-running helper), `mirror`
  and `scratchpad` (snapshot versioning / quota storage). None is a terminal
  correctness dependency.

If any P1/P2 candidate needs a host capability or a new `PanelType` that is not
yet a closed `CapabilityId` family or `PanelType` member, the boundary is
incomplete and the plugin must not ship until the family is promoted via a
reviewed RFC — no private channel, no first-party bypass.

## Capability and isolation sketch per candidate

| Plugin                                             | Closed capability families (candidate)                                                                                                                                | Data source                                                                                                                                                                                                                    | Budget host                                                                                                   | Isolation check                                                                                                                                                                                                                                                                   |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| file-manager                                       | `panel.provider`, `panel.create`, `fs.read:PATH_GLOB` (plus `fs.write:PATH_GLOB` only for explicit mutations)                                                         | bounded `Fs` real-path checked listing (+ optional `rg`/`fd` CLI adapter via `process.spawn:rg(...)`)                                                                                                                          | `PerSub 64` / `PerPanel 1024`/`256 KiB` / Global `8192`/`2 MiB`                                               | Path traversal, symlink, device rejection at host; `PROJECT_GLOB` narrowing for `project`-spawned file views; per `(PanelId, generation)` denies undeclared topic reads                                                                                                           |
| git-panel                                          | `panel.provider`, `panel.create`, `process.spawn:git(...)`, `terminal.semantic-read`, `fs.read:PATH_GLOB`                                                             | allowlisted `git` CLI outputs piped to panel UI, not raw PTY injection                                                                                                                                                         | same `PerSub`/`PerPanel`/`Global` + child process counted under requesting generation (RC-1/RC-2 attribution) | Tool allowlist is static in `manifest_hash`; raising optional `required=false` to `required=true` is a capability increase whose grant must be re-confirmed; `is_untrusted_surface = true` for any terminal bytes reflected in tool output                                        |
| browser-panel                                      | `panel.provider`, `panel.create`, `browser.embed` (high-risk), `browser.navigation`, `browser.file-url` (distinct), `browser.storage` (third gate)                    | embedder web process, `LogicalRect` placement, `browser.*` topic payloads (`<= 8 KiB` host admission)                                                                                                                          | embedder under RC-3 `512 MiB` aggregate, navigation pending BA-3 `32` FIFO, `DropOldest`                      | `PanelId`/`BrowserSurfaceId`/`Generation` pairwise distinct; navigation allowlist `https`-only default; `file://` validated against `PROJECT_GLOB`; overlay while browser is hidden retains handle but pauses media                                                               |
| ai-panel                                           | `panel.provider`, `panel.create`, `agent.context.terminal`, `agent.context.workspace`, `agent.memory:persist` (opt-in), `mcp.invoke:TOOL`, `ai.provider`, `ai.stream` | `ContextProvider` assembly with Stable Id `Instance->Window->Workspace->View->Terminal` and `32 KiB` budget, `AgentWorkspace` bounded files, MCP bounded frames (`256 KiB` frame, `512 KiB` in-flight, depth `32`, RC-9/RC-10) | RC-1 `10^7`/`50 ms`/`8 ms` per VM + RC-2 `32 MiB` per `AgentId` host + RC-4/RC-5 tasks/queues + BA-7..BA-10   | No `(AgentId, generation)` can read another agent's memory without a per-target grant; workspace `all` level needs its own consent; every memory record that may carry secrets is `SecretField` redacted for diagnostics; bus receipt never implies capability (`topic != grant`) |
| mail-panel                                         | `panel.provider`, `panel.create` + `browser.*` **or** `mcp.invoke:mail.*`/`network.connect:DESTINATION` + `fs.read:MAIL_GLOB` local cache only                        | web content untrusted observation **or** MCP-hosted mail service; both over bounded `8 KiB` per `mcp.invoke` + schema `4 KiB`                                                                                                  | RC-3 for helper/web process + global `8192`/`2 MiB` shared envelope                                           | No bundled `all` network grant that silently implies sibling mailboxes; destination allowlist is per-tool; any stored tokens are `SecretField` with `0600` and bounded retention, identical to `ai-panel` secret minimization                                                     |
| editor-preview / logs / peek / scratchpad / mirror | `panel.provider`, `ui.rich` / `process.spawn:CONSTRAINT` / `terminal.semantic-read` / `bitty.store` quota                                                             | snapshot, zone, or pending CLI stream surfaced via declarative `PanelPatch`                                                                                                                                                    | per-plugin `1024`/`256 KiB` with `DropOldest` counted via `bitty plugin doctor`                               | Panel disposes release every `(PanelId, generation)` resource; reclaim verified within PB-3 `<= 15%` retained-by-design tolerance declared in manifest                                                                                                                            |

All rows obey the same panel-bus rules: topics are manifest-declared `owner.name:topic`
`<= 64` bytes; producing or consuming an undeclared topic fails with
`UndisclosedTopic`; high-value topics that carry raw PTY or clipboard bytes inherit
the high-risk consent of `terminal.raw-read`/`clipboard.read`; no bus topic grants
a capability.

## Budget attribution (how the candidates fit inside accepted ceilings)

- **No new global budget family is introduced.** Every plugin candidate reuses
  the accepted OQ-014 three-level envelope (`PerSub 64`, `PerPlugin 1024`/`256 KiB`,
  `Global 8192`/`2 MiB` with `DropOldest`/`DropNewest` alternative) plus
  `PR-1..PR-12` Panel Runtime defaults and CTX-0120 BA-1..BA-12 Browser/Agent
  defaults. Aggregate Browser+MCP+Agent+Panel+Plugin bus traffic shares the same
  global `8192`/`2 MiB` envelope; a Browser burst is the same global-limit event
  as a plugin burst, not a second ceiling.
- **RC-1/RC-2 per-VM**: `file-manager` and `git-panel` when enabled are ordinary
  Lua VM plugins — `10^7` instructions / `50 ms` wall / `8 ms` instruction
  slice per turn and `32 MiB` heap via `piccolo` Fuel+wall and
  `Lua::total_memory()`, measured via `crates/bitty-plugin-host/tests/measurement.rs`
  and `crates/bitty-lua/tests/measurement_lua.rs` (CTX-0094 envelope).
  `browser-panel` surface memory beyond queues (web process heap, image cache)
  is accounted under RC-3 aggregate, not as a new per-VM free pool.
- **RC-3 aggregate**: kept `512 MiB` aggregate under which Browser embedder,
  Agent helper processes, and any MCP servers cohabit; no new per-window process
  budget is opened without an RFC.
- **Payloads**: `8 KiB` per `emit`/`mcp.invoke` (`BoundedText` strict) and batch
  `32`/`8 KiB` aggregate per wakeup, verified by `would_exceed_global_limits` +
  `evict_oldest_globally` and `invariant_global_bounds`.
- **Safe startup preserved**: even if any of `file-manager`, `git-panel`,
  `browser-panel`, `ai-panel`, or `mail-panel` were present in a future
  distribution, a fresh install with no user configuration still starts core only
  (`EffectiveConfig` empty, `bitty --safe` zero third-party VMs, 0.1.0 distribution
  integrity via `distribution.toml` + `checksums.sha256` and `PB-5` `<= 40 MiB`
  cap per the Default Distribution RFC).

## Placement reconciliation

Panel as candidate typed `View` content (`ViewContent::Panel(PanelId)`) with the
accepted `View` `Browser(BrowserSurfaceId)` already present (Option A of the
pre-study) is the research preference carried forward here, aligned with the
hierarchy `Instance -> Window -> Workspace -> LayoutTree { H | V | View }` owned
by `Workspace`. The alternatives considered are:

- Panel as `LayoutTree` leaf replacing `View` — rejected (breaks `ViewId`
  generation history).
- Panel side-car map `ViewId -> PanelId` outside `ViewContent` — rejected
  (second host map).

`LayoutProvider::propose` stays pure deterministic and bounded
(`WorkspaceSnapshot` + `ViewId` set + `LogicalRect` -> `LayoutTree`); any proposal
carrying decoration or mutating panel state is rejected. The Event Bus is
in-process first; any cross-process routing would reuse the accepted IPC transport
with peer-credential auth and per-request scope evaluation, not an ambient
channel. This matches the placement adopted for the shipped `tabs` proof
(`tabs.rs` reuses `Stack`/`Split` with deterministic leaf order, no new tiling
primitive, bounded `32` leaves).

## Explicit exclusions (not authorized)

The following remain out of scope for this matrix and are not authorized as
shipped, stable, or compatibility-guaranteed behavior by this draft:

- Daemon `bittyd` and session persistence across reboots — post-v1.0 per ADR 0008;
  process-scoped runtime only.
- Remote UI and cross-host transport — new trust boundary not evaluated here.
- Multi-window as global server — Window stays native OS object; orchestrating many
  windows adds focus/DPI/lifetime questions beyond `PR-1..PR-2`.
- WASM or helper-process strong isolation for panels — native in-process plugins
  remain rejected; WASM/helper design needs its own RFC (helper-process reuse is
  candidate only via IPC framing).
- Browser embed per-window process budget beyond isolation ceilings — reuses
  `browser.embed` gate and existing RC-3 aggregate; no new ceiling here.
- Panel distribution preset or marketplace ownership (`bitty-dev`, `LazyBitty`,
  `awesome-bitty`) — owned by the Default Distribution RFC and any future panel
  distribution RFC; this matrix notes them as composition outcomes, not as a new
  bundled-enabled set.
- New global file, network, or process ambient for Lua — violates invariant 2;
  panels obtain those only via explicit `fs.*`/`network.*`/`process.spawn:CONSTRAINT`.
- New hot-path `input.pre-encode` interception point — would put Lua on the hot
  path; panels observe via commands and `focus.changed` observation only.

Claiming any excluded behavior by citing this matrix is a documentation hygiene
violation.

## Verification — docs only

This task is docs-only and bounded. No product code is added. Verification is:

```bash
just check          # fmt-check + clippy -D warnings + test + actionlint + markdownlint (0 issues)
git diff --check    # no whitespace errors
```

On this worktree at `5c885f2` + `05e8803` + this task delta:

- `just check` — PASS expected (prettier via `bunx --bun` `prettier 3.9.6` pinned in
  `justfile`, markdownlint `0` issues, `actionlint` `1.7.12`).
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --all-targets`, `cargo check --workspace --all-targets`,
  `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets` — PASS
  expected (no code changes; existing suites `palette_statusline_project_panel`,
  `tabs_panel`, `shell_integration_panel`, `bundled_dogfood` at `5c885f2` remain green).

## Unresolved risks and follow-up

- **Built-in namespace prefix for Panel plugins**: `PluginHost::declare` safe-mode
  currently checks `starts_with("bitty.")` as the candidate built-in namespace.
  The shipped five use `bitty-terminal.*`, treated as non-builtin and thus rejected
  in safe mode (parity with third-party `xuepoo.*`). Future file-manager/git/browser
  panels would inherit the same check; promoting `bitty-terminal.*` needs a reviewed
  change.

- **Filesystem grant width**: `file-manager` must not widen into ambient `fs.read`.
  The shipped `project` shape shows the narrowing pattern: `fs.read:~/projects/**`
  (`4096` bound) with pure `is_within_projects` + host real-path/symlink/device
  rejection, no `fs.write`. Every new `fs.*` candidate needs the same pattern.

- **Queue budgets remain candidate values**: `PerSub 64`, `PerPlugin 1024`/`256 KiB`,
  `Global 8192`/`2 MiB`, batch `32`/`8 KiB`, Panel `PR-1..PR-12` and Browser/Agent
  `BA-1..BA-12` are enforced headlessly where implemented and remain candidate
  defaults elsewhere. Exact timeout milliseconds and `RC-1`/`RC-2` instruction/memory
  enforcement are OQ-014 candidates; the Lua VM (`piccolo`) seam is measured but
  not normative for Panel/Agent yet.

- **Panel placement not yet Accepted**: `ViewContent::Panel(PanelId)` Option A is
  research preference only. A future Panel RFC must decide placement and must state
  any `ViewId` vs `PanelId` migration explicitly before any `file-manager`/`git`/
  `browser`/`ai` implementation claims it.

- **Browser and Agent stay candidate**: this matrix and CTX-0120 both record
  `browser-panel` and `ai-panel` as **candidate only, not bundled before release**.
  Any future enabled-by-default addition needs the six Default Distribution
  promotion gates (lightweight budget proof PB-1/PB-2/PB-7, capability minimality,
  failure isolation, hot-path exclusion, explicit disable preservation, independent
  security + docs-curator sign-off).

## Cross-reference

- [Plugin dogfood — shipped five at 5c885f2](plugin-dogfood.md) — bounded, headless
  evidence for the bundled-disabled baseline this matrix builds on.
- [Plugin Roadmap](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/product/plugin-roadmap.md) — first-party wave and featured second wave (source for pet/activity/peek/mirror/lock/scratchpad sequencing).
- [Panel Extensibility Vision](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/product/panel-vision.md) — programmable terminal workspace positioning, four-layer hierarchy, browser-optional distribution culture.
- [Panel Runtime and Event Bus Pre-Study](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/panel-runtime-pre-study.md) — PR-1..PR-12, typed errors, placement reconciliation with `6f30c2f`/`c3a2928`.
- [Browser and Agent Panel Integration Pre-Study](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/browser-agent-pre-study.md) — BA-1..BA-12, Browser/WebView lifecycle, MCP dispatch, Agent memory and capability isolation.
- [Plugin Platform RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/plugin-platform-rfc.md) — accepted `v1` API, capability grammar, event pipeline, four interception points.
- [Isolation Resource RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/isolation-resource-rfc.md) — RC-1..RC-10 ceilings, FS-1..FS-9 failure semantics.
- [Default Distribution RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/default-distribution-rfc.md) — bundled-disabled, five disable surfaces, `generation` disposal, `PB-5` `<= 40 MiB`.
- [Terminal State RFC](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/specifications/terminal-state-rfc.md) — `Action::Print` as sole `State` write path, damage model.
- [Core and Plugin Boundaries](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/architecture/core-boundaries.md) — mechanism/policy split, declarative UI, generation lifecycle.
- [Release Ladder](release-ladder.md) — `v0.1` slice at `5c885f2` (this task does not move it).
- Implementation: `crates/bitty-runtime/src/palette.rs`, `statusline.rs`, `project.rs`, `tabs.rs`, `shell_integration.rs`, `registry.rs` at `5c885f2`/`05e8803`; tests `crates/bitty-runtime/tests/{shell_integration_panel,tabs_panel,palette_statusline_project_panel}.rs`.

## Revision history

- `2026-09-01` CTX-0107 `carryctx/ctx-0107` `5c885f2+05e8803` — draft creation: survey
  remaining first-party plugins as Panel Runtime consumers (file manager, git panel,
  browser, AI, mail plus preview/logs/peek/scratchpad/mirror) with matrix and
  P1/P2/P3 priority; reconcile with shipped shell-integration/tabs/palette/statusline/project
  at `5c885f2` and generic Panel Runtime at `05e8803` (PR-1..PR-12); cross-repo
  textual alignment with docs CTX-0120 BA-1..BA-12; gates `just check` + `git diff --check` PASS; docs only, bounded, English.
