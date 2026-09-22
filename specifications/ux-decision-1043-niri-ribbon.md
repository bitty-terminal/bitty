---
title: UX-36 Niri-style scrolling ribbon decision
description: Deferred decision record for the Niri-style scrolling ribbon viewport model (CTX-0679, bitty#1043)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# UX-36 Niri-style scrolling ribbon decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> proposes nothing normative and closes nothing: `OQ-052` and `CW-07`
> are still owner-pending, so the ribbon model stays undecided.
>
> Task: `CTX-0679` | Issue: `bitty`#1043 | Backlog: `UX-36`
> Source: `ui-compositor-gap-analysis.md` window-form directions;
> `OQ-052` | Blocked: `CW-07`, `OQ-052`

## Decision

**Defer** (Candidate): no Niri-style scrolling ribbon is adopted. The
bounded viewport model, focus semantics versus `FocusDirection`, and
the `LayoutProvider` versus new-mode question stay Open until the
owner resolves the blocking items.

## Rationale

The issue is explicitly blocked on two owner decisions. Choosing a
scrolling-strip viewport, a focus-movement rule, or a provider shape
now would pre-empt the compositor direction the owner has not set.
Deferral keeps tiling (`LayoutNode` composition) as the only shipped
model and avoids a second, half-specified layout authority.

## Current state (no new contract)

- Tiling composition via `LayoutNode` plus layer placements
  (`SceneLayer`) is the shipped model; no ribbon axis, strip cursor,
  or bounded-viewport window exists in `bitty-ui`.
- No code in this task adds a ribbon mode or a `LayoutProvider`.

## Gates to adopt later

- Owner resolves `OQ-052` (window-form direction) and `CW-07`.
- A follow-up RFC fixes the viewport bound, the focus rule, and the
  provider-vs-mode shape with fail-closed bounds.
- Implementation plus headless layout tests lands behind that RFC.

## Open points

- Focus movement semantics against the existing `FocusDirection`.
- Whether the ribbon is a `LayoutProvider` or a separate mode.
- Performance budget for a scrolling strip (overdraw, invalidation).
