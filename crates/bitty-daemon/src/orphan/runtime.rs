//! Production orphan supervision: journal, probe, and verified reaper
//! (contract `D-06`; bitty#2791).
//!
//! [`OrphanSupervisor::open`] is the only path that builds a served verifier.
//! It refuses on a platform with no containment mechanism, refuses on a Linux
//! cell without a boot id (the journal cannot fence a reboot), and otherwise
//! opens the owner-private journal with the compiled bounds. A journal that
//! cannot be opened stops daemon startup; an empty journal is still served and
//! reports zeros.
//!
//! [`OrphanSupervisor::verify`] runs the same classification as the startup
//! scan: foreign boot ids are stale, exact cgroup records are reaped through
//! the recorded subtree, a reused or unreadable subreaper identity is
//! quarantined, and a quarantined record is never signalled again. The report
//! carries counts by class and never a pid.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::containment::{ContainmentKind, ContainmentReport, MECHANISM_NONE};
use crate::limits::Limits;
use crate::orphan::identity::{BootId, SystemProbe};
use crate::orphan::journal::{
    JournalError, OrphanCounts, OrphanJournal, OrphanJournalLimits, ReapReport,
};
use crate::orphan::reaper::SystemOrphanReaper;
use crate::platform::PlatformCell;
use crate::registry::{MonotonicMs, SessionId};

/// Runtime subdirectory holding the orphan journal.
pub const ORPHAN_SUBDIR: &str = "orphans";

/// Why a served orphan verifier could not be opened.
#[derive(Debug)]
pub enum OrphanOpenError {
    /// The journal failed to open.
    Journal(JournalError),
    /// The cell exposes no boot identity, so a record cannot be fenced across
    /// a reboot.
    NoBootId,
}

impl fmt::Display for OrphanOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => write!(formatter, "orphan journal: {error}"),
            Self::NoBootId => formatter.write_str("no boot id for the orphan journal"),
        }
    }
}

impl std::error::Error for OrphanOpenError {}

impl From<JournalError> for OrphanOpenError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Counts for one verification pass plus the classes still stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrphanVerifyReport {
    /// Records removed after a verified cgroup sweep.
    pub reaped: usize,
    /// Records quarantined (or already quarantined) this pass.
    pub unverified: usize,
    /// Records discarded because their boot id is stale.
    pub stale: usize,
    /// Reserved records still stored after the pass.
    pub reserved: usize,
    /// Committed records still stored after the pass.
    pub committed: usize,
}

impl OrphanVerifyReport {
    fn from_pass(pass: ReapReport, counts: OrphanCounts) -> Self {
        Self {
            reaped: pass.reaped,
            unverified: pass.unverified,
            stale: pass.stale,
            reserved: counts.reserved,
            committed: counts.committed,
        }
    }

    /// The wire shape: counts only, never a pid, path, or command.
    #[must_use]
    pub fn to_json(self) -> Value {
        json!({
            "reaped": self.reaped,
            "unverified": self.unverified,
            "stale": self.stale,
            "reserved": self.reserved,
            "committed": self.committed,
        })
    }
}

/// A served production verifier over one journal.
pub struct OrphanRuntime {
    journal: OrphanJournal,
    probe: SystemProbe,
    reaper: SystemOrphanReaper,
    mechanism: ContainmentKind,
    fallback_reason: Option<String>,
    containment_limits: crate::containment::ContainmentLimits,
}

impl fmt::Debug for OrphanRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrphanRuntime")
            .field("mechanism", &self.mechanism)
            .field("journal", &self.journal)
            .finish_non_exhaustive()
    }
}

