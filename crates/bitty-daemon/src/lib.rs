//! `bitty-daemon`: the `bittyd` session-host crate seam.
//!
//! Status: **seam only**. This crate fixes the service version, the capability
//! set, and the version-negotiation skeleton required by the accepted
//! [`bittyd` Session-Host Contract]; it starts no listener, binds no endpoint,
//! creates no socket file, and owns no session state. The IPC service task
//! (bitty#2607) replaces the not-wired branch in [`seam::SeamStatus`] with the
//! real `bitty-ipc` v1 server loop; the registry task (bitty#2601) adds session
//! records and counters.
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
//!   (false), `events`, `control_transfer`, `idempotent_mutations`.
//! - [`contract::NEGOTIATE_METHOD`] and [`handshake::negotiate`]: the
//!   `daemon.negotiate` version half of the handshake.
//! - [`seam::SeamStatus`]: the explicit no-endpoint status the binary prints.
//!
//! # Deferred, by owning slice
//!
//! | Contract area | Owner |
//! | --- | --- |
//! | Instance/session ids, registry, counters, retention, orphan records | bitty#2601 |
//! | `bitty-ipc` v1 server loop, peer capture/re-check, scope evaluation, endpoint lock and tamper detection, method dispatch, error mapping | bitty#2607 |
//! | Advertised limits and compiled system maxima | bitty#2603 |
//! | Endpoint discovery and platform cells | bitty#2604 |
//! | Leases, control token, attach/detach, abort/kill | bitty#2609 |
//! | Event stream and heartbeat | bitty#2602 |
//! | Conformance corpus and fake-server parity | bitty#2605 |
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

#![forbid(unsafe_code)]

pub mod cli;
pub mod contract;
pub mod handshake;
pub mod seam;

/// Crate version reported by `bittyd --version`, from Cargo metadata.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
