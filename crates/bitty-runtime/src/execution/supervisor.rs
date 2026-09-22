//! Phase-3 detached supervisor daemon contract (CTX-0516).
//!
//! Phase 2 keeps job facts across a restart; phase 3 keeps the jobs
//! themselves alive while no GUI runs: a detached supervisor owns the
//! registry directory so multi-day jobs survive a Bitty exit, with
//! handoff/adoption when the GUI leaves and returns, plus resource
//! scheduling so one owner cannot starve the host.
//!
//! # What this slice implements
//!
//! - [`SupervisorDaemon`]: single-owner coordination over a registry
//!   directory. `claim` takes ownership (adopting a stale lock whose
//!   heartbeat stopped), `heartbeat` renews it, `release` gives it back. At
//!   most one supervisor — GUI-embedded or detached — owns a directory at a
//!   time, so two supervisors never reap the same child.
//! - [`HandoffOffer`]: the GUI's exit note (`write_handoff`) naming the jobs
//!   it leaves behind plus its event cursor; the returning owner reads it,
//!   reconciles via [`reconcile`](super::persistence::reconcile), then
//!   clears it. Handoff is a note, never a transfer of authority: adoption
//!   still re-validates everything against the manifest.
//! - [`SchedulePolicy`]: admission control over concurrent running jobs
//!   (`admit`) plus deterministic FIFO selection (`select_next`), so the
//!   detached supervisor sheds load fail-closed instead of oversubscribing
//!   the host.
//! - [`adoption_plan`]: maps reconciled rows onto adoption truth — terminal
//!   facts stay observable, unknown outcomes require an explicit respawn
//!   (never an automatic restart).
//!
//! # Deliberate non-goals (the ADR-0008 seam)
//!
//! This is the file contract a future daemon process uses, not the daemon
//! itself: nothing here spawns a background process, binds a socket, or
//! reaps a foreign pid. Actual daemon launch, liveness supervision, and IPC
//! transport stay deferred under the accepted headless/daemon decision; this
//! module composes with that deferral by keeping every coordination fact in
//! files the future daemon already understands. Claiming ownership of a
//! directory is coordination, not a process launch.
//!
//! # Invariants
//!
//! - AI-agnostic vocabulary: ids, cursors, pids, and counts only.
//! - Bounds: the handoff job list, heartbeat bytes, and schedule ceiling
//!   are bounded; overflow fails closed.
//! - No automatic retry, no cross-process kill: adoption observes and
//!   schedules; it never restarts a job or signals a pid it did not spawn.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::persistence::{MAX_PERSISTED_JOBS, ReconciledJob, ResumeDecision};
use super::{JobId, JobRegistry};

/// Lock file name inside a supervised directory.
pub const SUPERVISOR_LOCK_NAME: &str = "supervisor.lock";

/// Heartbeat file name inside a supervised directory.
pub const SUPERVISOR_HEARTBEAT_NAME: &str = "supervisor.heartbeat";

/// Handoff note name inside a supervised directory.
pub const HANDOFF_FILE_NAME: &str = "handoff";

/// Lock/heartbeat/handoff format version.
pub const SUPERVISOR_FORMAT_VERSION: u32 = 1;

/// Heartbeat age past which a lock is stale and adoptable.
pub const STALE_HEARTBEAT_MS: u64 = 30_000;

/// Maximum jobs one handoff note may name (the persistence row bound, so a
/// handoff can always name a full registry and nothing unbounded is read).
pub const MAX_HANDOFF_JOBS: usize = MAX_PERSISTED_JOBS;

/// Maximum handoff file bytes (fail-closed past this).
pub const MAX_HANDOFF_BYTES: usize = 256 * 1024;

/// Maximum heartbeat file bytes (a decimal timestamp; anything larger is
/// foreign and corrupt).
pub const MAX_HEARTBEAT_BYTES: usize = 64;

/// Default ceiling for concurrent running jobs under a fresh policy.
pub const DEFAULT_MAX_RUNNING: usize = 16;

/// Absolute ceiling for the schedule policy (fail-closed past this).
pub const MAX_SCHEDULE_RUNNING: usize = 256;