impl OrphanRuntime {
    fn open(
        directory: PathBuf,
        limits: OrphanJournalLimits,
        report: &ContainmentReport,
        containment_limits: crate::containment::ContainmentLimits,
    ) -> Result<Self, OrphanOpenError> {
        let boot_id = BootId::read_system().ok_or(OrphanOpenError::NoBootId)?;
        let journal = OrphanJournal::open(directory, limits, boot_id)?;
        Ok(Self {
            journal,
            probe: SystemProbe::new(),
            reaper: SystemOrphanReaper::new(report, containment_limits),
            mechanism: report.kind(),
            fallback_reason: report.fallback_reason().map(ToOwned::to_owned),
            containment_limits,
        })
    }

    /// Run one full verification pass.
    ///
    /// Used by the startup scan, which may block before the endpoint is bound:
    /// every record is examined and each record's own wait is already bounded.
    ///
    /// # Errors
    ///
    /// Propagates a journal rewrite or removal failure. A failed pass never
    /// claims a record verified.
    pub fn verify(&mut self, now: MonotonicMs) -> Result<OrphanVerifyReport, JournalError> {
        self.verify_within(now, None)
    }

    /// Run one pass whose waits are clamped to `budget`.
    ///
    /// The on-demand method uses [`Self::pass_budget`] so a request cannot hold
    /// the single-threaded service loop for the full per-record window on every
    /// remaining record. Every record is still examined; a record whose wait is
    /// clamped is deferred, keeps its `reserved`/`committed` class, and is
    /// retried by the next pass. A pass is idempotent once no record is
    /// deferred.
    ///
    /// # Errors
    ///
    /// Propagates a journal rewrite or removal failure. A failed pass never
    /// claims a record verified.
    pub fn verify_within(
        &mut self,
        now: MonotonicMs,
        budget: Option<Duration>,
    ) -> Result<OrphanVerifyReport, JournalError> {
        self.verify_within_excluding(now, budget, &std::collections::BTreeSet::new())
    }

    /// Run one pass while skipping records of supervised sessions.
    ///
    /// The served service owns those identities; a reconcile read must never
    /// signal a bound worker. The caller passes exactly the sessions whose
    /// worker handle is still live in the pool, never a terminal record: exit
    /// cleanup is classified, not skipped. The startup scan has no registry and
    /// skips nothing.
    ///
    /// # Errors
    ///
    /// The same failures as [`Self::verify_within`].
    pub fn verify_within_excluding(
        &mut self,
        now: MonotonicMs,
        budget: Option<Duration>,
        live: &std::collections::BTreeSet<SessionId>,
    ) -> Result<OrphanVerifyReport, JournalError> {
        self.reaper
            .set_deadline(budget.map(|budget| std::time::Instant::now() + budget));
        let pass = self
            .journal
            .reap_excluding(&self.probe, &mut self.reaper, now, live);
        // Clear the pass bound on every path; a failed pass must not leave a
        // stale deadline on the reaper for the next caller.
        self.reaper.set_deadline(None);
        let pass = pass?;
        let _expired = self.journal.collect_expired(now)?;
        Ok(OrphanVerifyReport::from_pass(pass, self.journal.counts()))
    }

    /// The bound one on-demand pass may block the service loop.
    #[must_use]
    pub const fn pass_budget(&self) -> Duration {
        self.containment_limits.pass_budget()
    }

    /// Retire one quarantined record without signalling or adopting it.
    ///
    /// `record` is the journal record id, which is the recorded session id.
    /// Only a quarantined record can be retired; removing it frees its journal
    /// slot so a quarantine cannot block new spawns for the retention window.
    ///
    /// # Errors
    ///
    /// Propagates a journal failure. An unknown or not-quarantined record is
    /// refused in the journal and nothing is removed.
    pub fn retire(&mut self, record: SessionId) -> Result<OrphanCounts, JournalError> {
        self.journal.retire(record)?;
        Ok(self.journal.counts())
    }

    /// Settle the record of a worker the service just terminated.
    ///
    /// # Errors
    ///
    /// Propagates a journal rewrite or removal failure.
    pub fn settle(
        &mut self,
        session_id: SessionId,
        cleanup: crate::registry::CleanupClass,
        now: MonotonicMs,
    ) -> Result<(), JournalError> {
        self.journal.settle(session_id, cleanup, now)
    }

