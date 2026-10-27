//! `bitty-daemon`: the `bittyd` session-host crate seam.
//!
//! Status: **seam plus the in-memory state core and the session event
//! stream**. This crate fixes the service version, the capability set, and the
//! version-negotiation skeleton required by the accepted [`bittyd` Session-Host
//! Contract], and implements the registry half of bitty#2601 (server-generated
//! ids, session records, counters, retention), the bounded orphan journal, the
//! event stream of bitty#2602 (snapshot plus bounded backlog, per-session
//! revision ordering, per-lease routing, control-frame reservation, the
//! terminal-event guarantee, and heartbeat/last-seen), the compiled limits and
//! advertised triples of bitty#2603, the two bounded idempotency stores, and
//! the bounded content-free audit ring. It still starts no listener, binds no
//! endpoint, creates no socket file, spawns no worker, and exposes no IPC
//! method. The IPC service task (bitty#2607) replaces the not-wired branch in
//! [`seam::SeamStatus`] with the real `bitty-ipc` v1 server loop and maps
//! registry, event, limit, idempotency, and audit refusals onto the service
//! error taxonomy.
//!
//! # Ownership and dependency direction
//!
//! `bittyd` sits above the mechanism crates: it may depend on
//! `bitty-term-state`, `bitty-pty`, `bitty-plugin-host`, `bitty-ipc`, and
//! other mechanism crates, and none of them may depend on this crate.
//! `tests/dependency_direction.rs` reads `cargo metadata` and fails on a
//! reverse edge, on a missing workspace member, or on a second binary. A
//! daemon-shaped global in a terminal or mechanism crate is a contract
//! violation, not a wiring detail (ADR 0008 / ADR 0019).
//!
//! # Frozen here
//!
//! - [`contract::SERVICE_API_VERSION`]: service API `1.0` with minimum client
//!   minor `0` (contract D-11).
//! - [`contract::CAPABILITIES`]: `terminal_sessions`, `worker_sessions`
//!   (false), `events`, `observer_grants`, `control_transfer`,
//!   `idempotent_mutations`.
//! - [`contract::NEGOTIATE_METHOD`] and [`handshake::negotiate`]: the
//!   `daemon.negotiate` version, capability, and effective-limits half of the
//!   handshake.
//! - [`seam::SeamStatus`]: the explicit no-endpoint status the binary prints.
//! - [`registry::SessionRegistry`] and [`registry::SessionSnapshot`]: instance
//!   and session ids, lifecycle generation/revision counters, derived attach
//!   count, terminal classification, and bounded retention (`D-02`, `D-03`,
//!   `D-04`, `D-14`).
//! - [`events::SessionEventStream`]: snapshot at the current revision plus a
//!   bounded backlog, one event per visible mutation in revision order,
//!   per-lease routing, the control-frame reservation, the terminal-event
//!   guarantee, and heartbeat/last-seen (`D-10`, `D-12`, `D-14`, `D-18`).
//! - [`limits::Limits`]: every `D-14` row and the inherited `RC-9`/`RC-10`
//!   transport bounds as compiled `system_max` values, with range-checked
//!   construction and the advertised `{effective, default, system_max}`
//!   triples (`D-14`).
//! - [`idempotency::IdempotencyStore`]: the bounded per-session and
//!   pre-session create-key stores, replay, fingerprint conflict, capacity
//!   retirement, and the create-key TTL floor (`D-18`).
//! - [`audit::AuditRing`]: bounded, content-free lifecycle and refusal records
//!   with stable event ids, injected time, counters, and visible overflow
//!   (`D-16`).
//! - [`orphan::OrphanJournal`]: the two-phase bounded orphan journal and the
//!   exact-identity [`orphan::OrphanReaper`] seam (`D-06`).
//!
//! # Deferred, by owning slice
//!
//! | Contract area | Owner |
//! | --- | --- |
//! | `bitty-ipc` v1 server loop, peer capture/re-check, scope evaluation, endpoint lock and tamper detection, method dispatch, error mapping, event framing and correlation id, refusal-to-result mapping, audit sink writer | bitty#2607 |
//! | Endpoint discovery, containment identity, production orphan reaper, platform probes and journal ACLs | bitty#2604 |
//! | Leases, control token, attach/detach authority, abort/kill | bitty#2609 |
//! | Conformance corpus and fake-server parity | bitty#2605 |
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

#![forbid(unsafe_code)]

pub mod audit;
pub mod cli;
pub mod codes;
pub mod contract;
pub mod events;
pub mod handshake;
pub mod idempotency;
pub mod limits;
pub mod orphan;
pub mod registry;
pub mod seam;

/// Crate version reported by `bittyd --version`, from Cargo metadata.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