/// Failure of a daemon, handoff, or scheduling operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// Filesystem failure with the operation context.
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying error text.
        reason: String,
    },
    /// A lock, heartbeat, or handoff file is malformed.
    Corrupt {
        /// What failed validation.
        reason: String,
    },
    /// The directory is already owned by a live supervisor.
    AlreadyOwned {
        /// Pid recorded in the live lock.
        owner_pid: u32,
    },
    /// The caller does not own the lock it touches.
    NotOwner {
        /// Owned reason.
        reason: String,
    },
    /// A handoff or policy value exceeds its bound; nothing was applied.
    TooLarge {
        /// What exceeded the bound.
        what: String,
        /// Observed size.
        actual: usize,
        /// Enforced bound.
        limit: usize,
    },
}

impl DaemonError {
    fn io(context: impl Into<String>, error: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            reason: error.to_string(),
        }
    }

    fn corrupt(reason: impl Into<String>) -> Self {
        Self::Corrupt {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { context, reason } => write!(f, "supervisor io ({context}): {reason}"),
            Self::Corrupt { reason } => write!(f, "corrupt supervisor file: {reason}"),
            Self::AlreadyOwned { owner_pid } => {
                write!(f, "supervisor directory already owned by pid {owner_pid}")
            }
            Self::NotOwner { reason } => write!(f, "not the owning supervisor: {reason}"),
            Self::TooLarge {
                what,
                actual,
                limit,
            } => {
                write!(f, "supervisor {what} too large ({actual} > {limit})")
            }
        }
    }
}

impl std::error::Error for DaemonError {}

/// Current epoch milliseconds for lock and heartbeat stamps.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

// ── ownership ─────────────────────────────────────────────────────────────

/// Single-owner coordination over one supervised directory.
///
/// A value exists only after a successful [`SupervisorDaemon::claim`]; the
/// lock file carries `(version, pid, claimed_at_ms)` and the heartbeat file
/// carries the newest liveness stamp. A second claimant reads both: a fresh
/// heartbeat denies with [`DaemonError::AlreadyOwned`], while a stale or
/// missing heartbeat means the owner died and the lock is adopted (crash
/// adoption without a babysitter process).
#[derive(Debug)]
pub struct SupervisorDaemon {
    dir: PathBuf,
    pid: u32,
}

impl SupervisorDaemon {
    /// Claims ownership of `dir`, adopting a stale lock when the previous
    /// owner's heartbeat stopped.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::AlreadyOwned`] when a live supervisor holds
    /// the directory, and [`DaemonError::Io`] when the lock or heartbeat
    /// cannot be written.
    pub fn claim(dir: &Path) -> Result<Self, DaemonError> {
        let pid = std::process::id();
        if let Some((owner_pid, claimed_at_ms)) = read_lock(dir)? {
            // A lock is adoptable only when BOTH stamps are stale: a fresh
            // claim may not have written its first heartbeat yet (a claimant
            // alive but mid-claim must never lose its directory), and a fresh
            // heartbeat always means a live owner whatever the claim stamp
            // says. Clock skew toward the past saturates to age zero, which
            // denies — the safe direction.
            let now = now_ms();
            let lock_stale = now.saturating_sub(claimed_at_ms) > STALE_HEARTBEAT_MS;
            let beat_stale = now.saturating_sub(read_heartbeat(dir)?) > STALE_HEARTBEAT_MS;
            if !(lock_stale && beat_stale) {
                return Err(DaemonError::AlreadyOwned { owner_pid });
            }
            // Stale: the owner died without releasing. Adoption overwrites
            // the lock below; the old pid is never trusted.
        }
        write_lock(dir, pid, now_ms())?;
        write_heartbeat(dir, now_ms())?;
        Ok(Self {
            dir: dir.to_owned(),
            pid,
        })
    }

    /// Supervised directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Owning pid recorded at claim time.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the lock file still names this supervisor.
    #[must_use]
    pub fn is_owner(&self) -> bool {
        read_lock(&self.dir)
            .ok()
            .flatten()
            .is_some_and(|(owner_pid, _)| owner_pid == self.pid)
    }

    /// Renews liveness; owners call this on a period well under
    /// [`STALE_HEARTBEAT_MS`].
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::NotOwner`] when the lock no longer names this
    /// supervisor (adopted or released elsewhere), and [`DaemonError::Io`]
    /// when the heartbeat cannot be written.
    pub fn heartbeat(&self) -> Result<(), DaemonError> {
        if !self.is_owner() {
            return Err(DaemonError::NotOwner {
                reason: "lock names another supervisor".into(),
            });
        }
        write_heartbeat(&self.dir, now_ms())
    }

    /// Releases ownership, removing the lock and heartbeat. Idempotent
    /// against a missing lock; refuses to remove another owner's lock.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::NotOwner`] when the lock names another live
    /// supervisor, and [`DaemonError::Io`] when removal fails.
    pub fn release(self) -> Result<(), DaemonError> {
        match read_lock(&self.dir)? {
            Some((owner_pid, _)) if owner_pid != self.pid => Err(DaemonError::NotOwner {
                reason: "lock names another supervisor".into(),
            }),
            _ => {
                let _ = fs::remove_file(self.dir.join(SUPERVISOR_LOCK_NAME));
                let _ = fs::remove_file(self.dir.join(SUPERVISOR_HEARTBEAT_NAME));
                Ok(())
            }
        }
    }
}

