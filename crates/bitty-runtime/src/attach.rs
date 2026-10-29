//! Bounded Core attach/detach seam for daemon-hosted sessions (bitty#2729,
//! CTX-1655).
//!
//! This module is the Core half of the accepted [Multiplexer Decision]
//! (Option C) and consumes the accepted [`bittyd` Session-Host Contract]
//! (`D-09`, `D-10`, `D-14`). It is a **binding-lifecycle** seam: a Core-side
//! consumer binds a window and panel to a session view through it, receives
//! metadata-only lease events, and releases the binding again. It adds no IPC
//! method, no scope, and no transport.
//!
//! # Boundary
//!
//! | Layer | Owns |
//! | --- | --- |
//! | Core (this module) | Local binding lifecycle, per-window bounds, idempotent attach/detach, event routing to a binding, local fail-closed cleanup |
//! | `bittyd` | Session, lease, attach-count, control, and last-seen truth; it is the sole writer (`D-09`) |
//! | Extension (bitty#2730) | Session naming, organization, persistence policy, and the session UI |
//! | Remote | Deferred by ADR 0008/ADR 0019; a separate ADR must land before any transport |
//!
//! The daemon remains authoritative. A successful [`AttachSeam::bind`] means
//! the host accepted one `session.attach`; the seam never invents a lease,
//! never treats its cached binding as the attach count, and never retries an
//! ambiguous host outcome. A refusal is returned unchanged in
//! [`AttachError::Refused`], and every local mutation either commits whole or
//! leaves no binding behind.
//!
//! [`SessionHost`] is the transport-agnostic port the future consumer-side
//! daemon client implements. The current tree has no client adapter, so this
//! slice deliberately defines the boundary and tests it against a fake host:
//! it does not claim an end-to-end daemon attachment. The adapter also owns
//! connection-level fencing: it checks the negotiated `daemon_instance_id`
//! before delivering an event, so an event from a restarted instance never
//! reaches a binding.
//!
//! # What crosses the seam
//!
//! Only metadata. [`SessionEvent`] is a fixed-size record carrying session and
//! lease ids, a revision, and a closed kind plus a fixed-size detail. It has no
//! grid, scrollback, text, prompt, or PTY field, and the module has no API that
//! returns terminal content. `session.*` never grants content: a content read
//! still needs a live lease **and** `terminal.inspect` on the daemon side
//! (`D-07`, `D-10`).
//!
//! The control token and observer grant are borrowed for the duration of
//! [`AttachSeam::bind`], forwarded to the host, and never stored. Debug output
//! redacts both, so a log line cannot leak one. A `control_transfer` frame is
//! delivered as metadata, but this lifecycle slice does not mint, receive, or
//! store the rotated control token and does not change the binding's role; a
//! future Core control path must re-claim through the daemon client.
//!
//! # Bounds
//!
//! - One live binding per `(window, session)`. A second bind with the same
//!   panel and role is idempotent and returns the existing handle; a different
//!   panel or role is [`AttachError::AlreadyAttached`]. This is stricter than
//!   `D-09` (which allows two leases for one client) because a Core window has
//!   one view per session.
//! - A configurable maximum per window, defaulting to the accepted
//!   `max_attaches_per_session` default (8) and capped at its system maximum
//!   (16). Configuration may lower it, including to zero to disable binding.
//!   A refused bind changes nothing and never evicts a live binding.
//! - One bounded event queue per binding: at most 256 records and 64 KiB,
//!   mirroring the daemon's per-lease queue bounds. Every frame is a control
//!   frame in v0.2.0, so a full queue closes the lease with
//!   [`ReleaseCause::TerminalEventLost`] instead of silently shedding the
//!   frame (`D-10`).
//!
//! # Detach triggers
//!
//! [`AttachSeam::release`] and [`AttachSeam::release_session`] are explicit.
//! [`AttachSeam::on_window_closed`] releases every binding for one window;
//! [`AttachSeam::on_session_terminated`] releases every binding for one
//! session; [`AttachSeam::on_daemon_stopped`] releases everything and refuses
//! new binds until [`AttachSeam::on_daemon_started`]. A `session_terminal`,
//! `lease_revoked`, or `daemon_stopping` event marks the binding closing and
//! releases it in the same [`AttachSeam::drain_events`] call that delivers the
//! event, so the event is not lost and the binding does not outlive its lease.
//!
//! Release always removes the local binding even when the host refuses the
//! detach: a stale handle, an unavailable daemon, or a terminal session must
//! not leave a local lease record behind. The refusal is preserved in the
//! [`ReleaseReceipt`] for the caller to report.
//!
//! # Complexity
//!
//! `bind`, `release`, and event delivery are `O(log n)` over the bounded
//! binding maps. Window close, session termination, and daemon stop scan the
//! bounded table and release the `k` affected bindings in
//! `O(n + k log n)`. Draining is `O(q)` for the `q` queued events of one
//! binding. `n` is bounded by the number of windows times the per-window
//! maximum.
//!
//! [Multiplexer Decision]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/multiplexer-decision.md
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::str::FromStr;

use bitty_ui::panel::PanelId;

/// Default maximum live bindings per window.
///
/// Reuses the accepted `D-14` `max_attaches_per_session` default instead of
/// inventing a second session bound.
pub const DEFAULT_MAX_BINDINGS_PER_WINDOW: usize = 8;

/// Compiled ceiling for live bindings per window.
///
/// Reuses the accepted `D-14` `max_attaches_per_session` system maximum.
/// Configuration may lower the effective value but never raise it.
pub const MAX_BINDINGS_PER_WINDOW: usize = 16;

/// Default and compiled ceiling for one binding's queued events.
///
/// Mirrors the daemon's `max_lease_queue_events`.
pub const MAX_QUEUED_EVENTS_PER_BINDING: usize = 256;

/// Compiled ceiling for one binding's queued event bytes.
///
/// Mirrors the daemon's `max_lease_queue_bytes`. [`SessionEvent`] is
/// fixed-size and heap-free, so the event count bound also keeps the byte
/// bound; both are enforced explicitly.
pub const MAX_QUEUED_EVENT_BYTES_PER_BINDING: usize = 65_536;

