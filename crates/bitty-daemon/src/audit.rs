//! Bounded, content-free audit records and the in-memory audit ring
//! (contract `D-16`, bitty#2603).
//!
//! The ring is the daemon's bounded staging buffer of typed lifecycle and
//! refusal records. It is not the owner-private sink: bitty#2607 drains these
//! records into the bounded, rotating file under `$XDG_STATE_HOME/bitty/audit/`
//! that `D-16` specifies. This module owns the record shape, the stable event
//! ids, the injected monotonic timestamp, the counters, and the overflow
//! behavior so the sink writer cannot invent a different vocabulary.
//!
//! The record carries the injected monotonic time for ordering. The
//! display-only wall-clock timestamp `D-16` also names is attached by the sink
//! writer when it writes the owner-private file; no wall-clock value enters the
//! in-memory ring or its order.
//!
//! # What a record may contain
//!
//! Every field is a closed enum, a numeric counter, a fixed-size identifier,
//! or a digest. There is no `String`, `Vec`, byte payload, or borrowed text
//! field:
//!
//! - no terminal content, PTY bytes, scrollback, clipboard, or transcript;
//! - no argv, environment, working directory, or spawn-spec value;
//! - no configuration value (`D-16` limits/counts are numeric facts, not
//!   config text);
//! - no raw `terminal_id`; a create or terminal transition carries
//!   [`AuditTarget::terminal_digest`] instead (`D-18`, `D-16`);
//! - no control token, operator capability, grant bearer, or peer secret;
//! - no caller-supplied label, message, path, or correlation value; the
//!   optional consumer correlation is already a digest
//!   (`client_correlation_digest`).
//!
//! [`AuditAction`] and [`crate::codes::RefusalCode`] are static vocabularies.
//! A refused request is representable as [`AuditResult::Refused`] with its
//! static code plus optional [`AuditCounts`]; it never echoes the refused
//! bytes.
//!
//! # Overflow is visible
//!
//! The ring holds `audit_file_bytes × audit_segments / AUDIT_RECORD_BYTES`
//! records at most. When it is full, [`AuditRing::record`] drops the oldest
//! record and increments [`AuditCounters::dropped`]; the append, the drop, the
//! refusal count, and the terminal-transition count are all observable. Event
//! ids are strictly increasing and never reused, so a reader can detect the
//! gap left by a dropped oldest record.

use std::collections::VecDeque;
use std::fmt;

pub use crate::limits::AUDIT_RECORD_BYTES;

use crate::codes::RefusalCode;
use crate::limits::{LimitId, LimitRefusal, Limits};
use crate::registry::{
    CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs, SessionId,
    TerminalReason,
};

/// Maximum bytes in a Windows security identifier.
///
/// A SID is a header plus up to 15 32-bit subauthorities, so 68 bytes is the
/// platform maximum. The actor field is fixed-size, not caller text.
pub const MAX_SID_BYTES: usize = 68;

/// A stable audit event id.
///
/// Ids start at [`Self::FIRST`] and increase by one per append. They are never
/// renumbered when the ring drops an old record and never reused, so a gap
/// identifies dropped records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuditEventId(u64);

impl AuditEventId {
    /// The first assigned event id.
    pub const FIRST: Self = Self(1);

    /// The numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for AuditEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The service action an audit record describes.
///
/// The spellings are the contract method names for wire actions plus two
/// internal lifecycle actions. Reads are included because a refused read is a
/// refusal record (`D-16`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditAction {
    /// `daemon.negotiate`.
    Negotiate,
    /// `session.create`.
    Create,
    /// `session.status`.
    Status,
    /// `session.list`.
    List,
    /// `session.attach`.
    Attach,
    /// `session.detach`.
    Detach,
    /// `session.grant` mint.
    Grant,
    /// `session.grant.revoke`.
    GrantRevoke,
    /// `session.control.transfer`.
    Transfer,
    /// Controller-only terminal resize.
    Resize,
    /// `session.abort`.
    Abort,
    /// `session.kill`.
    Kill,
    /// `daemon.status`.
    DaemonStatus,
    /// `daemon.inspect` operator view.
    DaemonInspect,
    /// `daemon.stop`.
    Stop,
    /// `daemon.orphans.verify`.
    OrphanVerify,
    /// `lease.heartbeat`.
    Heartbeat,
    /// The internal worker bind lifecycle step.
    Bind,
    /// The internal worker-exit lifecycle step.
    WorkerExit,
}

