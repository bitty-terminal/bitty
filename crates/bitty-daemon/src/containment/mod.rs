//! Process containment for the `bittyd` session host (contract `D-06`,
//! `D-15`; bitty#2791).
//!
//! Containment is a whole-tree identity, never a bare pid or a process-group
//! convention. This module exposes one typed probe and two enforced plans:
//!
//! | Plan | Mechanism | Whole-tree proof | Cleanup claim |
//! | --- | --- | --- | --- |
//! | `ContainmentKind::CgroupV2` | delegated cgroup v2 subtree | `cgroup.procs` empty and `cgroup.events` `populated 0` | `swept` |
//! | `ContainmentKind::Subreaper` | `PR_SET_CHILD_SUBREAPER` + recorded process group | recorded pids gone, no adopted child | `unverified` (always) |
//! | `ContainmentKind::Refused` | none | none | no spawn, typed refusal |
//!
//! The fallback is deliberately weaker and says so. A process group alone can
//! never prove a whole tree (`setsid`/`setpgid` escapes it), so the subreaper
//! plan kills only what it recorded and reports `unverified` instead of
//! claiming `swept`. Only the cgroup plan can produce a verified sweep.
//!
//! Selection is explicit: [`ContainmentReport::probe`] returns the plan it selected with the
//! reason the stronger plan was unavailable. There is no silent downgrade, no
//! name/pattern lookup, and no pid signal for a process the daemon did not
//! record.
//!
//! Platform cells other than Linux return
//! [`ContainmentRefusal::UnsupportedPlatform`]. Windows has a designed job
//! object boundary (one job per session, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
//! DACL owned by the current user) that needs the reviewed Win32 seam in
//! `bitty-winjob` plus a native-runner campaign; macOS has no containment
//! implementation in this slice. Both refuse rather than approximate.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::limits::{LimitId, Limits};
use crate::platform::PlatformCell;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod child;
#[cfg(target_os = "linux")]
mod linux_sys;
#[cfg(target_os = "linux")]
mod subreaper;

#[cfg(target_os = "linux")]
pub use self::cgroup::{CGROUP2_MOUNT, CgroupError, CgroupRefusal, CgroupRoot, SessionCgroup};
#[cfg(target_os = "linux")]
pub(crate) use self::child::drain_adopted_children;
#[cfg(target_os = "linux")]
pub use self::child::{
    CleanupOutcome, ContainedChild, ContainmentError, SessionMode, spawn, spawn_with,
};
#[cfg(target_os = "linux")]
pub(crate) use self::linux_sys::{
    establish_controlling_terminal, resize_pty, set_cloexec, set_nonblocking,
};
#[cfg(target_os = "linux")]
pub use self::subreaper::{SubreaperGuard, SubreaperRefusal};

/// Stable diagnostic name of the cgroup v2 plan.
pub const MECHANISM_CGROUP_V2: &str = "cgroup_v2";

/// Stable diagnostic name of the subreaper fallback.
pub const MECHANISM_SUBREAPER: &str = "subreaper_fallback";

/// Stable diagnostic name of a refused probe.
pub const MECHANISM_NONE: &str = "none";

/// Prefix every session cgroup name must carry.
pub const SESSION_CGROUP_PREFIX: &str = "bittyd-";

/// Maximum session cgroup name length.
///
/// Fits `bittyd-` plus a 32-hex daemon instance id and a 32-hex session id,
/// with separators, and stays far below the kernel's 255-byte name bound.
pub const SESSION_CGROUP_NAME_MAX: usize = 128;

/// Bounded escalation and reaping windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainmentLimits {
    term_grace: Duration,
    kill_wait: Duration,
    poll: Duration,
}

impl ContainmentLimits {
    /// Default polling interval for the emptiness loop.
    pub const POLL: Duration = Duration::from_millis(10);

    /// Build explicit bounds.
    #[must_use]
    pub const fn new(term_grace: Duration, kill_wait: Duration) -> Self {
        Self {
            term_grace,
            kill_wait,
            poll: Self::POLL,
        }
    }

