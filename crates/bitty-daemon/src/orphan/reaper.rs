//! The production orphan reaper (contract `D-06`; bitty#2791).
//!
//! The journal hands this implementation only a [`VerifiedOrphan`], and the
//! implementation acts only on the recorded containment identity:
//!
//! - a recorded cgroup subtree is opened only when it is a direct child of the
//!   delegated root and owned by this uid; members are signalled through
//!   `cgroup.procs` and escalated through `cgroup.kill`; an empty or absent
//!   subtree is already reaped;
//! - a subreaper record is refused as `unverified`, because a restarted daemon
//!   cannot prove the whole descendant set from a process group. It is never
//!   signalled on a person's machine just to look busy.
//!
//! No branch resolves an executable name, scans for a pattern, or signals a
//! pid outside the recorded identity. The reaper never returns `Swept` without
//! an empty cgroup subtree.

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use nix::sys::signal::Signal;

use crate::containment::ContainmentLimits;
#[cfg(target_os = "linux")]
use crate::containment::{
    CgroupError, CgroupRoot, ContainmentKind, ContainmentRecord, ContainmentReport, SessionCgroup,
};
use crate::orphan::identity::VerifiedOrphan;
use crate::orphan::journal::{OrphanReaper, ReapResult};

/// Cgroup-backed reaper for one daemon lifetime.
#[derive(Debug)]
pub struct SystemOrphanReaper {
    #[cfg(target_os = "linux")]
    root: Option<CgroupRoot>,
    #[cfg(target_os = "linux")]
    limits: ContainmentLimits,
    /// Absolute budget for the current pass, set by the caller for an
    /// on-demand pass. `None` means the full per-record window applies.
    #[cfg(target_os = "linux")]
    deadline: Option<Instant>,
}

#[cfg(target_os = "linux")]
impl SystemOrphanReaper {
    /// Build the reaper from the probe result and the advertised bounds.
    #[must_use]
    pub fn new(report: &ContainmentReport, limits: ContainmentLimits) -> Self {
        let root = match report.kind() {
            ContainmentKind::CgroupV2 => report
                .cgroup_root()
                .and_then(|path| CgroupRoot::from_absolute(path.to_path_buf()).ok()),
            ContainmentKind::Subreaper | ContainmentKind::Refused => None,
        };
        Self {
            root,
            limits,
            deadline: None,
        }
    }

    /// Bound the current pass to `deadline`.
    ///
    /// Every wait is clamped to the remaining budget, so one on-demand pass
    /// cannot block the service loop for the full per-record window on every
    /// remaining record.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    #[cfg(test)]
    fn with_root(root: CgroupRoot, limits: ContainmentLimits) -> Self {
        Self {
            root: Some(root),
            limits,
            deadline: None,
        }
    }

    fn reap_cgroup(&self, path: &std::path::Path) -> ReapResult {
        let Some(root) = self.root.as_ref() else {
            return ReapResult::Unverified;
        };
        let cgroup = match root.open_recorded(path) {
            Ok(cgroup) => cgroup,
            // A recorded subtree that is already gone was reaped before this
            // pass. A path this root does not own, or one owned by another uid,
            // is never touched.
            Err(CgroupError::Missing) => return ReapResult::Swept,
            Err(CgroupError::OutsideRoot | CgroupError::ForeignOwner { .. }) => {
                return ReapResult::Unverified;
            }
            Err(_) => return ReapResult::Unverified,
        };
        if cgroup.verify_empty().unwrap_or(false) {
            return swept(cgroup.remove());
        }

        let _ = cgroup.signal_members(Signal::SIGTERM);
        if wait_empty(
            &cgroup,
            self.limits.term_grace(),
            self.limits.poll(),
            self.deadline,
        ) {
            return swept(cgroup.remove());
        }
        let _ = cgroup.kill_all();
        if wait_empty(
            &cgroup,
            self.limits.kill_wait(),
            self.limits.poll(),
            self.deadline,
        ) {
            return swept(cgroup.remove());
        }
        ReapResult::Unverified
    }
}