/// Reads the lock file: `None` when absent, otherwise `(pid, claimed_at_ms)`.
fn read_lock(dir: &Path) -> Result<Option<(u32, u64)>, DaemonError> {
    let path = dir.join(SUPERVISOR_LOCK_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(DaemonError::io("read supervisor lock", error)),
    };
    if bytes.len() > MAX_HEARTBEAT_BYTES {
        return Err(DaemonError::corrupt("supervisor lock exceeds bound"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| DaemonError::corrupt(format!("lock not UTF-8: {error}")))?;
    let fields: Vec<&str> = text.trim_end().split('\t').collect();
    let [magic, version, pid, claimed] = fields.as_slice() else {
        return Err(DaemonError::corrupt("supervisor lock has the wrong shape"));
    };
    if *magic != "BITTY-SUPERVISOR" {
        return Err(DaemonError::corrupt("supervisor lock magic mismatch"));
    }
    if *version != format!("v{SUPERVISOR_FORMAT_VERSION}") {
        return Err(DaemonError::corrupt(format!(
            "unsupported supervisor version: {version}"
        )));
    }
    let owner_pid: u32 = pid
        .parse()
        .map_err(|_| DaemonError::corrupt("supervisor lock pid is not a number"))?;
    if owner_pid == 0 {
        return Err(DaemonError::corrupt("supervisor lock pid is zero"));
    }
    let claimed_at_ms: u64 = claimed
        .parse()
        .map_err(|_| DaemonError::corrupt("supervisor lock stamp is not a number"))?;
    Ok(Some((owner_pid, claimed_at_ms)))
}

/// Writes the lock file atomically (temp + rename; small enough that fsync
/// rides on the directory sync of the persistence layer's heavier writes —
/// the heartbeat, not the lock, is the liveness source of truth).
fn write_lock(dir: &Path, pid: u32, claimed_at_ms: u64) -> Result<(), DaemonError> {
    fs::create_dir_all(dir).map_err(|error| DaemonError::io("create supervised dir", error))?;
    let text = format!("BITTY-SUPERVISOR\tv{SUPERVISOR_FORMAT_VERSION}\t{pid}\t{claimed_at_ms}");
    atomic_small_write(&dir.join(SUPERVISOR_LOCK_NAME), text.as_bytes())
}

/// Reads the heartbeat stamp; a missing heartbeat reads as 0 (stale), so a
/// claimant that crashed before its first beat is still adoptable.
fn read_heartbeat(dir: &Path) -> Result<u64, DaemonError> {
    let path = dir.join(SUPERVISOR_HEARTBEAT_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(DaemonError::io("read supervisor heartbeat", error)),
    };
    if bytes.len() > MAX_HEARTBEAT_BYTES {
        return Err(DaemonError::corrupt("supervisor heartbeat exceeds bound"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| DaemonError::corrupt(format!("heartbeat not UTF-8: {error}")))?;
    text.trim_end()
        .parse::<u64>()
        .map_err(|_| DaemonError::corrupt("heartbeat is not a number"))
}

/// Writes the heartbeat stamp atomically.
fn write_heartbeat(dir: &Path, at_ms: u64) -> Result<(), DaemonError> {
    atomic_small_write(
        &dir.join(SUPERVISOR_HEARTBEAT_NAME),
        at_ms.to_string().as_bytes(),
    )
}

/// Atomic temp-plus-rename write for small coordination files.
fn atomic_small_write(path: &Path, bytes: &[u8]) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| DaemonError::io("create supervisor dir", error))?;
        }
    }
    let temp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name().map_or_else(
            || SUPERVISOR_LOCK_NAME.into(),
            |name| name.to_string_lossy().into_owned()
        ),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    let outcome = (|| -> Result<(), std::io::Error> {
        use std::io::Write as _;
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(DaemonError::io("write supervisor file", error))
        }
    }
}