    /// Derive the orphan-reap graceful window from the advertised limit.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self::new(
            Duration::from_millis(limits.effective(LimitId::OrphanReapGraceMs)),
            Duration::from_secs(2),
        )
    }

    /// Derive the terminal-session abort window from the advertised limit.
    #[must_use]
    pub fn for_session_stop(limits: &Limits) -> Self {
        Self::new(
            Duration::from_millis(limits.effective(LimitId::AbortGraceMs)),
            Duration::from_secs(2),
        )
    }

    /// The graceful window after `SIGTERM`.
    #[must_use]
    pub const fn term_grace(self) -> Duration {
        self.term_grace
    }

    /// The window after `SIGKILL`.
    #[must_use]
    pub const fn kill_wait(self) -> Duration {
        self.kill_wait
    }

    /// The polling interval inside a bounded wait.
    #[must_use]
    pub const fn poll(self) -> Duration {
        self.poll
    }

    /// The most one on-demand verification pass may block the caller.
    ///
    /// One record is allowed `term_grace + kill_wait`; the deadline clamps
    /// every subsequent record's waits, so a journal full of live orphans
    /// cannot make one request hold the service loop for minutes.
    #[must_use]
    pub const fn pass_budget(self) -> Duration {
        self.term_grace.saturating_add(self.kill_wait)
    }
}

impl Default for ContainmentLimits {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(2))
    }
}

/// Why a containment mechanism is not available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentRefusal {
    /// The platform cell has no accepted containment mechanism.
    UnsupportedPlatform {
        /// The compiled cell.
        cell: PlatformCell,
    },
    /// This invocation did not probe the host, so it makes no claim about the
    /// selected plan. Used by the side-effect-free `bittyd` status report; the
    /// served daemon probes once at startup.
    NotProbed,
    /// Neither cgroup v2 nor the subreaper fallback is usable.
    NoMechanism {
        /// Why the cgroup probe failed.
        cgroup: String,
        /// Why the subreaper probe failed.
        subreaper: String,
    },
}

impl fmt::Display for ContainmentRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { cell } => {
                write!(formatter, "unsupported platform cell {cell}")
            }
            Self::NotProbed => formatter.write_str("not probed by this invocation"),
            Self::NoMechanism { cgroup, subreaper } => write!(
                formatter,
                "no containment mechanism: cgroup v2 {cgroup}; subreaper {subreaper}"
            ),
        }
    }
}

impl std::error::Error for ContainmentRefusal {}

/// The selected containment mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentKind {
    /// Delegated cgroup v2 subtree; the only plan that can claim `swept`.
    CgroupV2,
    /// `PR_SET_CHILD_SUBREAPER` plus the recorded process group.
    Subreaper,
    /// No mechanism; the daemon must not spawn a contained child.
    Refused,
}

impl ContainmentKind {
    /// Stable diagnostic name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CgroupV2 => MECHANISM_CGROUP_V2,
            Self::Subreaper => MECHANISM_SUBREAPER,
            Self::Refused => MECHANISM_NONE,
        }
    }

    /// Whether this plan can enforce a whole-tree kill.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::CgroupV2 | Self::Subreaper)
    }

    /// The cleanup class this plan can truthfully claim.
    #[must_use]
    pub const fn cleanup_claim(self) -> &'static str {
        match self {
            Self::CgroupV2 => "swept",
            Self::Subreaper => "unverified",
            Self::Refused => "none",
        }
    }
}

impl fmt::Display for ContainmentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The containment identity recorded for one spawn.
///
/// A cgroup record names the delegated subtree created before the spawn; a
/// subreaper record has no pre-spawn identity, which is exactly why a reserved
/// subreaper record is quarantined after a crash instead of being reaped by
/// pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentRecord {
    /// A per-session cgroup v2 subtree.
    Cgroup {
        /// Absolute path of the session subtree.
        path: PathBuf,
    },
    /// The subreaper fallback: no path, no pre-spawn identity.
    Subreaper,
}

impl ContainmentRecord {
    /// Build a cgroup record from the session subtree path.
    #[must_use]
    pub fn cgroup(path: impl Into<PathBuf>) -> Self {
        Self::Cgroup { path: path.into() }
    }

    /// Build the fallback record.
    #[must_use]
    pub const fn subreaper() -> Self {
        Self::Subreaper
    }

    /// The recorded cgroup path, when this is a cgroup record.
    #[must_use]
    pub fn cgroup_path(&self) -> Option<&Path> {
        match self {
            Self::Cgroup { path } => Some(path),
            Self::Subreaper => None,
        }
    }

