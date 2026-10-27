//! Session records, lifecycle, terminal classification, and the read-only
//! snapshot (contract `D-02`, `D-03`, `D-04`).
//!
//! A record owns only metadata: kind, immutable `terminal_id`, lifecycle,
//! generation, state revision, derived attach count, opaque owner peer facts,
//! and the terminal classification. Terminal content never enters a record.
//! Fields are private or crate-visible; the service surface reads
//! [`SessionSnapshot`] and mutates through the operations in
//! [`crate::registry::SessionRegistry`].

use std::fmt;

use super::id::{Entropy, SessionId, random_hex};

/// Maximum bytes in a client-supplied `terminal_id` (contract `D-02`,
/// `terminal_id_bytes`).
pub const TERMINAL_ID_MAX_BYTES: usize = 128;

/// Maximum bytes in the opaque owner peer facts this registry stores.
///
/// The service layer (bitty#2607) captures authenticated peer credentials and
/// passes a bounded encoding; the registry never parses it and never treats it
/// as authority. The bound keeps one record inside `session_record_bytes` and
/// keeps a peer descriptor from becoming a content channel.
pub const MAX_OWNER_PEER_BYTES: usize = 256;

/// Session kind. `worker` stays disabled in every v0.2.0 build (`D-01`), so the
/// registry only admits terminal sessions; the service layer refuses a worker
/// kind before it reaches this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// A terminal session backed by one worker PTY.
    Terminal,
}

impl SessionKind {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
        }
    }
}

/// Lifecycle state (`D-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// Registered; no worker bound yet.
    Created,
    /// A worker bound and is alive.
    Running,
    /// A graceful abort or drain is in progress.
    Stopping,
    /// Terminal: the worker exited on its own.
    Exited,
    /// Terminal: spawn failure, signal termination, or supervision loss.
    Failed,
    /// Terminal: the daemon terminated the worker.
    Terminated,
}

impl LifecycleState {
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

    /// Whether the state is terminal and therefore immutable (`D-04`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::Terminated)
    }

    /// Whether a terminal transition to `target` is allowed from this state
    /// (`D-04`): no state after a terminal one, and no running-to-terminal
    /// shortcut that skips `stopping`.
    pub(crate) const fn allows_terminal(self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Created, Self::Failed | Self::Terminated)
                | (Self::Running, Self::Exited | Self::Failed)
                | (
                    Self::Stopping,
                    Self::Exited | Self::Failed | Self::Terminated
                )
        )
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Closed terminal-reason set (`D-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalReason {
    /// The worker exited on its own; an exit code is recorded.
    WorkerExit,
    /// The worker died from a signal the daemon did not send.
    WorkerSignal,
    /// The worker never bound (spawn error, missing executable, bind timeout).
    SpawnFailed,
    /// The daemon could not observe, reap, or verify the worker's exit.
    SupervisionFailed,
    /// A controller aborted with the `cancel` reason.
    ConsumerCancel,
    /// A controller aborted with the `timeout` reason.
    ConsumerTimeout,
    /// An operator aborted the session.
    OperatorAbort,
    /// An explicit `daemon.stop` drained or forced the session.
    DaemonStop,
    /// The host or service manager signalled the daemon to stop.
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

    /// Whether this reason can terminate a session in `origin` (`D-04`).
    ///
    /// `spawn_failed` means the worker never bound, so only `created` can carry
    /// it. `worker_exit` and `worker_signal` mean a bound worker existed, so
    /// `created` cannot. The remaining reasons are constrained by the lifecycle
    /// transition table itself.
    pub(crate) const fn can_terminate_from(self, origin: LifecycleState) -> bool {
        match self {
            Self::SpawnFailed => matches!(origin, LifecycleState::Created),
            Self::WorkerExit | Self::WorkerSignal => {
                matches!(origin, LifecycleState::Running | LifecycleState::Stopping)
            }
            Self::SupervisionFailed
            | Self::ConsumerCancel
            | Self::ConsumerTimeout
            | Self::OperatorAbort
            | Self::DaemonStop
            | Self::HostShutdown => true,
        }
    }

    /// The one terminal state this reason maps to (`D-04`).
    #[must_use]
    pub const fn terminal_state(self) -> LifecycleState {
        match self {
            Self::WorkerExit => LifecycleState::Exited,
            Self::WorkerSignal | Self::SpawnFailed | Self::SupervisionFailed => {
                LifecycleState::Failed
            }
            Self::ConsumerCancel
            | Self::ConsumerTimeout
            | Self::OperatorAbort
            | Self::DaemonStop
            | Self::HostShutdown => LifecycleState::Terminated,
        }
    }
}

impl fmt::Display for TerminalReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Cleanup classification carried by every terminal transition (`D-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupClass {
    /// The whole containment identity was terminated and verified gone.
    Swept,
    /// The daemon could not verify the whole containment identity.
    Unverified,
    /// No worker process was ever spawned.
    NotStarted,
}

impl CleanupClass {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Swept => "swept",
            Self::Unverified => "unverified",
            Self::NotStarted => "not_started",
        }
    }
}