// ── handoff ───────────────────────────────────────────────────────────────

/// The GUI's exit note: which jobs it leaves behind and where its event
/// cursor stopped, so the next owner adopts without replays from the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffOffer {
    /// Pid of the departing owner.
    pub from_pid: u32,
    /// Exit-note time (epoch milliseconds).
    pub at_ms: u64,
    /// Departing owner's delivery-log head `seq`.
    pub event_cursor: u64,
    /// Jobs left behind, in id order.
    pub job_ids: Vec<JobId>,
}

impl HandoffOffer {
    /// Builds a handoff from the departing registry state, capturing the
    /// event cursor and every tracked id.
    #[must_use]
    pub fn from_registry(registry: &JobRegistry, at_ms: u64) -> Self {
        let mut job_ids = registry
            .list()
            .iter()
            .map(|snapshot| snapshot.id)
            .collect::<Vec<_>>();
        job_ids.sort_unstable();
        Self {
            from_pid: std::process::id(),
            at_ms,
            event_cursor: registry.event_head_seq(),
            job_ids,
        }
    }

    /// Validates the offer shape (fail-closed before any write).
    fn validate(&self) -> Result<(), DaemonError> {
        if self.from_pid == 0 {
            return Err(DaemonError::corrupt("handoff pid is zero"));
        }
        if self.job_ids.len() > MAX_HANDOFF_JOBS {
            return Err(DaemonError::TooLarge {
                what: "handoff jobs".into(),
                actual: self.job_ids.len(),
                limit: MAX_HANDOFF_JOBS,
            });
        }
        let mut sorted = self.job_ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() != self.job_ids.len() {
            return Err(DaemonError::corrupt("handoff carries duplicate job ids"));
        }
        Ok(())
    }
}

/// Writes the GUI's exit note atomically.
///
/// # Errors
///
/// Returns [`DaemonError::TooLarge`] when the job list exceeds its bound,
/// [`DaemonError::Corrupt`] on duplicate ids, and [`DaemonError::Io`] on
/// filesystem failure. A failed write leaves the previous note untouched.
pub fn write_handoff(dir: &Path, offer: &HandoffOffer) -> Result<(), DaemonError> {
    offer.validate()?;
    let mut text = format!(
        "BITTY-HANDOFF\tv{SUPERVISOR_FORMAT_VERSION}\t{}\t{}\t{}\t{}",
        offer.from_pid,
        offer.at_ms,
        offer.event_cursor,
        offer.job_ids.len()
    );
    for id in &offer.job_ids {
        text.push_str(&format!("\t{}", id.get()));
    }
    text.push('\n');
    if text.len() > MAX_HANDOFF_BYTES {
        return Err(DaemonError::TooLarge {
            what: "handoff note".into(),
            actual: text.len(),
            limit: MAX_HANDOFF_BYTES,
        });
    }
    fs::create_dir_all(dir).map_err(|error| DaemonError::io("create supervised dir", error))?;
    atomic_small_write(&dir.join(HANDOFF_FILE_NAME), text.as_bytes())
}

/// Reads the exit note, or `None` when no handoff is pending.
///
/// # Errors
///
/// Returns [`DaemonError::Io`] when the note is unreadable,
/// [`DaemonError::TooLarge`] when it exceeds its bound, and
/// [`DaemonError::Corrupt`] when any field is malformed.
pub fn read_handoff(dir: &Path) -> Result<Option<HandoffOffer>, DaemonError> {
    let path = dir.join(HANDOFF_FILE_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(DaemonError::io("read handoff", error)),
    };
    if bytes.len() > MAX_HANDOFF_BYTES {
        return Err(DaemonError::TooLarge {
            what: "handoff note".into(),
            actual: bytes.len(),
            limit: MAX_HANDOFF_BYTES,
        });
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| DaemonError::corrupt(format!("handoff not UTF-8: {error}")))?;
    let fields: Vec<&str> = text.trim_end().split('\t').collect();
    if fields.len() < 6 {
        return Err(DaemonError::corrupt("handoff has the wrong shape"));
    }
    if fields[0] != "BITTY-HANDOFF" {
        return Err(DaemonError::corrupt("handoff magic mismatch"));
    }
    if fields[1] != format!("v{SUPERVISOR_FORMAT_VERSION}") {
        return Err(DaemonError::corrupt(format!(
            "unsupported handoff version: {}",
            fields[1]
        )));
    }
    let from_pid: u32 = fields[2]
        .parse()
        .map_err(|_| DaemonError::corrupt("handoff pid is not a number"))?;
    if from_pid == 0 {
        return Err(DaemonError::corrupt("handoff pid is zero"));
    }
    let at_ms: u64 = fields[3]
        .parse()
        .map_err(|_| DaemonError::corrupt("handoff stamp is not a number"))?;
    let event_cursor: u64 = fields[4]
        .parse()
        .map_err(|_| DaemonError::corrupt("handoff cursor is not a number"))?;
    let count: usize = fields[5]
        .parse()
        .map_err(|_| DaemonError::corrupt("handoff count is not a number"))?;
    if count > MAX_HANDOFF_JOBS {
        return Err(DaemonError::corrupt(format!(
            "handoff count exceeds bound ({count})"
        )));
    }
    if fields.len() != 6 + count {
        return Err(DaemonError::corrupt("handoff count disagrees with fields"));
    }
    let mut job_ids = Vec::with_capacity(count.min(64));
    for raw in &fields[6..] {
        let raw_id: u64 = raw
            .parse()
            .map_err(|_| DaemonError::corrupt("handoff job id is not a number"))?;
        job_ids.push(
            JobId::from_raw(raw_id)
                .ok_or_else(|| DaemonError::corrupt("handoff job id is zero"))?,
        );
    }
    let offer = HandoffOffer {
        from_pid,
        at_ms,
        event_cursor,
        job_ids,
    };
    offer.validate()?;
    Ok(Some(offer))
}

