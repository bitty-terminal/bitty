---
title: Semantic UI Slot Inventory and Status-Component Provider Contract
description: Design record defining the declarable UI slot inventory, per-slot bounds, conflict resolution, and status-fragment composition and ordering rules (OQ-053 gap, OQ-056/OQ-082 linkage)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# Semantic UI Slot Inventory and Status-Component Provider Contract

> Status: **draft design record** (candidate, not accepted). This document
> proposes the declarable UI slot inventory, per-slot bounds, conflict
> resolution, and status-fragment composition and ordering rules that the
> accepted Plugin API v1 Lua surface leaves implicit. It authorizes no
> implementation, adds no Core API, adds no plugin-specific API, and weakens
> no security control. Lifecycle: this record is bitty-side candidate text;
> canonicalization into `bitty-terminal-docs` specifications and any
> `OQ-056` / `OQ-082` register update are follow-up tasks in their owning
> repositories and are explicitly out of scope here.
>
> Task: `CTX-0429` | Issue: `bitty`#688 | Primary RFC: `OQ-056`
> Related: `OQ-053` (split context, `CTX-0398`), `OQ-082` (streaming, out of scope)

## Claim-status legend

Every statement below carries one label. Unlabeled statements are candidate
proposals from this record.

- **Shipped** — implemented and tested in the `bitty` repository (file and
  symbol cited; reproducible with the repo test suite).
- **Accepted** — decided in an accepted ADR or RFC, whether or not it is
  implemented yet.
- **Draft** — proposed in a draft RFC or draft specification, not accepted.
- **Open** — recorded in the open-question register, no contract of any kind.
- **Candidate** — proposed by this record, needs its own acceptance.

## Problem statement

The statusline split (`CTX-0398`) and the palette split (`CTX-0397`) moved two
first-party UI contributors out of the bundled catalog into independent
packages. Both now compose through generic host-owned paths, but the rules of
that composition were never written down:

1. The accepted Lua surface (`bitty.ui.mount` / `bitty.ui.update`) names a
   closed slot set and states that status components are ordinary `Row`
   subtrees in the `statusline` slot — **accepted** — but assigns no
   per-slot bounds, no conflict resolution, and no fragment ordering.
   (`LUA-OQ-7`, `LUA-OQ-11` in ADR-0009; UI-contributions section of the
   Plugin API v1 Lua Surface RFC.)
2. The shipped Rust helpers enforce bounds (8 components, 64 chars each, 128
   chars total for the statusline; 128-char overlay text; the `4+1` overlay
   bound) — **shipped** — but those bounds exist only in code and tests, not
   in any declarable contract a plugin author can read.
