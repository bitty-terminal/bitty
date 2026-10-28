//! Bounded, owner-private orphan journal (contract `D-06`).
//!
//! The journal is written before a worker can be observed and never contains
//! argv, environment, working directory, terminal content, or credentials. It
//! stores exactly the fencing tuple: `session_id`, `generation`, boot id, the
//! reserved-at time, the containment identity, and — after the spawn returns —
//! the pid, process start time, process group, and uid.
//!
//! Two phases keep a crash on either side of the spawn defined:
//!
//! 1. [`OrphanJournal::reserve`] creates and records the containment identity,
//!    then writes and fsyncs a `reserved` record before the spawn syscall.
//! 2. [`OrphanJournal::commit`] fills the exact process identity and fsyncs it
//!    again before the worker is exposed as bound.
//!
//! A record is removed after a verified sweep, when its boot id is stale,
//! after quarantine retention expires, or by explicit operator retirement
//! (which never signals). Classification compares the boot id first: a
//! different boot id is unconditionally stale. Within the current boot, a
//! cgroup record is actionable through its recorded subtree even without a
//! pid; a subreaper record needs the exact process tuple; and an unreadable,
//! absent, reused, or mismatched subreaper record is quarantined. Only a
//! cgroup reaper can return [`ReapResult::Swept`].
//!
//! # Still deferred
//!
//! The executable path digest and a native Windows job-object implementation
//! remain out of this slice; the journal carries no argv or executable path.
//! On non-Linux cells the boot id and `/proc` probe are unavailable, so every
//! record is quarantined; Unix enforces `0700`/`0600` here, and the Windows
//! owner-only runtime directory ACL is owned by the endpoint slice.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::containment::ContainmentRecord;
use crate::limits::{LimitId, Limits};
use crate::registry::{MonotonicMs, SessionId};

use super::identity::{
    BootId, Pid, ProcessGroupId, ProcessIdentity, ProcessObservation, ProcessProbe,
    ProcessStartTime, VerifiedOrphan,
};

/// Compiled ceiling for journal records (`D-14` `max_orphan_records`).
pub const MAX_ORPHAN_RECORDS_SYSTEM_MAX: usize =
    LimitId::MaxOrphanRecords.system_max_value() as usize;

/// Compiled ceiling for one journal record (`D-14` `orphan_record_bytes`).
pub const ORPHAN_RECORD_BYTES_SYSTEM_MAX: usize =
    LimitId::OrphanRecordBytes.system_max_value() as usize;

/// Default quarantine retention (`D-14` `orphan_record_retention_ms`).
pub const ORPHAN_RECORD_RETENTION_MS_DEFAULT: u64 =
    LimitId::OrphanRecordRetentionMs.default_value();

/// Compiled ceiling for quarantine retention (`D-14`
/// `orphan_record_retention_ms` system maximum).
pub const ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX: u64 =
    LimitId::OrphanRecordRetentionMs.system_max_value();

const JOURNAL_HEADER: &str = "bittyd-orphan 2";
const LEGACY_JOURNAL_HEADER: &str = "bittyd-orphan 1";
const MAX_RECORDED_CGROUP_PATH_BYTES: usize = 1024;
const RECORD_SUFFIX: &str = ".orphan";
const TEMP_PREFIX: &str = ".tmp-";

/// Enforced journal bounds for one daemon instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrphanJournalLimits {
    max_records: usize,
    record_bytes: usize,
    quarantine_retention_ms: u64,
}

impl OrphanJournalLimits {
    /// Validate and build journal limits.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::InvalidLimits`] when a value exceeds its
    /// compiled system maximum.
    pub fn new(
        max_records: usize,
        record_bytes: usize,
        quarantine_retention_ms: u64,
    ) -> Result<Self, JournalError> {
        if max_records > MAX_ORPHAN_RECORDS_SYSTEM_MAX {
            return Err(JournalError::InvalidLimits {
                limit: "max_orphan_records",
                value: u64::try_from(max_records).unwrap_or(u64::MAX),
                maximum: u64::try_from(MAX_ORPHAN_RECORDS_SYSTEM_MAX).unwrap_or(u64::MAX),
            });
        }
        if record_bytes > ORPHAN_RECORD_BYTES_SYSTEM_MAX {
            return Err(JournalError::InvalidLimits {
                limit: "orphan_record_bytes",
                value: u64::try_from(record_bytes).unwrap_or(u64::MAX),
                maximum: u64::try_from(ORPHAN_RECORD_BYTES_SYSTEM_MAX).unwrap_or(u64::MAX),
            });
        }
        if quarantine_retention_ms > ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX {
            return Err(JournalError::InvalidLimits {
                limit: "orphan_record_retention_ms",
                value: quarantine_retention_ms,
                maximum: ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX,
            });
        }
        Ok(Self {
            max_records,
            record_bytes,
            quarantine_retention_ms,
        })
    }

    /// Maximum records in the journal.
    #[must_use]
    pub const fn max_records(self) -> usize {
        self.max_records
    }

    /// Maximum encoded bytes in one record.
    #[must_use]
    pub const fn record_bytes(self) -> usize {
        self.record_bytes
    }

    /// Quarantine retention in milliseconds.
    #[must_use]
    pub const fn quarantine_retention_ms(self) -> u64 {
        self.quarantine_retention_ms
    }

    /// Derive the journal bounds from the effective advertised limits.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            max_records: limits.effective_usize(LimitId::MaxOrphanRecords),
            record_bytes: limits.effective_usize(LimitId::OrphanRecordBytes),
            quarantine_retention_ms: limits.effective(LimitId::OrphanRecordRetentionMs),
        }
    }
}

impl Default for OrphanJournalLimits {
    fn default() -> Self {
        Self {
            max_records: MAX_ORPHAN_RECORDS_SYSTEM_MAX,
            record_bytes: ORPHAN_RECORD_BYTES_SYSTEM_MAX,
            quarantine_retention_ms: ORPHAN_RECORD_RETENTION_MS_DEFAULT,
        }
    }
}

