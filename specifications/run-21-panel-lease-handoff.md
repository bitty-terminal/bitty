---
title: RUN-21 Panel lease description handoff analysis
description: Panel lease and handoff candidate evidence for OQ-083 (CTX-0678, bitty#1052)
category: specifications
audience: contributor
document_type: design-record
status: candidate
---

<!-- markdownlint-disable MD025 -->

# RUN-21 Panel lease/description/handoff analysis

> Status: **candidate evidence record** (CTX-0678). This record logs
> fail-closed defaults and a tested transition kernel as evidence for the
> owner ruling. It closes nothing: the lease contract needs the OQ-083
> acceptance decision first.
>
> Task: `CTX-0678` | Issue: `bitty`#1052 | Backlog: `RUN-21`
> Source: `OQ-083` (open) | Refines: `OQ-058` | Cross-ref: `SEC-26`

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in an accepted contract.
- **Candidate** — recorded direction with no contract force.
- **Open** — follow-up work with no contract.

## Analysis

[OQ-083](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md)
asks for the panel lease, description, and handoff contract of an embodied
multi-agent workspace (panel as workstation: stable id, title/description,
`Idle`/`Occupied(agent)` state, acquire/release/handoff events), composed
with the Stable Id hierarchy and the inter-panel event bus. The candidate
direction is recorded in the AI Architecture section
"Workspace-anchored multi-agent runtime: panels as leased workstations
(candidate)": one agent may hold many panels, one panel may present several
executions, a background agent may hold no panel, and presentation stays
non-authoritative. Panel lifecycle, leases, and delivery semantics are owned
by the Agent coordination architecture; role panels and lifecycle coupling
stay under OQ-058.

This record contributes the transition kernel that any such contract needs,
without pre-deciding the bus, the identity registry, or the lease term:

- Identity: the kernel takes an opaque [`LeaseHolder`] tag the host assigns
  and reuses the accepted Stable Id hierarchy instead of inventing a
  parallel one — no `AgentId` symbol enters this crate.
- State: [`LeaseState`] is exactly `Idle` or `Occupied { holder }`, created
  idle.
- Moves: [`PanelLease`] offers `acquire`, `release`, and `handoff`; every
  move answers with a [`LeaseEvent`] (`Acquired` / `Released` / `Handoff`)
  or a [`LeaseError`] (`AlreadyOccupied` / `NotOccupied` / `NotHolder`).
- Description: [`validate_title`] (1–128 chars,
  `MAX_PANEL_TITLE_CHARS`, no control characters) and
  [`validate_description`] (up to 1024 chars,
  `MAX_PANEL_DESCRIPTION_CHARS`, newline excepted) bound the chrome-facing
  text without becoming a data channel.
- Human takeover is a release back to `Idle`, never a holder value: the
  human path acts outside the lease, matching the non-authoritative
  presentation rule.

`SEC-26` (panel write-lease contract) cross-references this work and waits
on the same OQ-083 ruling; nothing here authorizes write-lease enforcement.

## Fail-closed defaults (**Candidate**)

- L-1: A fresh lease is `Idle`; occupancy is never assumed.
- L-2: Acquiring an occupied panel fails with `AlreadyOccupied`; the current
  holder keeps the lease.
- L-3: Release and handoff require the current holder; anything else fails
  with `NotHolder` and the lease is unchanged.
- L-4: An event is produced only for a transition that happened; refusals
  emit nothing and change nothing.
- L-5: Titles and descriptions past their bounds, empty titles, and control
  characters fail validation; no unbounded text reaches chrome.

## Candidate decisions log

- C-1: Holder tags are opaque `u64` values assigned by the host; the kernel
  defines no agent ontology.
- C-2: Handoff moves occupancy directly with no idle gap, so a handoff
  cannot be intercepted mid-release by a third acquirer.
- C-3: There is no clock and no bounded term in the kernel; lease expiry,
  roaming, bus event kinds, and delivery semantics stay OQ-083 / OQ-058
  open work.

## Evidence (**Shipped** as candidate implementation evidence)

- Implementation: `crates/bitty-runtime/src/execution/lease.rs`
  (`PanelLease`, `LeaseState`, `LeaseHolder`, `LeaseEvent`, `LeaseError`,
  `validate_title`, `validate_description`, `MAX_PANEL_TITLE_CHARS`,
  `MAX_PANEL_DESCRIPTION_CHARS`), re-exported from
  `crates/bitty-runtime/src/execution.rs`.
- Unit tests in the same file: acquire/release round trip, double-acquire
  refusal, non-holder release/handoff refusals, idle release/handoff
  refusals, title and description bounds, stable audit names.
- Reproduce: `cargo test -p bitty-runtime --lib lease::`.

## Live wiring (CTX-0720)

- Host binding: `PanelRuntime` (`crates/bitty-runtime/src/registry/host.rs`)
  issues one `PanelLease` per panel at `create_panel` (fresh `Idle`),
  moves it only through `acquire_panel_lease` / `release_panel_lease` /
  `handoff_panel_lease` (handle generation validated first; refusals map
  to `PanelError::LeaseDenied` with the stable kernel audit name first),
  reads it via `panel_lease_state`, stores validated orientation text via
  `set_panel_description` / `panel_description` (`InvalidDescription` on
  refusal, nothing stored), and clears the binding at `dispose_panel`.
  Lease moves never disturb panel lifecycle state.
- Fail-closed wiring defaults: occupancy is never assumed (unissued reads
  `Idle`); stale handles are rejected before the kernel; a disposed panel
  holds no lease.
- Reproduce: `cargo test -p bitty-runtime --test run_wiring lease_`.

## Gates

- `cargo fmt --check`, `cargo clippy --workspace --all-targets` with
  `-D warnings`, and `cargo test -p bitty-runtime` stay green.
- The kernel is pure and deterministic: no wall-clock, no randomness, no
  platform handle, no bus traffic.

## Open points (owner ruling needed)

OQ-083 must still accept: the bounded lease term and expiry behavior; the
event-bus kinds, routing, and delivery semantics (shared with OQ-058); the
Stable Id hierarchy composition; roaming vocabulary; and the SEC-26
write-lease enforcement built on this evidence. Until that ruling, C-1–C-3
are design candidates and authorize no bus or enforcement work.

## Verdict

**EVIDENCE_ONLY** — the issue is marked `[BLOCKED: OQ-083]` ("do not start
until the blocking item is resolved"). This record is evidence for that
ruling, not a substitute for it.
