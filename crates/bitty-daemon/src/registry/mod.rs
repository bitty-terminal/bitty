//! Instance and session identity, the in-memory session registry, lifecycle
//! counters, retention, and explicit refusals (bitty#2601, contract `D-02`,
//! `D-03`, `D-04`, `D-14`).
//!
//! This module owns metadata only. It starts no listener, spawns no worker,
//! reads no spawn specification, and holds no terminal content. The future
//! service layer drives it:
//!
//! | Operation | Contract use |
//! | --- | --- |
//! | [`SessionRegistry::create`] | `session.create`, after scopes/consent/spawn validation |
//! | [`SessionRegistry::bind`] | worker bind, exactly once in v0.2.0 |
//! | [`SessionRegistry::attach`] / [`SessionRegistry::detach`] | lease layer (bitty#2609) updates the derived attach count |
//! | [`SessionRegistry::heartbeat`] | `D-12` last-seen refresh, never a revision bump |
//! | [`SessionRegistry::note_visible_mutation`] | one revision bump for a mutation the lease layer owns |
//! | [`SessionRegistry::begin_stopping`] | accepted abort/drain before termination |
//! | [`SessionRegistry::terminate`] | terminal transition with reason, exit, and cleanup |
//! | [`SessionRegistry::collect_retained`] | TTL collection after the terminal snapshot |
//!
//! Ids are minted server-side from the OS CSPRNG; no client-supplied value is
//! an identity. `daemon_instance_id`, `session_id`, and `attach_id` are 128-bit
//! lowercase hex. A terminal record is read-only: it never bumps a counter again
//! and is only removed by retention.
//!
//! The effective bounds are derived from the compiled advertised table with
//! [`RegistryLimits::from_limits`]. Deferred to their owning slices: lease
//! authority and attach caps (bitty#2609), IPC decoding, error mapping,
//! `daemon.status` serialization, and the audit sink writer (bitty#2607), and
//! process containment or orphan reaping (bitty#2604).

mod id;
mod session;
mod table;

pub use self::id::{AttachId, DaemonInstanceId, ID_HEX_CHARS, IdError, SessionId};
pub use self::session::{
    CleanupClass, ExitInformation, LifecycleState, MAX_OWNER_PEER_BYTES, MonotonicMs, OwnerPeer,
    OwnerPeerError, SessionKind, SessionSnapshot, TERMINAL_ID_MAX_BYTES, TerminalId,
    TerminalIdError, TerminalOutcome, TerminalOutcomeError, TerminalReason,
};
pub use self::table::{
    CreateSession, MAX_RETAINED_TERMINAL_RECORDS_SYSTEM_MAX, MAX_SESSIONS_DEFAULT,
    MAX_SESSIONS_SYSTEM_MAX, RegistryCounts, RegistryLimits, RegistryRefusal,
    SESSION_RECORD_BYTES_SYSTEM_MAX, SessionRegistry, TERMINAL_RETENTION_MS_DEFAULT,
    TERMINAL_RETENTION_MS_SYSTEM_MAX,
};

/// Entropy plumbing shared with the event stream (bitty#2602): the stream mints
/// `attach_id` values from the same source the registry mints sessions from, so
/// tests can drive both sequences deterministically.
pub(crate) use self::id::{Entropy, ID_GENERATION_ATTEMPTS, OsEntropy};
