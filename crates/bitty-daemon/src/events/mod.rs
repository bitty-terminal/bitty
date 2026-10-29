//! Per-session event stream: attach snapshot plus bounded backlog, revision
//! ordering, per-lease routing, control-frame reservation, the terminal-event
//! guarantee, and heartbeat/last-seen (bitty#2792; contract `D-10`, `D-12`,
//! `D-14`, `D-18`).
//!
//! The stream is in memory only. It starts no listener, serializes nothing, and
//! holds no terminal content: the wire envelope, the server-generated
//! per-connection correlation id, and the JSON framing live in
//! `service::wire` and `service::server`, and lease authority (control tokens,
//! roles, observer grants) is bitty#2609's.
//!
//! `bus` is the delivery half the served loop uses: the same bounds and
//! policy applied to the daemon's own session truth, drained onto the
//! negotiated connection by the poll loop (bitty#2792).
//!
//! # Compiled caps
//!
//! | Bound | Default | Compiled maximum | Contract |
//! | --- | --- | --- | --- |
//! | One session's retained event ring, events | 256 | 256 | `D-14` `max_event_backlog_events` |
//! | One session's retained event ring, bytes | 262,144 | 262,144 | `D-14` `max_event_backlog_bytes` |
//! | One lease's outbound queue, events | 256 | 256 | `D-14` `max_lease_queue_events` |
//! | One lease's outbound queue, bytes | 65,536 | 65,536 | `D-14` `max_lease_queue_bytes` |
//! | Leases per session | 8 | 16 | `D-14` `max_attaches_per_session` |
//! | Lease heartbeat TTL | 15,000 ms | 15,000 ms | `D-14` `attach_lease_ttl_ms` |
//! | Attach keys per session | 512 | 512 | `D-18` `max_idempotency_keys_per_session` |
//! | Attach-key store bytes per session | 65,536 | 65,536 | `D-18` `max_idempotency_bytes_per_session` |
//! | Control reservation per lease queue | 8 frames / 8,192 bytes | same | `D-10` control frames are never shed |
//!
//! # Admission policy
//!
//! - A control frame (`session_state`, `session_terminal`, `control_transfer`,
//!   `lease_revoked`, `daemon_stopping`) is never shed. If a lease queue cannot
//!   accept it, that lease is closed with
//!   [`LeaseCloseReason::TerminalEventLost`] and the caller sees the typed
//!   close.
//! - A retired event is always evicted whole from the ring, so a replay never
//!   begins with a partial event; `backlog_truncated` plus `earliest_revision`
//!   make the gap explicit.
//! - A future shed-able observability class is admitted only into the share of
//!   a lease queue that the control reservation leaves free; v0.2.0 emits none,
//!   and the policy is compiled and tested so adding the class cannot silently
//!   steal the terminal frame's slot.
//!
//! # What bitty#2603 and bitty#2605 must add
//!
//! - bitty#2603: advertise [`limits`] as `{effective, default, system_max}`
//!   limit objects and wire `daemon.status`/`daemon.inspect` counters
//!   ([`SessionEventStream::backlog_len`], [`SessionEventStream::backlog_bytes`],
//!   [`SessionEventStream::lease_queue_len`], [`SessionEventStream::lease_queue_bytes`])
//!   into the status payload. It also owns the configuration surface that
//!   produces [`EventLimits`].
//! - bitty#2605: corpus vectors V-21 (snapshot, bounded backlog, truncation
//!   marker), V-23 (bounded queue under pressure), C-04 (revision increments and
//!   no increment on heartbeat), and C-06 (a control frame is never shed;
//!   `TerminalEventLost` on failure) against a real transport. The stream exposes
//!   no wire types, so the conformance harness must add the framing.

pub mod event;
pub mod limits;

mod backlog;
mod bus;
mod lease;
mod stream;

pub(crate) use self::bus::{EventBus, ReplaySlice};
pub use self::event::{
    ATTACH_KEY_MAX_BYTES, AttachKey, AttachKeyError, Event, EventDetail, EventKind, TerminalMarker,
};
pub use self::lease::{LeaseClose, LeaseCloseReason};
pub use self::limits::{
    ATTACH_LEASE_TTL_MS, EventLimits, LEASE_CONTROL_RESERVATION_BYTES,
    LEASE_CONTROL_RESERVATION_EVENTS, MAX_ATTACH_KEYS_BYTES_PER_SESSION,
    MAX_ATTACH_KEYS_PER_SESSION, MAX_ATTACHES_PER_SESSION_DEFAULT,
    MAX_ATTACHES_PER_SESSION_SYSTEM_MAX, MAX_EVENT_BACKLOG_BYTES, MAX_EVENT_BACKLOG_EVENTS,
    MAX_LEASE_QUEUE_BYTES, MAX_LEASE_QUEUE_EVENTS,
};
pub use self::stream::{
    AttachOutcome, AttachRequest, DaemonStoppingReport, DetachCause, DetachOutcome, EventRefusal,
    ExpiryOutcome, HeartbeatReport, LeaseHeartbeat, MutationReport, Replay, RetentionReport,
    RetiredSession, SessionEventStream, TerminalReport,
};