3. The draft Provider Ecology RFC names a `StatusProvider` ("many compose via
   host-owned layout") and the draft Status System specification names a
   `Provider:status.component` with Waybar-style `left` / `center` / `right`
   slots — **draft** — using a different slot vocabulary from the accepted
   8-slot inventory, with no mapping between the two.
4. Streaming UI components have no contract at all — **open** (`OQ-082`) —
   so nothing stops a future status fragment from assuming 30-60 Hz updates
   against render and input hot paths.

This record closes items 1-3 with candidate rules grounded in shipped bounds
and accepted vocabulary, and draws an explicit boundary around item 4.

## Accepted v1 slot inventory (accepted)

The closed slot set for `bitty.ui.mount(slot, component)` (**accepted**,
ADR-0009 `LUA-OQ-7`, Plugin API v1 Lua Surface RFC):

`terminal | top | bottom | left | right | tabline | statusline | overlay`

Component vocabulary (**accepted**): declarative node tables shaped by the
`SceneNode` contract, restricted for v1 to `Text`, `Row`, `Column`, and
`List`. Status components are ordinary subtrees mounted in the `statusline`
slot; popups are overlay-slot subtrees, not a new node kind. `Image`,
`CodeBlock`, `Table`, `Rule`, and bordered `Block` nodes are excluded from
v1. Handles are opaque, generation-owned integers (`block_id`); `update`
replaces the subtree under the same `block_id` with an incremented version
per the `RichBlock` replacement rule, returns `false` for a stale or foreign
handle, and raises `E_UI_COMPONENT_INVALID` for scene-contract violations.

Capability gating (**accepted**): rich content requires `ui.rich`; the
`overlay` slot additionally requires `ui.overlay`; terminal observation uses
`terminal.semantic-read` with the `semantic` snapshot scope only (`raw`
rejected in v1). The `overlay` slot is presentation-only and non-focusable:
it never claims focus, never mutates a view or terminal, and never becomes a
`PanelProvider` (**accepted**, `LUA-OQ-11`). There are no global coordinates,
shaders, pipelines, glyph injection, native windows, or renderer handles.

Implementation status (honest): `bitty.ui.mount` / `bitty.ui.update` are
**accepted but unimplemented** — the host Lua bridge (`crates/bitty-lua/src/host.rs`,
Gap A) implements commands, events, settings, store, terminal snapshots,
notifications, and timers, but no mount path. Implementation is tracked by
`CTX-0428` (ready, not started). Nothing in this record may be read as
evidence that the mount path exists.

## Per-slot contract (candidate rules grounded in shipped bounds)

The table states, per slot, whether v1 plugins may declare content there,
which capability gates it, whether contributions claim (exclusive) or compose
(many), the envelope that bounds them, and how conflicts resolve. Rows marked
**shipped** restate enforced behavior with its source; the rest is
**candidate** and needs acceptance before `CTX-0428` (or any later task) may
rely on it.

| Slot                                | Declarable in v1  | Gate                     | Claim or compose                                                                                                                                      | Envelope                                                                                                                                                                                                                                                                                                                                                                                         | Conflict resolution                                                                                                                                                                                         |
| ----------------------------------- | ----------------- | ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `statusline`                        | Yes               | `ui.rich`                | Compose (many)                                                                                                                                        | **Shipped** envelope, proposed as the generic envelope (**candidate**): at most 8 fragments, each at most 64 chars, 128 chars total; fragments joined with `&#124;`; source: `STATUSLINE_MAX_COMPONENTS`, `STATUSLINE_COMPONENT_MAX_CHARS`, `STATUSLINE_MAX_CHARS` in `crates/bitty-runtime/src/statusline.rs`                                                                                   | Overflow truncates at fragment boundary from the tail; one invalid fragment fails that mount/update only (`E_UI_COMPONENT_INVALID`), never the whole bar (extends the **accepted** per-handle failure rule) |
| `overlay`                           | Yes               | `ui.rich` + `ui.overlay` | Compose within bound (many, bounded)                                                                                                                  | **Shipped**: text truncated at char boundary to 128 (`MAX_OVERLAY_TEXT_LEN`, `crates/bitty-ui/src/panel.rs`); at most 4 non-modal + 1 modal per window (`MAX_OVERLAYS_PER_WINDOW`, `OverlayManager` `4+1` bound); host-computed centered-and-clipped bounds (`PaletteIntegration::palette_overlay_bounds` returns `None` when the overlay does not fit); focus MRU stays with the panel contract | **Shipped**: full or modally-busy manager fails closed with typed errors (`OverlayBusy`, `TooManyOverlays`) — no silent eviction, no last-wins replacement                                                  |
| `tabline`                           | Claim only        | `ui.rich`                | Claim (exclusive, single claimant)                                                                                                                    | **Shipped** claim semantics (`crates/bitty-plugin-host/src/bundled.rs`): the canonical workspace claim is `workspaceline`; `tabline` survives only as a deprecated alias (removal at or after v0.2.0); duplicate claims are diagnosed, not last-wins                                                                                                                                             | A second claimant is rejected fail-closed with a diagnostic; the incumbent keeps the slot (extends the **shipped** register-versus-claim rule)                                                              |
| `terminal`                          | Yes, content only | `ui.rich`                | Compose (host places)                                                                                                                                 | Scene bounds only (`SCN-2`: max tree depth 32 per block per the accepted Rich Presentation RFC); no plugin-set geometry                                                                                                                                                                                                                                                                          | Host layout owns placement and decoration (**accepted**); geometry conflicts are impossible by construction — plugins declare content, never rectangles                                                     |
| `top` / `bottom` / `left` / `right` | Yes, content only | `ui.rich`                | Compose (host places)                                                                                                                                 | Same scene bounds as `terminal`; chrome surfaces additionally honor host decoration policy                                                                                                                                                                                                                                                                                                       | Same as `terminal`: host-owned placement, no coordinate negotiation                                                                                                                                         |
| `palette` (effective)               | Via `overlay`     | `ui.overlay`             | One effective picker per invocation (**candidate**, mirrors the draft provider-ecology row "claimed slot with one effective provider per invocation") | Palette envelope (**shipped**, `crates/bitty-runtime/src/palette.rs`): per-panel command bound 32, query and entry text within the 128-char overlay bound, duplicates rejected                                                                                                                                                                                                                   | A second concurrent picker invocation is rejected or queued behind the active one (**candidate**); overlay exhaustion still fails closed per the `4+1` rule                                                 |

Notes on the table:

- The `statusline` 8 / 64 / 128 envelope is **shipped** for the first-party
  statusline and **candidate** as the generic per-fragment envelope. Adopting
  it generically keeps every Lua contributor inside the presentation budget
  the host already enforces, instead of inventing a second set of numbers.
- `SCN-2` depth 32 is the **accepted** scene bound. Any tighter Lua-v1 depth
  bound is **candidate** and must be justified by `CTX-0428`, not assumed
  from schema-depth numbers elsewhere (for example the depth-16 JSON-Schema
  bound, which constrains interface schemas, not scene trees).
- Palette and picker rows describe composition through the `overlay` slot,
  not a distinct slot kind: there is no `palette` member in the accepted
  inventory, and this record proposes none.

## Status-fragment composition and ordering rules (candidate)

These rules define how independently mounted statusline fragments become one
bar. All are **candidate** except where a shipped or accepted basis is cited.

1. **One mount per fragment.** A status contributor mounts one `Row`
   subtree per fragment and holds its `block_id`. No ambient placement, no
   direct bar mutation, no cross-fragment handles (**candidate**, consistent
   with the **accepted** handle model and the **draft** "no ambient
   placement" principle).
2. **Ordering key is mount sequence.** Within the `statusline` slot, render
   order is ascending mount sequence: first-mounted fragment is leftmost.
   `update` preserves position; remount appends at the tail. There is no
   numeric priority field in v1 — priority schemes are deferred to the
   provider-contract acceptance (**candidate**). Rationale: mount order is
   the only ordering the accepted surface already implies (stable `block_id`
   identity plus host-owned layout), so it adds no new mechanism.
3. **Separators and decoration are host-owned.** Fragments declare text
   content only; the host inserts the `&#124;` separator and owns padding,
   truncation markers, and bar chrome (**candidate**, consistent with
   **accepted** "host layout owns placement and decoration" and the
   **shipped** join rule).
4. **Overflow truncates from the tail at fragment boundaries.** When composed
   fragments exceed 128 chars, whole trailing fragments are dropped (or
   ellipsized where the host supports it) until the bar fits; a single
   fragment is first truncated to its 64-char envelope (**candidate**,
   extends the **shipped** char-boundary truncation rule from one producer
   to many).
5. **Failure isolation per fragment.** An invalid fragment fails its own
   mount/update with `E_UI_COMPONENT_INVALID`; a stale or foreign handle
   update returns `false`; the bar recomposes from the remaining valid
   fragments (**candidate** composition rule over the **accepted**
   per-handle failure rule). One misbehaving contributor never blanks the
   bar.
6. **Reactivity allowlist.** Status fragments update on committed-state
   observations only (cwd, title, semantic zones via OSC 7/133; the
   `terminal.cwd-changed` / `terminal.title-changed` event classes), never on
   hot-path signals (bytes received, cell changes, per-frame ticks)
   (**candidate** rule generalizing the **shipped** `is_reactive_event`
   allowlist). The host may coalesce and throttle redundant updates; exact
   throttle budgets are deferred to implementation (`CTX-0428`) and its
   benchmarks, not fixed here.
7. **Streaming exclusion.** Continuous high-frequency fragment updates
   (illustratively, sustained multi-Hz spectrum-style repainting) are
   excluded from this contract. The streaming-component contract is
   **open** (`OQ-082`): no registration, damage budget, backpressure, or
   lifecycle rule is defined here, and no fragment may assume one.
8. **No panel semantics.** Mounted fragments never claim focus, never receive
   input routing, and never graduate into panels. If the Panel RFC later
   redefines focusable surfaces, the slot stays a content source and the
   panel contract owns focus (**accepted** boundary, `LUA-OQ-11`;
   restated here because fragment authors are the most likely to test it).

## StatusProvider shape (draft synthesis, not accepted)

The draft Provider Ecology RFC proposes a `StatusProvider` ("composable
fragment for the statusline or tabline composition; many compose via
host-owned layout; no ambient placement") and the draft Status System
specification proposes `Provider:status.component` values composed by a
registry with `left` / `center` / `right` slots. This record maps those
drafts onto the accepted inventory without accepting them:

- A future `StatusProvider` declaration compiles to one `statusline`-slot
  mount per fragment under the rules above; the `left` / `center` /
  `right` vocabulary, if ever adopted, describes _intra-bar regions owned by
  the host layout_, never additional mount slots (**candidate** mapping).
- Provider multiplicity is many-compose; there is no selection or
  displacement between status providers (**candidate**, mirrors the draft
  table row).
- The provider interface name, version, metadata, and filtering protocol are
  **open** (listed in the provider RFC's own post-1.0 elaboration items) and
  stay behind a capability-gated host surface when defined.
- This mapping is a synthesis aid only. The `StatusProvider` contract needs
  its own acceptance (target: the `OQ-056` decision on which capability
  dimensions get contracts and in which API version) before any Core,
  service-registry, or manifest work begins. Per the task constraint, no
  plugin-specific Core API is added here or authorized by this record.

## Palette composition note (candidate)

The palette (now the independent `bitty-terminal.palette` package) presents
through the `overlay` slot as declarative content: centered-and-clipped
bounds computed by the host, 128-char text bound, per-panel command bound
32, duplicates rejected (**shipped**). At most one picker is effective per
invocation (**candidate**); concurrent invocations do not stack pickers.
Focus, modality, and dismissal stay with the panel contract, never with the
overlay content (**accepted** boundary). A future `PickerProvider`
(one effective provider per invocation in the draft table) must accept this
note as its composition constraint when its own contract is written.

## Overlay ownership decision (accepted, CTX-0482)

Three overlay systems exist in the workspace and each owns exactly one
concern; they never share state:

| System                                                     | Owns                                                                                                        | Does not own                                 |
| ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| `LayoutNode::Overlay` (`crates/bitty-ui/src/layout.rs`)    | Geometry (base + overlay bounds) and paint tier order                                                       | Modality, key capture, per-leaf display mode |
| `OverlayManager` (`crates/bitty-ui/src/panel.rs`)          | Presentation stacking over the `4+1` envelope and the single panel-overlay modal authority (`modal_active`) | Grid truth, geometry, per-leaf mode          |
| `PresentationMode` (`crates/bitty-ui/src/presentation.rs`) | Requested per-leaf display mode (transitions gated; the solver ignores it)                                  | Painting, stacking, modality                 |

The panel-overlay modal authority is surfaced through the runtime bit
(`Runtime::overlay_modal_active`, set from `OverlayManager::modal_active` by
the panel integration) and read by the app's single modal-capture predicate
(`crates/bitty-app/src/chrome_keys.rs`; CTX-0482). A new overlay feature
extends exactly one owner instead of adding a fourth system or a parallel
modal gate.

## Shipped versus accepted versus draft versus open ledger

| Claim                                                                                                           | Status                  | Authority                                                                                            |
| --------------------------------------------------------------------------------------------------------------- | ----------------------- | ---------------------------------------------------------------------------------------------------- |
| 8-slot mount inventory; `Text`/`Row`/`Column`/`List` v1 subset; `block_id` versioning; `E_UI_COMPONENT_INVALID` | Accepted, unimplemented | ADR-0009 (`LUA-OQ-7`), Plugin API v1 Lua Surface RFC                                                 |
| Overlay non-focusable presentation-only boundary                                                                | Accepted                | ADR-0009 (`LUA-OQ-11`)                                                                               |
| `ui.rich` / `ui.overlay` / `terminal.semantic-read` (semantic scope) gating                                     | Accepted                | Plugin API v1 Lua Surface RFC; closed capability set in `crates/bitty-plugin-host/src/capability.rs` |
| Minimal service consumer/provider contract (`services.get` / `services.provide`)                                | Accepted                | ADR-0009 (`LUA-OQ-8`)                                                                                |
| Statusline 8 / 64 / 128 envelope, `&#124;` join, observation-only helpers, reactive-event allowlist             | Shipped                 | `crates/bitty-runtime/src/statusline.rs` and its tests                                               |
| Palette overlay envelope (128 chars, 32 commands, centered/clipped, focus MRU)                                  | Shipped                 | `crates/bitty-runtime/src/palette.rs` and its tests                                                  |
| Overlay `4+1` bound with fail-closed typed errors; panel workspace/window bounds; 8 KiB bus payload bound       | Shipped                 | `crates/bitty-ui/src/panel.rs`, `crates/bitty-runtime/src/registry/panel.rs` and their tests         |
| Overlay ownership: geometry / stacking + modal bit / requested mode stay disjoint, one owner each               | Accepted                | This record (`CTX-0482`); ownership decision in `crates/bitty-ui/src/panel.rs`                       |
| `workspaceline` exclusive claim; duplicate diagnosed not last-wins; `tabline` deprecated alias                  | Shipped                 | `crates/bitty-plugin-host/src/bundled.rs` and its tests                                              |
| `bitty.ui.mount` / `update` host implementation                                                                 | Missing (tracked)       | `CTX-0428` (ready); `crates/bitty-lua/src/host.rs` has no mount path                                 |
| `StatusProvider` / `PickerProvider` / `ContextProvider` ecology                                                 | Draft, post-1.0         | Plugin Reuse and Provider Ecology RFC (status: draft)                                                |
| `Provider:status.component` with `left`/`center`/`right` registry; `SystemMetricsService`                       | Draft                   | Status System specification (status: draft)                                                          |
| Which dimensions get contracts and in which API version                                                         | Open                    | `OQ-056`                                                                                             |
| Streaming-component contract (damage, backpressure, lifecycle)                                                  | Open                    | `OQ-082`                                                                                             |
| Per-slot bounds, conflict resolution, fragment ordering in this record                                          | Candidate               | This record (`CTX-0429`); needs acceptance                                                           |

## Non-goals and explicit non-acceptance

- No Core API is added or changed by this task (docs-only); in particular,
  no `StatusProvider` registration function, no provider metadata schema, no
  manifest field, and no host-bridge entry point is proposed as accepted.
- No PanelProvider, panel identity, or focusable-overlay work is proposed;
  the `LUA-OQ-11` boundary is restated, not reopened.
- No streaming contract is proposed; `OQ-082` stays open.
- No Workspace-core behavior is changed: the `workspaceline` claim,
  workspace lifecycle, and shell integration (OSC 7/133 semantic-zone
  provider) stay bundled per the `CTX-0398` scope boundary.
- Nothing here revises the bundled-catalog revisions (`CTX-0397`,
  `CTX-0398`) or the Default Distribution amendment; those are decided.

## Follow-ups (other repositories, not this task)

- Canonicalize the accepted subset of this record into `bitty-terminal-docs`
  specifications (owning repo `bitty-terminal-docs`; do not commit there
  from this task).
- Record the `OQ-056` linkage (and the `OQ-082` boundary) in the
  open-question register (owning repo `bitty-docs`; do not edit from here).
- Implement the accepted mount surface (`CTX-0428`, this repo) against the
  candidate rules; any rule that implementation cannot honor returns here
  as a defect, not as a silent deviation.

## References

- Plugin API v1 Lua Surface RFC, UI-contributions section (accepted):
  `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/specifications/plugin-api-v1-lua-surface-rfc.md`
- ADR-0009 Plugin API v1 Lua surface (accepted):
  `https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0009-plugin-api-v1-lua-surface.md`
- Plugin Reuse and Provider Ecology RFC (draft, post-1.0):
  `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/specifications/plugin-reuse-and-providers.md`
- Status System specification (draft):
  `https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/status-system.md`
- UI Extensibility Architecture (draft / candidate):
  `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/specifications/ui-extensibility-architecture.md`
- Open-question register, `OQ-053` / `OQ-056` / `OQ-082` rows:
  `https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md`
- Rich Presentation RFC, `SCN-2` depth bound (accepted):
  `https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md`