/// `swept` requires the subtree removed, not only an empty read. A removal
/// failure leaves the identity in place and is reported as unverified.
#[cfg(target_os = "linux")]
fn swept(removed: Result<(), CgroupError>) -> ReapResult {
    if removed.is_ok() {
        ReapResult::Swept
    } else {
        ReapResult::Unverified
    }
}

#[cfg(target_os = "linux")]
fn wait_empty(
    cgroup: &SessionCgroup,
    window: Duration,
    poll: Duration,
    budget: Option<Instant>,
) -> bool {
    let window = match budget {
        Some(budget) => window.min(budget.saturating_duration_since(Instant::now())),
        None => window,
    };
    let deadline = Instant::now() + window;
    loop {
        if cgroup.verify_empty().unwrap_or(false) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(not(target_os = "linux"))]
impl SystemOrphanReaper {
    /// No accepted containment mechanism on this cell.
    #[must_use]
    pub fn new(
        _report: &crate::containment::ContainmentReport,
        _limits: ContainmentLimits,
    ) -> Self {
        Self {}
    }

    /// No pass can run on a cell without a mechanism; the bound is unused.
    pub fn set_deadline(&mut self, _deadline: Option<std::time::Instant>) {}
}

impl OrphanReaper for SystemOrphanReaper {
    fn reap(&mut self, orphan: &VerifiedOrphan) -> ReapResult {
        #[cfg(target_os = "linux")]
        match orphan.containment() {
            ContainmentRecord::Cgroup { path } => self.reap_cgroup(path),
            // A restarted daemon is no longer the parent; the fallback cannot
            // prove the descendant set and must not claim a sweep.
            ContainmentRecord::Subreaper => ReapResult::Unverified,
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = orphan;
            ReapResult::Unverified
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::containment::ContainmentRefusal;
    use crate::orphan::identity::{
        BootId, Pid, ProcessGroupId, ProcessIdentity, ProcessStartTime, VerifiedOrphan,
    };
    use crate::platform::PlatformCell;

    fn boot() -> BootId {
        BootId::parse("44444444-4444-4444-8444-444444444444").expect("boot id")
    }

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(
            Pid::new(std::process::id()).expect("pid"),
            ProcessStartTime::new(1),
            ProcessGroupId::new(1).expect("group"),
            0,
        )
    }

    #[test]
    fn a_subreaper_record_is_refused_not_signalled() {
        let report = ContainmentReport::subreaper(PlatformCell::LinuxX86_64, None);
        let mut reaper = SystemOrphanReaper::new(&report, ContainmentLimits::default());
        let orphan = VerifiedOrphan::new(boot(), Some(identity()), ContainmentRecord::Subreaper);
        assert_eq!(reaper.reap(&orphan), ReapResult::Unverified);
    }

    #[test]
    fn a_cgroup_record_without_a_delegated_root_is_refused() {
        let report = ContainmentReport::refused(
            PlatformCell::LinuxX86_64,
            ContainmentRefusal::UnsupportedPlatform {
                cell: PlatformCell::LinuxX86_64,
            },
        );
        let mut reaper = SystemOrphanReaper::new(&report, ContainmentLimits::default());
        let orphan = VerifiedOrphan::new(
            boot(),
            None,
            ContainmentRecord::cgroup("/sys/fs/cgroup/not-ours/bittyd-x"),
        );
        assert_eq!(reaper.reap(&orphan), ReapResult::Unverified);
    }

    #[test]
    fn a_recorded_path_outside_the_root_is_refused() {
        let root = CgroupRoot::from_test_parts(
            std::path::PathBuf::from("/sys/fs/cgroup/bittyd-root"),
            std::path::PathBuf::from("bittyd-root"),
        );
        let mut reaper = SystemOrphanReaper::with_root(root, ContainmentLimits::default());
        let orphan = VerifiedOrphan::new(
            boot(),
            None,
            ContainmentRecord::cgroup("/sys/fs/cgroup/other/bittyd-x"),
        );
        assert_eq!(reaper.reap(&orphan), ReapResult::Unverified);
    }
}
