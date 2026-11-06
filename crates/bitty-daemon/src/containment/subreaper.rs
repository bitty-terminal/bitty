//! `PR_SET_CHILD_SUBREAPER` for the documented containment fallback.
//!
//! The flag makes this daemon the reaper for orphaned descendants of its
//! children, so a daemonized grandchild is not lost to init. A stop records a
//! pidfd for every live descendant before it signals, then claims `swept` only
//! when the recorded process group is empty, the leader pidfd reports exit,
//! and every recorded descendant pidfd reports exit. A `setsid` survivor is
//! still a descendant at scan time, so it fails the check; an unreadable
//! `/proc` or a missing pidfd makes the cleanup `unverified` rather than
//! assuming a sweep. A natural exit is observed after the tree detaches and
//! stays `unverified`.

use nix::sys::prctl;

/// Why the subreaper fallback is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubreaperRefusal {
    /// The `errno` reported by `prctl(2)`.
    pub errno: i32,
}

impl std::fmt::Display for SubreaperRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "PR_SET_CHILD_SUBREAPER failed (errno {})",
            self.errno
        )
    }
}

impl std::error::Error for SubreaperRefusal {}

/// A confirmed subreaper setting for this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubreaperGuard(());

impl SubreaperGuard {
    /// Set and confirm `PR_SET_CHILD_SUBREAPER` for this process.
    ///
    /// Idempotent: the daemon may call it again before a spawn.
    ///
    /// # Errors
    ///
    /// Returns [`SubreaperRefusal`] when the kernel refuses the setting or the
    /// read-back does not confirm it.
    pub fn enable() -> Result<Self, SubreaperRefusal> {
        prctl::set_child_subreaper(true).map_err(errno)?;
        match prctl::get_child_subreaper() {
            Ok(true) => Ok(Self(())),
            Ok(false) => Err(SubreaperRefusal { errno: 0 }),
            Err(error) => Err(errno(error)),
        }
    }
}

fn errno(error: nix::errno::Errno) -> SubreaperRefusal {
    SubreaperRefusal {
        errno: error as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_reads_back_true_after_enabling() {
        let guard = SubreaperGuard::enable().expect("subreaper is available on test hosts");
        assert_eq!(guard, SubreaperGuard::enable().expect("idempotent"));
    }
}