impl AuditAction {
    /// Static spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Negotiate => "daemon.negotiate",
            Self::Create => "session.create",
            Self::Status => "session.status",
            Self::List => "session.list",
            Self::Attach => "session.attach",
            Self::Detach => "session.detach",
            Self::Grant => "session.grant",
            Self::GrantRevoke => "session.grant.revoke",
            Self::Transfer => "session.control.transfer",
            Self::Resize => "session.resize",
            Self::Abort => "session.abort",
            Self::Kill => "session.kill",
            Self::DaemonStatus => "daemon.status",
            Self::DaemonInspect => "daemon.inspect",
            Self::Stop => "daemon.stop",
            Self::OrphanVerify => "daemon.orphans.verify",
            Self::Heartbeat => "lease.heartbeat",
            Self::Bind => "session.bind",
            Self::WorkerExit => "session.worker_exit",
        }
    }
}

impl fmt::Display for AuditAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The static result of an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResult {
    /// The lifecycle action succeeded.
    Ok,
    /// The action was refused whole with a static code.
    Refused(RefusalCode),
}

impl AuditResult {
    /// The refusal code, when the result is a refusal.
    #[must_use]
    pub const fn refusal_code(self) -> Option<RefusalCode> {
        match self {
            Self::Ok => None,
            Self::Refused(code) => Some(code),
        }
    }

    /// Whether this is a refusal.
    #[must_use]
    pub const fn is_refused(self) -> bool {
        matches!(self, Self::Refused(_))
    }
}

/// A Windows security identifier, stored in a fixed-size buffer.
///
/// A SID is an authenticated peer identity, not content or a secret. The
/// constructor bounds it to the platform maximum and the type has no heap
/// allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AuditSid {
    bytes: [u8; MAX_SID_BYTES],
    len: u8,
}

impl AuditSid {
    /// Store a bounded SID byte string.
    ///
    /// # Errors
    ///
    /// Refuses an empty value or one above [`MAX_SID_BYTES`]. The refusal never
    /// echoes the bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, AuditSidError> {
        if bytes.is_empty() {
            return Err(AuditSidError::Empty);
        }
        if bytes.len() > MAX_SID_BYTES {
            return Err(AuditSidError::TooLong { bytes: bytes.len() });
        }
        let mut stored = [0u8; MAX_SID_BYTES];
        stored[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: stored,
            len: u8::try_from(bytes.len()).expect("bounded by MAX_SID_BYTES"),
        })
    }

    /// The stored SID bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Debug for AuditSid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuditSid(")?;
        for byte in self.as_bytes() {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// A refused SID value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSidError {
    /// Zero bytes.
    Empty,
    /// Above [`MAX_SID_BYTES`].
    TooLong {
        /// Observed length; the value itself is not echoed.
        bytes: usize,
    },
}

impl fmt::Display for AuditSidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("audit SID is empty"),
            Self::TooLong { bytes } => {
                write!(formatter, "audit SID is {bytes} bytes, above the bound")
            }
        }
    }
}

impl std::error::Error for AuditSidError {}

/// The authenticated actor facts on a record (`D-16`).
///
/// The raw consumer correlation value is never accepted; only its truncated
/// 128-bit digest is stored. The control token and operator capability are
/// absent by construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditActor {
    /// Unix peer uid, when the endpoint is a Unix socket.
    pub peer_uid: Option<u32>,
    /// Windows peer SID, when the endpoint is a named pipe.
    pub peer_sid: Option<AuditSid>,
    /// Peer process id, observational only.
    pub peer_pid: Option<u32>,
    /// Server-assigned connection identity.
    pub client_id: Option<u64>,
    /// Truncated digest of the optional consumer correlation value.
    pub client_correlation_digest: Option<[u8; 16]>,
}

/// The target of an audited action (`D-16`).
///
/// A raw `terminal_id` is never a target: create and terminal transitions use
/// its digest. Attach and grant targets are non-secret ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditTarget {
    kind: AuditTargetKind,
    id: [u8; 16],
}

/// What an [`AuditTarget`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditTargetKind {
    /// No target (for example `daemon.negotiate`).
    None,
    /// A session id.
    Session,
    /// An attach lease id.
    Attach,
    /// A non-secret grant id.
    Grant,
    /// A `terminal_id_digest`; the raw terminal id is never stored.
    TerminalDigest,
}