    /// Stable mechanism name.
    #[must_use]
    pub const fn mechanism(&self) -> &'static str {
        match self {
            Self::Cgroup { .. } => MECHANISM_CGROUP_V2,
            Self::Subreaper => MECHANISM_SUBREAPER,
        }
    }
}

/// A probe result: the compiled cell and the plan it selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentReport {
    cell: PlatformCell,
    kind: ContainmentKind,
    refusal: Option<ContainmentRefusal>,
    fallback_reason: Option<String>,
    cgroup_root: Option<PathBuf>,
}

impl ContainmentReport {
    /// Probe this process's containment capability.
    ///
    /// On Linux this creates and removes a probe cgroup under the delegated
    /// root (a write that proves delegation rather than assuming it). A cgroup
    /// failure falls back to the subreaper only when that mechanism is actually
    /// available; otherwise the report is
    /// [`ContainmentKind::Refused`] with both reasons recorded.
    #[must_use]
    pub fn probe() -> Self {
        #[cfg(target_os = "linux")]
        {
            let cell = PlatformCell::current();
            match CgroupRoot::detect() {
                Ok(root) => Self {
                    cell,
                    kind: ContainmentKind::CgroupV2,
                    refusal: None,
                    fallback_reason: None,
                    cgroup_root: Some(root.path().to_path_buf()),
                },
                Err(cgroup_refusal) => match SubreaperGuard::enable() {
                    Ok(_) => Self {
                        cell,
                        kind: ContainmentKind::Subreaper,
                        refusal: None,
                        fallback_reason: Some(cgroup_refusal.to_string()),
                        cgroup_root: None,
                    },
                    Err(subreaper_refusal) => Self {
                        cell,
                        kind: ContainmentKind::Refused,
                        refusal: Some(ContainmentRefusal::NoMechanism {
                            cgroup: cgroup_refusal.to_string(),
                            subreaper: subreaper_refusal.to_string(),
                        }),
                        fallback_reason: None,
                        cgroup_root: None,
                    },
                },
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let cell = PlatformCell::current();
            Self {
                cell,
                kind: ContainmentKind::Refused,
                refusal: Some(ContainmentRefusal::UnsupportedPlatform { cell }),
                fallback_reason: None,
                cgroup_root: None,
            }
        }
    }

    /// Build a report that claims nothing because this invocation did not
    /// probe the host.
    ///
    /// Probing creates and removes a cgroup, so a status or diagnostic command
    /// must not do it. The daemon probes once at startup; this constructor is
    /// for the display-only seam status.
    #[must_use]
    pub fn unprobed(cell: PlatformCell) -> Self {
        Self {
            cell,
            kind: ContainmentKind::Refused,
            refusal: Some(ContainmentRefusal::NotProbed),
            fallback_reason: None,
            cgroup_root: None,
        }
    }

    /// Build a report for tests and typed refusals without touching the host.
    #[must_use]
    pub fn refused(cell: PlatformCell, refusal: ContainmentRefusal) -> Self {
        Self {
            cell,
            kind: ContainmentKind::Refused,
            refusal: Some(refusal),
            fallback_reason: None,
            cgroup_root: None,
        }
    }

    /// Build a subreaper report for tests.
    #[must_use]
    pub fn subreaper(cell: PlatformCell, fallback_reason: Option<String>) -> Self {
        Self {
            cell,
            kind: ContainmentKind::Subreaper,
            refusal: None,
            fallback_reason,
            cgroup_root: None,
        }
    }

    /// Build a cgroup report for tests.
    #[must_use]
    pub fn cgroup_v2(cell: PlatformCell, root: PathBuf) -> Self {
        Self {
            cell,
            kind: ContainmentKind::CgroupV2,
            refusal: None,
            fallback_reason: None,
            cgroup_root: Some(root),
        }
    }

    /// The compiled platform cell.
    #[must_use]
    pub const fn cell(&self) -> PlatformCell {
        self.cell
    }

    /// The selected plan.
    #[must_use]
    pub const fn kind(&self) -> ContainmentKind {
        self.kind
    }

    /// The typed refusal, when the plan is [`ContainmentKind::Refused`].
    #[must_use]
    pub fn refusal(&self) -> Option<&ContainmentRefusal> {
        self.refusal.as_ref()
    }

    /// Why the stronger cgroup plan was not selected, when the fallback is in
    /// use.
    #[must_use]
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// The delegated cgroup root recorded by the probe.
    #[must_use]
    pub fn cgroup_root(&self) -> Option<&Path> {
        self.cgroup_root.as_deref()
    }

    /// Whether the plan can enforce a whole-tree kill.
    #[must_use]
    pub const fn is_enforced(&self) -> bool {
        self.kind.is_enforced()
    }

    /// The claim this plan can make about cleanup.
    #[must_use]
    pub const fn cleanup_claim(&self) -> &'static str {
        match self.kind {
            ContainmentKind::CgroupV2 => "swept",
            ContainmentKind::Subreaper => "unverified",
            ContainmentKind::Refused => "none",
        }
    }