/// Exit information specific to the terminal reason (`D-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitInformation {
    /// A wait-status exit code for `worker_exit`.
    ExitCode(u8),
    /// A terminating signal number for `worker_signal`.
    Signal(u32),
    /// No exit status is meaningful for this reason.
    NotApplicable,
}

/// A validated terminal classification.
///
/// Construction enforces the `reason -> terminal state` mapping and the
/// exit-information rule, so a record can never hold a contradictory
/// classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalOutcome {
    reason: TerminalReason,
    exit: ExitInformation,
    cleanup: CleanupClass,
}

impl TerminalOutcome {
    /// Validate and build a terminal classification.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalOutcomeError::ExitInformation`] when the exit payload
    /// does not match the reason, or
    /// [`TerminalOutcomeError::Cleanup`] when `not_started` is claimed for a
    /// worker that must have existed.
    pub fn new(
        reason: TerminalReason,
        exit: ExitInformation,
        cleanup: CleanupClass,
    ) -> Result<Self, TerminalOutcomeError> {
        match (reason, exit) {
            (TerminalReason::WorkerExit, ExitInformation::ExitCode(_)) => {}
            (TerminalReason::WorkerSignal, ExitInformation::Signal(signal)) if signal > 0 => {}
            (
                TerminalReason::SpawnFailed
                | TerminalReason::SupervisionFailed
                | TerminalReason::ConsumerCancel
                | TerminalReason::ConsumerTimeout
                | TerminalReason::OperatorAbort
                | TerminalReason::DaemonStop
                | TerminalReason::HostShutdown,
                ExitInformation::NotApplicable,
            ) => {}
            _ => return Err(TerminalOutcomeError::ExitInformation { reason }),
        }

        if cleanup == CleanupClass::NotStarted
            && matches!(
                reason,
                TerminalReason::WorkerExit
                    | TerminalReason::WorkerSignal
                    | TerminalReason::SupervisionFailed
            )
        {
            return Err(TerminalOutcomeError::Cleanup { reason });
        }

        Ok(Self {
            reason,
            exit,
            cleanup,
        })
    }

    /// The terminal reason.
    #[must_use]
    pub const fn reason(self) -> TerminalReason {
        self.reason
    }

    /// The exit information.
    #[must_use]
    pub const fn exit(self) -> ExitInformation {
        self.exit
    }

    /// The cleanup classification.
    #[must_use]
    pub const fn cleanup(self) -> CleanupClass {
        self.cleanup
    }
}

/// A refused terminal classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcomeError {
    /// The exit payload does not belong to the reason.
    ExitInformation { reason: TerminalReason },
    /// `not_started` was claimed for a worker that existed.
    Cleanup { reason: TerminalReason },
}

impl fmt::Display for TerminalOutcomeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitInformation { reason } => {
                write!(formatter, "exit information does not match reason {reason}")
            }
            Self::Cleanup { reason } => {
                write!(
                    formatter,
                    "not_started cleanup is impossible for reason {reason}"
                )
            }
        }
    }
}

impl std::error::Error for TerminalOutcomeError {}

