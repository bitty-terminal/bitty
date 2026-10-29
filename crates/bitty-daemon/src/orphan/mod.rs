//! The bounded orphan journal, production verifier, and exact-identity reaping
//! seam (bitty#2601, bitty#2791; contract `D-06`).
//!
//! The journal records the fencing tuple for a spawn before the worker can be
//! observed, keeps at most `max_orphan_records` records inside an owner-private
//! directory, and classifies each record against the current boot id, process
//! facts, and the recorded containment identity. Only a verified record reaches
//! [`OrphanReaper`]: a foreign boot id is stale and discarded; an unverified
//! record is quarantined and never signalled; a cgroup record is actioned
//! through its pre-created subtree; and the subreaper fallback is refused as
//! unverified because a process group cannot prove a whole tree.
//!
//! [`OrphanSupervisor`] is the served production surface behind
//! `daemon.orphans.verify`. It reports counts by class and never a pid, and it
//! is idempotent once every examined record is swept or quarantined; a record
//! deferred by the pass budget is retried by the next pass.
//!
//! Still out of this slice: the executable path digest and a native Windows
//! job-object reaper (designed, typed refusal). The journal never claims a
//! sweep without a reaper that verified the whole cgroup identity.

pub(crate) mod identity;
pub(crate) mod journal;
pub(crate) mod reaper;
pub(crate) mod runtime;

pub use self::identity::{
    BootId, BootIdError, Pid, ProcessGroupId, ProcessIdentity, ProcessObservation, ProcessProbe,
    ProcessStartTime, SystemProbe, VerifiedOrphan,
};
pub use self::journal::{
    JournalError, MAX_ORPHAN_RECORDS_SYSTEM_MAX, ORPHAN_RECORD_BYTES_SYSTEM_MAX,
    ORPHAN_RECORD_RETENTION_MS_DEFAULT, ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX, OrphanCounts,
    OrphanJournal, OrphanJournalLimits, OrphanReaper, ReapReport, ReapResult,
};
pub use self::reaper::SystemOrphanReaper;
pub use self::runtime::{
    ORPHAN_SUBDIR, OrphanOpenError, OrphanRuntime, OrphanSupervisor, OrphanVerifyReport,
    journal_limits,
};