/// Recently delivered `(revision, kind)` pairs kept per binding for duplicate
/// suppression (`D-10`).
///
/// The contract deduplicates by `(session_id, revision, kind, attach_id)`; the
/// binding already fixes session and attach id. The window is bounded so a
/// long-lived lease cannot grow state.
pub const DEDUPE_HISTORY: usize = 64;

/// Longest accepted control-token or observer-grant encoding.
///
/// The contract mints a 256-bit token; the ceiling leaves room for a hex or
/// base64 encoding without accepting an unbounded caller string.
pub const MAX_CREDENTIAL_LEN: usize = 512;

/// Number of lowercase hex characters in a daemon identifier (`D-02`).
pub const ID_HEX_CHARS: usize = 32;

/// Characters in the raw identifier width.
const ID_BYTES: usize = 16;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// An identifier decoding failure. No variant echoes the caller's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// The encoded value was not exactly [`ID_HEX_CHARS`] characters.
    InvalidLength,
    /// The encoded value was not lowercase hexadecimal.
    InvalidCharacter,
}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => formatter.write_str("identifier is not 32 hex characters"),
            Self::InvalidCharacter => formatter.write_str("identifier is not lowercase hex"),
        }
    }
}

impl std::error::Error for IdError {}

fn hex_nibble(byte: u8) -> Result<u8, IdError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdError::InvalidCharacter),
    }
}

macro_rules! hex_identifier {
    ($name:ident, $summary:literal) => {
        #[doc = $summary]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_BYTES]);

        impl $name {
            /// Build an identifier from its raw 128 bits.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; ID_BYTES]) -> Self {
                Self(bytes)
            }

            /// Parse the canonical 32-character lowercase hex encoding.
            ///
            /// # Errors
            ///
            /// Returns [`IdError::InvalidLength`] or
            /// [`IdError::InvalidCharacter`]. Parsing confers no authority: the
            /// daemon still decides whether the record exists.
            pub fn parse(value: &str) -> Result<Self, IdError> {
                if value.len() != ID_HEX_CHARS {
                    return Err(IdError::InvalidLength);
                }
                let mut bytes = [0u8; ID_BYTES];
                for (slot, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
                    let high = hex_nibble(pair[0])?;
                    let low = hex_nibble(pair[1])?;
                    *slot = (high << 4) | low;
                }
                Ok(Self(bytes))
            }

            /// The raw 128 bits.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; ID_BYTES] {
                &self.0
            }

            /// The canonical 32-character lowercase hex encoding.
            #[must_use]
            pub fn as_hex(self) -> String {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let mut out = String::with_capacity(ID_HEX_CHARS);
                for byte in self.0 {
                    out.push(char::from(HEX[usize::from(byte >> 4)]));
                    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
                }
                out
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.as_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($name), self.as_hex())
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
    };
}

hex_identifier!(
    SessionId,
    "Server-assigned session identity (`D-02`). Parsing confers no authority."
);
hex_identifier!(
    AttachId,
    "Server-assigned attach-lease identity (`D-02`, `D-09`). Parsing confers \
     no authority."
);

/// Opaque Core window identity assigned by the composition root.
///
/// The seam never interprets the value; it is only a bound key and is not a
/// daemon or OS window handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    /// Build a window key.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw caller-assigned value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "window({})", self.0)
    }
}

/// Monotonic, never-reused local binding identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingId(u64);

impl BindingId {
    /// The raw local value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "binding({})", self.0)
    }
}

/// A local binding handle.
///
/// A handle is valid only while the binding is live. A released handle is not
/// reused, so a stale handle resolves to
/// [`ReleaseOutcome::NotAttached`] instead of a newer binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingHandle {
    window: WindowId,
    binding: BindingId,
}

impl BindingHandle {
    /// The window this handle belongs to.
    #[must_use]
    pub const fn window(self) -> WindowId {
        self.window
    }

    /// The local binding identity.
    #[must_use]
    pub const fn binding(self) -> BindingId {
        self.binding
    }
}

// ---------------------------------------------------------------------------
// Lease and session metadata
// ---------------------------------------------------------------------------

/// Role of one attach lease (`D-09`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeaseRole {
    /// Reads metadata events; no input, resize, or control authority.
    Observer,
    /// The single lease that holds control, input, and resize authority.
    Controller,
}

impl LeaseRole {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }

    /// Whether the role carries control authority.
    #[must_use]
    pub const fn can_control(self) -> bool {
        matches!(self, Self::Controller)
    }
}

impl fmt::Display for LeaseRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle state (`D-04`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionState {
    /// Registered; no worker bound yet.
    Created,
    /// A worker bound and is alive.
    Running,
    /// A graceful abort or drain is in progress.
    Stopping,
    /// Terminal: the worker exited on its own.
    Exited,
    /// Terminal: spawn, signal, or supervision failure.
    Failed,
    /// Terminal: the daemon terminated the worker.
    Terminated,
}

impl SessionState {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Terminated => "terminated",
        }
    }

    /// Whether the state is terminal and immutable (`D-04`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::Terminated)
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Closed terminal-reason set (`D-04`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TerminalReason {
    /// The worker exited on its own.
    WorkerExit,
    /// The worker died from a signal the daemon did not send.
    WorkerSignal,
    /// The worker never bound.
    SpawnFailed,
    /// The daemon could not observe, reap, or verify the exit.
    SupervisionFailed,
    /// A controller aborted with the `cancel` reason.
    ConsumerCancel,
    /// A controller aborted with the `timeout` reason.
    ConsumerTimeout,
    /// An operator aborted the session.
    OperatorAbort,
    /// An explicit `daemon.stop` terminated the session.
    DaemonStop,
    /// The host or service manager stopped the daemon.
    HostShutdown,
}

impl TerminalReason {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkerExit => "worker_exit",
            Self::WorkerSignal => "worker_signal",
            Self::SpawnFailed => "spawn_failed",
            Self::SupervisionFailed => "supervision_failed",
            Self::ConsumerCancel => "consumer_cancel",
            Self::ConsumerTimeout => "consumer_timeout",
            Self::OperatorAbort => "operator_abort",
            Self::DaemonStop => "daemon_stop",
            Self::HostShutdown => "host_shutdown",
        }
    }

    /// The one terminal state this reason maps to (`D-04`).
    #[must_use]
    pub const fn terminal_state(self) -> SessionState {
        match self {
            Self::WorkerExit => SessionState::Exited,
            Self::WorkerSignal | Self::SpawnFailed | Self::SupervisionFailed => {
                SessionState::Failed
            }
            Self::ConsumerCancel
            | Self::ConsumerTimeout
            | Self::OperatorAbort
            | Self::DaemonStop
            | Self::HostShutdown => SessionState::Terminated,
        }
    }
}

