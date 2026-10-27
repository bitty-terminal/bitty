//! The owner-private audit sink and the fail-closed admission gate (`D-16`).
//!
//! [`crate::audit::AuditRing`] is the bounded, content-free staging buffer. This
//! module is the writer that drains it and the admission gate that makes a
//! privileged mutation fail closed when the sink cannot be written:
//!
//! - the sink is an owner-private append-only file under
//!   `$XDG_STATE_HOME/bitty/audit/` (directory `0700`, file `0600`), bounded by
//!   `audit_file_bytes` and `audit_segments`;
//! - rotation happens when the active segment reaches `audit_file_bytes`: the
//!   active segment is synced, a new segment is opened, and the oldest segment
//!   is removed so at most `audit_segments` exist;
//! - [`AuditSink::admit_mutation`] flushes every pending record and syncs
//!   before the effect. A failure sets `audit_degraded` and refuses the action
//!   with `Denied/Internal`; no state changes on that path;
//! - a record that fails to append stays in the sink and is retried in event-id
//!   order, so a transient failure neither reorders nor silently drops the
//!   tail. The sink is not tamper-evident against the account owner and is not
//!   a durable ledger: a crash may lose the unflushed tail of the active
//!   segment, and the loss is bounded by one segment.
//!
//! The rendered line contains only closed enum spellings, fixed-size ids,
//! digests, and numeric counters. No caller bytes, path, token, bearer,
//! terminal text, argv, environment, or prompt text can reach it.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::audit::{
    AuditCounters, AuditEventId, AuditRecord, AuditResult, AuditRing, AuditTerminal,
};
use crate::codes::RefusalCode;
#[cfg(unix)]
use crate::limits::LimitId;
use crate::limits::Limits;
use crate::registry::{
    CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs, TerminalReason,
};

/// Owner-private state subdirectory under `$XDG_STATE_HOME`.
pub const STATE_SUBDIR: &str = "bitty";

/// Audit subdirectory under [`STATE_SUBDIR`].
pub const AUDIT_SUBDIR: &str = "audit";

/// Active segment file name.
pub const ACTIVE_SEGMENT: &str = "audit.log";

/// Directory mode (`0700`).
pub const AUDIT_DIR_MODE: u32 = 0o700;

/// Segment file mode (`0600`).
pub const SEGMENT_MODE: u32 = 0o600;

/// Records drained and rendered per flush.
pub const FLUSH_BATCH: usize = 16;

/// Largest rendered line the sink accepts.
///
/// The in-memory record budget is [`crate::limits::AUDIT_RECORD_BYTES`] (320
/// accounted bytes). The rendered line carries every `D-16` field and can be
/// larger; this is the hard bound that keeps one line from growing without
/// limit. A line over the bound is a structural defect and fails closed.
pub const MAX_RENDERED_LINE_BYTES: usize = 512;

/// A refused audit-sink operation.
///
/// The variants are static: no path, record content, or caller bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSinkError {
    /// The sink directory or segment is not owner-private.
    UnsafeStorage,
    /// A filesystem operation failed.
    Io,
    /// The record would render above [`MAX_RENDERED_LINE_BYTES`].
    RecordTooLarge,
    /// The ring's event-id sequence would wrap.
    EventIdExhausted,
    /// The platform cell has no owner-private file sink in this slice.
    UnsupportedPlatform,
}

impl fmt::Display for AuditSinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeStorage => {
                formatter.write_str("audit storage is not a private owned directory or file")
            }
            Self::Io => formatter.write_str("audit sink write failed"),
            Self::RecordTooLarge => formatter.write_str("audit record exceeds the line bound"),
            Self::EventIdExhausted => formatter.write_str("audit event id would wrap"),
            Self::UnsupportedPlatform => {
                formatter.write_str("this platform cell has no audit file sink")
            }
        }
    }
}

impl std::error::Error for AuditSinkError {}

/// A destination for rendered audit lines.
///
/// The trait exists so the sink's fail-closed behavior is testable with an
/// injected writer; production uses [`AuditFileWriter`].
pub trait AuditWriter {
    /// Append one rendered line.
    ///
    /// # Errors
    ///
    /// Any failure is reported as an [`AuditSinkError`]; earlier lines of the
    /// same flush may already be written.
    fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError>;