impl AuditTarget {
    /// No target.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            kind: AuditTargetKind::None,
            id: [0u8; 16],
        }
    }

    /// A session target.
    #[must_use]
    pub const fn session(session_id: SessionId) -> Self {
        Self {
            kind: AuditTargetKind::Session,
            id: *session_id.as_bytes(),
        }
    }

    /// An attach-lease target.
    #[must_use]
    pub const fn attach(attach_id: [u8; 16]) -> Self {
        Self {
            kind: AuditTargetKind::Attach,
            id: attach_id,
        }
    }

    /// A non-secret grant target.
    #[must_use]
    pub const fn grant(grant_id: [u8; 16]) -> Self {
        Self {
            kind: AuditTargetKind::Grant,
            id: grant_id,
        }
    }

    /// A terminal-id digest target.
    #[must_use]
    pub const fn terminal_digest(digest: [u8; 16]) -> Self {
        Self {
            kind: AuditTargetKind::TerminalDigest,
            id: digest,
        }
    }

    /// What the target names.
    #[must_use]
    pub const fn kind(self) -> AuditTargetKind {
        self.kind
    }

    /// The 128-bit target value; zero when [`AuditTargetKind::None`].
    #[must_use]
    pub const fn id(self) -> [u8; 16] {
        self.id
    }
}

impl Default for AuditTarget {
    fn default() -> Self {
        Self::none()
    }
}

/// Counts and the limit a refusal hit (`D-16`, `D-14`).
///
/// Both values are numeric facts; no configuration text is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCounts {
    /// The observed count or size.
    pub actual: u64,
    /// The enforced bound.
    pub limit: u64,
    /// The limit identifier, when the refusal maps to a compiled limit.
    pub limit_id: Option<LimitId>,
}

impl AuditCounts {
    /// Build counts from an observed value and its bound.
    #[must_use]
    pub const fn new(actual: u64, limit: u64, limit_id: Option<LimitId>) -> Self {
        Self {
            actual,
            limit,
            limit_id,
        }
    }

    /// Build counts from a whole-request limit refusal.
    #[must_use]
    pub const fn from_limit_refusal(refusal: LimitRefusal) -> Self {
        Self {
            actual: refusal.actual(),
            limit: refusal.effective(),
            limit_id: Some(refusal.limit()),
        }
    }
}

/// The terminal classification on a terminal transition (`D-16`, `D-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditTerminal {
    /// Terminal lifecycle state.
    pub state: LifecycleState,
    /// Terminal reason.
    pub reason: TerminalReason,
    /// Exit code, signal, or not applicable.
    pub exit: ExitInformation,
    /// Cleanup classification.
    pub cleanup: CleanupClass,
}

/// The session counters a record carries when a session exists (`D-16`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditSession {
    /// Session identity.
    pub session_id: SessionId,
    /// Generation at the time of the record.
    pub generation: u64,
    /// State revision at the time of the record.
    pub state_revision: u64,
}

/// The caller-supplied facts of one audit event, before the ring assigns the
/// event id and timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditEntry {
    /// Service action.
    pub action: AuditAction,
    /// Static result.
    pub result: AuditResult,
    /// Session counters, when a session exists.
    pub session: Option<AuditSession>,
    /// Authenticated actor facts, when known.
    pub actor: Option<AuditActor>,
    /// Action target.
    pub target: AuditTarget,
    /// Refusal counts and limit.
    pub counts: Option<AuditCounts>,
    /// Terminal classification for a terminal transition.
    pub terminal: Option<AuditTerminal>,
}

impl AuditEntry {
    /// Start a successful action with no target, session, actor, counts, or
    /// terminal classification.
    #[must_use]
    pub const fn new(action: AuditAction) -> Self {
        Self {
            action,
            result: AuditResult::Ok,
            session: None,
            actor: None,
            target: AuditTarget::none(),
            counts: None,
            terminal: None,
        }
    }

    /// Set the static result.
    #[must_use]
    pub const fn result(mut self, result: AuditResult) -> Self {
        self.result = result;
        self
    }

    /// Set the session counters.
    #[must_use]
    pub const fn session(mut self, session: AuditSession) -> Self {
        self.session = Some(session);
        self
    }

    /// Set the authenticated actor facts.
    #[must_use]
    pub const fn actor(mut self, actor: AuditActor) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Set the action target.
    #[must_use]
    pub const fn target(mut self, target: AuditTarget) -> Self {
        self.target = target;
        self
    }