/// Clears a consumed handoff note (best-effort: a missing note is fine).
pub fn clear_handoff(dir: &Path) -> Result<(), DaemonError> {
    match fs::remove_file(dir.join(HANDOFF_FILE_NAME)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DaemonError::io("clear handoff", error)),
    }
}

// ── scheduling ────────────────────────────────────────────────────────────

/// Admission verdict for one more running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScheduleDecision {
    /// A running slot is free; the job may start.
    Admit,
    /// All running slots are full; the job waits queued.
    Defer,
}

impl ScheduleDecision {
    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Defer => "defer",
        }
    }
}

/// Resource scheduling: how many jobs may run at once.
///
/// The detached supervisor owns no priority ontology — callers order the
/// queue, this policy only caps concurrency and picks FIFO-first, so load
/// sheds fail-closed (queued, observable) instead of oversubscribing the
/// host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulePolicy {
    max_running: usize,
}

impl SchedulePolicy {
    /// Policy admitting at most `max_running` concurrent running jobs.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::TooLarge`] when `max_running` is zero or past
    /// [`MAX_SCHEDULE_RUNNING`].
    pub fn new(max_running: usize) -> Result<Self, DaemonError> {
        if max_running == 0 || max_running > MAX_SCHEDULE_RUNNING {
            return Err(DaemonError::TooLarge {
                what: "schedule ceiling".into(),
                actual: max_running,
                limit: MAX_SCHEDULE_RUNNING,
            });
        }
        Ok(Self { max_running })
    }

    /// Policy with the default concurrency ceiling.
    #[must_use]
    pub const fn default_policy() -> Self {
        Self {
            max_running: DEFAULT_MAX_RUNNING,
        }
    }

    /// Configured concurrency ceiling.
    #[must_use]
    pub const fn max_running(self) -> usize {
        self.max_running
    }

    /// Admits one more running job when a slot is free, else defers it.
    /// Pure: no clock, no I/O, no side effects.
    #[must_use]
    pub const fn admit(self, running: usize) -> ScheduleDecision {
        if running < self.max_running {
            ScheduleDecision::Admit
        } else {
            ScheduleDecision::Defer
        }
    }

    /// First queued id (FIFO): the next job to admit when a slot frees.
    /// Pure; returns `None` for an empty queue.
    #[must_use]
    pub fn select_next(self, queued: &[JobId]) -> Option<JobId> {
        let _ = self;
        queued.first().copied()
    }
}

// ── adoption ──────────────────────────────────────────────────────────────

/// What adoption decided for one reconciled row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdoptionKind {
    /// Terminal facts survived: metadata, output index, and spilled logs
    /// stay observable; nothing runs, nothing is reaped.
    SurvivingFacts,
    /// The outcome is unknown and the job is not running anywhere: it needs
    /// an explicit respawn (a new spawn with a fresh id) when — and only
    /// when — a principal asks for one. Adoption never restarts it.
    UnknownRequiresExplicitRespawn,
}

