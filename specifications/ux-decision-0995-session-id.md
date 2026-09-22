---
title: CW-17 SessionId type decision
description: Deferred decision record for a distinct SessionId newtype versus ViewId keying (CTX-0679, bitty#995)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# CW-17 SessionId type decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> introduces no `SessionId` newtype and closes nothing: `OQ-058` is
> still owner-pending, so `ViewId` keying stays until then.
>
> Task: `CTX-0679` | Issue: `bitty`#995 | Backlog: `CW-17`
> Source: `workspace-panel-invariants.md F-3` | Blocked: `OQ-058`

## Decision

**Defer with a keep-current recommendation** (Candidate): do not
introduce a distinct `SessionId` newtype now; keep `ViewId` keying.
The final accept/refuse belongs to the owner with `OQ-058`.

## Rationale

Session identity splits presentation keying (`ViewId`), panel
identity (`PanelId`), and terminal binding (`TerminalBinding`) one
more way. Adding a fourth key without the owner settling F-3 would
churn every scene join (`bind`, `move_panel`, attachment lookup)
and every test that pins `PanelId != ViewId != TerminalId` by
construction. Keeping `ViewId` keying is the minimal, honest
holding position.

## Current state (no new contract)

- Shipped: pairwise distinct `PanelId`, `ViewId`, `TerminalBinding`
  with no `From` bridge; scene joins key on `ViewId`
  (`crates/bitty-ui/src/workspace_scene.rs`,
  `crates/bitty-ui/src/panel.rs`, `crates/bitty-ui/src/view.rs`).
- Not shipped: any `SessionId` type. A repository search for
  `SessionId` returns only this record. No code in this task adds
  the newtype.

## Gates to adopt later

- Owner resolves `OQ-058` (whether session identity exists apart
  from view/panel/terminal).
- A follow-up RFC fixes the newtype shape, the keying migration
  (`ViewId` to `SessionId` where applicable), and the bridge bans.
- Implementation plus identity-inequality tests lands behind that
  RFC; until then `ViewId` keying is the contract.

## Open points

- What a session owns that a view, panel, or terminal binding does
  not.
- Migration path for existing `ViewId`-keyed joins and tests.
- Display and persistence of session identity, if any.
