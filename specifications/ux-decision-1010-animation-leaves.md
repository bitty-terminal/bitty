---
title: UX-04 PW-3 animation leaves decision
description: Deferred decision record for move resize float-toggle animation leaves (CTX-0679, bitty#1010)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# UX-04 PW-3 animation leaves decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> adds no animation leaf to the accepted RFC-0002 set and closes
> nothing: `UX-01`, `UX-02`, `UX-03` are still pending.
>
> Task: `CTX-0679` | Issue: `bitty`#1010 | Backlog: `UX-04`
> Source: `PW-3` | Depends: `UX-01`, `UX-02`, `UX-03`

## Decision

**Defer** (Candidate): no move/resize/float-toggle animation leaf is
adopted. The shipped `MotionConfig` (durations, curves, mandatory
reduced-motion forcing instant) stays a candidate implementation of
the animation framework; which leaves join the accepted RFC-0002 set
stays Open, as do duration/easing ownership and the reduced-motion
interaction.

## Rationale

Animation leaves change the accepted motion contract: each new leaf
commits duration bounds, easing ownership, interruption behavior, and
the reduced-motion fallback. Adding the three PW-3 leaves before the
`UX-01` through `UX-03` foundations settle would fork the contract
the RFC owns. Deferral keeps motion candidate-only until then.

## Current state (no new contract)

- Shipped as candidate: `crates/bitty-ui/src/motion.rs`
  (`MotionConfig`, `with_reduced_motion`, duration caps, scope
  resolution). Nothing here is accepted RFC-0002 text.
- Not adopted: move, resize, or float-toggle as accepted leaves. No
  code in this task adds a leaf.

## Gates to adopt later

- `UX-01`, `UX-02`, `UX-03` settle the motion foundations.
- A follow-up RFC nominates each leaf with duration/easing owner,
  interruption rule, and reduced-motion mapping, against RFC-0002.
- Implementation plus headless motion tests lands behind that RFC.

## Open points

- Duration/easing ownership per leaf.
- Reduced-motion interaction (instant vs shortened vs off).
- Committed-state equivalence across animation modes (no flicker).