    /// Render the one-line status used by `bittyd` and diagnostics.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.kind {
            ContainmentKind::CgroupV2 => {
                format!("{MECHANISM_CGROUP_V2} (enforced; cleanup swept)")
            }
            ContainmentKind::Subreaper => {
                let reason = self.fallback_reason.as_deref().unwrap_or("no reason");
                format!("{MECHANISM_SUBREAPER} (enforced; cleanup unverified; {reason})")
            }
            ContainmentKind::Refused => {
                let reason = self
                    .refusal
                    .as_ref()
                    .map_or_else(|| "no reason".to_owned(), ToString::to_string);
                format!("{MECHANISM_NONE} (refused; {reason})")
            }
        }
    }
}

impl fmt::Display for ContainmentReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_report_names_the_platform_and_claims_nothing() {
        let report = ContainmentReport::refused(
            PlatformCell::MacosArm64,
            ContainmentRefusal::UnsupportedPlatform {
                cell: PlatformCell::MacosArm64,
            },
        );
        assert_eq!(report.kind(), ContainmentKind::Refused);
        assert!(!report.is_enforced());
        assert_eq!(report.cleanup_claim(), "none");
        assert!(report.summary().contains("refused"));
        assert!(report.summary().contains("macos-arm64"));
    }

    #[test]
    fn the_fallback_reports_unverified_and_the_reason() {
        let report =
            ContainmentReport::subreaper(PlatformCell::LinuxX86_64, Some("no delegation".into()));
        assert_eq!(report.kind(), ContainmentKind::Subreaper);
        assert_eq!(report.cleanup_claim(), "unverified");
        assert_eq!(report.fallback_reason(), Some("no delegation"));
        assert!(report.summary().contains("subreaper_fallback"));
        assert!(report.summary().contains("unverified"));
    }

    #[test]
    fn cgroup_is_the_only_swept_claim() {
        let report = ContainmentReport::cgroup_v2(
            PlatformCell::LinuxX86_64,
            PathBuf::from("/sys/fs/cgroup/probe"),
        );
        assert_eq!(report.kind(), ContainmentKind::CgroupV2);
        assert_eq!(report.cleanup_claim(), "swept");
        assert!(report.is_enforced());
    }

    #[test]
    fn records_round_trip_their_mechanism_without_argv_or_content() {
        let record = ContainmentRecord::cgroup("/sys/fs/cgroup/bittyd-a/b");
        assert_eq!(record.mechanism(), MECHANISM_CGROUP_V2);
        assert_eq!(
            record.cgroup_path(),
            Some(Path::new("/sys/fs/cgroup/bittyd-a/b"))
        );
        assert_eq!(
            ContainmentRecord::subreaper().mechanism(),
            MECHANISM_SUBREAPER
        );
        assert!(ContainmentRecord::subreaper().cgroup_path().is_none());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_non_linux_cell_refuses_with_unsupported_platform() {
        let report = ContainmentReport::probe();
        assert_eq!(report.kind(), ContainmentKind::Refused);
        assert!(!report.is_enforced());
        assert_eq!(report.cleanup_claim(), "none");
        assert!(matches!(
            report.refusal(),
            Some(ContainmentRefusal::UnsupportedPlatform { cell }) if *cell == report.cell()
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_live_probe_selects_an_enforced_plan_or_a_typed_refusal() {
        let report = ContainmentReport::probe();
        assert_eq!(report.cell(), PlatformCell::current());
        match report.kind() {
            ContainmentKind::CgroupV2 => assert_eq!(report.cleanup_claim(), "swept"),
            ContainmentKind::Subreaper => assert_eq!(report.cleanup_claim(), "unverified"),
            ContainmentKind::Refused => {
                assert!(report.refusal().is_some());
                assert!(!report.is_enforced());
            }
        }
    }
}