/// A journal failure. A failed mutation leaves the previous on-disk record
/// intact (writes go through a synced temporary file and a rename).
#[derive(Debug)]
pub enum JournalError {
    /// Filesystem failure.
    Io(io::Error),
    /// The journal already holds `max_records` records.
    LimitExceeded {
        /// The enforced bound.
        limit: usize,
    },
    /// One encoded record exceeds `orphan_record_bytes`.
    RecordTooLarge {
        /// Encoded length.
        bytes: usize,
        /// The enforced bound.
        limit: usize,
    },
    /// A commit arrived for a record that is not `reserved`.
    NotReserved,
    /// The session already has a journal record.
    DuplicateRecord,
    /// No journal record exists for the session.
    UnknownRecord,
    /// The record is not quarantined, so operator retirement does not apply.
    NotQuarantined,
    /// The path exists but is not a directory.
    NotADirectory,
    /// The directory or record file is readable or writable by group or others.
    InsecurePermissions {
        /// Observed permission bits (mode and `0o777`).
        mode: u32,
    },
    /// A record could not be encoded because its required facts are missing.
    IncompleteRecord,
    /// A configured bound exceeds the reviewed system maximum.
    InvalidLimits {
        /// Limit name.
        limit: &'static str,
        /// Requested value.
        value: u64,
        /// Compiled ceiling.
        maximum: u64,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "orphan journal I/O error: {error}"),
            Self::LimitExceeded { limit } => {
                write!(formatter, "orphan journal holds {limit} records")
            }
            Self::RecordTooLarge { bytes, limit } => {
                write!(
                    formatter,
                    "orphan record is {bytes} bytes, above limit {limit}"
                )
            }
            Self::NotReserved => formatter.write_str("orphan record is not reserved"),
            Self::DuplicateRecord => formatter.write_str("orphan record already exists"),
            Self::UnknownRecord => formatter.write_str("orphan record does not exist"),
            Self::NotQuarantined => formatter.write_str("orphan record is not quarantined"),
            Self::NotADirectory => formatter.write_str("orphan journal path is not a directory"),
            Self::InsecurePermissions { mode } => {
                write!(
                    formatter,
                    "orphan journal permissions {mode:o} are not owner-private"
                )
            }
            Self::IncompleteRecord => {
                formatter.write_str("orphan record is missing required facts")
            }
            Self::InvalidLimits {
                limit,
                value,
                maximum,
            } => write!(
                formatter,
                "limit {limit} is {value}, above compiled maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Journal counts by persisted class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanCounts {
    /// Reserved before spawn, no pid committed.
    pub reserved: usize,
    /// Committed with an exact process identity.
    pub committed: usize,
    /// Quarantined: never signalled, retained until retirement or expiry.
    pub unverified: usize,
}

impl OrphanCounts {
    /// Every record in the journal.
    #[must_use]
    pub const fn total(self) -> usize {
        self.reserved + self.committed + self.unverified
    }
}

/// The outcome of a verified sweep attempt, reported by an
/// [`OrphanReaper`] implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapResult {
    /// The whole containment identity was terminated and verified gone.
    Swept,
    /// The reaper could not verify the whole identity.
    Unverified,
}

/// The signalling seam for exact-identity orphans.
///
/// The journal calls this only with a [`VerifiedOrphan`], which can only be
/// built after a boot-id match and either a recorded cgroup subtree or an exact
/// boot-id, pid, start-time, process-group, and uid match. The implementation
/// (bitty#2791) terminates the *containment identity*, applies the
/// graceful-then-kill bound, verifies the identity empty, and returns
/// [`ReapResult::Swept`] only when that verification succeeds. It must never
/// fall back to a pid-only kill.
pub trait OrphanReaper {
    /// Terminate and verify one matched orphan.
    fn reap(&mut self, orphan: &VerifiedOrphan) -> ReapResult;
}

/// One reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Records removed after a verified sweep.
    pub reaped: usize,
    /// Records quarantined as unverified.
    pub unverified: usize,
    /// Records from another boot id, discarded without a signal.
    pub stale: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanClass {
    Reserved,
    Committed,
    Unverified,
}

impl OrphanClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Committed => "committed",
            Self::Unverified => "unverified",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "committed" => Some(Self::Committed),
            "unverified" => Some(Self::Unverified),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct OrphanRecord {
    session_id: SessionId,
    generation: u64,
    class: OrphanClass,
    boot_id: Option<BootId>,
    reserved_at: MonotonicMs,
    validated_at: Option<MonotonicMs>,
    identity: Option<ProcessIdentity>,
    containment: Option<ContainmentRecord>,
}

impl OrphanRecord {
    fn reserved(
        session_id: SessionId,
        generation: u64,
        boot_id: BootId,
        now: MonotonicMs,
        containment: ContainmentRecord,
    ) -> Self {
        Self {
            session_id,
            generation,
            class: OrphanClass::Reserved,
            boot_id: Some(boot_id),
            reserved_at: now,
            validated_at: None,
            identity: None,
            containment: Some(containment),
        }
    }

    fn placeholder(session_id: SessionId) -> Self {
        Self {
            session_id,
            generation: 0,
            class: OrphanClass::Unverified,
            boot_id: None,
            reserved_at: MonotonicMs::ZERO,
            validated_at: Some(MonotonicMs::ZERO),
            identity: None,
            containment: None,
        }
    }

    fn encode(&self) -> Option<String> {
        let boot_id = self.boot_id.as_ref()?;
        let pid = optional_number(
            self.identity
                .map(|identity| u64::from(identity.pid().get())),
        );
        let start_time = optional_number(self.identity.map(|identity| identity.start_time().get()));
        let process_group = optional_number(
            self.identity
                .map(|identity| u64::from(identity.process_group().get())),
        );
        let uid = optional_number(self.identity.map(|identity| u64::from(identity.uid())));
        let containment = containment_field(self.containment.as_ref())?;
        let validated_at = optional_number(self.validated_at.map(MonotonicMs::as_millis));
        Some(format!(
            "{JOURNAL_HEADER}\nsession_id {}\ngeneration {}\nclass {}\nboot_id {boot_id}\nreserved_at_ms {}\nvalidated_at_ms {validated_at}\ncontainment {containment}\npid {pid}\nstart_time {start_time}\nprocess_group {process_group}\nuid {uid}\n",
            self.session_id,
            self.generation,
            self.class.as_str(),
            self.reserved_at.as_millis(),
        ))
    }

    /// Worst-case encoded length for this record across every valid process
    /// identity, the committed state, and a quarantine timestamp.
    ///
    /// `reserve` uses this to reject a byte bound that admits the reserved
    /// encoding but not the post-spawn commit (or a later quarantine rewrite).
    /// Every reservation that succeeds can therefore commit any identity that
    /// fits `Pid`, `ProcessStartTime`, and `ProcessGroupId`.
    fn worst_case_encoded_bytes(&self) -> usize {
        let identity = ProcessIdentity::new(
            Pid::new(u32::MAX).expect("u32::MAX is a valid pid"),
            ProcessStartTime::new(u64::MAX),
            ProcessGroupId::new(u32::MAX).expect("u32::MAX is a valid process group"),
            u32::MAX,
        );
        let mut worst_case = self.clone();
        worst_case.class = OrphanClass::Unverified;
        worst_case.identity = Some(identity);
        worst_case.validated_at = Some(MonotonicMs::from_millis(u64::MAX));
        if worst_case.containment.is_none() {
            worst_case.containment = Some(ContainmentRecord::cgroup(
                "x".repeat(MAX_RECORDED_CGROUP_PATH_BYTES),
            ));
        }
        worst_case
            .encode()
            .map_or(usize::MAX, |encoded| encoded.len())
    }

    fn parse(session_id: SessionId, text: &str) -> Self {
        Self::parse_valid(session_id, text).unwrap_or_else(|| Self::placeholder(session_id))
    }