    /// Reserve the pre-spawn journal record.
    ///
    /// # Errors
    ///
    /// Propagates a full journal, an oversized record, a duplicate record, or
    /// a filesystem failure.
    pub fn reserve(
        &mut self,
        session_id: SessionId,
        generation: u64,
        now: MonotonicMs,
        containment: crate::containment::ContainmentRecord,
    ) -> Result<(), JournalError> {
        self.journal
            .reserve(session_id, generation, now, containment)
    }

    /// Commit the observed identity after the spawn returned.
    ///
    /// # Errors
    ///
    /// Propagates an unknown/committed record or a filesystem failure.
    pub fn commit(
        &mut self,
        session_id: SessionId,
        identity: crate::orphan::identity::ProcessIdentity,
    ) -> Result<(), JournalError> {
        self.journal.commit(session_id, identity)
    }

    /// Current stored counts.
    #[must_use]
    pub fn counts(&self) -> OrphanCounts {
        self.journal.counts()
    }

    /// The enforced containment mechanism.
    #[must_use]
    pub const fn mechanism(&self) -> ContainmentKind {
        self.mechanism
    }

    /// Why a weaker mechanism is enforced, when applicable.
    #[must_use]
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    fn status_json(&self) -> Value {
        json!({
            "verify": "served",
            "mechanism": self.mechanism.as_str(),
            "claim": self.mechanism.cleanup_claim(),
            "fallback": self.fallback_reason,
            "records": {
                "reserved": self.journal.counts().reserved,
                "committed": self.journal.counts().committed,
                "unverified": self.journal.counts().unverified,
            },
        })
    }
}

/// The service-facing orphan surface: served, or typed unsupported.
#[derive(Debug)]
pub enum OrphanSupervisor {
    /// A Linux verifier over the owner-private journal.
    Served(Box<OrphanRuntime>),
    /// No accepted containment mechanism or no boot id.
    Unsupported {
        /// The compiled cell.
        cell: PlatformCell,
        /// Static reason; never a path or caller text.
        reason: String,
    },
}

impl OrphanSupervisor {
    /// Build the served verifier, or a typed unsupported surface.
    ///
    /// # Errors
    ///
    /// Refuses daemon startup when the plan is enforced but the journal cannot
    /// be opened; an unsupported cell returns the typed refusal instead.
    pub fn open(
        directory: PathBuf,
        limits: &OrphanJournalLimits,
        report: &ContainmentReport,
        containment_limits: crate::containment::ContainmentLimits,
    ) -> Result<Self, OrphanOpenError> {
        if !report.is_enforced() {
            return Ok(Self::Unsupported {
                cell: report.cell(),
                reason: report
                    .refusal()
                    .map_or_else(|| "containment refused".to_owned(), ToString::to_string),
            });
        }
        let runtime = OrphanRuntime::open(directory, *limits, report, containment_limits)?;
        Ok(Self::Served(Box::new(runtime)))
    }

    /// Build a typed unsupported surface (tests and non-served cells).
    #[must_use]
    pub fn unsupported(cell: PlatformCell, reason: impl Into<String>) -> Self {
        Self::Unsupported {
            cell,
            reason: reason.into(),
        }
    }

    /// Whether `daemon.orphans.verify` is served.
    #[must_use]
    pub const fn is_served(&self) -> bool {
        matches!(self, Self::Served(_))
    }

    /// The wire status object carried by `daemon.status`.
    #[must_use]
    pub fn status_json(&self) -> Value {
        match self {
            Self::Served(runtime) => runtime.status_json(),
            Self::Unsupported { cell, reason } => json!({
                "verify": "unsupported_capability",
                "mechanism": MECHANISM_NONE,
                "cell": cell.name(),
                "reason": reason,
            }),
        }
    }