    /// Flush the destination to stable storage.
    ///
    /// # Errors
    ///
    /// Any failure is reported as an [`AuditSinkError`].
    fn sync(&mut self) -> Result<(), AuditSinkError>;
}

impl<T: AuditWriter + ?Sized> AuditWriter for Box<T> {
    fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError> {
        (**self).append_line(line)
    }

    fn sync(&mut self) -> Result<(), AuditSinkError> {
        (**self).sync()
    }
}

/// An owner-private, bounded, rotating audit file.
#[cfg(unix)]
#[derive(Debug)]
pub struct AuditFileWriter {
    dir: PathBuf,
    active: std::fs::File,
    active_len: u64,
    file_limit: u64,
    segments: usize,
}

#[cfg(unix)]
impl AuditFileWriter {
    /// Open (or create) the audit directory and its active segment.
    ///
    /// # Errors
    ///
    /// [`AuditSinkError::UnsafeStorage`] when the directory or active segment is
    /// a symlink, the wrong type, owned by another user, or does not carry the
    /// contract mode; [`AuditSinkError::Io`] for a filesystem failure.
    pub fn open(dir: &Path, limits: &Limits) -> Result<Self, AuditSinkError> {
        use std::fs::{DirBuilder, OpenOptions, Permissions};
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

        let uid = nix::unistd::geteuid().as_raw();
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || metadata.uid() != uid
                    || metadata.mode() & 0o777 != AUDIT_DIR_MODE
                {
                    return Err(AuditSinkError::UnsafeStorage);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.recursive(true).mode(AUDIT_DIR_MODE);
                builder.create(dir).map_err(|_| AuditSinkError::Io)?;
                std::fs::set_permissions(dir, Permissions::from_mode(AUDIT_DIR_MODE))
                    .map_err(|_| AuditSinkError::Io)?;
            }
            Err(_) => return Err(AuditSinkError::Io),
        }
        let path = dir.join(ACTIVE_SEGMENT);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != uid
                || metadata.mode() & 0o777 != SEGMENT_MODE
            {
                return Err(AuditSinkError::UnsafeStorage);
            }
        }
        let active = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(SEGMENT_MODE)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| AuditSinkError::Io)?;
        active
            .set_permissions(Permissions::from_mode(SEGMENT_MODE))
            .map_err(|_| AuditSinkError::Io)?;
        let active_len = active.metadata().map_err(|_| AuditSinkError::Io)?.len();
        Ok(Self {
            dir: dir.to_path_buf(),
            active,
            active_len,
            file_limit: limits.effective(LimitId::AuditFileBytes).max(1),
            segments: usize::try_from(limits.effective(LimitId::AuditSegments))
                .unwrap_or(usize::MAX)
                .max(1),
        })
    }

    /// Rotate the active segment and drop the oldest one.
    fn rotate(&mut self) -> Result<(), AuditSinkError> {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        self.active.sync_all().map_err(|_| AuditSinkError::Io)?;
        if self.segments > 1 {
            let oldest = self.segments - 1;
            match std::fs::remove_file(self.segment_path(oldest)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(AuditSinkError::Io),
            }
            for index in (1..oldest).rev() {
                let from = self.segment_path(index);
                let to = self.segment_path(index + 1);
                match std::fs::rename(&from, &to) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(AuditSinkError::Io),
                }
            }
            std::fs::rename(self.dir.join(ACTIVE_SEGMENT), self.segment_path(1))
                .map_err(|_| AuditSinkError::Io)?;
        } else {
            std::fs::remove_file(self.dir.join(ACTIVE_SEGMENT)).map_err(|_| AuditSinkError::Io)?;
        }
        let active = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(SEGMENT_MODE)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.dir.join(ACTIVE_SEGMENT))
            .map_err(|_| AuditSinkError::Io)?;
        active
            .set_permissions(std::fs::Permissions::from_mode(SEGMENT_MODE))
            .map_err(|_| AuditSinkError::Io)?;
        self.active = active;
        self.active_len = 0;
        Ok(())
    }

    fn segment_path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("audit.{index}.log"))
    }
}

