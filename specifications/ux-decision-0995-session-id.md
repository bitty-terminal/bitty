---
title: CW-17 SessionId type decision
description: Re-confirmed decision record for a distinct SessionId newtype versus ViewId keying (CTX-0721, bitty#995)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# CW-17 SessionId type decision

> Status: **re-confirmed keep-current** (CTX-0721). This record introduces
> no `SessionId` newtype: the owner ruling of 2026-09-23 adopts `OQ-058`
> `SMO-1..SMO-4` including delivery semantics (merged `bitty` #1300) with
> terms from `ADR-0013` (`bitty-docs` #367, `bb96efe`), and accepts
> `RFC-OQ-3` Option A (`bitty` #997 closed). `ViewId` keying stays.
>
> Task: `CTX-0721` | Issue: `bitty`#995 | Backlog: `CW-17`
> Source: `workspace-panel-invariants.md F-3` | Was blocked: `OQ-058`
> (now Accepted) | Ontology: `ADR-0013`

## Decision

**Keep current with re-confirmation** (Accepted premise): do not
introduce a distinct `SessionId` newtype in the UI layer; keep `ViewId`
keying for pane sessions and `RuntimeId` for registry terminal
incarnations.

## Rationale

Session identity splits presentation keying (`ViewId`), panel
identity (`PanelId`), and terminal binding (`TerminalBinding`) one
more way. Adding a fourth key without the owner settling F-3 would
churn every scene join (`bind`, `move_panel`, attachment lookup)
and every test that pins `PanelId != ViewId != TerminalId` by
construction. Keeping `ViewId` keying is the minimal, honest
holding position, now re-confirmed against accepted terms.

`ADR-0013` fixes `Session` as an attachable continuity scope
(detach/reattach without losing the underlying execution,
Session-grained per the headless ADR, bounded persistence per ADR 0008),
distinct from pane-session keying. The `ADR-0013` `Session` maps to the
headless continuity layer, not to the view key. Panel/Execution
separation holds: panels never own executions, the origin panel is
provenance only, and presentation stays non-authoritative (`SMO-2`).
Introducing a UI-layer `SessionId` now would conflate the two layers.

## Current state (no new contract)

- Shipped: pairwise distinct `PanelId`, `ViewId`, `TerminalBinding`
  with no `From` bridge; scene joins key on `ViewId`
  (`crates/bitty-ui/src/workspace_scene.rs`,
  `crates/bitty-ui/src/panel.rs`, `crates/bitty-ui/src/view.rs`).
- Shipped (CTX-0721): `OQ-058` routable envelope
  (`crates/bitty-runtime/src/registry/routable.rs`) carries
  `task_id`/`parent_run_id` as provenance only, never ownership,
  per the Panel/Execution separation.
- Not shipped: any `SessionId` type. A repository search for
  `SessionId` returns only this record plus the re-confirmation
  comments in `view.rs` and the `session_f3` tests. No code in this
  task adds the newtype.

## Gates met by CTX-0721

- Owner resolved `OQ-058` (adopt `SMO-1..SMO-4` including delivery
  semantics; `bitty` #1001 routable contract lands in the same task).
- `ADR-0013` gives stable terms; F-3 re-confirmation records the
  mapping (pane session = `ViewId`, terminal incarnation = `RuntimeId`,
  headless continuity = `ADR-0013` `Session`) instead of adding a type.
- Identity-inequality tests keep pinning `ViewId` keying
  (`session_f3_keeps_viewid_keying`,
  `session_f3_reconfirmed_vs_adr0013`).

## Open points

- What a headless continuity `SessionId` would own beyond the pane
  session, if a future RFC wants one at that layer.
- Migration path for existing `ViewId`-keyed joins and tests, if ever.
- Display and persistence of session identity, if any (Restore vs
  Persistence separation: restore re-derives, persistence is explicit
  and consented).
