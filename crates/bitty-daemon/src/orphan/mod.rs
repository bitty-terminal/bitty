//! The bounded orphan journal and the exact-identity reaping seam
//! (bitty#2601, contract `D-06`).
//!
//! The journal records the fencing tuple for a spawn before the worker can be
//! observed, keeps at most `max_orphan_records` records inside an owner-private
//! directory, and classifies each record against the current boot id and
//! process facts. Only an exact `(boot id, pid, start time, process group)`
//! match reaches [`OrphanReaper`]; a reserved record, a different boot id, an
//! absent pid, an unreadable start time, or a reused pid is quarantined or
//! discarded and is never signalled.
//!
//! Deferred to bitty#2604: cgroup v2 / subreaper containment, the executable
//! path digest, the actual graceful-then-kill schedule, platform probes beyond
//! Linux `/proc`, and the Windows owner-only directory ACL. The journal never
//! claims a sweep without a reaper that verifies the whole identity.

mod identity;
mod journal;

pub use self::identity::{
    BootId, BootIdError, Pid, ProcessGroupId, ProcessIdentity, ProcessObservation, ProcessProbe,
    ProcessStartTime, SystemProbe, VerifiedOrphan,
};
pub use self::journal::{
    JournalError, MAX_ORPHAN_RECORDS_SYSTEM_MAX, ORPHAN_RECORD_BYTES_SYSTEM_MAX,
    ORPHAN_RECORD_RETENTION_MS_DEFAULT, ORPHAN_RECORD_RETENTION_MS_SYSTEM_MAX, OrphanCounts,
    OrphanJournal, OrphanJournalLimits, OrphanReaper, ReapReport, ReapResult,
};