#[cfg(unix)]
impl AuditWriter for AuditFileWriter {
    fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError> {
        use std::io::Write as _;

        let line_len = u64::try_from(line.len()).map_err(|_| AuditSinkError::RecordTooLarge)?;
        if self.active_len > 0 && self.active_len + line_len > self.file_limit {
            self.rotate()?;
        }
        self.active
            .write_all(line)
            .map_err(|_| AuditSinkError::Io)?;
        self.active
            .write_all(b"\n")
            .map_err(|_| AuditSinkError::Io)?;
        self.active_len = self.active_len.saturating_add(line_len).saturating_add(1);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), AuditSinkError> {
        self.active.sync_data().map_err(|_| AuditSinkError::Io)
    }
}

/// A stub writer on cells without an owner-private file sink.
#[cfg(not(unix))]
#[derive(Debug, Default)]
pub struct AuditFileWriter;

#[cfg(not(unix))]
impl AuditFileWriter {
    /// Refuse to open a sink on an unsupported cell.
    ///
    /// # Errors
    ///
    /// Always [`AuditSinkError::UnsupportedPlatform`].
    pub fn open(_dir: &Path, _limits: &Limits) -> Result<Self, AuditSinkError> {
        Err(AuditSinkError::UnsupportedPlatform)
    }
}

#[cfg(not(unix))]
impl AuditWriter for AuditFileWriter {
    fn append_line(&mut self, _line: &[u8]) -> Result<(), AuditSinkError> {
        Err(AuditSinkError::UnsupportedPlatform)
    }

    fn sync(&mut self) -> Result<(), AuditSinkError> {
        Err(AuditSinkError::UnsupportedPlatform)
    }
}

/// The bounded ring plus its writer and the fail-closed admission gate.
pub struct AuditSink<W: AuditWriter> {
    ring: AuditRing,
    writer: W,
    retry: VecDeque<AuditRecord>,
    last_written: Option<AuditEventId>,
    degraded: bool,
}

impl<W: AuditWriter> AuditSink<W> {
    /// Build an empty sink for one daemon instance.
    #[must_use]
    pub fn new(instance_id: DaemonInstanceId, limits: &Limits, writer: W) -> Self {
        Self {
            ring: AuditRing::new(instance_id, limits),
            writer,
            retry: VecDeque::new(),
            last_written: None,
            degraded: false,
        }
    }

    /// Whether the sink is currently degraded.
    ///
    /// A degraded sink refuses every privileged mutation with
    /// `Denied/Internal` until a flush succeeds again.
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// The stable event id of the last record handed to the writer.
    #[must_use]
    pub const fn last_written(&self) -> Option<AuditEventId> {
        self.last_written
    }

    /// The ring counters plus the sink's own pending count.
    #[must_use]
    pub fn counters(&self) -> AuditCounters {
        self.ring.counters()
    }

    /// Records still waiting for the writer (retry buffer only).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.retry.len() + self.ring.len()
    }

    /// The staging ring.
    #[must_use]
    pub const fn ring(&self) -> &AuditRing {
        &self.ring
    }

    /// Admit one privileged mutation.
    ///
    /// Flushes every pending record and syncs the sink. A failure marks the
    /// sink degraded and returns the error; the caller must refuse the action
    /// with `Denied/Internal` and change no state.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`AuditSinkError`] when the sink cannot be
    /// written or synced.
    pub fn admit_mutation(&mut self) -> Result<(), AuditSinkError> {
        self.flush()?;
        let synced = self.writer.sync();
        if synced.is_err() {
            self.degraded = true;
        }
        synced
    }

    /// Append one record and flush it.
    ///
    /// # Errors
    ///
    /// [`AuditSinkError::EventIdExhausted`] when the ring cannot assign an id;
    /// any writer failure for the flush. The record stays queued in event-id
    /// order on a writer failure.
    pub fn record(
        &mut self,
        entry: crate::audit::AuditEntry,
        now: MonotonicMs,
    ) -> Result<AuditEventId, AuditSinkError> {
        let event_id = self
            .ring
            .record(entry, now)
            .map_err(|_| AuditSinkError::EventIdExhausted)?;
        self.flush()?;
        Ok(event_id)
    }

    /// Drain and write pending records in event-id order.
    fn flush(&mut self) -> Result<(), AuditSinkError> {
        loop {
            if self.retry.is_empty() {
                self.retry.extend(self.ring.drain(FLUSH_BATCH));
            }
            while let Some(record) = self.retry.front().copied() {
                let line = render_record(&record)?;
                match self.writer.append_line(&line) {
                    Ok(()) => {
                        self.retry.pop_front();
                        self.last_written = Some(record.event_id);
                    }
                    Err(error) => {
                        self.degraded = true;
                        return Err(error);
                    }
                }
            }
            if self.ring.is_empty() {
                self.degraded = false;
                return Ok(());
            }
        }
    }
}