impl AdoptionKind {
    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SurvivingFacts => "surviving_facts",
            Self::UnknownRequiresExplicitRespawn => "unknown_requires_explicit_respawn",
        }
    }
}

/// One reconciled row plus its adoption verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdoptedJob {
    /// Adopted job identity (the persisted id, kept as provenance).
    pub id: JobId,
    /// The adoption verdict.
    pub kind: AdoptionKind,
}

/// Maps reconciled rows onto adoption truth.
///
/// Terminal rows become observable facts; unknown rows require an explicit
/// respawn and are never restarted here. The mapping is total and pure: no
/// I/O, no signals, no spawns.
#[must_use]
pub fn adoption_plan(reconciled: &[ReconciledJob]) -> Vec<AdoptedJob> {
    reconciled
        .iter()
        .map(|row| {
            let kind = match row.decision {
                ResumeDecision::TerminalFacts => AdoptionKind::SurvivingFacts,
                ResumeDecision::UnknownOutcome => AdoptionKind::UnknownRequiresExplicitRespawn,
            };
            AdoptedJob {
                id: row.record.id,
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::persistence::{PersistedJob, PersistedStore, ResumeCursor};
    use super::super::{JobState, JobStop};
    use super::*;
    use bitty_ipc::execution::EnvPolicy;

    fn terminal_row(id: u64) -> ReconciledJob {
        ReconciledJob {
            record: PersistedJob {
                id: JobId::from_raw(id).expect("id"),
                state: JobState::Done(JobStop::Exited),
                kind: super::super::JobKind::Command,
                lifetime: super::super::JobLifetime::Detached,
                io: super::super::JobIo::Pipes,
                program: "true".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: EnvPolicy::Isolated,
                hard_timeout_ms: None,
                idle_timeout_ms: None,
                retention_ms: None,
                origin_panel: None,
                started_at_ms: Some(1),
                finished_at_ms: Some(2),
                output: super::super::OutputIndex::default(),
                stdout_log: None,
                stderr_log: None,
            },
            decision: ResumeDecision::TerminalFacts,
        }
    }

    #[test]
    fn schedule_admits_up_to_the_ceiling_then_defers() {
        let policy = SchedulePolicy::new(2).expect("policy");
        assert_eq!(policy.admit(0), ScheduleDecision::Admit);
        assert_eq!(policy.admit(1), ScheduleDecision::Admit);
        assert_eq!(policy.admit(2), ScheduleDecision::Defer);
        assert_eq!(policy.admit(usize::MAX), ScheduleDecision::Defer);
    }

    #[test]
    fn schedule_rejects_a_zero_or_huge_ceiling() {
        assert!(SchedulePolicy::new(0).is_err());
        assert!(SchedulePolicy::new(MAX_SCHEDULE_RUNNING + 1).is_err());
        assert!(SchedulePolicy::new(MAX_SCHEDULE_RUNNING).is_ok());
    }

    #[test]
    fn select_next_is_fifo_and_total() {
        let policy = SchedulePolicy::default_policy();
        assert_eq!(policy.select_next(&[]), None);
        let first = JobId::from_raw(3).expect("id");
        let second = JobId::from_raw(9).expect("id");
        assert_eq!(policy.select_next(&[first, second]), Some(first));
    }

    #[test]
    fn adoption_never_restarts_an_unknown_job() {
        let mut unknown = terminal_row(5);
        unknown.record.state = JobState::Running;
        unknown.decision = ResumeDecision::UnknownOutcome;
        let plan = adoption_plan(&[terminal_row(4), unknown]);
        assert_eq!(plan[0].kind, AdoptionKind::SurvivingFacts);
        assert_eq!(plan[1].kind, AdoptionKind::UnknownRequiresExplicitRespawn);
    }

    #[test]
    fn handoff_rejects_duplicates_and_overflow() {
        let id = JobId::from_raw(1).expect("id");
        let duplicated = HandoffOffer {
            from_pid: 7,
            at_ms: 8,
            event_cursor: 9,
            job_ids: vec![id, id],
        };
        assert!(duplicated.validate().is_err());
        let zero_pid = HandoffOffer {
            from_pid: 0,
            at_ms: 8,
            event_cursor: 9,
            job_ids: Vec::new(),
        };
        assert!(zero_pid.validate().is_err());
        let _ = PersistedStore {
            version: super::super::persistence::PERSIST_FORMAT_VERSION,
            cursor: ResumeCursor {
                event_head_seq: 0,
                max_issued_id: 0,
            },
            jobs: Vec::new(),
        };
    }
}