/// Immutable terminal identifier (`D-02`).
///
/// A supplied value is 1 to [`TERMINAL_ID_MAX_BYTES`] bytes from the safe
/// opaque alphabet `[A-Za-z0-9._:-]`; a server-assigned value is 32 lowercase
/// hex characters. The registry keeps the raw value for authorized status
/// reads; audit digests belong to the audit slice, not here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TerminalId(String);

impl TerminalId {
    /// Validate a client-supplied terminal id.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalIdError`] for an empty, oversized, whitespace,
    /// control, NUL, or otherwise out-of-alphabet value. The refusal never
    /// echoes the bytes.
    pub fn supplied(value: &str) -> Result<Self, TerminalIdError> {
        if value.is_empty() {
            return Err(TerminalIdError::Empty);
        }
        if value.len() > TERMINAL_ID_MAX_BYTES {
            return Err(TerminalIdError::TooLong { bytes: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
        {
            return Err(TerminalIdError::InvalidCharacter);
        }
        Ok(Self(value.to_owned()))
    }

    /// Mint a server-assigned terminal id: 32 lowercase hex characters.
    pub(crate) fn generate_with(entropy: &mut dyn Entropy) -> Result<Self, TerminalIdError> {
        random_hex(entropy)
            .map(Self)
            .map_err(|_| TerminalIdError::EntropyUnavailable)
    }

    /// The raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A refused terminal identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalIdError {
    /// Zero bytes.
    Empty,
    /// More than [`TERMINAL_ID_MAX_BYTES`] bytes.
    TooLong {
        /// Observed length; the value itself is not echoed.
        bytes: usize,
    },
    /// A byte outside the safe opaque alphabet.
    InvalidCharacter,
    /// The OS CSPRNG failed while minting a server-assigned id.
    EntropyUnavailable,
}

impl fmt::Display for TerminalIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("terminal id is empty"),
            Self::TooLong { bytes } => {
                write!(formatter, "terminal id is {bytes} bytes, above the bound")
            }
            Self::InvalidCharacter => formatter.write_str("terminal id has an unsafe byte"),
            Self::EntropyUnavailable => formatter.write_str("terminal id entropy unavailable"),
        }
    }
}

impl std::error::Error for TerminalIdError {}

/// Opaque, bounded owner peer facts.
///
/// The registry stores and returns the bytes without parsing them. Authority
/// comes from the authenticated peer check in the IPC slice (bitty#2607), not
/// from this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerPeer(Box<[u8]>);

impl OwnerPeer {
    /// Store opaque peer facts.
    ///
    /// # Errors
    ///
    /// Returns [`OwnerPeerError`] when the value is empty or above
    /// [`MAX_OWNER_PEER_BYTES`].
    pub fn new(bytes: &[u8]) -> Result<Self, OwnerPeerError> {
        if bytes.is_empty() {
            return Err(OwnerPeerError::Empty);
        }
        if bytes.len() > MAX_OWNER_PEER_BYTES {
            return Err(OwnerPeerError::TooLong { bytes: bytes.len() });
        }
        Ok(Self(bytes.into()))
    }

    /// The stored bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Stored length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the stored value is empty. Construction rejects empty values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A refused owner peer value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerPeerError {
    /// Zero bytes.
    Empty,
    /// More than [`MAX_OWNER_PEER_BYTES`] bytes.
    TooLong {
        /// Observed length; the value itself is not echoed.
        bytes: usize,
    },
}

impl fmt::Display for OwnerPeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("owner peer facts are empty"),
            Self::TooLong { bytes } => {
                write!(
                    formatter,
                    "owner peer facts are {bytes} bytes, above the bound"
                )
            }
        }
    }
}

impl std::error::Error for OwnerPeerError {}

/// Process-local monotonic time in milliseconds.
///
/// Callers supply the value (the service layer owns its clock), which keeps
/// retention and heartbeat behavior deterministic in tests. It is never a wall
/// clock and never crosses a process boundary as an absolute time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicMs(u64);

impl MonotonicMs {
    /// The origin.
    pub const ZERO: Self = Self(0);