    fn parse_valid(session_id: SessionId, text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let header = lines.next()?;
        let legacy = header == LEGACY_JOURNAL_HEADER;
        if !legacy && header != JOURNAL_HEADER {
            return None;
        }
        let encoded_session = field(lines.next()?, "session_id")?;
        if encoded_session.parse::<SessionId>().ok()? != session_id {
            return None;
        }
        let generation = field(lines.next()?, "generation")?.parse().ok()?;
        let class = OrphanClass::parse(field(lines.next()?, "class")?)?;
        let boot_id = BootId::parse(field(lines.next()?, "boot_id")?).ok()?;
        let reserved_at =
            MonotonicMs::from_millis(field(lines.next()?, "reserved_at_ms")?.parse().ok()?);
        let mut validated_at = parse_optional_ms(field(lines.next()?, "validated_at_ms")?)?;
        if class == OrphanClass::Unverified && validated_at.is_none() {
            validated_at = Some(MonotonicMs::ZERO);
        }

        let (containment, identity) = if legacy {
            // A version 1 record predates the uid and containment identity and
            // is therefore unverifiable by construction. It is loaded as a
            // quarantined record, never signalled.
            let _pid = field(lines.next()?, "pid")?;
            let _start_time = field(lines.next()?, "start_time")?;
            let _process_group = field(lines.next()?, "process_group")?;
            if lines.next().is_some() {
                return None;
            }
            (None, None)
        } else {
            let containment = parse_containment(field(lines.next()?, "containment")?)?;
            let pid = field(lines.next()?, "pid")?;
            let start_time = field(lines.next()?, "start_time")?;
            let process_group = field(lines.next()?, "process_group")?;
            let uid = field(lines.next()?, "uid")?;
            if lines.next().is_some() {
                return None;
            }
            (
                containment,
                parse_identity(pid, start_time, process_group, uid)?,
            )
        };

        if !legacy {
            match (class, identity) {
                (OrphanClass::Reserved, Some(_)) => return None,
                (OrphanClass::Committed, None) => return None,
                _ => {}
            }
        }

        Some(Self {
            session_id,
            generation,
            class,
            boot_id: Some(boot_id),
            reserved_at,
            validated_at,
            identity,
            containment,
        })
    }
}

fn containment_field(containment: Option<&ContainmentRecord>) -> Option<String> {
    match containment {
        None => Some("-".to_owned()),
        Some(ContainmentRecord::Subreaper) => Some("subreaper".to_owned()),
        Some(ContainmentRecord::Cgroup { path }) => {
            let text = path.to_str()?;
            if !valid_recorded_path(text) {
                return None;
            }
            Some(format!("cgroup {text}"))
        }
    }
}

fn parse_containment(value: &str) -> Option<Option<ContainmentRecord>> {
    if value == "-" {
        return Some(None);
    }
    if value == "subreaper" {
        return Some(Some(ContainmentRecord::Subreaper));
    }
    let path = value.strip_prefix("cgroup ")?;
    if !valid_recorded_path(path) {
        return None;
    }
    Some(Some(ContainmentRecord::cgroup(path)))
}

fn valid_recorded_path(path: &str) -> bool {
    let path = Path::new(path);
    // `has_root` not `is_absolute`: a recorded cgroup path is a Linux path,
    // and this parser also runs on the non-served cells where a leading `/`
    // has a root but no Windows drive prefix.
    path.has_root()
        && path.as_os_str().len() <= MAX_RECORDED_CGROUP_PATH_BYTES
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        && !path
            .to_string_lossy()
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

/// Read boot-relative milliseconds from `/proc/uptime`.
///
/// The cgroup/orphan path is Linux-only for serving, but the journal type also
/// compiles on the refusing cells; there the origin is zero and retention still
/// works for tests that inject `now`.
fn boot_uptime_ms() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let Ok(text) = fs::read_to_string("/proc/uptime") else {
            return 0;
        };
        let Some(seconds) = text.split_whitespace().next() else {
            return 0;
        };
        let Ok(seconds) = seconds.parse::<f64>() else {
            return 0;
        };
        if !seconds.is_finite() || seconds < 0.0 {
            return 0;
        }
        // `/proc/uptime` has centisecond resolution and is stable across
        // process restarts within one boot.
        (seconds * 1000.0).round() as u64
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        0
    }
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |number| number.to_string())
}

fn parse_optional_ms(value: &str) -> Option<Option<MonotonicMs>> {
    if value == "-" {
        return Some(None);
    }
    value
        .parse()
        .ok()
        .map(|millis| Some(MonotonicMs::from_millis(millis)))
}

fn parse_identity(
    pid: &str,
    start_time: &str,
    process_group: &str,
    uid: &str,
) -> Option<Option<ProcessIdentity>> {
    if pid == "-" && start_time == "-" && process_group == "-" && uid == "-" {
        return Some(None);
    }
    let pid = Pid::new(pid.parse().ok()?)?;
    let start_time = ProcessStartTime::new(start_time.parse().ok()?);
    let process_group = ProcessGroupId::new(process_group.parse().ok()?)?;
    let uid = uid.parse().ok()?;
    Some(Some(ProcessIdentity::new(
        pid,
        start_time,
        process_group,
        uid,
    )))
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?.strip_prefix(' ')
}

enum Disposition {
    Exact(VerifiedOrphan),
    Unverified,
    Stale,
}

/// The orphan journal directory.
///
/// One file per session (`<session_id>.orphan`, mode `0600`) inside an
/// owner-private directory (`0700`). Each write goes to a synced temporary
/// file and is renamed into place, so a crash leaves either the previous
/// record or the complete new one, never a partially written identity.
///
/// The caller owns the runtime root and passes this directory path; the
/// journal checks Unix mode bits but not the owner uid. The endpoint slice owns
/// the runtime root ownership, and the Windows current-user-only DACL is part
/// of the designed named-pipe transport.
pub struct OrphanJournal {
    directory: PathBuf,
    limits: OrphanJournalLimits,
    boot_id: BootId,
    /// Boot-relative milliseconds at journal open. Quarantine timestamps are
    /// persisted in boot time, not the process-local [`MonotonicMs`] origin,
    /// so retention survives a daemon restart within the same boot.
    boot_origin_ms: u64,
    records: BTreeMap<SessionId, OrphanRecord>,
}

impl fmt::Debug for OrphanJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrphanJournal")
            .field("directory", &self.directory)
            .field("limits", &self.limits)
            .field("records", &self.records.len())
            .finish()
    }
}

impl OrphanJournal {
    /// Open (creating if absent) the journal directory and load its records.
    ///
    /// `boot_id` is the current cell's boot identity; Linux callers use
    /// [`BootId::read_system`] through [`crate::orphan::OrphanSupervisor`].
    ///
    /// # Errors
    ///
    /// Refuses when the path is not a directory, the directory or a record file
    /// is not owner-private on Unix, the on-disk record count exceeds the
    /// bound, or the filesystem cannot be read.
    pub fn open(
        directory: impl Into<PathBuf>,
        limits: OrphanJournalLimits,
        boot_id: BootId,
    ) -> Result<Self, JournalError> {
        Self::open_with_origin(directory, limits, boot_id, boot_uptime_ms())
    }