    /// Set the refusal counts.
    #[must_use]
    pub const fn counts(mut self, counts: AuditCounts) -> Self {
        self.counts = Some(counts);
        self
    }

    /// Set the terminal classification.
    #[must_use]
    pub const fn terminal(mut self, terminal: AuditTerminal) -> Self {
        self.terminal = Some(terminal);
        self
    }
}

/// One immutable audit record.
///
/// The type is `Copy`, has no destructor, and every field is a closed enum,
/// numeric counter, fixed-size id, or digest. A content field (`String`,
/// `Vec`, `&str`, byte payload) would make one of those properties fail; the
/// unit tests assert them structurally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditRecord {
    /// Stable, never-reused event id.
    pub event_id: AuditEventId,
    /// Injected monotonic timestamp; ordering is by event id, never by time.
    pub at_ms: MonotonicMs,
    /// Daemon instance that produced the record.
    pub daemon_instance_id: DaemonInstanceId,
    /// Service action.
    pub action: AuditAction,
    /// Static result.
    pub result: AuditResult,
    /// Session counters, when a session exists.
    pub session: Option<AuditSession>,
    /// Authenticated actor facts, when known.
    pub actor: Option<AuditActor>,
    /// Action target.
    pub target: AuditTarget,
    /// Refusal counts and limit.
    pub counts: Option<AuditCounts>,
    /// Terminal classification for a terminal transition.
    pub terminal: Option<AuditTerminal>,
}

/// Observable ring counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditCounters {
    /// Records appended over the ring's lifetime.
    pub appended: u64,
    /// Oldest records dropped because the ring was full.
    pub dropped: u64,
    /// Appended records whose result was a refusal.
    pub refusals: u64,
    /// Appended records carrying a terminal classification.
    pub terminal_transitions: u64,
}

/// An audit append failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditError {
    /// The event-id sequence would wrap; the ring fails closed.
    EventIdExhausted,
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventIdExhausted => formatter.write_str("audit event id would wrap"),
        }
    }
}

impl std::error::Error for AuditError {}

/// The record capacity derived from the effective audit byte budget.
///
/// `audit_file_bytes × audit_segments` is the `D-14` sink bound; dividing by
/// the compiled per-record budget gives a fixed record count. The compiled
/// floor on `audit_file_bytes` guarantees at least one record.
#[must_use]
pub fn audit_ring_capacity(limits: &Limits) -> usize {
    let bytes = limits
        .effective(LimitId::AuditFileBytes)
        .saturating_mul(limits.effective(LimitId::AuditSegments));
    let records = bytes / AUDIT_RECORD_BYTES as u64;
    usize::try_from(records).unwrap_or(usize::MAX).max(1)
}

/// A bounded ring of typed audit records.
///
/// One ring belongs to one daemon instance. The ring is not the owner-private
/// sink; it is the bounded staging buffer the sink writer drains. A full ring
/// drops its oldest record and makes the drop visible through
/// [`Self::counters`].
pub struct AuditRing {
    instance_id: DaemonInstanceId,
    capacity: usize,
    records: VecDeque<AuditRecord>,
    next_event_id: u64,
    counters: AuditCounters,
}

impl AuditRing {
    /// Build an empty ring for `instance_id` at the effective audit budget.
    #[must_use]
    pub fn new(instance_id: DaemonInstanceId, limits: &Limits) -> Self {
        let capacity = audit_ring_capacity(limits);
        Self {
            instance_id,
            capacity,
            records: VecDeque::with_capacity(capacity),
            next_event_id: AuditEventId::FIRST.get(),
            counters: AuditCounters::default(),
        }
    }

    /// The daemon instance every record carries.
    #[must_use]
    pub const fn instance_id(&self) -> DaemonInstanceId {
        self.instance_id
    }

    /// The fixed record capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current record count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the ring is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Current counters.
    #[must_use]
    pub const fn counters(&self) -> AuditCounters {
        self.counters
    }