impl<W: AuditWriter + fmt::Debug> fmt::Debug for AuditSink<W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditSink")
            .field("ring", &self.ring)
            .field("pending", &self.pending())
            .field("degraded", &self.degraded)
            .finish_non_exhaustive()
    }
}

/// Render one record as a bounded, content-free line.
///
/// # Errors
///
/// [`AuditSinkError::RecordTooLarge`] when the rendered line exceeds
/// [`MAX_RENDERED_LINE_BYTES`]. This is a structural defect, never caller
/// input, because every field is a closed spelling, id, or number.
pub fn render_record(record: &AuditRecord) -> Result<Vec<u8>, AuditSinkError> {
    let mut line = String::with_capacity(MAX_RENDERED_LINE_BYTES);
    line.push_str("v1");
    push_field(&mut line, "ev", &record.event_id.to_string());
    push_field(&mut line, "t", &record.at_ms.as_millis().to_string());
    push_field(&mut line, "i", &record.daemon_instance_id.as_hex());
    push_field(&mut line, "a", record.action.as_str());
    match record.result {
        AuditResult::Ok => push_field(&mut line, "r", "ok"),
        AuditResult::Refused(code) => {
            push_field(&mut line, "r", "refused");
            push_field(&mut line, "c", code.as_str());
        }
    }
    if let Some(session) = record.session {
        push_field(&mut line, "s", &session.session_id.as_hex());
        push_field(&mut line, "g", &session.generation.to_string());
        push_field(&mut line, "rv", &session.state_revision.to_string());
    }
    if let Some(actor) = record.actor {
        if let Some(uid) = actor.peer_uid {
            push_field(&mut line, "u", &uid.to_string());
        }
        if let Some(pid) = actor.peer_pid {
            push_field(&mut line, "p", &pid.to_string());
        }
        if let Some(client) = actor.client_id {
            push_field(&mut line, "cid", &client.to_string());
        }
        if let Some(digest) = actor.client_correlation_digest {
            push_field(&mut line, "x", &crate::handshake::hex(&digest));
        }
    }
    push_field(&mut line, "k", target_kind(record.target.kind()));
    push_field(&mut line, "d", &crate::handshake::hex(&record.target.id()));
    if let Some(counts) = record.counts {
        push_field(&mut line, "ca", &counts.actual.to_string());
        push_field(&mut line, "cl", &counts.limit.to_string());
        if let Some(limit) = counts.limit_id {
            push_field(&mut line, "m", limit.name());
        }
    }
    if let Some(terminal) = record.terminal {
        push_terminal(&mut line, terminal);
    }
    if line.len() > MAX_RENDERED_LINE_BYTES {
        return Err(AuditSinkError::RecordTooLarge);
    }
    Ok(line.into_bytes())
}

fn push_field(line: &mut String, key: &str, value: &str) {
    if !line.is_empty() {
        line.push(' ');
    }
    line.push_str(key);
    line.push('=');
    line.push_str(value);
}

fn push_terminal(line: &mut String, terminal: AuditTerminal) {
    push_field(line, "z", terminal.state.as_str());
    push_field(line, "zr", terminal.reason.as_str());
    match terminal.exit {
        ExitInformation::ExitCode(code) => push_field(line, "zc", &code.to_string()),
        ExitInformation::Signal(signal) => push_field(line, "zs", &signal.to_string()),
        ExitInformation::NotApplicable => {}
    }
    push_field(line, "zk", cleanup(terminal.cleanup));
}

const fn cleanup(class: CleanupClass) -> &'static str {
    class.as_str()
}

const fn target_kind(kind: crate::audit::AuditTargetKind) -> &'static str {
    use crate::audit::AuditTargetKind;
    match kind {
        AuditTargetKind::None => "none",
        AuditTargetKind::Session => "session",
        AuditTargetKind::Attach => "attach",
        AuditTargetKind::Grant => "grant",
        AuditTargetKind::TerminalDigest => "terminal_digest",
    }
}