    fn open_with_origin(
        directory: impl Into<PathBuf>,
        limits: OrphanJournalLimits,
        boot_id: BootId,
        boot_origin_ms: u64,
    ) -> Result<Self, JournalError> {
        let directory = directory.into();
        ensure_private_directory(&directory)?;
        let records = load_records(&directory, limits)?;
        Ok(Self {
            directory,
            limits,
            boot_id,
            boot_origin_ms,
            records,
        })
    }

    /// Map a process-local timestamp into boot time for persisted retention.
    fn boot_time(&self, now: MonotonicMs) -> u64 {
        self.boot_origin_ms.saturating_add(now.as_millis())
    }

    /// The enforced bounds.
    #[must_use]
    pub const fn limits(&self) -> OrphanJournalLimits {
        self.limits
    }

    /// Records by persisted class.
    #[must_use]
    pub fn counts(&self) -> OrphanCounts {
        let mut counts = OrphanCounts::default();
        for record in self.records.values() {
            match record.class {
                OrphanClass::Reserved => counts.reserved += 1,
                OrphanClass::Committed => counts.committed += 1,
                OrphanClass::Unverified => counts.unverified += 1,
            }
        }
        counts
    }

    /// Whether a record exists for `session_id`.
    #[must_use]
    pub fn contains(&self, session_id: SessionId) -> bool {
        self.records.contains_key(&session_id)
    }

    /// Reserve and fsync a bounded record before the spawn syscall.
    ///
    /// # Errors
    ///
    /// Refuses when the session already has a record, the journal is full, or
    /// even the worst-case committed encoding (maximum pid, start-time,
    /// process-group, and uid widths, the recorded containment path, plus a
    /// later quarantine timestamp) exceeds `orphan_record_bytes`. Nothing is written on refusal, so a reservation
    /// never precedes a spawn unless `commit` can hold any valid identity.
    pub fn reserve(
        &mut self,
        session_id: SessionId,
        generation: u64,
        now: MonotonicMs,
        containment: ContainmentRecord,
    ) -> Result<(), JournalError> {
        if self.records.contains_key(&session_id) {
            return Err(JournalError::DuplicateRecord);
        }
        if self.records.len() >= self.limits.max_records {
            return Err(JournalError::LimitExceeded {
                limit: self.limits.max_records,
            });
        }

        let record = OrphanRecord::reserved(
            session_id,
            generation,
            self.boot_id.clone(),
            now,
            containment,
        );
        let worst_case = record.worst_case_encoded_bytes();
        if worst_case > self.limits.record_bytes {
            return Err(JournalError::RecordTooLarge {
                bytes: worst_case,
                limit: self.limits.record_bytes,
            });
        }
        self.store(&record)?;
        self.records.insert(session_id, record);
        Ok(())
    }

    /// Commit the exact process identity and fsync the record after the spawn
    /// returns, before the worker can be exposed as bound.
    ///
    /// The caller passes the identity observed for the child under the current
    /// boot; the journal records it verbatim and the classification fences it
    /// against pid reuse and a reboot.
    ///
    /// # Errors
    ///
    /// Refuses an unknown or already committed record. The previous on-disk
    /// record is untouched when the write fails.
    pub fn commit(
        &mut self,
        session_id: SessionId,
        identity: ProcessIdentity,
    ) -> Result<(), JournalError> {
        let updated = {
            let record = self
                .records
                .get(&session_id)
                .ok_or(JournalError::UnknownRecord)?;
            if record.class != OrphanClass::Reserved {
                return Err(JournalError::NotReserved);
            }
            let mut updated = record.clone();
            updated.class = OrphanClass::Committed;
            updated.identity = Some(identity);
            updated
        };
        self.store(&updated)?;
        self.records.insert(session_id, updated);
        Ok(())
    }

    /// Classify every record and hand only exact matches to `reaper`.
    ///
    /// * different boot id -> stale, discarded, never signalled;
    /// * reserved (no pid), absent pid, unreadable start time, pid reuse, or a
    ///   process-group mismatch -> quarantined, never signalled;
    /// * exact identity -> `reaper`; a verified sweep removes the record, and
    ///   anything else stays quarantined.
    ///
    /// # Errors
    ///
    /// Propagates a filesystem failure while rewriting or removing a record.
    pub fn reap(
        &mut self,
        probe: &dyn ProcessProbe,
        reaper: &mut dyn OrphanReaper,
        now: MonotonicMs,
    ) -> Result<ReapReport, JournalError> {
        let probe_boot = probe.boot_id();
        let session_ids: Vec<SessionId> = self.records.keys().copied().collect();
        let mut report = ReapReport::default();

        for session_id in session_ids {
            let Some(record) = self.records.get(&session_id).cloned() else {
                continue;
            };
            match classify(&record, probe_boot.as_ref(), probe) {
                Disposition::Stale => {
                    self.remove(session_id)?;
                    report.stale += 1;
                }
                Disposition::Unverified => {
                    self.quarantine(record, now)?;
                    report.unverified += 1;
                }
                Disposition::Exact(verified) => match reaper.reap(&verified) {
                    ReapResult::Swept => {
                        self.remove(session_id)?;
                        report.reaped += 1;
                    }
                    ReapResult::Unverified => {
                        self.quarantine(record, now)?;
                        report.unverified += 1;
                    }
                },
            }
        }
        Ok(report)
    }

    /// Remove quarantined records whose retention has expired.
    ///
    /// Returns removed ids in journal order. A record with no quarantine time
    /// (a malformed file loaded as unverified) is quarantined from the origin
    /// and expires with its retention.
    ///
    /// # Errors
    ///
    /// Propagates a filesystem failure while removing a record.
    pub fn collect_expired(&mut self, now: MonotonicMs) -> Result<Vec<SessionId>, JournalError> {
        let retention = self.limits.quarantine_retention_ms;
        let boot_now = self.boot_time(now);
        let expired: Vec<SessionId> = self
            .records
            .values()
            .filter(|record| record.class == OrphanClass::Unverified)
            .filter(|record| {
                record.validated_at.is_some_and(|validated| {
                    boot_now.saturating_sub(validated.as_millis()) >= retention
                })
            })
            .map(|record| record.session_id)
            .collect();

        for session_id in &expired {
            self.remove(*session_id)?;
        }
        Ok(expired)
    }

    /// Operator retirement of a quarantined record: remove it, never signal.
    ///
    /// # Errors
    ///
    /// Refuses an unknown record or a record that is not quarantined.
    pub fn retire(&mut self, session_id: SessionId) -> Result<(), JournalError> {
        let record = self
            .records
            .get(&session_id)
            .ok_or(JournalError::UnknownRecord)?;
        if record.class != OrphanClass::Unverified {
            return Err(JournalError::NotQuarantined);
        }
        self.remove(session_id)
    }

    fn quarantine(
        &mut self,
        mut record: OrphanRecord,
        now: MonotonicMs,
    ) -> Result<(), JournalError> {
        record.class = OrphanClass::Unverified;
        if record.validated_at.is_none() {
            record.validated_at = Some(MonotonicMs::from_millis(self.boot_time(now)));
        }
        if record.boot_id.is_some() {
            self.store(&record)?;
        }
        self.records.insert(record.session_id, record);
        Ok(())
    }

