//! `bitty-daemon`: the `bittyd` session-host crate seam.
//!
//! Status: **service loop plus the in-memory state core, the session event
//! stream, the compiled limits with their idempotency and audit stores, the
//! endpoint, instance-lock, and peer-trust primitives, and the attach/control
//! state machine**. This crate fixes the service version, the capability set,
//! and the version negotiation required by the accepted [`bittyd` Session-Host
//! Contract]. It implements the registry half of bitty#2601 (server-generated
//! ids, session records, counters, retention), the bounded orphan journal, the
//! event stream of bitty#2602 (snapshot plus bounded backlog, per-session
//! revision ordering, per-lease routing, control-frame reservation, the
//! terminal-event guarantee, and heartbeat/last-seen), the compiled limits and
//! advertised triples of bitty#2603, the two bounded idempotency stores, the
//! bounded content-free audit ring and its owner-private sink, the endpoint,
//! instance-lock, and peer-trust primitives of bitty#2604 ([`endpoint`],
//! [`peer`], [`platform`]), the lease, control-token, control transfer, and
//! abort/kill half of bitty#2609, and the bitty#2607 service surface:
//!
//! - [`service::scopes`]: the `session.*` / `daemon.*` registry and the frozen
//!   method catalog, deny-by-default;
//! - [`service::wire`] and [`service::protocol`]: the bounded v1 envelopes and
//!   the per-connection state;
//! - [`service::grants`]: host-consent and observer grants;
//! - [`service::frame`]: per-frame `SCM_CREDENTIALS` on Linux;
//! - [`service::dispatch`]: the eleven-stage authorization order and the
//!   closed refusal mapping;
//! - [`service::server`]: the `bittyd start` admission loop, limits, signals,
//!   and shutdown;
//! - [`operator`] and [`audit_sink`]: the per-boot operator capability and the
//!   fail-closed audit writer.
//!
//! `bittyd start` binds the per-user endpoint, mints the operator capability,
//! serves the bitty-ipc v1 envelope, and stops on `daemon.stop` or a signal.
//! It still spawns no worker, signals no process, and opens no TCP listener.
//! Plain `bittyd` prints status and creates no runtime file.
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
//! - [`lease::SessionHost`]: observer/controller leases, the observer cap,
//!   distinct attach ids, lease expiry and renewal, the 256-bit control token
//!   (digest-only, constant-time), explicit control transfer with the rotated
//!   token delivered only to the target lease, detach and revocation,
//!   generation-0 cancel intent, the graceful-to-kill stop pipeline, exit
//!   classification, and the one terminal notice (`D-03`, `D-04`, `D-05`,
//!   `D-07`, `D-09`, `D-17`, `D-18`).
//! - [`orphan::OrphanJournal`]: the two-phase bounded orphan journal and the
//!   exact-identity [`orphan::OrphanReaper`] seam (`D-06`).
//!
//! # Deferred, by owning slice
//!
//! | Contract area | Owner | Answer today |
//! | --- | --- | --- |
//! | Worker spawn and bind | bitty#2604 | `session.create` registers a `created` session; nothing binds generation 1 |
//! | Containment identity, signalling, reaping, the production orphan reaper, platform probes and journal ACLs | bitty#2604 | `daemon.orphans.verify` is `Denied/UnsupportedCapability` |
//! | Terminal content plane, including `session.resize` | bitty#2604 / terminal contract | no `terminal.*` scope is granted; `session.resize` is `Denied/ScopeViolation` |
//! | Pushing session events on a connection (no subscribe method) | bitty#2602 | `session.status`/`session.list` are the reconciliation reads |
//! | Per-connection refusal-audit budget aggregation | bitty#2603 follow-up | every refusal is recorded; aggregation is not implemented |
//! | Conformance corpus and fake-server parity | bitty#2605 | `tests/harness` corpus, fake and real drivers, parity gate; V-07/bitty#2768 and C-04/bitty#2767 are recorded known defects |
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

#![forbid(unsafe_code)]

pub mod audit;
pub mod audit_sink;
pub mod cli;
pub mod codes;
pub mod contract;
pub mod endpoint;
pub mod events;
pub mod handshake;
pub mod idempotency;
pub mod lease;
pub mod limits;
pub mod operator;
pub mod orphan;
pub mod peer;
pub mod platform;
pub mod registry;
pub mod seam;
pub mod service;

/// Crate version reported by `bittyd --version`, from Cargo metadata.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