    /// Build a timestamp from milliseconds.
    #[must_use]
    pub const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    /// Milliseconds since the process-local origin.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Saturating elapsed milliseconds since `earlier`.
    #[must_use]
    pub const fn saturating_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// The read-only service snapshot of one session (`D-03`, "Status and list").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// Server-assigned session identity.
    pub session_id: SessionId,
    /// Session kind; terminal in v0.2.0.
    pub kind: SessionKind,
    /// Immutable terminal identity.
    pub terminal_id: TerminalId,
    /// Lifecycle state.
    pub state: LifecycleState,
    /// Bind generation; `0` before a bind, `1` in v0.2.0.
    pub generation: u64,
    /// Snapshot-visible mutation counter.
    pub state_revision: u64,
    /// Live lease count, derived from the lease layer.
    pub attach_count: u16,
    /// `attach_count > 0`, derived rather than stored.
    pub attached: bool,
    /// Opaque owner peer facts captured by the service layer.
    ///
    /// Operator-only data: consumer `session.status` responses must not
    /// serialize it, while the operator view (`daemon.inspect`) may include
    /// it. The registry never parses or authorizes on these bytes.
    pub owner_peer: Option<OwnerPeer>,
    /// Terminal reason, present exactly in a terminal state.
    pub terminal_reason: Option<TerminalReason>,
    /// Exit information for the terminal reason.
    pub exit: ExitInformation,
    /// Cleanup classification, present exactly in a terminal state.
    pub cleanup: Option<CleanupClass>,
    /// Creation timestamp.
    pub created_at_ms: MonotonicMs,
    /// Last successful request or heartbeat timestamp.
    pub last_seen_ms: MonotonicMs,
}

/// Registry-internal session record.
#[derive(Debug)]
pub(crate) struct SessionRecord {
    session_id: SessionId,
    kind: SessionKind,
    terminal_id: TerminalId,
    pub(crate) state: LifecycleState,
    pub(crate) generation: u64,
    pub(crate) state_revision: u64,
    pub(crate) attach_count: u16,
    owner_peer: Option<OwnerPeer>,
    pub(crate) terminal_reason: Option<TerminalReason>,
    pub(crate) exit: ExitInformation,
    pub(crate) cleanup: Option<CleanupClass>,
    created_at: MonotonicMs,
    pub(crate) last_seen: MonotonicMs,
    pub(crate) terminalized_at: Option<MonotonicMs>,
}

impl SessionRecord {
    /// A fresh `created` record at revision and generation `0`.
    pub(crate) fn new(
        session_id: SessionId,
        terminal_id: TerminalId,
        owner_peer: Option<OwnerPeer>,
        now: MonotonicMs,
    ) -> Self {
        Self {
            session_id,
            kind: SessionKind::Terminal,
            terminal_id,
            state: LifecycleState::Created,
            generation: 0,
            state_revision: 0,
            attach_count: 0,
            owner_peer,
            terminal_reason: None,
            exit: ExitInformation::NotApplicable,
            cleanup: None,
            created_at: now,
            last_seen: now,
            terminalized_at: None,
        }
    }

    /// The immutable terminal id.
    pub(crate) fn terminal_id(&self) -> &TerminalId {
        &self.terminal_id
    }