    /// Run one verification pass.
    ///
    /// # Errors
    ///
    /// Returns `None` for an unsupported cell and propagates a journal failure.
    pub fn verify(&mut self, now: MonotonicMs) -> Result<Option<OrphanVerifyReport>, JournalError> {
        match self {
            Self::Served(runtime) => runtime.verify(now).map(Some),
            Self::Unsupported { .. } => Ok(None),
        }
    }

    /// Run one on-demand pass under this container's pass budget.
    ///
    /// # Errors
    ///
    /// Returns `None` for an unsupported cell and propagates a journal failure.
    pub fn verify_on_demand(
        &mut self,
        now: MonotonicMs,
    ) -> Result<Option<OrphanVerifyReport>, JournalError> {
        self.verify_on_demand_excluding(now, &std::collections::BTreeSet::new())
    }

    /// Run one on-demand pass while skipping the records of live sessions.
    ///
    /// # Errors
    ///
    /// Returns `None` for an unsupported cell and propagates a journal failure.
    pub fn verify_on_demand_excluding(
        &mut self,
        now: MonotonicMs,
        live: &std::collections::BTreeSet<SessionId>,
    ) -> Result<Option<OrphanVerifyReport>, JournalError> {
        match self {
            Self::Served(runtime) => {
                let budget = runtime.pass_budget();
                runtime
                    .verify_within_excluding(now, Some(budget), live)
                    .map(Some)
            }
            Self::Unsupported { .. } => Ok(None),
        }
    }

    /// Retire one quarantined record without signalling or adopting it.
    ///
    /// # Errors
    ///
    /// Returns `None` for an unsupported cell and propagates a journal failure.
    pub fn retire(&mut self, record: SessionId) -> Result<Option<OrphanCounts>, JournalError> {
        match self {
            Self::Served(runtime) => runtime.retire(record).map(Some),
            Self::Unsupported { .. } => Ok(None),
        }
    }

    /// Settle the journal record of a worker after its terminal transition.
    ///
    /// An unsupported cell has no journal, so this is a no-op there.
    ///
    /// # Errors
    ///
    /// Propagates a journal rewrite or removal failure.
    pub fn settle(
        &mut self,
        session_id: SessionId,
        cleanup: crate::registry::CleanupClass,
        now: MonotonicMs,
    ) -> Result<(), JournalError> {
        match self {
            Self::Served(runtime) => runtime.settle(session_id, cleanup, now),
            Self::Unsupported { .. } => Ok(()),
        }
    }

    /// Reserve the pre-spawn journal record.
    ///
    /// An unsupported cell has no journal and no worker path, so this is a
    /// no-op there.
    ///
    /// # Errors
    ///
    /// Propagates a full journal, an oversized record, a duplicate record, or
    /// a filesystem failure.
    pub fn reserve(
        &mut self,
        session_id: SessionId,
        generation: u64,
        now: MonotonicMs,
        containment: crate::containment::ContainmentRecord,
    ) -> Result<(), JournalError> {
        match self {
            Self::Served(runtime) => runtime.reserve(session_id, generation, now, containment),
            Self::Unsupported { .. } => Ok(()),
        }
    }

    /// Commit the observed identity after the spawn returned.
    ///
    /// # Errors
    ///
    /// Propagates an unknown/committed record or a filesystem failure.
    pub fn commit(
        &mut self,
        session_id: SessionId,
        identity: crate::orphan::identity::ProcessIdentity,
    ) -> Result<(), JournalError> {
        match self {
            Self::Served(runtime) => runtime.commit(session_id, identity),
            Self::Unsupported { .. } => Ok(()),
        }
    }

    /// The enforced mechanism name.
    #[must_use]
    pub fn mechanism(&self) -> &'static str {
        match self {
            Self::Served(runtime) => runtime.mechanism().as_str(),
            Self::Unsupported { .. } => MECHANISM_NONE,
        }
    }
}

