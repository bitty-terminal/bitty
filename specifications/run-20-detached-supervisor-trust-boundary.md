---
title: RUN-20 Detached-supervisor trust boundary analysis
description: Trust-boundary analysis for detached supervision (CTX-0678, bitty#1051)
category: specifications
audience: contributor
document_type: design-record
status: accepted
---

<!-- markdownlint-disable MD025 -->

# RUN-20 Detached-supervisor trust boundary analysis

> Status: **accepted analysis record** (CTX-0678). This record performs the
> mandatory analysis the issue demands and verifies it as done. It ships no
> daemon, authorizes no daemon work, and changes no normative control.
>
> Task: `CTX-0678` | Issue: `bitty`#1051 | Backlog: `RUN-20`
> Source: `ADR-0008`; `panel-runtime-rfc.md` exclusions | Depends: `RUN-06`

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in this record or in the cited accepted contract.
- **Candidate** — recorded direction with no contract force.
- **Open** — follow-up work with no contract.

## Analysis

### What "detached supervisor" means in-tree today

There is no daemon process in the repository. The only detached-supervisor
material is the **file coordination contract** (**Shipped**,
`crates/bitty-runtime/src/execution/supervisor.rs`, CTX-0516):

- [`SupervisorDaemon`] coordinates single ownership over one supervised
  directory: `claim` takes ownership (adopting a stale lock whose heartbeat
  stopped), `heartbeat` renews it, `release` gives it back. At most one
  supervisor — GUI-embedded or detached — owns a directory at a time.
- [`HandoffOffer`] is the GUI's exit note (`write_handoff`) naming the jobs
  left behind plus its event cursor; the returning owner reads it,
  reconciles via `reconcile`, then clears it. Handoff is a note, never a
  transfer of authority: adoption re-validates everything against the
  manifest.
- [`SchedulePolicy`] admits concurrent running jobs (`admit`) with
  deterministic FIFO selection (`select_next`); ceiling `DEFAULT_MAX_RUNNING`
  (16), absolute bound `MAX_SCHEDULE_RUNNING` (256), fail-closed past it.
- [`adoption_plan`] maps reconciled rows onto adoption truth: terminal facts
  stay observable, `ResumeDecision::UnknownOutcome` requires an explicit
  respawn and never an automatic restart.
- The module's own non-goals state the seam explicitly: nothing here spawns a
  background process, binds a socket, or reaps a foreign pid. Actual daemon
  launch, liveness supervision, and IPC transport stay deferred under the
  accepted headless/daemon decision.

Recorded dependency `RUN-06` (the execution-supervisor foundation) is
satisfied in-tree by the shipped CTX-0511–CTX-0516 slices and the closed
`RUN-16`–`RUN-19` issues; this analysis builds on that contract.

### Boundary mapping against ADR-0008

[ADR 0008](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0008-headless.md)
defers the headless daemon, detach/reattach, and remote UI to post-v1.0 and
makes trust-boundary analysis a mandatory gate for any later daemon. The
accepted [Panel Runtime RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/panel-runtime-rfc.md)
mirrors the deferral in its explicit exclusions table (process-scoped runtime
only; no `bittyd`, no session persistence across reboots, no remote UI).

| Coordination fact                                                                                                    | Trust transition                             | Verdict                                                                                                                        |
| -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Lock + heartbeat files (`supervisor.lock`, 30 s stale bound `STALE_HEARTBEAT_MS`, 64-byte cap `MAX_HEARTBEAT_BYTES`) | Same-user filesystem only                    | **Accepted**: no new principal; a stale lock is adoptable exactly because the owner is gone, not because a peer is trusted     |
| Handoff note (256 KiB cap `MAX_HANDOFF_BYTES`, versioned, cleared after reconcile)                                   | Same-user filesystem only                    | **Accepted**: re-validated against the manifest on read; a forged note cannot widen authority because adoption never trusts it |
| Adoption (`adoption_plan`)                                                                                           | Observer to scheduler                        | **Accepted**: observes and schedules; never restarts a job, never signals a pid it did not spawn                               |
| Schedule admission (`SchedulePolicy`)                                                                                | Load shedding                                | **Accepted**: sheds fail-closed instead of oversubscribing the host                                                            |
| Grants across restart                                                                                                | None — grants are re-issued, never persisted | **Accepted**: a restart cannot resurrect revoked authority                                                                     |

No row above introduces the `remote client -> daemon -> PTY/host primitives`
transition ADR-0008 names: there is no socket, no peer credential to check,
no scope taxonomy to extend, and no network framing. The shipped contract
stays inside the local-user trust domain the accepted IPC surface already
governs.

### Residual risks (not new boundaries)

- A co-tenanted same-user process can read or plant coordination files; that
  is the existing same-user filesystem trust domain, not a daemon boundary,
  and planting a note buys no authority per the table above.
- Heartbeat staleness (30 s) bounds, but does not eliminate, dual-owner
  windows on filesystems with coarse timestamp granularity; mutual exclusion
  here is coordination hygiene, never a security principal boundary.
- The lock records a pid; pid reuse can misattribute a live owner. The
  consequence is a refused `claim` (`AlreadyOwned`), i.e. denial of
  coordination, never confused-deputy execution.

## Fail-closed defaults (**Accepted**)

- F-1: No daemon process exists; any claim of detached execution beyond file
  coordination is refused by this record.
- F-2: Handoff bytes past `MAX_HANDOFF_BYTES`, heartbeats past
  `MAX_HEARTBEAT_BYTES`, and schedules past `MAX_SCHEDULE_RUNNING` fail
  closed; nothing oversized is applied.
- F-3: Unknown outcomes require explicit respawn; adoption never restarts.
- F-4: Grants never persist; restarts re-issue.

## Candidate decisions log

None. This record logs no candidate mechanism: every row above restates
shipped or accepted behavior, and the checklist below is explicitly Open.

## Evidence (**Shipped**)

- Implementation: `crates/bitty-runtime/src/execution/supervisor.rs`
  (`SupervisorDaemon`, `HandoffOffer`, `SchedulePolicy`, `adoption_plan`,
  `DaemonError`, `write_handoff` / `read_handoff` / `clear_handoff`).
- Unit coverage in the same file plus integration coverage in
  `crates/bitty-runtime/tests/job_persistence_supervisor.rs`.
- Reproduce: `cargo test -p bitty-runtime supervisor` and
  `cargo test -p bitty-runtime --test job_persistence_supervisor`.

## Gates

- `cargo fmt --check`, `cargo clippy --workspace --all-targets` with
  `-D warnings`, and `cargo test -p bitty-runtime` stay green.
- No new crate, binary, socket, or IPC verb is added by this record.

## Open points (post-v1.0 daemon ADR, not this record)

A daemon proposal must still decide, with independent security review:
authentication for the daemon channel (mTLS or equivalent, never a silent
broadening of the local IPC transport); the daemon IPC wire and session
persistence format; the remote rendering protocol; multiplexing and
window-lifecycle ownership; and the T-09 / R-011 / R-012 / R-013 deltas for a
network-reachable supervisor. ADR-0008 stays the gate; this record does not
pre-approve any of it.

## Verdict

**CLOSE_OK** — the issue demands analysis before any daemon work, and this
record delivers exactly that: the boundary mapping, the fail-closed
defaults, and the residual checklist for the later ADR. No owner ruling
blocks it.