/// The default `$XDG_STATE_HOME/bitty/audit` directory.
///
/// Returns `None` when neither `XDG_STATE_HOME` nor `HOME` is usable; the
/// daemon then refuses to start rather than run with an unwritten sink.
#[must_use]
pub fn default_audit_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    if !state.is_absolute() {
        return None;
    }
    Some(state.join(STATE_SUBDIR).join(AUDIT_SUBDIR))
}

/// The static refusal a degraded sink produces for a privileged mutation.
#[must_use]
pub const fn degraded_refusal() -> RefusalCode {
    RefusalCode::Internal
}

/// Re-exported for callers that report the terminal spelling.
#[must_use]
pub const fn lifecycle_spelling(state: LifecycleState) -> &'static str {
    state.as_str()
}

/// Re-exported for callers that report the terminal reason.
#[must_use]
pub const fn reason_spelling(reason: TerminalReason) -> &'static str {
    reason.as_str()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::audit::{
        AuditAction, AuditActor, AuditCounts, AuditEntry, AuditSession, AuditTarget,
    };
    use crate::limits::LimitOverride;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    struct FailingWriter {
        fail: bool,
        lines: Vec<Vec<u8>>,
    }

    impl AuditWriter for FailingWriter {
        fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError> {
            if self.fail {
                return Err(AuditSinkError::Io);
            }
            self.lines.push(line.to_vec());
            Ok(())
        }

        fn sync(&mut self) -> Result<(), AuditSinkError> {
            if self.fail {
                return Err(AuditSinkError::Io);
            }
            Ok(())
        }
    }

    fn limits_with(audit_file_bytes: u64) -> Limits {
        Limits::new(&[LimitOverride::new(
            LimitId::AuditFileBytes,
            audit_file_bytes,
        )])
        .expect("valid override")
    }

    fn entry(action: AuditAction) -> AuditEntry {
        AuditEntry::new(action)
    }

    fn instance() -> DaemonInstanceId {
        DaemonInstanceId::generate().expect("entropy")
    }

    #[test]
    fn a_degraded_sink_refuses_admission_and_changes_nothing() {
        let mut sink = AuditSink::new(
            instance(),
            &Limits::default(),
            FailingWriter {
                fail: true,
                lines: Vec::new(),
            },
        );
        let error = sink.admit_mutation().expect_err("sink is broken");
        assert_eq!(error, AuditSinkError::Io);
        assert!(sink.is_degraded());
        assert!(sink.pending() == 0);

        let record = sink
            .record(entry(AuditAction::Create), MonotonicMs::from_millis(1))
            .expect_err("record append also fails");
        assert_eq!(record, AuditSinkError::Io);
        assert_eq!(sink.pending(), 1, "the record is retained for retry");
        assert!(sink.is_degraded());
    }

    #[test]
    fn a_retried_record_is_written_once_in_event_id_order() {
        let mut sink = AuditSink::new(
            instance(),
            &Limits::default(),
            FailingWriter {
                fail: true,
                lines: Vec::new(),
            },
        );
        sink.record(entry(AuditAction::Create), MonotonicMs::from_millis(1))
            .expect_err("writer fails");
        assert!(sink.is_degraded());
        assert_eq!(sink.pending(), 1, "the record is retained, not dropped");
        assert_eq!(sink.last_written(), None);

        sink.writer.fail = false;
        sink.admit_mutation().expect("writer recovers");
        assert!(!sink.is_degraded());
        assert_eq!(sink.pending(), 0);
        assert_eq!(
            sink.last_written().map(AuditEventId::get),
            Some(1),
            "the first record keeps its original event id"
        );
        assert_eq!(sink.writer.lines.len(), 1, "written exactly once");
        let line = String::from_utf8(sink.writer.lines[0].clone()).expect("utf-8");
        assert!(line.contains("ev=1 "), "{line}");
        assert!(line.contains("a=session.create"), "{line}");
    }

    #[test]
    fn recovery_clears_degraded_and_allows_the_next_mutation() {
        let mut sink = AuditSink::new(
            instance(),
            &Limits::default(),
            FailingWriter {
                fail: true,
                lines: Vec::new(),
            },
        );
        sink.admit_mutation().expect_err("broken");
        assert!(sink.is_degraded());
        sink.writer.fail = false;
        sink.admit_mutation().expect("recovered");
        assert!(!sink.is_degraded());
    }

    #[test]
    fn rendered_lines_are_bounded_and_carry_no_caller_bytes() {
        let mut entry = AuditEntry::new(AuditAction::Transfer)
            .result(AuditResult::Refused(RefusalCode::StaleGeneration));
        entry = entry
            .session(AuditSession {
                session_id: crate::registry::SessionId::parse(&"f".repeat(32)).expect("session id"),
                generation: u64::MAX,
                state_revision: u64::MAX,
            })
            .actor(AuditActor {
                peer_uid: Some(u32::MAX),
                peer_sid: None,
                peer_pid: Some(u32::MAX),
                client_id: Some(u64::MAX),
                client_correlation_digest: Some([0xab; 16]),
            })
            .target(AuditTarget::attach([0xcd; 16]))
            .counts(AuditCounts::new(
                u64::MAX,
                u64::MAX,
                Some(LimitId::MaxSessions),
            ));
        let mut ring = AuditRing::new(instance(), &Limits::default());
        ring.record(entry, MonotonicMs::from_millis(u64::MAX))
            .expect("append");
        let record = ring.oldest().expect("record");
        let line = render_record(record).expect("renders");
        assert!(
            line.len() <= MAX_RENDERED_LINE_BYTES,
            "{} bytes",
            line.len()
        );
        let text = String::from_utf8(line).expect("utf-8");
        assert!(text.starts_with("v1 ev="));
        assert!(text.contains("a=session.control.transfer"));
        assert!(text.contains("r=refused"));
        assert!(text.contains("c=stale_generation"));
        assert!(text.contains("k=attach"));
        assert!(text.contains("m=max_sessions"));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn file_sink_is_private_bounded_and_rotating() {
        let dir = tempfile::tempdir().expect("temp dir");
        let audit_dir = dir.path().join("audit");
        let limits = limits_with(4_096);
        let instance = instance();
        let writer = AuditFileWriter::open(&audit_dir, &limits).expect("open");
        assert_eq!(
            std::fs::symlink_metadata(&audit_dir).expect("dir").mode() & 0o777,
            AUDIT_DIR_MODE
        );
        assert_eq!(
            std::fs::symlink_metadata(audit_dir.join(ACTIVE_SEGMENT))
                .expect("active")
                .mode()
                & 0o777,
            SEGMENT_MODE
        );
        let mut sink = AuditSink::new(instance, &limits, writer);
        for index in 0..400u64 {
            sink.record(
                AuditEntry::new(AuditAction::Status),
                MonotonicMs::from_millis(index),
            )
            .expect("append");
        }
        sink.admit_mutation().expect("flush");
        assert!(!sink.is_degraded());
        let mut total = 0u64;
        let mut segments = 0;
        for entry in std::fs::read_dir(&audit_dir).expect("read dir") {
            let path = entry.expect("entry").path();
            let metadata = std::fs::symlink_metadata(&path).expect("metadata");
            assert_eq!(metadata.mode() & 0o777, SEGMENT_MODE);
            segments += 1;
            total += metadata.len();
        }
        assert!(segments <= 2, "at most audit_segments files: {segments}");
        assert!(
            total <= 4_096 * 2 + MAX_RENDERED_LINE_BYTES as u64,
            "bounded by segments plus one line: {total}"
        );
    }

    #[test]
    fn default_audit_dir_is_xdg_compliant() {
        // The helper reads process environment; only its shape is asserted
        // here, and the explicit-directory paths above carry the behavior.
        if let Some(path) = default_audit_dir() {
            assert!(path.is_absolute());
            assert!(path.ends_with("bitty/audit"));
        }
    }

    #[test]
    fn degraded_refusal_is_the_static_internal_code() {
        assert_eq!(degraded_refusal(), RefusalCode::Internal);
        assert_eq!(lifecycle_spelling(LifecycleState::Created), "created");
        assert_eq!(reason_spelling(TerminalReason::DaemonStop), "daemon_stop");
    }
}