impl fmt::Display for TerminalReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The closed event-kind set (`D-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    /// Lifecycle, attach, detach, claim, transfer, or revision change.
    SessionState,
    /// Terminal transition carrying state and reason.
    SessionTerminal,
    /// Control moved to another lease.
    ControlTransfer,
    /// The lease was revoked.
    LeaseRevoked,
    /// Drain started.
    DaemonStopping,
}

impl EventKind {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionState => "session_state",
            Self::SessionTerminal => "session_terminal",
            Self::ControlTransfer => "control_transfer",
            Self::LeaseRevoked => "lease_revoked",
            Self::DaemonStopping => "daemon_stopping",
        }
    }

    /// Whether this is a control frame. v0.2.0 has no shed-able class.
    #[must_use]
    pub const fn is_control(self) -> bool {
        true
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Kind-specific event payload. Every variant is fixed-size and content-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDetail {
    /// No extra payload.
    None,
    /// Terminal state and reason, present exactly on
    /// [`EventKind::SessionTerminal`].
    Terminal {
        /// The immutable terminal state.
        state: SessionState,
        /// The immutable terminal reason.
        reason: TerminalReason,
    },
    /// The lease that received control, present exactly on
    /// [`EventKind::ControlTransfer`].
    ControlTransfer {
        /// The target lease.
        to: AttachId,
    },
    /// The drain deadline, present exactly on [`EventKind::DaemonStopping`].
    DaemonStopping {
        /// Server monotonic milliseconds after which the daemon stops.
        deadline_ms: u64,
    },
}

/// One metadata-only event routed to exactly one lease (`D-10`).
///
/// The record is fixed-size and heap-free. It has no text, grid, scrollback,
/// prompt, PTY, or provider field. The consumer-side adapter must materialize a
/// canonical backlog entry for its own lease before calling
/// [`AttachSeam::deliver_event`]: `attach_id` is never `None` here, and an event
/// is never broadcast to another lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEvent {
    /// The session the event belongs to.
    pub session: SessionId,
    /// The lease this copy is routed to.
    pub attach_id: AttachId,
    /// The session state revision the event describes.
    pub revision: u64,
    /// The event kind.
    pub kind: EventKind,
    /// Kind-specific fixed-size payload.
    pub detail: EventDetail,
}

impl SessionEvent {
    /// Accounted bytes for one queued copy.
    #[must_use]
    pub const fn accounted_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    fn detail_matches_kind(&self) -> bool {
        match (self.kind, self.detail) {
            (EventKind::SessionState | EventKind::LeaseRevoked, EventDetail::None) => true,
            (EventKind::SessionTerminal, EventDetail::Terminal { state, reason }) => {
                state.is_terminal() && reason.terminal_state() == state
            }
            (EventKind::ControlTransfer, EventDetail::ControlTransfer { .. }) => true,
            (EventKind::DaemonStopping, EventDetail::DaemonStopping { .. }) => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Host port and its frozen refusal vocabulary
// ---------------------------------------------------------------------------

/// The attach/detach calls a consumer-side daemon client implements.
///
/// Implementations speak the already-frozen `session.attach` and
/// `session.detach` methods of the accepted contract. They must not add a
/// method, widen a scope, or return terminal content. The control token and
/// observer grant are borrowed; an implementation must not store, log, or
/// audit them.
pub trait SessionHost {
    /// Open one lease for `request`.
    ///
    /// # Errors
    ///
    /// Returns a [`HostRefusal`] naming the daemon's frozen refusal. A refusal
    /// must leave the daemon state unchanged.
    fn attach(&mut self, request: HostAttach<'_>) -> Result<HostAttachGrant, HostRefusal>;

    /// Close one lease by its server-assigned attach id.
    ///
    /// A duplicate detach is not an error: it returns
    /// [`HostDetach::was_attached`] as `false` (`not_attached` semantics).
    ///
    /// # Errors
    ///
    /// Returns a [`HostRefusal`] when the daemon cannot process the detach.
    /// The seam still drops the local binding.
    fn detach(
        &mut self,
        session: SessionId,
        attach_id: AttachId,
    ) -> Result<HostDetach, HostRefusal>;
}

/// The attach fields forwarded to the host.
///
/// Window and panel stay local: the daemon has no window concept.
#[derive(Clone, Copy)]
pub struct HostAttach<'a> {
    /// Target session.
    pub session: SessionId,
    /// Requested lease role.
    pub role: LeaseRole,
    /// Expected generation for a controller claim.
    pub expected_generation: Option<u64>,
    /// Controller token, borrowed and never stored.
    pub control_token: Option<&'a str>,
    /// Observer grant, borrowed and never stored.
    pub observer_grant: Option<&'a str>,
}

impl fmt::Debug for HostAttach<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostAttach")
            .field("session", &self.session)
            .field("role", &self.role)
            .field("expected_generation", &self.expected_generation)
            .field("control_token", &self.control_token.map(|_| "<redacted>"))
            .field("observer_grant", &self.observer_grant.map(|_| "<redacted>"))
            .finish()
    }
}

/// A successful host attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostAttachGrant {
    /// Server-assigned lease identity.
    pub attach_id: AttachId,
    /// Effective role.
    pub role: LeaseRole,
    /// The session generation the lease is bound to.
    pub generation: u64,
}

/// A host detach result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostDetach {
    /// Whether this call closed a live lease. `false` is the idempotent
    /// `not_attached` repeat.
    pub was_attached: bool,
}

/// The frozen daemon refusal vocabulary relevant to attach and detach.
///
/// This mirrors the contract's `Denied/*` results and `NotFound` without
/// copying daemon internals or opening a reverse crate dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRefusal {
    /// Unknown or unauthorized session, uniformly `NotFound`.
    NotFound,
    /// The session is terminal (`Denied/NotRunning`).
    NotRunning,
    /// A controller claim arrived while stopping
    /// (`Denied/SessionStopping`).
    SessionStopping,
    /// The expected generation does not match
    /// (`Denied/StaleGeneration`).
    StaleGeneration {
        /// Current generation.
        current_generation: u64,
        /// Current state revision.
        current_revision: u64,
    },
    /// The actor is an observer or not the controller
    /// (`Denied/ControllerRequired`).
    ControllerRequired,
    /// A live controller already exists (`Denied/ControlHeld`).
    ControlHeld,
    /// The token is missing, stale, or rotated
    /// (`Denied/InvalidControlToken`).
    InvalidControlToken,
    /// An observer attach lacked a valid grant
    /// (`Denied/ObserverGrantRequired`).
    ObserverGrantRequired,
    /// The daemon's per-session attach cap is reached (`LimitExceeded`).
    MaxAttaches {
        /// The advertised cap.
        limit: usize,
    },
    /// The named lease is not attached.
    NotAttached,
    /// No reachable daemon endpoint.
    Unavailable,
    /// The daemon response did not match the frozen shape.
    Protocol,
}

impl fmt::Display for HostRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("session not found"),
            Self::NotRunning => formatter.write_str("session is not running"),
            Self::SessionStopping => formatter.write_str("session is stopping"),
            Self::StaleGeneration {
                current_generation,
                current_revision,
            } => write!(
                formatter,
                "stale generation {current_generation} at revision {current_revision}"
            ),
            Self::ControllerRequired => formatter.write_str("controller authority required"),
            Self::ControlHeld => formatter.write_str("control is held by another lease"),
            Self::InvalidControlToken => formatter.write_str("invalid control token"),
            Self::ObserverGrantRequired => formatter.write_str("observer grant required"),
            Self::MaxAttaches { limit } => write!(formatter, "attach limit {limit} reached"),
            Self::NotAttached => formatter.write_str("lease is not attached"),
            Self::Unavailable => formatter.write_str("session host is unavailable"),
            Self::Protocol => formatter.write_str("session host returned an invalid response"),
        }
    }
}

impl std::error::Error for HostRefusal {}

// ---------------------------------------------------------------------------
// Configuration, requests, receipts
// ---------------------------------------------------------------------------

/// Seam bounds. Configuration may lower a bound, never raise it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachConfig {
    max_bindings_per_window: usize,
    max_queued_events_per_binding: usize,
}

impl AttachConfig {
    /// Build a configuration. [`AttachSeam::new`] validates it.
    #[must_use]
    pub const fn with_bounds(
        max_bindings_per_window: usize,
        max_queued_events_per_binding: usize,
    ) -> Self {
        Self {
            max_bindings_per_window,
            max_queued_events_per_binding,
        }
    }

    /// Effective maximum live bindings per window.
    #[must_use]
    pub const fn max_bindings_per_window(self) -> usize {
        self.max_bindings_per_window
    }

    /// Effective maximum queued events per binding.
    #[must_use]
    pub const fn max_queued_events_per_binding(self) -> usize {
        self.max_queued_events_per_binding
    }
}

impl Default for AttachConfig {
    fn default() -> Self {
        Self {
            max_bindings_per_window: DEFAULT_MAX_BINDINGS_PER_WINDOW,
            max_queued_events_per_binding: MAX_QUEUED_EVENTS_PER_BINDING,
        }
    }
}

/// A bind request from the Core consumer.
///
/// Use [`AttachRequest::controller`] or [`AttachRequest::observer`] so the
/// role, generation, and credential combination matches `D-09`. The public
/// fields exist for the consumer's own construction; [`AttachSeam::bind`]
/// re-validates every combination and refuses a malformed one before the host
/// is called.
#[derive(Clone, Copy)]
pub struct AttachRequest<'a> {
    /// Target window.
    pub window: WindowId,
    /// Panel that will present the session view.
    pub panel: PanelId,
    /// Target session.
    pub session: SessionId,
    /// Requested lease role.
    pub role: LeaseRole,
    /// Expected generation. Required for a controller.
    pub expected_generation: Option<u64>,
    /// Controller token. Required for a controller, forbidden for an observer.
    pub control_token: Option<&'a str>,
    /// Observer grant. Required for an observer, forbidden for a controller.
    pub observer_grant: Option<&'a str>,
}

impl<'a> AttachRequest<'a> {
    /// A controller claim at `expected_generation`.
    #[must_use]
    pub const fn controller(
        window: WindowId,
        panel: PanelId,
        session: SessionId,
        expected_generation: u64,
        control_token: &'a str,
    ) -> Self {
        Self {
            window,
            panel,
            session,
            role: LeaseRole::Controller,
            expected_generation: Some(expected_generation),
            control_token: Some(control_token),
            observer_grant: None,
        }
    }

    /// An observer attach with a single-use grant.
    #[must_use]
    pub const fn observer(
        window: WindowId,
        panel: PanelId,
        session: SessionId,
        observer_grant: &'a str,
    ) -> Self {
        Self {
            window,
            panel,
            session,
            role: LeaseRole::Observer,
            expected_generation: None,
            control_token: None,
            observer_grant: Some(observer_grant),
        }
    }
}

impl fmt::Debug for AttachRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachRequest")
            .field("window", &self.window)
            .field("panel", &self.panel)
            .field("session", &self.session)
            .field("role", &self.role)
            .field("expected_generation", &self.expected_generation)
            .field("control_token", &self.control_token.map(|_| "<redacted>"))
            .field("observer_grant", &self.observer_grant.map(|_| "<redacted>"))
            .finish()
    }
}

/// Read-only binding metadata. Contains no token, PTY, grid, or content field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    /// Local handle.
    pub handle: BindingHandle,
    /// Panel presenting the session view.
    pub panel: PanelId,
    /// Bound session.
    pub session: SessionId,
    /// Effective lease role.
    pub role: LeaseRole,
    /// Session generation at bind time.
    pub generation: u64,
    /// Server-assigned lease identity.
    pub attach_id: AttachId,
    /// Events currently queued for this binding.
    pub queued_events: usize,
}

/// Why a local binding was released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseCause {
    /// The consumer called release.
    Requested,
    /// The window closed or was destroyed.
    WindowClosed,
    /// The session reached a terminal state.
    SessionTerminated,
    /// The daemon stopped or restarted.
    DaemonStopped,
    /// The daemon revoked this lease.
    LeaseRevoked,
    /// The bounded queue could not accept a control frame.
    TerminalEventLost,
}

impl fmt::Display for ReleaseCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = match self {
            Self::Requested => "requested",
            Self::WindowClosed => "window_closed",
            Self::SessionTerminated => "session_terminated",
            Self::DaemonStopped => "daemon_stopped",
            Self::LeaseRevoked => "lease_revoked",
            Self::TerminalEventLost => "terminal_event_lost",
        };
        formatter.write_str(spelling)
    }
}

/// The host's answer to a detach. Recorded even when the local binding is
/// already gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostDetachOutcome {
    /// The host processed the detach.
    Acknowledged {
        /// Whether a live host lease was closed (`not_attached` when false).
        was_attached: bool,
    },
    /// The host refused or could not be reached; the local binding was still
    /// removed.
    Refused(HostRefusal),
}

impl HostDetachOutcome {
    /// Whether the host acknowledged the detach.
    #[must_use]
    pub const fn is_acknowledged(self) -> bool {
        matches!(self, Self::Acknowledged { .. })
    }
}

/// One released binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseReceipt {
    /// The binding as it was when released.
    pub binding: SessionBinding,
    /// Why it was released.
    pub cause: ReleaseCause,
    /// The host's detach answer.
    pub host: HostDetachOutcome,
    /// Queued events discarded by the release.
    pub discarded_events: usize,
}

/// The result of an explicit release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The binding was live and was released.
    Released(ReleaseReceipt),
    /// No live binding matched; nothing changed (`not_attached` semantics).
    NotAttached,
}

/// The result of routing one host event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDelivery {
    /// The event was queued for its lease.
    Queued {
        /// The owning binding.
        handle: BindingHandle,
        /// Events now queued for the binding.
        queued_events: usize,
    },
    /// No live binding owns this attach id. This includes every event for a
    /// binding that was already released.
    DroppedUnknownLease,
    /// The event names a different session than the lease's binding.
    DroppedForeignSession,
    /// The revision is older than the last delivered event, or the same
    /// revision and kind was already delivered.
    DroppedStale,
    /// The binding is already closing after a control frame; later events are
    /// not delivered.
    DroppedClosing,
    /// The event detail does not match its kind, or a terminal detail is not
    /// terminal.
    DroppedInvalid,
    /// The bounded queue could not accept a control frame, so the lease was
    /// closed (`D-10`).
    LeaseClosed {
        /// The binding that was closed.
        handle: BindingHandle,
        /// The release receipt, including the discarded event count.
        receipt: ReleaseReceipt,
    },
}

/// The result of draining one binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Events in revision order, oldest first.
    pub events: Vec<SessionEvent>,
    /// Present when the drain delivered the closing control frame and the
    /// binding was released in the same call.
    pub released: Option<ReleaseReceipt>,
}

/// Current seam occupancy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttachStats {
    /// Live bindings.
    pub bindings: usize,
    /// Windows with at least one binding.
    pub windows: usize,
    /// Events queued across all bindings.
    pub queued_events: usize,
    /// Accounted bytes queued across all bindings.
    pub queued_event_bytes: usize,
}

// ---------------------------------------------------------------------------
// Errors and outcomes
// ---------------------------------------------------------------------------

/// A seam refusal or host refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
    /// The binding is not live (`not_attached` semantics).
    NotAttached,
    /// The `(window, session)` key is live with a different panel or role, or
    /// a different generation.
    AlreadyAttached {
        /// Existing role.
        role: LeaseRole,
        /// Existing panel.
        panel: PanelId,
    },
    /// The window already holds its configured maximum.
    WindowLimitReached {
        /// Effective maximum.
        max: usize,
        /// Current live bindings for the window.
        current: usize,
    },
    /// The daemon was marked stopped; new binds are refused until
    /// [`AttachSeam::on_daemon_started`].
    DaemonStopped,
    /// A controller request carried no token.
    MissingControlToken,
    /// A controller request carried an observer grant.
    UnexpectedObserverGrant,
    /// An observer request carried no grant.
    MissingObserverGrant,
    /// An observer request carried a controller token.
    UnexpectedControlToken,
    /// A controller request carried no expected generation.
    GenerationRequired,
    /// The host granted a different generation than the one requested.
    GenerationMismatch {
        /// Requested generation.
        expected: u64,
        /// Granted generation.
        found: u64,
    },
    /// The host granted a different role than the one requested.
    RoleMismatch {
        /// Requested role.
        requested: LeaseRole,
        /// Granted role.
        granted: LeaseRole,
    },
    /// The token or grant was empty or exceeded the bounded length.
    InvalidCredential,
    /// A configured bound is outside the reviewed range.
    InvalidLimit {
        /// Bound name.
        limit: &'static str,
        /// Requested value.
        value: usize,
        /// Compiled maximum.
        maximum: usize,
    },
    /// The host refused the attach with its frozen refusal.
    Refused(HostRefusal),
    /// The host returned an attach id that already belongs to a live local
    /// binding.
    ///
    /// Detaching by that id would close the existing lease, not the duplicate,
    /// so the seam leaves both the existing binding and the host state
    /// untouched and fails closed. The caller must treat this as a
    /// connection-fatal protocol error and recycle the daemon connection
    /// rather than retrying the bind.
    DuplicateAttachId {
        /// The ambiguous attach id returned by the host.
        attach_id: AttachId,
    },
    /// The local binding index is inconsistent with itself.
    HostProtocol,
    /// The local binding counter would wrap.
    BindingIdExhausted,
}

impl fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAttached => formatter.write_str("binding is not attached"),
            Self::AlreadyAttached { role, panel } => write!(
                formatter,
                "session already bound to panel {panel} as {role}"
            ),
            Self::WindowLimitReached { max, current } => {
                write!(
                    formatter,
                    "window binding limit {max} reached (current {current})"
                )
            }
            Self::DaemonStopped => formatter.write_str("session host is stopped"),
            Self::MissingControlToken => formatter.write_str("controller token is required"),
            Self::UnexpectedObserverGrant => {
                formatter.write_str("controller request must not carry an observer grant")
            }
            Self::MissingObserverGrant => formatter.write_str("observer grant is required"),
            Self::UnexpectedControlToken => {
                formatter.write_str("observer request must not carry a control token")
            }
            Self::GenerationRequired => {
                formatter.write_str("controller request requires an expected generation")
            }
            Self::GenerationMismatch { expected, found } => {
                write!(
                    formatter,
                    "generation mismatch: expected {expected}, found {found}"
                )
            }
            Self::RoleMismatch { requested, granted } => {
                write!(
                    formatter,
                    "role mismatch: requested {requested}, granted {granted}"
                )
            }
            Self::InvalidCredential => formatter.write_str("credential is empty or over-long"),
            Self::InvalidLimit {
                limit,
                value,
                maximum,
            } => write!(
                formatter,
                "bound {limit} value {value} exceeds maximum {maximum}"
            ),
            Self::Refused(refusal) => write!(formatter, "host refused: {refusal}"),
            Self::DuplicateAttachId { attach_id } => {
                write!(
                    formatter,
                    "host returned an already-bound attach id ({attach_id})"
                )
            }
            Self::HostProtocol => formatter.write_str("local binding index is inconsistent"),
            Self::BindingIdExhausted => formatter.write_str("local binding id space exhausted"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<HostRefusal> for AttachError {
    fn from(refusal: HostRefusal) -> Self {
        Self::Refused(refusal)
    }
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Why a binding is closing after its last control frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closing {
    SessionTerminated,
    LeaseRevoked,
    DaemonStopped,
}

impl Closing {
    const fn cause(self) -> ReleaseCause {
        match self {
            Self::SessionTerminated => ReleaseCause::SessionTerminated,
            Self::LeaseRevoked => ReleaseCause::LeaseRevoked,
            Self::DaemonStopped => ReleaseCause::DaemonStopped,
        }
    }
}

struct Binding {
    handle: BindingHandle,
    panel: PanelId,
    session: SessionId,
    role: LeaseRole,
    generation: u64,
    attach_id: AttachId,
    events: VecDeque<SessionEvent>,
    queued_bytes: usize,
    last_revision: Option<u64>,
    recent: VecDeque<(u64, EventKind)>,
    closing: Option<Closing>,
}

impl Binding {
    fn snapshot(&self) -> SessionBinding {
        SessionBinding {
            handle: self.handle,
            panel: self.panel,
            session: self.session,
            role: self.role,
            generation: self.generation,
            attach_id: self.attach_id,
            queued_events: self.events.len(),
        }
    }
}

/// Bounded, headless binding table for daemon-hosted session views.
///
/// The daemon stays the sole writer of session, lease, attach-count, and
/// last-seen truth. This type caches only the bindings a Core window created.
pub struct AttachSeam<H: SessionHost> {
    host: H,
    config: AttachConfig,
    daemon_available: bool,
    next_binding: u64,
    bindings: BTreeMap<BindingId, Binding>,
    by_key: BTreeMap<(WindowId, SessionId), BindingId>,
    by_attach: BTreeMap<AttachId, BindingId>,
    window_counts: BTreeMap<WindowId, usize>,
}

impl<H: SessionHost> AttachSeam<H> {
    /// Build the seam over a host adapter.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::InvalidLimit`] when a configured bound is
    /// outside `[0, MAX_BINDINGS_PER_WINDOW]` or
    /// `[1, MAX_QUEUED_EVENTS_PER_BINDING]`.
    pub fn new(host: H, config: AttachConfig) -> Result<Self, AttachError> {
        if config.max_bindings_per_window > MAX_BINDINGS_PER_WINDOW {
            return Err(AttachError::InvalidLimit {
                limit: "max_bindings_per_window",
                value: config.max_bindings_per_window,
                maximum: MAX_BINDINGS_PER_WINDOW,
            });
        }
        if config.max_queued_events_per_binding == 0
            || config.max_queued_events_per_binding > MAX_QUEUED_EVENTS_PER_BINDING
        {
            return Err(AttachError::InvalidLimit {
                limit: "max_queued_events_per_binding",
                value: config.max_queued_events_per_binding,
                maximum: MAX_QUEUED_EVENTS_PER_BINDING,
            });
        }
        Ok(Self {
            host,
            config,
            daemon_available: true,
            next_binding: 1,
            bindings: BTreeMap::new(),
            by_key: BTreeMap::new(),
            by_attach: BTreeMap::new(),
            window_counts: BTreeMap::new(),
        })
    }

    /// The effective configuration.
    #[must_use]
    pub const fn config(&self) -> AttachConfig {
        self.config
    }

    /// Read-only access to the host adapter.
    #[must_use]
    pub const fn host(&self) -> &H {
        &self.host
    }

    /// Mutable access to the host adapter for the consumer's other calls.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Whether binds are currently accepted.
    #[must_use]
    pub const fn daemon_available(&self) -> bool {
        self.daemon_available
    }

    /// Bind a window/panel to a session view.
    ///
    /// A repeat bind for the same `(window, session)` with the same panel,
    /// role, and expected generation returns the existing handle without a
    /// second host call. Every other repeat is
    /// [`AttachError::AlreadyAttached`].
    ///
    /// # Errors
    ///
    /// Returns a typed refusal for a stopped daemon, a malformed role/credential
    /// combination, an exhausted window bound, a host refusal, or a host
    /// response that does not match the request. A refusal creates no binding.
    pub fn bind(&mut self, request: AttachRequest<'_>) -> Result<BindingHandle, AttachError> {
        if !self.daemon_available {
            return Err(AttachError::DaemonStopped);
        }
        self.validate_role(&request)?;

        let key = (request.window, request.session);
        if let Some(existing_id) = self.by_key.get(&key).copied() {
            let existing = self
                .bindings
                .get(&existing_id)
                .ok_or(AttachError::HostProtocol)?;
            let same_generation = request
                .expected_generation
                .is_none_or(|expected| expected == existing.generation);
            if existing.panel == request.panel && existing.role == request.role && same_generation {
                return Ok(existing.handle);
            }
            return Err(AttachError::AlreadyAttached {
                role: existing.role,
                panel: existing.panel,
            });
        }

        let current = self
            .window_counts
            .get(&request.window)
            .copied()
            .unwrap_or(0);
        if current >= self.config.max_bindings_per_window {
            return Err(AttachError::WindowLimitReached {
                max: self.config.max_bindings_per_window,
                current,
            });
        }

        // Mint the local id before the host call so an exhausted local id space
        // cannot leave a host lease behind.
        let binding_id = self.mint_binding_id()?;
        let host_request = HostAttach {
            session: request.session,
            role: request.role,
            expected_generation: request.expected_generation,
            control_token: request.control_token,
            observer_grant: request.observer_grant,
        };
        let grant = self
            .host
            .attach(host_request)
            .map_err(AttachError::Refused)?;

        if grant.role != request.role {
            let _ = self.host.detach(request.session, grant.attach_id);
            return Err(AttachError::RoleMismatch {
                requested: request.role,
                granted: grant.role,
            });
        }
        if let Some(expected) = request.expected_generation {
            if grant.generation != expected {
                let _ = self.host.detach(request.session, grant.attach_id);
                return Err(AttachError::GenerationMismatch {
                    expected,
                    found: grant.generation,
                });
            }
        }
        if self.by_attach.contains_key(&grant.attach_id) {
            // The id names a live binding, so detaching it would close that
            // binding's lease. Leave both states untouched and fail closed;
            // the adapter recycles the connection on this protocol error.
            return Err(AttachError::DuplicateAttachId {
                attach_id: grant.attach_id,
            });
        }

        let handle = BindingHandle {
            window: request.window,
            binding: binding_id,
        };
        let binding = Binding {
            handle,
            panel: request.panel,
            session: request.session,
            role: grant.role,
            generation: grant.generation,
            attach_id: grant.attach_id,
            events: VecDeque::new(),
            queued_bytes: 0,
            last_revision: None,
            recent: VecDeque::new(),
            closing: None,
        };
        self.bindings.insert(binding_id, binding);
        self.by_key.insert(key, binding_id);
        self.by_attach.insert(grant.attach_id, binding_id);
        *self.window_counts.entry(request.window).or_insert(0) += 1;
        Ok(handle)
    }

    /// Release one binding. A stale or already-released handle returns
    /// [`ReleaseOutcome::NotAttached`] and changes nothing.
    pub fn release(&mut self, handle: BindingHandle) -> ReleaseOutcome {
        match self.bindings.get(&handle.binding) {
            Some(binding) if binding.handle == handle => {}
            _ => return ReleaseOutcome::NotAttached,
        }
        match self.release_inner(handle.binding, ReleaseCause::Requested) {
            Some(receipt) => ReleaseOutcome::Released(receipt),
            None => ReleaseOutcome::NotAttached,
        }
    }

    /// Release the binding for one `(window, session)` key.
    pub fn release_session(&mut self, window: WindowId, session: SessionId) -> ReleaseOutcome {
        let Some(binding_id) = self.by_key.get(&(window, session)).copied() else {
            return ReleaseOutcome::NotAttached;
        };
        match self.release_inner(binding_id, ReleaseCause::Requested) {
            Some(receipt) => ReleaseOutcome::Released(receipt),
            None => ReleaseOutcome::NotAttached,
        }
    }

    /// Release every binding for a window that closed or was destroyed.
    ///
    /// Idempotent: a window with no bindings returns an empty vector.
    pub fn on_window_closed(&mut self, window: WindowId) -> Vec<ReleaseReceipt> {
        self.release_where(
            |binding| binding.handle.window == window,
            ReleaseCause::WindowClosed,
        )
    }

    /// Release every binding for a session that reached a terminal state.
    ///
    /// This is the out-of-band status path. The event path is
    /// [`SessionEvent`] with [`EventKind::SessionTerminal`] followed by
    /// [`AttachSeam::drain_events`].
    pub fn on_session_terminated(&mut self, session: SessionId) -> Vec<ReleaseReceipt> {
        self.release_where(
            |binding| binding.session == session,
            ReleaseCause::SessionTerminated,
        )
    }

    /// Release every binding and refuse new binds until
    /// [`AttachSeam::on_daemon_started`].
    ///
    /// Idempotent. The daemon's restart introduces a new instance id and an
    /// empty registry (`D-06`), so no binding is adopted.
    pub fn on_daemon_stopped(&mut self) -> Vec<ReleaseReceipt> {
        self.daemon_available = false;
        self.release_where(|_| true, ReleaseCause::DaemonStopped)
    }

    /// Re-enable binds after a daemon start.
    pub fn on_daemon_started(&mut self) {
        self.daemon_available = true;
    }

    /// Route one host event to the lease that owns it.
    ///
    /// Only the matching live binding receives the event. A foreign session,
    /// an unknown or released attach id, a duplicate/stale revision, or a
    /// closing binding is dropped without delivery. A terminal, revocation, or
    /// drain frame marks the binding closing; the event stays queued and the
    /// binding is released by the drain that delivers it.
    pub fn deliver_event(&mut self, event: SessionEvent) -> EventDelivery {
        let Some(binding_id) = self.by_attach.get(&event.attach_id).copied() else {
            return EventDelivery::DroppedUnknownLease;
        };
        let Some(binding) = self.bindings.get(&binding_id) else {
            return EventDelivery::DroppedUnknownLease;
        };
        let handle = binding.handle;
        if binding.session != event.session {
            return EventDelivery::DroppedForeignSession;
        }
        if !event.detail_matches_kind() {
            return EventDelivery::DroppedInvalid;
        }
        if binding.closing.is_some() {
            return EventDelivery::DroppedClosing;
        }
        if let Some(last_revision) = binding.last_revision {
            if event.revision < last_revision {
                return EventDelivery::DroppedStale;
            }
        }
        if binding
            .recent
            .iter()
            .any(|&(revision, kind)| revision == event.revision && kind == event.kind)
        {
            return EventDelivery::DroppedStale;
        }

        let event_bytes = event.accounted_bytes();
        let overflow = {
            let Some(binding) = self.bindings.get(&binding_id) else {
                return EventDelivery::DroppedUnknownLease;
            };
            binding.events.len() >= self.config.max_queued_events_per_binding
                || binding.queued_bytes.saturating_add(event_bytes)
                    > MAX_QUEUED_EVENT_BYTES_PER_BINDING
        };
        if overflow {
            return match self.release_inner(binding_id, ReleaseCause::TerminalEventLost) {
                Some(receipt) => EventDelivery::LeaseClosed { handle, receipt },
                None => EventDelivery::DroppedUnknownLease,
            };
        }
        let Some(binding) = self.bindings.get_mut(&binding_id) else {
            return EventDelivery::DroppedUnknownLease;
        };

        let closing = match event.kind {
            EventKind::SessionTerminal => Some(Closing::SessionTerminated),
            EventKind::LeaseRevoked => Some(Closing::LeaseRevoked),
            EventKind::DaemonStopping => Some(Closing::DaemonStopped),
            EventKind::SessionState | EventKind::ControlTransfer => None,
        };
        binding.events.push_back(event);
        binding.queued_bytes += event_bytes;
        binding.last_revision = Some(event.revision);
        binding.recent.push_back((event.revision, event.kind));
        if binding.recent.len() > DEDUPE_HISTORY {
            binding.recent.pop_front();
        }
        if closing.is_some() {
            binding.closing = closing;
        }
        EventDelivery::Queued {
            handle,
            queued_events: binding.events.len(),
        }
    }

    /// Drain one binding's queued events, oldest first.
    ///
    /// When the binding is closing, the drain also releases it and reports the
    /// receipt, so the terminal/revocation/drain frame is delivered exactly
    /// once and the lease does not outlive it.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::NotAttached`] for a stale or released handle.
    pub fn drain_events(&mut self, handle: BindingHandle) -> Result<DrainOutcome, AttachError> {
        let (events, closing) = {
            let binding = self
                .bindings
                .get_mut(&handle.binding)
                .filter(|binding| binding.handle == handle)
                .ok_or(AttachError::NotAttached)?;
            let events = binding.events.drain(..).collect::<Vec<_>>();
            binding.queued_bytes = 0;
            (events, binding.closing.take())
        };
        let released =
            closing.and_then(|closing| self.release_inner(handle.binding, closing.cause()));
        Ok(DrainOutcome { events, released })
    }

    /// Number of queued events for a live binding.
    ///
    /// # Errors
    ///
    /// Returns [`AttachError::NotAttached`] for a stale or released handle.
    pub fn pending_events(&self, handle: BindingHandle) -> Result<usize, AttachError> {
        self.bindings
            .get(&handle.binding)
            .filter(|binding| binding.handle == handle)
            .map(|binding| binding.events.len())
            .ok_or(AttachError::NotAttached)
    }

    /// Current binding metadata, or `None` for a stale handle.
    #[must_use]
    pub fn binding(&self, handle: BindingHandle) -> Option<SessionBinding> {
        self.bindings
            .get(&handle.binding)
            .filter(|binding| binding.handle == handle)
            .map(Binding::snapshot)
    }

    /// Bindings for one window, in local binding-id order.
    #[must_use]
    pub fn bindings_for_window(&self, window: WindowId) -> Vec<SessionBinding> {
        self.bindings
            .values()
            .filter(|binding| binding.handle.window == window)
            .map(Binding::snapshot)
            .collect()
    }

    /// Occupancy and queue totals.
    #[must_use]
    pub fn stats(&self) -> AttachStats {
        let mut stats = AttachStats {
            bindings: self.bindings.len(),
            windows: self.window_counts.len(),
            queued_events: 0,
            queued_event_bytes: 0,
        };
        for binding in self.bindings.values() {
            stats.queued_events += binding.events.len();
            stats.queued_event_bytes += binding.queued_bytes;
        }
        stats
    }

    fn validate_role(&self, request: &AttachRequest<'_>) -> Result<(), AttachError> {
        match request.role {
            LeaseRole::Controller => {
                if request.control_token.is_none() {
                    return Err(AttachError::MissingControlToken);
                }
                if request.observer_grant.is_some() {
                    return Err(AttachError::UnexpectedObserverGrant);
                }
                if request.expected_generation.is_none() {
                    return Err(AttachError::GenerationRequired);
                }
            }
            LeaseRole::Observer => {
                if request.observer_grant.is_none() {
                    return Err(AttachError::MissingObserverGrant);
                }
                if request.control_token.is_some() {
                    return Err(AttachError::UnexpectedControlToken);
                }
            }
        }
        for credential in [request.control_token, request.observer_grant]
            .into_iter()
            .flatten()
        {
            if credential.is_empty() || credential.len() > MAX_CREDENTIAL_LEN {
                return Err(AttachError::InvalidCredential);
            }
        }
        Ok(())
    }

    fn mint_binding_id(&mut self) -> Result<BindingId, AttachError> {
        let id = self.next_binding;
        self.next_binding = id.checked_add(1).ok_or(AttachError::BindingIdExhausted)?;
        Ok(BindingId(id))
    }

    fn release_where(
        &mut self,
        predicate: impl Fn(&Binding) -> bool,
        cause: ReleaseCause,
    ) -> Vec<ReleaseReceipt> {
        let ids: Vec<BindingId> = self
            .bindings
            .iter()
            .filter(|(_, binding)| predicate(binding))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.release_inner(id, cause))
            .collect()
    }

    fn release_inner(
        &mut self,
        binding_id: BindingId,
        cause: ReleaseCause,
    ) -> Option<ReleaseReceipt> {
        let binding = self.bindings.remove(&binding_id)?;
        self.by_key
            .remove(&(binding.handle.window, binding.session));
        self.by_attach.remove(&binding.attach_id);
        if let Some(count) = self.window_counts.get_mut(&binding.handle.window) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.window_counts.remove(&binding.handle.window);
            }
        }
        let snapshot = binding.snapshot();
        let discarded_events = binding.events.len();
        let host = match self.host.detach(binding.session, binding.attach_id) {
            Ok(detach) => HostDetachOutcome::Acknowledged {
                was_attached: detach.was_attached,
            },
            Err(refusal) => HostDetachOutcome::Refused(refusal),
        };
        Some(ReleaseReceipt {
            binding: snapshot,
            cause,
            host,
            discarded_events,
        })
    }
}

impl<H: SessionHost> fmt::Debug for AttachSeam<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachSeam")
            .field("config", &self.config)
            .field("daemon_available", &self.daemon_available)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}
