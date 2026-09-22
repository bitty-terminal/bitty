---
title: CW-28 browser view process budget decision
description: Deferred decision record for extra per-window browser budget beyond RC-3 aggregate (CTX-0679, bitty#1006)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# CW-28 browser view process budget decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> sets no browser process budget and closes nothing: `RFC-OQ-8` is
> still owner-pending.
>
> Task: `CTX-0679` | Issue: `bitty`#1006 | Backlog: `CW-28`
> Source: `panel-runtime-rfc.md RFC-OQ-8` | Blocked: `RFC-OQ-8`

## Decision

**Defer** (Candidate): no extra per-window browser budget beyond the
RC-3 aggregate is adopted. `BrowserSurfaceId` identity and the
aggregate `ResourceBudget` tiers stay as-is; whether a browser view
needs its own process or memory cap stays Open for the owner.

## Rationale

A per-window browser budget is a resource and isolation call the RFC
owner has explicitly reserved (`RFC-OQ-8`). Guessing a process count
or byte cap now would either over-promise isolation or throttle the
embedder without evidence. Deferral keeps the aggregate budget as
the only resource contract until the RFC answers.

## Current state (no new contract)

- Shipped: `BrowserSurfaceId` distinct newtype in
  `crates/bitty-ui/src/panel.rs`; aggregate tiers in
  `crates/bitty-ui/src/budget.rs` (`essential`, `standard`, `rich`
  with node/texture/blur/draw caps and refuse-vs-degrade admission).
- Not shipped: any per-window or per-browser-view extra budget. No
  code in this task adds one.

## Gates to adopt later

- Owner resolves `RFC-OQ-8` (process model plus whether a distinct
  browser cap exists at all).
- A follow-up RFC fixes the cap dimensions, the accounting point
  (per view vs per window), and the refuse-vs-degrade rule.
- Implementation plus budget admission tests lands behind that RFC.

## Open points

- Process vs in-process budget for browser views.
- Per-view versus per-window accounting granularity.
- Interaction with the RC-3 aggregate when both apply.