    /// Accounted record bytes: the fixed in-memory record plus the variable
    /// identifier and peer bytes it owns.
    ///
    /// The contract's `session_record_bytes` bound covers per-session metadata,
    /// not terminal content, so this sum is the enforcement unit. It is
    /// deterministic for a given record.
    pub(crate) fn record_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.terminal_id.as_str().len()
            + self.owner_peer.as_ref().map_or(0, OwnerPeer::len)
    }

    /// A read-only snapshot with the derived attach flag.
    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id,
            kind: self.kind,
            terminal_id: self.terminal_id.clone(),
            state: self.state,
            generation: self.generation,
            state_revision: self.state_revision,
            attach_count: self.attach_count,
            attached: self.attach_count > 0,
            owner_peer: self.owner_peer.clone(),
            terminal_reason: self.terminal_reason,
            exit: self.exit,
            cleanup: self.cleanup,
            created_at_ms: self.created_at,
            last_seen_ms: self.last_seen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedEntropy;

    impl Entropy for FixedEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), crate::registry::id::IdError> {
            for (index, byte) in destination.iter_mut().enumerate() {
                *byte = u8::try_from(index % 251).expect("bounded");
            }
            Ok(())
        }
    }

    #[test]
    fn terminal_id_accepts_the_safe_opaque_alphabet() {
        for value in [
            "a",
            "A0._:-",
            "terminal-1.example:tab_9",
            &"a".repeat(TERMINAL_ID_MAX_BYTES),
        ] {
            assert!(TerminalId::supplied(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn terminal_id_refuses_empty_unsafe_and_oversized_values() {
        assert_eq!(TerminalId::supplied(""), Err(TerminalIdError::Empty));
        for value in ["a b", "a\tb", "a\0b", "a/b", "a\\b", "é"] {
            assert_eq!(
                TerminalId::supplied(value),
                Err(TerminalIdError::InvalidCharacter),
                "{value:?}"
            );
        }
        let oversized = "a".repeat(TERMINAL_ID_MAX_BYTES + 1);
        assert_eq!(
            TerminalId::supplied(&oversized),
            Err(TerminalIdError::TooLong {
                bytes: TERMINAL_ID_MAX_BYTES + 1
            })
        );
    }

    #[test]
    fn server_terminal_id_is_32_lowercase_hex() {
        let id = TerminalId::generate_with(&mut FixedEntropy).expect("fixed entropy succeeds");
        assert_eq!(id.as_str().len(), 32);
        assert!(
            id.as_str()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn owner_peer_is_opaque_and_bounded() {
        assert_eq!(OwnerPeer::new(b""), Err(OwnerPeerError::Empty));
        let peer = OwnerPeer::new(b"uid=1000\0pid=42").expect("bounded peer facts");
        assert_eq!(peer.as_bytes(), b"uid=1000\0pid=42");
        assert_eq!(peer.len(), 15);
        assert!(!peer.is_empty());

        let oversized = vec![0u8; MAX_OWNER_PEER_BYTES + 1];
        assert_eq!(
            OwnerPeer::new(&oversized),
            Err(OwnerPeerError::TooLong {
                bytes: MAX_OWNER_PEER_BYTES + 1
            })
        );
        assert!(OwnerPeer::new(&vec![0u8; MAX_OWNER_PEER_BYTES]).is_ok());
    }

    #[test]
    fn terminal_reasons_map_to_the_contract_terminal_states() {
        assert_eq!(
            TerminalReason::WorkerExit.terminal_state(),
            LifecycleState::Exited
        );
        assert_eq!(
            TerminalReason::WorkerSignal.terminal_state(),
            LifecycleState::Failed
        );
        assert_eq!(
            TerminalReason::SpawnFailed.terminal_state(),
            LifecycleState::Failed
        );
        assert_eq!(
            TerminalReason::SupervisionFailed.terminal_state(),
            LifecycleState::Failed
        );
        for reason in [
            TerminalReason::ConsumerCancel,
            TerminalReason::ConsumerTimeout,
            TerminalReason::OperatorAbort,
            TerminalReason::DaemonStop,
            TerminalReason::HostShutdown,
        ] {
            assert_eq!(reason.terminal_state(), LifecycleState::Terminated);
            assert!(reason.terminal_state().is_terminal());
        }
        assert!(!LifecycleState::Created.is_terminal());
        assert!(!LifecycleState::Running.is_terminal());
        assert!(!LifecycleState::Stopping.is_terminal());
    }

    #[test]
    fn terminal_outcome_validates_exit_information() {
        assert!(
            TerminalOutcome::new(
                TerminalReason::WorkerExit,
                ExitInformation::ExitCode(0),
                CleanupClass::Swept
            )
            .is_ok()
        );
        assert!(
            TerminalOutcome::new(
                TerminalReason::WorkerSignal,
                ExitInformation::Signal(9),
                CleanupClass::Swept
            )
            .is_ok()
        );
        assert!(
            TerminalOutcome::new(
                TerminalReason::SpawnFailed,
                ExitInformation::NotApplicable,
                CleanupClass::NotStarted
            )
            .is_ok()
        );

        assert_eq!(
            TerminalOutcome::new(
                TerminalReason::WorkerExit,
                ExitInformation::NotApplicable,
                CleanupClass::Swept
            ),
            Err(TerminalOutcomeError::ExitInformation {
                reason: TerminalReason::WorkerExit
            })
        );
        assert_eq!(
            TerminalOutcome::new(
                TerminalReason::WorkerSignal,
                ExitInformation::Signal(0),
                CleanupClass::Swept
            ),
            Err(TerminalOutcomeError::ExitInformation {
                reason: TerminalReason::WorkerSignal
            })
        );
        assert_eq!(
            TerminalOutcome::new(
                TerminalReason::ConsumerCancel,
                ExitInformation::ExitCode(1),
                CleanupClass::Swept
            ),
            Err(TerminalOutcomeError::ExitInformation {
                reason: TerminalReason::ConsumerCancel
            })
        );
        assert_eq!(
            TerminalOutcome::new(
                TerminalReason::WorkerExit,
                ExitInformation::ExitCode(1),
                CleanupClass::NotStarted
            ),
            Err(TerminalOutcomeError::Cleanup {
                reason: TerminalReason::WorkerExit
            })
        );
    }

    #[test]
    fn lifecycle_allows_only_the_contract_terminal_transitions() {
        assert!(!LifecycleState::Created.allows_terminal(LifecycleState::Exited));
        assert!(LifecycleState::Created.allows_terminal(LifecycleState::Failed));
        assert!(LifecycleState::Created.allows_terminal(LifecycleState::Terminated));
        assert!(LifecycleState::Running.allows_terminal(LifecycleState::Exited));
        assert!(LifecycleState::Running.allows_terminal(LifecycleState::Failed));
        assert!(!LifecycleState::Running.allows_terminal(LifecycleState::Terminated));
        for target in [
            LifecycleState::Exited,
            LifecycleState::Failed,
            LifecycleState::Terminated,
        ] {
            assert!(LifecycleState::Stopping.allows_terminal(target));
            assert!(!LifecycleState::Exited.allows_terminal(target));
            assert!(!LifecycleState::Failed.allows_terminal(target));
            assert!(!LifecycleState::Terminated.allows_terminal(target));
        }
    }

    #[test]
    fn snapshot_derives_attached_from_the_attach_count() {
        let record = SessionRecord::new(
            SessionId::parse("00000000000000000000000000000000").expect("canonical"),
            TerminalId::supplied("term").expect("valid"),
            None,
            MonotonicMs::ZERO,
        );
        let snapshot = record.snapshot();
        assert!(!snapshot.attached);
        assert_eq!(snapshot.attach_count, 0);
        assert_eq!(snapshot.kind, SessionKind::Terminal);
        assert_eq!(snapshot.state, LifecycleState::Created);
        assert_eq!(snapshot.generation, 0);
        assert_eq!(snapshot.state_revision, 0);
    }

    #[test]
    fn record_bytes_include_the_identifier_and_peer_payload() {
        let short = SessionRecord::new(
            SessionId::parse("11111111111111111111111111111111").expect("canonical"),
            TerminalId::supplied("a").expect("valid"),
            None,
            MonotonicMs::ZERO,
        );
        let long = SessionRecord::new(
            SessionId::parse("22222222222222222222222222222222").expect("canonical"),
            TerminalId::supplied(&"a".repeat(TERMINAL_ID_MAX_BYTES)).expect("valid"),
            Some(OwnerPeer::new(&[7u8; MAX_OWNER_PEER_BYTES]).expect("bounded")),
            MonotonicMs::ZERO,
        );
        assert_eq!(
            long.record_bytes() - short.record_bytes(),
            (TERMINAL_ID_MAX_BYTES - 1) + MAX_OWNER_PEER_BYTES
        );
    }
}