    fn remove(&mut self, session_id: SessionId) -> Result<(), JournalError> {
        match fs::remove_file(self.record_path(session_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.records.remove(&session_id);
        sync_directory(&self.directory)?;
        Ok(())
    }

    fn store(&self, record: &OrphanRecord) -> Result<(), JournalError> {
        let encoded = record.encode().ok_or(JournalError::IncompleteRecord)?;
        if encoded.len() > self.limits.record_bytes {
            return Err(JournalError::RecordTooLarge {
                bytes: encoded.len(),
                limit: self.limits.record_bytes,
            });
        }

        let temporary = self
            .directory
            .join(format!("{TEMP_PREFIX}{}", record.session_id));
        // A crash can leave an incomplete temporary file; it is never a
        // record. Remove it before the create-new write so a same-user symlink
        // is never followed and a stale partial identity is never reused.
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        write_private_file(&temporary, encoded.as_bytes())?;
        fs::rename(temporary, self.record_path(record.session_id))?;
        sync_directory(&self.directory)?;
        Ok(())
    }

    fn record_path(&self, session_id: SessionId) -> PathBuf {
        self.directory.join(format!("{session_id}{RECORD_SUFFIX}"))
    }
}

fn classify(
    record: &OrphanRecord,
    probe_boot: Option<&BootId>,
    probe: &dyn ProcessProbe,
) -> Disposition {
    let Some(record_boot) = record.boot_id.as_ref() else {
        return Disposition::Unverified;
    };
    let Some(probe_boot) = probe_boot else {
        return Disposition::Unverified;
    };
    // D-06 makes a foreign boot id unconditionally stale; do this before the
    // quarantine check so a quarantined record cannot occupy a slot for the
    // retention window after a reboot.
    if record_boot != probe_boot {
        return Disposition::Stale;
    }
    if record.class == OrphanClass::Unverified {
        // Within the current boot, quarantine is a record class, not a licence
        // to guess: once a record is unverified it never causes another signal,
        // even if the identity later looks exact. Only a verified sweep,
        // retirement, or retention removes it.
        return Disposition::Unverified;
    }
    let Some(containment) = record.containment.clone() else {
        // A record without a containment identity (a version 1 record or a
        // malformed one) can never be verified or signalled.
        return Disposition::Unverified;
    };
    match containment {
        ContainmentRecord::Subreaper => {
            let Some(identity) = record.identity else {
                // A reserved subreaper record has no pre-spawn identity; a
                // crash before the first child was adopted is unverifiable.
                return Disposition::Unverified;
            };
            match probe.observe(identity.pid()) {
                ProcessObservation::Present {
                    start_time,
                    process_group,
                    uid,
                } if start_time == identity.start_time()
                    && process_group == identity.process_group()
                    && uid == identity.uid() =>
                {
                    Disposition::Exact(VerifiedOrphan::new(
                        record_boot.clone(),
                        Some(identity),
                        containment,
                    ))
                }
                ProcessObservation::Absent
                | ProcessObservation::Unreadable
                | ProcessObservation::Present { .. } => Disposition::Unverified,
            }
        }
        ContainmentRecord::Cgroup { .. } => {
            // A cgroup record is actionable through the pre-created subtree
            // even when the pid is gone or reused. The reaper never signals by
            // pid; it only passes the exact identity through for the record
            // when it still matches.
            let identity = record.identity.filter(|identity| {
                matches!(
                    probe.observe(identity.pid()),
                    ProcessObservation::Present {
                        start_time,
                        process_group,
                        uid,
                    } if start_time == identity.start_time()
                        && process_group == identity.process_group()
                        && uid == identity.uid()
                )
            });
            Disposition::Exact(VerifiedOrphan::new(
                record_boot.clone(),
                identity,
                containment,
            ))
        }
    }
}

fn load_records(
    directory: &Path,
    limits: OrphanJournalLimits,
) -> Result<BTreeMap<SessionId, OrphanRecord>, JournalError> {
    let mut records = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with(TEMP_PREFIX) {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        let Some(encoded_session) = name.strip_suffix(RECORD_SUFFIX) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(encoded_session) else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            continue;
        }
        check_private_file(&metadata)?;

        let size_bound = u64::try_from(limits.record_bytes).unwrap_or(u64::MAX);
        let record = if metadata.len() > size_bound {
            // Never read a record that cannot be within the bound; a corrupt
            // oversized file is evidence to quarantine, not data to load.
            OrphanRecord::placeholder(session_id)
        } else {
            let bytes = fs::read(entry.path())?;
            // The stat gate above is advisory: a file that grows between the
            // stat and the read is still read at its new size, so re-check
            // before decoding rather than trusting the metadata alone.
            if bytes.len() as u64 > size_bound {
                OrphanRecord::placeholder(session_id)
            } else {
                match String::from_utf8(bytes) {
                    Ok(text) => OrphanRecord::parse(session_id, &text),
                    Err(_) => OrphanRecord::placeholder(session_id),
                }
            }
        };
        records.insert(session_id, record);
        if records.len() > limits.max_records {
            return Err(JournalError::LimitExceeded {
                limit: limits.max_records,
            });
        }
    }
    Ok(records)
}

fn ensure_private_directory(directory: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(JournalError::NotADirectory);
            }
            check_private_mode(&metadata)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(directory)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir(directory)?;
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn check_private_file(metadata: &fs::Metadata) -> Result<(), JournalError> {
    check_private_mode(metadata)
}

#[cfg(unix)]
fn check_private_mode(metadata: &fs::Metadata) -> Result<(), JournalError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(JournalError::InsecurePermissions { mode });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_mode(_metadata: &fs::Metadata) -> Result<(), JournalError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), JournalError> {
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), JournalError> {
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), JournalError> {
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;

    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::orphan::identity::{ProcessIdentity, ProcessObservation};

    const BOOT_A: &str = "11111111-1111-4111-8111-111111111111";
    const BOOT_B: &str = "22222222-2222-4222-8222-222222222222";

    fn boot(value: &str) -> BootId {
        BootId::parse(value).expect("canonical boot id")
    }

    fn session(byte: u8) -> SessionId {
        SessionId::parse(&format!("{byte:02x}").repeat(16)).expect("canonical session id")
    }

    fn identity(pid: u32, start_time: u64, group: u32) -> ProcessIdentity {
        ProcessIdentity::new(
            Pid::new(pid).expect("positive pid"),
            ProcessStartTime::new(start_time),
            ProcessGroupId::new(group).expect("positive group"),
            1000,
        )
    }

    fn containment() -> ContainmentRecord {
        ContainmentRecord::cgroup("/sys/fs/cgroup/bittyd-test/bittyd-aaaaaaaa")
    }

    fn subreaper() -> ContainmentRecord {
        ContainmentRecord::Subreaper
    }

    fn limits(
        max_records: usize,
        record_bytes: usize,
        quarantine_retention_ms: u64,
    ) -> OrphanJournalLimits {
        OrphanJournalLimits::new(max_records, record_bytes, quarantine_retention_ms)
            .expect("valid test limits")
    }

    fn journal_in(directory: &TempDir) -> OrphanJournal {
        OrphanJournal::open(
            directory.path().join("journal"),
            limits(64, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens")
    }

    struct FakeProbe {
        boot_id: Option<BootId>,
        observations: BTreeMap<u32, ProcessObservation>,
    }

    impl FakeProbe {
        fn with_boot(value: &str) -> Self {
            Self {
                boot_id: Some(boot(value)),
                observations: BTreeMap::new(),
            }
        }

        fn observe(mut self, pid: u32, observation: ProcessObservation) -> Self {
            self.observations.insert(pid, observation);
            self
        }
    }

    impl ProcessProbe for FakeProbe {
        fn boot_id(&self) -> Option<BootId> {
            self.boot_id.clone()
        }

        fn observe(&self, pid: Pid) -> ProcessObservation {
            self.observations
                .get(&pid.get())
                .copied()
                .unwrap_or(ProcessObservation::Unreadable)
        }
    }

    struct RecordingReaper {
        result: ReapResult,
        calls: Vec<VerifiedOrphan>,
    }

    impl RecordingReaper {
        fn new(result: ReapResult) -> Self {
            Self {
                result,
                calls: Vec::new(),
            }
        }
    }

    impl OrphanReaper for RecordingReaper {
        fn reap(&mut self, orphan: &VerifiedOrphan) -> ReapResult {
            self.calls.push(orphan.clone());
            self.result
        }
    }

    fn reserved_and_committed(
        history: &mut OrphanJournal,
        session_id: SessionId,
        process: ProcessIdentity,
    ) {
        history
            .reserve(session_id, 1, MonotonicMs::from_millis(1), subreaper())
            .expect("reserve succeeds");
        history
            .commit(session_id, process)
            .expect("commit succeeds");
    }

    #[test]
    fn reserve_writes_a_bounded_record_before_the_spawn() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x01);

        history
            .reserve(session_id, 0, MonotonicMs::from_millis(7), subreaper())
            .expect("reserve succeeds");

        assert!(history.contains(session_id));
        assert_eq!(
            history.counts(),
            OrphanCounts {
                reserved: 1,
                committed: 0,
                unverified: 0
            }
        );
        let text = fs::read_to_string(history.record_path(session_id)).expect("record file");
        assert!(text.contains("class reserved"));
        assert!(text.contains("pid -"));
        assert!(text.contains("start_time -"));
        assert!(text.contains("process_group -"));
        assert!(text.contains("uid -"));
        assert!(text.contains("containment subreaper"));
    }

    #[test]
    fn commit_fills_the_exact_identity_and_round_trips() {
        let directory = TempDir::new().expect("temp dir");
        let session_id = session(0x02);
        let process = identity(4242, 987_654, 4242);

        {
            let mut history = journal_in(&directory);
            reserved_and_committed(&mut history, session_id, process);
            assert_eq!(history.counts().committed, 1);
        }

        let history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(64, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
            boot(BOOT_A),
        )
        .expect("journal reopens");
        assert!(history.contains(session_id));
        assert_eq!(history.counts().committed, 1);
        let text = fs::read_to_string(history.record_path(session_id)).expect("record file");
        assert!(text.contains("pid 4242"));
        assert!(text.contains("start_time 987654"));
        assert!(text.contains("process_group 4242"));
    }

    #[test]
    fn exact_identity_match_is_reaped_and_removed() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x03);
        let process = identity(4242, 987_654, 4242);
        reserved_and_committed(&mut history, session_id, process);

        let probe = FakeProbe::with_boot(BOOT_A).observe(
            4242,
            ProcessObservation::Present {
                start_time: ProcessStartTime::new(987_654),
                process_group: ProcessGroupId::new(4242).expect("positive"),
                uid: 1000,
            },
        );
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(
            report,
            ReapReport {
                reaped: 1,
                unverified: 0,
                stale: 0
            }
        );
        assert_eq!(reaper.calls.len(), 1);
        assert_eq!(reaper.calls[0].identity(), Some(process));
        assert_eq!(reaper.calls[0].boot_id(), &boot(BOOT_A));
        assert!(!history.contains(session_id));
        assert_eq!(history.counts().total(), 0);
        assert!(!history.record_path(session_id).exists());
    }

    #[test]
    fn pid_reuse_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x04);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe::with_boot(BOOT_A).observe(
            4242,
            ProcessObservation::Present {
                start_time: ProcessStartTime::new(101),
                process_group: ProcessGroupId::new(4242).expect("positive"),
                uid: 1000,
            },
        );
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert_eq!(report.reaped, 0);
        assert!(reaper.calls.is_empty(), "pid reuse must never be signalled");
        assert_eq!(history.counts().unverified, 1);
        assert!(history.contains(session_id));
    }

    #[test]
    fn different_boot_id_is_stale_and_never_signalled() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x05);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe::with_boot(BOOT_B);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.stale, 1);
        assert_eq!(report.reaped, 0);
        assert!(reaper.calls.is_empty());
        assert!(!history.contains(session_id));
        assert_eq!(history.counts().total(), 0);
    }

    #[test]
    fn missing_pid_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x06);
        history
            .reserve(session_id, 0, MonotonicMs::from_millis(1), subreaper())
            .expect("reserve succeeds");

        let probe = FakeProbe::with_boot(BOOT_A);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().unverified, 1);
        assert!(history.contains(session_id));
    }

    #[test]
    fn unreadable_start_time_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x07);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Unreadable);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn absent_pid_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x08);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Absent);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn group_mismatch_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x09);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe::with_boot(BOOT_A).observe(
            4242,
            ProcessObservation::Present {
                start_time: ProcessStartTime::new(100),
                process_group: ProcessGroupId::new(4243).expect("positive"),
                uid: 1000,
            },
        );
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn unreadable_boot_id_is_never_signalled_and_is_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x0a);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let probe = FakeProbe {
            boot_id: None,
            observations: BTreeMap::new(),
        };
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn journal_capacity_refuses_a_new_reservation_without_creating_a_record() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(2, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens");
        history
            .reserve(session(0x0b), 0, MonotonicMs::ZERO, containment())
            .expect("first reserve succeeds");
        history
            .reserve(session(0x0c), 0, MonotonicMs::ZERO, containment())
            .expect("second reserve succeeds");

        assert!(matches!(
            history.reserve(session(0x0d), 0, MonotonicMs::ZERO, containment()),
            Err(JournalError::LimitExceeded { limit: 2 })
        ));
        assert_eq!(history.counts().total(), 2);
        assert!(!history.contains(session(0x0d)));
        assert!(!history.record_path(session(0x0d)).exists());
    }

    #[test]
    fn record_byte_cap_refuses_an_oversized_record() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(8, 32, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens");

        assert!(matches!(
            history.reserve(session(0x0e), 0, MonotonicMs::ZERO, containment()),
            Err(JournalError::RecordTooLarge { limit: 32, .. })
        ));
        assert_eq!(history.counts().total(), 0);
        assert!(!history.contains(session(0x0e)));
        assert!(!history.record_path(session(0x0e)).exists());
    }

    #[test]
    fn reservation_admits_every_valid_commit_and_rejects_a_lower_bound() {
        let directory = TempDir::new().expect("temp dir");
        let session_id = session(0x17);
        let worst_case = OrphanRecord::reserved(
            session_id,
            0,
            boot(BOOT_A),
            MonotonicMs::from_millis(1),
            containment(),
        )
        .worst_case_encoded_bytes();
        assert!(
            worst_case > 205,
            "the reproduced 205-byte bound must be below the committed worst case"
        );

        // The reported reproduction: a bound that admits the reserved encoding
        // but not the committed encoding is refused before any reservation.
        let mut reproduced = OrphanJournal::open(
            directory.path().join("reproduced"),
            limits(64, 205, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens");
        assert!(matches!(
            reproduced.reserve(session_id, 0, MonotonicMs::from_millis(1), containment()),
            Err(JournalError::RecordTooLarge { limit: 205, .. })
        ));
        assert_eq!(reproduced.counts().total(), 0);
        assert!(!reproduced.contains(session_id));
        assert!(!reproduced.record_path(session_id).exists());

        // The exact worst-case bound accepts the reservation and every valid
        // identity the commit can carry.
        let mut boundary = OrphanJournal::open(
            directory.path().join("boundary"),
            limits(64, worst_case, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens");
        let max_identity = identity(u32::MAX, u64::MAX, u32::MAX);
        boundary
            .reserve(session_id, 0, MonotonicMs::from_millis(1), containment())
            .expect("worst-case reservation fits");
        boundary
            .commit(session_id, max_identity)
            .expect("any valid identity fits after reservation");
        assert_eq!(boundary.counts().committed, 1);
        let text = fs::read_to_string(boundary.record_path(session_id)).expect("record file");
        assert!(text.contains("pid 4294967295"));
        assert!(text.contains("start_time 18446744073709551615"));
        assert!(text.contains("process_group 4294967295"));

        // One byte below the worst case cannot admit a reservation.
        let mut too_small = OrphanJournal::open(
            directory.path().join("too-small"),
            limits(64, worst_case - 1, 1000),
            boot(BOOT_A),
        )
        .expect("journal opens");
        assert!(matches!(
            too_small.reserve(session_id, 0, MonotonicMs::from_millis(1), containment()),
            Err(JournalError::RecordTooLarge { limit, .. }) if limit == worst_case - 1
        ));
        assert_eq!(too_small.counts().total(), 0);
        assert!(!too_small.record_path(session_id).exists());
    }

    #[test]
    fn exact_match_with_an_unverified_reaper_keeps_the_record_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x18);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));
        let probe = FakeProbe::with_boot(BOOT_A).observe(
            4242,
            ProcessObservation::Present {
                start_time: ProcessStartTime::new(100),
                process_group: ProcessGroupId::new(4242).expect("positive"),
                uid: 1000,
            },
        );
        let mut reaper = RecordingReaper::new(ReapResult::Unverified);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(
            report,
            ReapReport {
                reaped: 0,
                unverified: 1,
                stale: 0
            }
        );
        assert_eq!(reaper.calls.len(), 1);
        assert!(history.contains(session_id));
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn quarantine_retention_retires_unverified_records() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 100),
            boot(BOOT_A),
        )
        .expect("journal opens");
        let session_id = session(0x0f);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));
        let probe = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Absent);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert!(
            history
                .collect_expired(MonotonicMs::from_millis(109))
                .expect("collect succeeds")
                .is_empty()
        );
        assert_eq!(history.counts().unverified, 1);
        assert_eq!(
            history
                .collect_expired(MonotonicMs::from_millis(110))
                .expect("collect succeeds"),
            vec![session_id]
        );
        assert!(!history.contains(session_id));
        assert_eq!(history.counts().total(), 0);
    }

    #[test]
    fn quarantine_retention_is_restart_safe_across_process_origins() {
        let directory = TempDir::new().expect("temp dir");
        let journal_dir = directory.path().join("journal");
        let session_id = session(0x1b);
        {
            let mut first = OrphanJournal::open_with_origin(
                journal_dir.clone(),
                limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
                boot(BOOT_A),
                1_000,
            )
            .expect("first journal");
            reserved_and_committed(&mut first, session_id, identity(4242, 100, 4242));
            let absent = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Absent);
            let mut reaper = RecordingReaper::new(ReapResult::Swept);
            first
                .reap(&absent, &mut reaper, MonotonicMs::from_millis(100))
                .expect("quarantine");
            assert_eq!(first.counts().unverified, 1);
        }

        // The next process starts later in the same boot. Its process-local
        // clock is zero again, but the stored quarantine time is boot-relative.
        let mut later = OrphanJournal::open_with_origin(
            journal_dir.clone(),
            limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
            boot(BOOT_A),
            5_000,
        )
        .expect("later journal");
        assert_eq!(
            later.collect_expired(MonotonicMs::ZERO).expect("collect"),
            vec![session_id]
        );

        // Within the retention window it is retained.
        let mut soon = OrphanJournal::open_with_origin(
            journal_dir,
            limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
            boot(BOOT_A),
            1_500,
        )
        .expect("soon journal");
        assert!(
            soon.collect_expired(MonotonicMs::ZERO)
                .expect("collect")
                .is_empty()
        );
    }

    #[test]
    fn a_quarantined_record_is_never_signalled_on_later_passes() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x16);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let absent = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Absent);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        history
            .reap(&absent, &mut reaper, MonotonicMs::from_millis(10))
            .expect("first reap succeeds");
        assert_eq!(history.counts().unverified, 1);
        assert!(reaper.calls.is_empty());

        let exact = FakeProbe::with_boot(BOOT_A).observe(
            4242,
            ProcessObservation::Present {
                start_time: ProcessStartTime::new(100),
                process_group: ProcessGroupId::new(4242).expect("positive"),
                uid: 1000,
            },
        );
        let report = history
            .reap(&exact, &mut reaper, MonotonicMs::from_millis(20))
            .expect("second reap succeeds");
        assert_eq!(report.unverified, 1);
        assert!(
            reaper.calls.is_empty(),
            "a quarantined record caused a signal"
        );
        assert_eq!(history.counts().unverified, 1);
    }

    #[test]
    fn quarantined_record_from_a_previous_boot_is_stale_and_removed() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x19);
        reserved_and_committed(&mut history, session_id, identity(4242, 100, 4242));

        let absent = FakeProbe::with_boot(BOOT_A).observe(4242, ProcessObservation::Absent);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        history
            .reap(&absent, &mut reaper, MonotonicMs::from_millis(10))
            .expect("first reap succeeds");
        assert_eq!(history.counts().unverified, 1);

        let foreign_boot = FakeProbe::with_boot(BOOT_B);
        let report = history
            .reap(&foreign_boot, &mut reaper, MonotonicMs::from_millis(20))
            .expect("second reap succeeds");
        assert_eq!(
            report,
            ReapReport {
                reaped: 0,
                unverified: 0,
                stale: 1
            }
        );
        assert!(reaper.calls.is_empty());
        assert_eq!(history.counts().total(), 0);
        assert!(!history.contains(session_id));
    }

    #[test]
    fn operator_retirement_removes_only_a_quarantined_record() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let quarantined = session(0x10);
        let committed = session(0x11);
        let reserved = session(0x14);
        reserved_and_committed(&mut history, quarantined, identity(4242, 100, 4242));
        reserved_and_committed(&mut history, committed, identity(4243, 100, 4243));

        let probe = FakeProbe::with_boot(BOOT_A);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");
        assert_eq!(history.counts().unverified, 2);
        assert!(reaper.calls.is_empty());

        history
            .reserve(reserved, 0, MonotonicMs::from_millis(11), containment())
            .expect("reserve succeeds");
        assert!(matches!(
            history.retire(reserved),
            Err(JournalError::NotQuarantined)
        ));

        history
            .retire(quarantined)
            .expect("quarantine retirement succeeds");
        assert!(!history.contains(quarantined));
        assert!(history.contains(committed));
        assert!(reaper.calls.is_empty(), "retirement never signals");
    }

    #[test]
    fn malformed_record_is_quarantined_and_never_signalled() {
        let directory = TempDir::new().expect("temp dir");
        let history = journal_in(&directory);
        let session_id = session(0x12);
        write_private_file(&history.record_path(session_id), b"not a record\n")
            .expect("malformed file written");
        drop(history);

        let mut history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 100),
            boot(BOOT_A),
        )
        .expect("journal reopens");
        assert_eq!(history.counts().unverified, 1);
        let probe = FakeProbe::with_boot(BOOT_A);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");

        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
        assert_eq!(
            history
                .collect_expired(MonotonicMs::from_millis(100))
                .expect("collect succeeds"),
            vec![session_id]
        );
    }

    #[test]
    fn oversized_record_file_is_quarantined_without_being_read() {
        let directory = TempDir::new().expect("temp dir");
        let history = journal_in(&directory);
        let session_id = session(0x1a);
        write_private_file(&history.record_path(session_id), &vec![b'x'; 256])
            .expect("oversized file written");
        drop(history);

        let mut history = OrphanJournal::open(
            directory.path().join("journal"),
            limits(8, 64, 100),
            boot(BOOT_A),
        )
        .expect("journal reopens");
        assert_eq!(history.counts().unverified, 1);
        let probe = FakeProbe::with_boot(BOOT_A);
        let mut reaper = RecordingReaper::new(ReapResult::Swept);
        let report = history
            .reap(&probe, &mut reaper, MonotonicMs::from_millis(10))
            .expect("reap succeeds");
        assert_eq!(report.unverified, 1);
        assert!(reaper.calls.is_empty());
    }

    #[test]
    fn record_encoding_has_no_argv_env_or_content() {
        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let session_id = session(0x01);
        history
            .reserve(session_id, 1, MonotonicMs::from_millis(1), containment())
            .expect("reserve succeeds");
        history
            .commit(session_id, identity(4242, 987_654, 4242))
            .expect("commit succeeds");
        let text = fs::read_to_string(history.record_path(session_id)).expect("record file");

        let expected = format!(
            "bittyd-orphan 2\nsession_id {session_id}\ngeneration 1\nclass committed\nboot_id {BOOT_A}\nreserved_at_ms 1\nvalidated_at_ms -\ncontainment cgroup /sys/fs/cgroup/bittyd-test/bittyd-aaaaaaaa\npid 4242\nstart_time 987654\nprocess_group 4242\nuid 1000\n"
        );
        assert_eq!(text, expected);
        for forbidden in ["argv", "env", "cwd", "content", "spawn", "SECRET"] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} leaked into the journal"
            );
        }
    }

    #[test]
    fn journal_limits_above_contract_maxima_are_refused() {
        assert!(matches!(
            OrphanJournalLimits::new(
                MAX_ORPHAN_RECORDS_SYSTEM_MAX + 1,
                ORPHAN_RECORD_BYTES_SYSTEM_MAX,
                1000
            ),
            Err(JournalError::InvalidLimits {
                limit: "max_orphan_records",
                ..
            })
        ));
        assert!(matches!(
            OrphanJournalLimits::new(
                MAX_ORPHAN_RECORDS_SYSTEM_MAX,
                ORPHAN_RECORD_BYTES_SYSTEM_MAX + 1,
                1000
            ),
            Err(JournalError::InvalidLimits {
                limit: "orphan_record_bytes",
                ..
            })
        ));
        assert!(matches!(
            OrphanJournalLimits::new(
                MAX_ORPHAN_RECORDS_SYSTEM_MAX,
                ORPHAN_RECORD_BYTES_SYSTEM_MAX,
                ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX + 1
            ),
            Err(JournalError::InvalidLimits {
                limit: "orphan_record_retention_ms",
                ..
            })
        ));
        assert_eq!(
            OrphanJournalLimits::default(),
            limits(
                MAX_ORPHAN_RECORDS_SYSTEM_MAX,
                ORPHAN_RECORD_BYTES_SYSTEM_MAX,
                ORPHAN_RECORD_RETENTION_MS_DEFAULT
            )
        );
    }

    #[test]
    fn journal_refuses_a_non_directory_path() {
        let directory = TempDir::new().expect("temp dir");
        let path = directory.path().join("not-a-directory");
        fs::write(&path, b"x").expect("file written");

        assert!(matches!(
            OrphanJournal::open(path, OrphanJournalLimits::default(), boot(BOOT_A)),
            Err(JournalError::NotADirectory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn journal_creates_an_owner_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = TempDir::new().expect("temp dir");
        let mut history = journal_in(&directory);
        let journal_dir = directory.path().join("journal");
        assert_eq!(
            fs::metadata(&journal_dir)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let session_id = session(0x13);
        history
            .reserve(session_id, 0, MonotonicMs::ZERO, containment())
            .expect("reserve succeeds");
        assert_eq!(
            fs::metadata(history.record_path(session_id))
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_refuses_a_group_readable_record_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = TempDir::new().expect("temp dir");
        let session_id = session(0x15);
        {
            let mut history = journal_in(&directory);
            history
                .reserve(session_id, 0, MonotonicMs::ZERO, containment())
                .expect("reserve succeeds");
            fs::set_permissions(
                history.record_path(session_id),
                fs::Permissions::from_mode(0o644),
            )
            .expect("permissions changed");
        }

        assert!(matches!(
            OrphanJournal::open(
                directory.path().join("journal"),
                limits(8, ORPHAN_RECORD_BYTES_SYSTEM_MAX, 1000),
                boot(BOOT_A)
            ),
            Err(JournalError::InsecurePermissions { mode: 0o644 })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn journal_refuses_a_group_readable_directory() {
        use std::os::unix::fs::DirBuilderExt as _;

        let directory = TempDir::new().expect("temp dir");
        let journal_dir = directory.path().join("journal");
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o755);
        builder.create(&journal_dir).expect("directory created");

        assert!(matches!(
            OrphanJournal::open(journal_dir, OrphanJournalLimits::default(), boot(BOOT_A)),
            Err(JournalError::InsecurePermissions { mode: 0o755 })
        ));
    }
}