    /// Append one record with the next stable event id and the injected
    /// timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::EventIdExhausted`] when the event-id sequence
    /// would wrap. A failed append changes nothing.
    pub fn record(
        &mut self,
        entry: AuditEntry,
        now: MonotonicMs,
    ) -> Result<AuditEventId, AuditError> {
        let event_id = AuditEventId(self.next_event_id);
        self.next_event_id = self
            .next_event_id
            .checked_add(1)
            .ok_or(AuditError::EventIdExhausted)?;

        if self.records.len() >= self.capacity {
            self.records.pop_front();
            self.counters.dropped = self.counters.dropped.saturating_add(1);
        }

        let record = AuditRecord {
            event_id,
            at_ms: now,
            daemon_instance_id: self.instance_id,
            action: entry.action,
            result: entry.result,
            session: entry.session,
            actor: entry.actor,
            target: entry.target,
            counts: entry.counts,
            terminal: entry.terminal,
        };
        self.records.push_back(record);
        self.counters.appended = self.counters.appended.saturating_add(1);
        if entry.result.is_refused() {
            self.counters.refusals = self.counters.refusals.saturating_add(1);
        }
        if entry.terminal.is_some() {
            self.counters.terminal_transitions =
                self.counters.terminal_transitions.saturating_add(1);
        }
        Ok(event_id)
    }

    /// Records in event-id order, oldest first.
    pub fn records(&self) -> impl DoubleEndedIterator<Item = &AuditRecord> + '_ {
        self.records.iter()
    }

    /// The oldest retained record.
    #[must_use]
    pub fn oldest(&self) -> Option<&AuditRecord> {
        self.records.front()
    }

    /// The newest retained record.
    #[must_use]
    pub fn newest(&self) -> Option<&AuditRecord> {
        self.records.back()
    }
}

