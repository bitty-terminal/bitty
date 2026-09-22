---
title: UX-05 PW-4 Bar configurability decision
description: Deferred decision record for Bar configurability edge height colors indicator animations hide (CTX-0679, bitty#1011)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# UX-05 PW-4 Bar configurability decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> adopts no Bar configuration surface and closes nothing: `CW-24` is
> still pending, so edge, height, colors, indicator, animations, hide,
> and Workspace-area recomputation stay undecided.
>
> Task: `CTX-0679` | Issue: `bitty`#1011 | Backlog: `UX-05`
> Source: `PW-4`; `status-system.md` (draft) | Depends: `CW-24`

## Decision

**Defer** (Candidate): no PW-4 Bar configurability is adopted. The
shipped `ChromeSurface::StatusBar` visibility toggle plus the
`chrome.bar.*` theme keys remain the only Bar-adjacent contract;
everything else (edge, height, indicator, animations, hide behavior,
area recomputation) stays Open.

## Rationale

Bar placement changes Workspace-area geometry, theme contrast, and
the status-provider contract at once. Freezing an edge/height/color
scheme before `CW-24` (the status/chrome dependency) lands would
bake geometry bugs and theme debt into the layout path. Deferral
keeps the small shipped toggle while the owning design settles.

## Current state (no new contract)

- Shipped: `ChromeSurface::StatusBar` show/hide in
  `crates/bitty-ui/src/window_chrome.rs`; `chrome.bar.background`,
  `chrome.bar.foreground`, `chrome.bar.active` theme keys in
  `crates/bitty-ui/src/theme.rs`.
- Not shipped: edge selection, height, indicator, animations, hide
  semantics, or Workspace-area recomputation per edge. No code in
  this task adds any of them.

## Gates to adopt later

- `CW-24` resolves the status/chrome dependency.
- A follow-up RFC fixes the edge set, the height bound, the hide
  rule, and the exact area-recomputation formula (fail-closed,
  deterministic, headless-tested).
- Implementation lands in `window_chrome` plus layout geometry
  behind that RFC.

## Open points

- Edge set (top/bottom/left/right) and per-edge area math.
- Indicator vs color vs animation ownership and reduced-motion rule.
- Interaction with the status-component provider contract.