/// Derive journal bounds from the advertised limits.
#[must_use]
pub fn journal_limits(limits: &Limits) -> OrphanJournalLimits {
    OrphanJournalLimits::from_limits(limits)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::containment::{ContainmentRecord, ContainmentReport};
    use crate::orphan::identity::{Pid, ProcessObservation, ProcessProbe, ProcessStartTime};
    use crate::orphan::journal::{OrphanJournal, OrphanJournalLimits};

    fn plan() -> ContainmentReport {
        // A subreaper plan is enough for an empty journal and for quarantine
        // paths; no test here needs a live cgroup.
        ContainmentReport::subreaper(PlatformCell::current(), None)
    }

    fn runtime(directory: &TempDir) -> OrphanRuntime {
        OrphanRuntime::open(
            directory.path().join("orphans"),
            OrphanJournalLimits::default(),
            &plan(),
            crate::containment::ContainmentLimits::default(),
        )
        .expect("runtime opens")
    }

    #[test]
    fn an_empty_journal_reports_zeros_and_is_idempotent() {
        let directory = TempDir::new().expect("temp dir");
        let mut runtime = runtime(&directory);
        let first = runtime
            .verify(MonotonicMs::from_millis(1))
            .expect("first verify");
        let second = runtime
            .verify(MonotonicMs::from_millis(2))
            .expect("second verify");
        assert_eq!(first.reaped, 0);
        assert_eq!(first.unverified, 0);
        assert_eq!(first.stale, 0);
        assert_eq!(first.reserved, 0);
        assert_eq!(first.committed, 0);
        assert_eq!(second, first);
        assert_eq!(runtime.counts().total(), 0);
    }

    #[test]
    fn an_unsupported_cell_refuses_without_opening_a_journal() {
        let report = ContainmentReport::refused(
            PlatformCell::MacosArm64,
            crate::containment::ContainmentRefusal::UnsupportedPlatform {
                cell: PlatformCell::MacosArm64,
            },
        );
        let mut supervisor = OrphanSupervisor::open(
            PathBuf::from("/nonexistent/bittyd-orphans"),
            &OrphanJournalLimits::default(),
            &report,
            crate::containment::ContainmentLimits::default(),
        )
        .expect("unsupported is not an open failure");
        assert!(!supervisor.is_served());
        assert_eq!(supervisor.verify(MonotonicMs::ZERO).expect("verify"), None);
        assert_eq!(supervisor.mechanism(), MECHANISM_NONE);
        let status = supervisor.status_json();
        assert_eq!(
            status.get("verify").and_then(Value::as_str),
            Some("unsupported_capability")
        );
    }

    fn boot() -> BootId {
        BootId::read_system().expect("Linux boot id")
    }

    #[test]
    fn a_foreign_boot_record_is_stale_and_never_verified() {
        let directory = TempDir::new().expect("temp dir");
        let journal_dir = directory.path().join("orphans");
        let foreign =
            BootId::parse("00000000-0000-4000-8000-000000000001").expect("canonical boot id");
        {
            let mut journal =
                OrphanJournal::open(journal_dir.clone(), OrphanJournalLimits::default(), foreign)
                    .expect("journal");
            journal
                .reserve(
                    crate::registry::SessionId::parse(&"77".repeat(16)).expect("session"),
                    0,
                    MonotonicMs::from_millis(1),
                    ContainmentRecord::Subreaper,
                )
                .expect("reserve");
        }
        let mut runtime = runtime(&directory);
        let report = runtime.verify(MonotonicMs::from_millis(2)).expect("verify");
        assert_eq!(report.stale, 1);
        assert_eq!(report.reaped, 0);
        assert_eq!(report.unverified, 0);
        assert_eq!(runtime.counts().total(), 0);
    }

    #[test]
    fn an_unverifiable_record_is_reported_not_claimed_and_stays_quarantined() {
        let directory = TempDir::new().expect("temp dir");
        let mut decoy = Command::new("sleep").arg("30").spawn().expect("decoy");
        let pid = Pid::new(decoy.id()).expect("pid");
        let probe = SystemProbe::new();
        let ProcessObservation::Present {
            start_time,
            process_group,
            uid,
        } = probe.observe(pid)
        else {
            let _ = decoy.kill();
            let _ = decoy.wait();
            panic!("decoy must be observable");
        };
        let identity = crate::orphan::identity::ProcessIdentity::new(
            pid,
            ProcessStartTime::new(start_time.get()),
            process_group,
            uid,
        );
        {
            let mut journal = OrphanJournal::open(
                directory.path().join("orphans"),
                OrphanJournalLimits::default(),
                boot(),
            )
            .expect("journal");
            journal
                .reserve(
                    crate::registry::SessionId::parse(&"88".repeat(16)).expect("session"),
                    0,
                    MonotonicMs::from_millis(1),
                    ContainmentRecord::Subreaper,
                )
                .expect("reserve");
            journal
                .commit(
                    crate::registry::SessionId::parse(&"88".repeat(16)).expect("session"),
                    identity,
                )
                .expect("commit");
        }

        let mut runtime = runtime(&directory);
        let first = runtime
            .verify(MonotonicMs::from_millis(2))
            .expect("first verify");
        assert_eq!(first.unverified, 1);
        assert_eq!(first.reaped, 0);
        assert_eq!(first.committed, 0, "the record is quarantined");
        assert!(
            crate::orphan::identity::observe(pid) != ProcessObservation::Absent,
            "the subreaper record must never be signalled"
        );
        let second = runtime
            .verify(MonotonicMs::from_millis(3))
            .expect("second verify");
        assert_eq!(second, first);
        let _ = decoy.kill();
        let _ = decoy.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_absent_cgroup_subtree_is_already_reaped() {
        let report = ContainmentReport::probe();
        let Some(root) = report.cgroup_root() else {
            eprintln!("cgroup unavailable; typed skip");
            return;
        };
        let directory = TempDir::new().expect("temp dir");
        let path = root.join(format!("bittyd-test-gone-{}", std::process::id()));
        let identity = crate::orphan::identity::ProcessIdentity::new(
            Pid::new(1).expect("pid"),
            ProcessStartTime::new(1),
            crate::orphan::identity::ProcessGroupId::new(1).expect("group"),
            0,
        );
        {
            let mut journal = OrphanJournal::open(
                directory.path().join("orphans"),
                OrphanJournalLimits::default(),
                boot(),
            )
            .expect("journal");
            let session = crate::registry::SessionId::parse(&"99".repeat(16)).expect("session");
            journal
                .reserve(
                    session,
                    0,
                    MonotonicMs::from_millis(1),
                    ContainmentRecord::cgroup(&path),
                )
                .expect("reserve");
            journal.commit(session, identity).expect("commit");
        }
        let mut runtime = OrphanRuntime::open(
            directory.path().join("orphans"),
            OrphanJournalLimits::default(),
            &report,
            crate::containment::ContainmentLimits::default(),
        )
        .expect("runtime");
        let report = runtime.verify(MonotonicMs::from_millis(2)).expect("verify");
        assert_eq!(report.reaped, 1);
        assert_eq!(report.unverified, 0);
    }

    #[test]
    fn the_status_shape_never_carries_a_pid_or_path() {
        let directory = TempDir::new().expect("temp dir");
        let runtime = runtime(&directory);
        let status = runtime.status_json().to_string();
        for forbidden in ["pid", "path", "bittyd-", "argv", "env"] {
            assert!(!status.contains(forbidden), "{forbidden} leaked: {status}");
        }
        assert!(status.contains("\"verify\":\"served\""));
        assert!(status.contains("subreaper_fallback"));
    }
}