impl fmt::Debug for AuditRing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditRing")
            .field("instance_id", &self.instance_id)
            .field("capacity", &self.capacity)
            .field("records", &self.records.len())
            .field("counters", &self.counters)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitOverride;

    fn ms(value: u64) -> MonotonicMs {
        MonotonicMs::from_millis(value)
    }

    fn instance(seed: u8) -> DaemonInstanceId {
        DaemonInstanceId::parse(&format!("{seed:02x}").repeat(16)).expect("canonical instance id")
    }

    fn session(seed: u8) -> SessionId {
        SessionId::parse(&format!("{seed:02x}").repeat(16)).expect("canonical session id")
    }

    fn small_limits(records: u64) -> Limits {
        Limits::new(&[
            LimitOverride::new(
                LimitId::AuditFileBytes,
                records.saturating_mul(AUDIT_RECORD_BYTES as u64),
            ),
            LimitOverride::new(LimitId::AuditSegments, 1),
        ])
        .expect("valid audit budget")
    }

    #[test]
    fn record_is_fixed_size_copy_and_has_no_content_fields() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<AuditRecord>();
        assert!(!std::mem::needs_drop::<AuditRecord>());
        assert!(
            std::mem::size_of::<AuditRecord>() <= AUDIT_RECORD_BYTES,
            "record is {} bytes, budget is {AUDIT_RECORD_BYTES}",
            std::mem::size_of::<AuditRecord>()
        );
        // A String, Vec, or borrowed text field would make the type non-Copy
        // and give it a destructor; the assertions above are the structural
        // guard, and the field list is the closed vocabulary documented on the
        // type.
    }

    #[test]
    fn a_record_carries_no_terminal_id_or_caller_text() {
        let marker = "terminal-marker-do-not-copy";
        let terminal_id = crate::registry::TerminalId::supplied(marker).expect("safe marker");
        assert_eq!(terminal_id.as_str(), marker);

        let entry = AuditEntry::new(AuditAction::Create)
            .target(AuditTarget::terminal_digest([0x5a; 16]))
            .session(AuditSession {
                session_id: session(1),
                generation: 0,
                state_revision: 0,
            });
        let mut ring = AuditRing::new(instance(2), &small_limits(4));
        let event_id = ring.record(entry, ms(1)).expect("append");
        assert_eq!(event_id, AuditEventId::FIRST);
        let record = ring.oldest().expect("record retained");
        let rendered = format!("{record:?}");
        assert!(!rendered.contains(marker));
        assert!(!rendered.contains("terminal_id"));
        assert_eq!(record.target.kind(), AuditTargetKind::TerminalDigest);
        assert_eq!(record.target.id(), [0x5a; 16]);
    }

    #[test]
    fn refusals_are_representable_with_a_code_and_counts() {
        let limits =
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 4)]).expect("valid limit");
        let refusal = limits.refusal(LimitId::MaxSessions, 5);
        let entry = AuditEntry::new(AuditAction::Create)
            .result(AuditResult::Refused(RefusalCode::LimitExceeded))
            .counts(AuditCounts::from_limit_refusal(refusal));
        let mut ring = AuditRing::new(instance(1), &Limits::default());
        ring.record(entry, ms(1)).expect("append");
        let record = ring.newest().expect("record");
        assert_eq!(
            record.result,
            AuditResult::Refused(RefusalCode::LimitExceeded)
        );
        assert_eq!(
            record.counts,
            Some(AuditCounts::new(5, 4, Some(LimitId::MaxSessions)))
        );
        assert_eq!(record.counts.expect("counts").actual, 5);
        assert_eq!(ring.counters().refusals, 1);
        assert_eq!(ring.counters().appended, 1);
    }

    #[test]
    fn event_ids_are_stable_and_never_reused_across_overflow() {
        let mut ring = AuditRing::new(instance(1), &small_limits(2));
        assert_eq!(ring.capacity(), 2);
        assert_eq!(
            ring.record(AuditEntry::new(AuditAction::Negotiate), ms(10))
                .expect("append"),
            AuditEventId::FIRST
        );
        let second = ring
            .record(AuditEntry::new(AuditAction::Status), ms(20))
            .expect("append");
        let third = ring
            .record(AuditEntry::new(AuditAction::List), ms(30))
            .expect("append");

        assert_eq!(second.get(), 2);
        assert_eq!(third.get(), 3);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.counters().appended, 3);
        assert_eq!(ring.counters().dropped, 1);
        assert_eq!(ring.oldest().expect("record").event_id.get(), 2);
        assert_eq!(ring.newest().expect("record").event_id.get(), 3);
        assert_eq!(
            ring.records()
                .map(|record| record.event_id.get())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn record_order_is_event_id_order_not_timestamp_order() {
        let mut ring = AuditRing::new(instance(1), &small_limits(4));
        ring.record(AuditEntry::new(AuditAction::Negotiate), ms(3_000))
            .expect("append");
        ring.record(AuditEntry::new(AuditAction::Create), ms(1_000))
            .expect("append");
        let ids: Vec<_> = ring
            .records()
            .map(|record| (record.event_id.get(), record.at_ms))
            .collect();
        assert_eq!(ids, vec![(1, ms(3_000)), (2, ms(1_000))]);
    }

    #[test]
    fn counters_track_refusals_and_terminal_transitions() {
        let mut ring = AuditRing::new(instance(1), &Limits::default());
        ring.record(
            AuditEntry::new(AuditAction::Create)
                .result(AuditResult::Refused(RefusalCode::IdempotencyCapacity)),
            ms(1),
        )
        .expect("append");
        ring.record(
            AuditEntry::new(AuditAction::Kill).terminal(AuditTerminal {
                state: LifecycleState::Terminated,
                reason: TerminalReason::OperatorAbort,
                exit: ExitInformation::NotApplicable,
                cleanup: CleanupClass::Swept,
            }),
            ms(2),
        )
        .expect("append");

        assert_eq!(ring.counters().appended, 2);
        assert_eq!(ring.counters().refusals, 1);
        assert_eq!(ring.counters().terminal_transitions, 1);
        assert_eq!(ring.counters().dropped, 0);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn audit_sid_is_bounded_and_never_heap_allocated() {
        assert_eq!(AuditSid::new(&[]), Err(AuditSidError::Empty));
        assert_eq!(
            AuditSid::new(&[0u8; MAX_SID_BYTES + 1]),
            Err(AuditSidError::TooLong {
                bytes: MAX_SID_BYTES + 1
            })
        );
        let sid = AuditSid::new(b"S-1-5-21").expect("bounded sid");
        assert_eq!(sid.as_bytes(), b"S-1-5-21");
        assert!(!std::mem::needs_drop::<AuditSid>());
    }

    #[test]
    fn capacity_derives_from_the_effective_audit_budget() {
        let limits = small_limits(3);
        assert_eq!(audit_ring_capacity(&limits), 3);
        let defaults = Limits::default();
        assert_eq!(
            audit_ring_capacity(&defaults),
            (1_048_576 * 2) / AUDIT_RECORD_BYTES
        );
    }

    #[test]
    fn ring_debug_does_not_print_records_or_keys() {
        let mut ring = AuditRing::new(instance(1), &small_limits(1));
        ring.record(
            AuditEntry::new(AuditAction::Negotiate).actor(AuditActor {
                peer_uid: Some(1000),
                ..AuditActor::default()
            }),
            ms(1),
        )
        .expect("append");
        let rendered = format!("{ring:?}");
        assert!(rendered.contains("records: 1"));
        assert!(!rendered.contains("1000"));
    }
}
