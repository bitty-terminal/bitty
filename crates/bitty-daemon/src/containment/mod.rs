//! Process containment for the `bittyd` session host (contract `D-06`,
//! `D-15`; bitty#2791, bitty#2949).
//!
//! Containment is a whole-tree identity, never a bare pid or a process-group
//! convention. This module owns the platform-neutral vocabulary; one
//! per-OS module owns the probe, spawn, and kill path:
//!
//! | Plan | Mechanism | Whole-tree proof | Cleanup claim | Reachable from the product path |
//! | --- | --- | --- | --- | --- |
//! | `ContainmentKind::CgroupV2` | delegated cgroup v2 subtree | `cgroup.procs` empty and `cgroup.events` `populated 0` | `swept` | yes |
//! | `ContainmentKind::Subreaper` | `PR_SET_CHILD_SUBREAPER` + recorded process group | recorded group empty, leader pidfd exited, every pre-signal descendant pidfd exited | `swept` when verified, else `unverified` | yes |
//! | `ContainmentKind::WindowsJob` | one kill-on-close job object per session | every member assigned before it runs; `ActiveProcesses` zero after termination | `swept` when verified | no: implemented-only |
//! | `ContainmentKind::FreebsdReaper` | `procctl(PROC_REAP_ACQUIRE)` reaper + recorded process group | recorded group empty, no live recorded descendant, and no live descendant the session cannot attribute elsewhere | `swept` when verified | no: implemented-only |
//! | `ContainmentKind::ProcessGroup` | recorded process group only (macOS) | the group is signalled and observed gone; a reparented or re-sessioned descendant is not covered | `group-signalled` (`D-05`) | no: implemented-only |
//! | `ContainmentKind::Refused` | none | none | no spawn, typed refusal | yes (it refuses) |
//!
//! # Reachability
//!
//! `probe()` selects a plan on every supported cell, but only the Linux plans
//! are on the served spawn path: [`crate::worker::spawn_path_served`] is
//! `cfg!(target_os = "linux")`, `crate::service::dispatch` refuses
//! `session.create` with `UnsupportedCapability` before any spawn on a cell it
//! does not serve, and `crate::service::server` exports the Linux-only `unix`
//! module with a typed stub everywhere else. The Windows, FreeBSD, and macOS
//! plans are therefore **implemented-only (unwired)**: they are exercised by
//! their own unit tests, and no product path reaches them until the PTY spawn
//! path is ported. `is_enforced()` describes the mechanism, not reachability.
//!
//! The fallback plans are deliberately weaker and say so. A process group
//! alone can never prove a whole tree (`setsid`/`setpgid` escapes it), so the
//! subreaper plan kills only what it recorded and reports `unverified` when a
//! check fails. macOS has no reaper, so its plan reports the dedicated
//! `group-signalled` cleanup class instead of borrowing `swept`. Only a plan
//! whose verification covers the whole recorded tree can produce a verified
//! sweep.
//!
//! Selection is explicit: [`ContainmentReport::probe`] returns the plan it
//! selected with the reason a stronger plan was unavailable. There is no
//! silent downgrade, no name/pattern lookup, and no pid signal for a process
//! the daemon did not record.
//!
//! # Platform cells
//!
//! - Linux: delegated cgroup v2, falling back to the subreaper plan.
//! - Windows: one job object per session, created before the spawn, with the
//!   child assigned while suspended (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
//!   Implemented-only: not on the served spawn path.
//! - FreeBSD: the daemon acquires the `procctl(2)` reaper role so orphaned
//!   descendants reparent to it, and records the child's process group.
//!   Implemented-only: not on the served spawn path.
//! - macOS: the recorded process group only; it must not claim `swept`.
//!   Implemented-only: not on the served spawn path.
//! - Any other cell, or a Windows build whose job object cannot be created,
//!   returns [`ContainmentRefusal::UnsupportedPlatform`].
//!
//! The FreeBSD `procctl(2)` calls need raw FFI and no safe wrapper exists for
//! `PROC_REAP_*`. Following the `bitty-sandbox` and `bitty-winjob` precedent,
//! the crate root keeps `#![deny(unsafe_code)]` and only the private
//! `freebsd_sys` module is allowed it, with every block carrying a `SAFETY`
//! rationale and a safe API above it.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::limits::{LimitId, Limits};
use crate::orphan::identity::ProcessGroupId;
use crate::platform::PlatformCell;
use crate::registry::CleanupClass;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod child;
mod exec;
#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)]
mod freebsd_sys;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod linux_sys;
#[cfg(target_os = "macos")]
mod macos;
mod plan;
#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
mod posix;
#[cfg(target_os = "linux")]
mod subreaper;
#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "macos"
)))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use self::cgroup::{CGROUP2_MOUNT, CgroupError, CgroupRefusal, CgroupRoot, SessionCgroup};
#[cfg(target_os = "linux")]
pub(crate) use self::child::drain_adopted_children;
pub use self::exec::{ExecRefusal, ExecRefusalKind};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use self::linux_sys::test_signal_state;
#[cfg(target_os = "linux")]
pub(crate) use self::linux_sys::{
    close_inherited_descriptors, dup_cloexec, establish_controlling_terminal,
    reset_child_signal_state, resize_pty, set_cloexec, set_nonblocking,
};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use self::linux_sys::{drop_cloexec_before_exec, inheritable_probe_descriptor};
pub use self::plan::ContainedChild;
#[cfg(target_os = "linux")]
pub use self::subreaper::{SubreaperGuard, SubreaperRefusal};

/// Test-only serialization for PTY creation and spawn.
///
/// `openpty` returns descriptors without close-on-exec, and a `fork` in a
/// concurrent test can inherit them for the brief window before they are
/// flagged or dropped. Tests that create a PTY and then spawn hold this lock,
/// so an exact child-descriptor assertion cannot observe another test's
/// transient handles.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_lock {
    use std::sync::{Mutex, MutexGuard};

    /// Serializes PTY creation plus the spawn that follows it.
    static PTY_SPAWN: Mutex<()> = Mutex::new(());

    /// Acquire the PTY spawn lock, ignoring poisoning: a panicking test must
    /// not make every later test fail on the lock instead of its own behavior.
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        PTY_SPAWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Stable diagnostic name of the cgroup v2 plan.
pub const MECHANISM_CGROUP_V2: &str = "cgroup_v2";

/// Stable diagnostic name of the subreaper fallback.
pub const MECHANISM_SUBREAPER: &str = "subreaper_fallback";

/// Stable diagnostic name of the Windows job-object plan.
pub const MECHANISM_WINDOWS_JOB: &str = "windows_job";

/// Stable diagnostic name of the FreeBSD `procctl(2)` reaper plan.
pub const MECHANISM_FREEBSD_REAPER: &str = "freebsd_reaper";

/// Stable diagnostic name of the macOS process-group plan.
pub const MECHANISM_PROCESS_GROUP: &str = "process_group";

/// Stable diagnostic name of a refused probe.
pub const MECHANISM_NONE: &str = "none";

/// Cleanup class of a plan that verified the whole recorded tree gone.
pub const CLEANUP_SWEPT: &str = "swept";

/// Cleanup class `D-05`: the recorded process group was signalled and observed
/// gone, but a reparented or re-sessioned descendant is not provably covered.
pub const CLEANUP_GROUP_SIGNALLED: &str = "group-signalled";

/// Cleanup class of a plan that started nothing.
pub const CLEANUP_NONE: &str = "none";

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
    /// Delegated cgroup v2 subtree; one of the plans that can claim `swept`.
    CgroupV2,
    /// `PR_SET_CHILD_SUBREAPER` plus the recorded process group, the leader
    /// pidfd, and descendant pidfds recorded before a stop signals; claims
    /// `swept` only when all of them verify gone.
    Subreaper,
    /// One kill-on-close Windows job object per session; the child is
    /// assigned before it runs and `swept` requires the job's active process
    /// count to reach zero.
    WindowsJob,
    /// The FreeBSD `procctl(2)` reaper plus the recorded process group;
    /// claims `swept` only when the group is empty and the reaper lists no
    /// live recorded descendant.
    FreebsdReaper,
    /// The recorded process group only. It can signal the group and observe
    /// it gone, but a reparented or re-sessioned descendant is not provably
    /// covered, so it reports [`CLEANUP_GROUP_SIGNALLED`] (`D-05`), never
    /// `swept`.
    ProcessGroup,
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
            Self::WindowsJob => MECHANISM_WINDOWS_JOB,
            Self::FreebsdReaper => MECHANISM_FREEBSD_REAPER,
            Self::ProcessGroup => MECHANISM_PROCESS_GROUP,
            Self::Refused => MECHANISM_NONE,
        }
    }

    /// Whether this plan can enforce a whole-tree kill.
    ///
    /// A process-group-only plan can signal the group but cannot prove a
    /// reparented or re-sessioned descendant is covered, so it is not a
    /// whole-tree mechanism.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(
            self,
            Self::CgroupV2 | Self::Subreaper | Self::WindowsJob | Self::FreebsdReaper
        )
    }

    /// Whether the served orphan verifier can re-check this plan's records
    /// after a restart.
    ///
    /// Only the Linux cgroup and subreaper plans record a durable identity the
    /// journal can re-verify: a boot id, a start time, and a process group
    /// read from `/proc`. A Windows job handle is process-local (kill-on-close
    /// already ended the tree at a crash), and the FreeBSD/macOS process-group
    /// records have no observable start time in this slice.
    #[must_use]
    pub const fn verifies_orphans(self) -> bool {
        matches!(self, Self::CgroupV2 | Self::Subreaper)
    }

    /// The cleanup class this plan can truthfully claim.
    #[must_use]
    pub const fn cleanup_claim(self) -> &'static str {
        match self {
            Self::CgroupV2 | Self::Subreaper | Self::WindowsJob | Self::FreebsdReaper => {
                CLEANUP_SWEPT
            }
            Self::ProcessGroup => CLEANUP_GROUP_SIGNALLED,
            Self::Refused => CLEANUP_NONE,
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
/// pid. The process-group records carry the group once the child exists; the
/// reserved (pre-spawn) form has no group yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentRecord {
    /// A per-session cgroup v2 subtree.
    Cgroup {
        /// Absolute path of the session subtree.
        path: PathBuf,
    },
    /// The subreaper fallback: no path, no pre-spawn identity.
    Subreaper,
    /// A Windows job object. The job handle is process-local; the record only
    /// names the mechanism, and kill-on-close means a daemon crash cannot
    /// leave a job member running.
    WindowsJob,
    /// The FreeBSD reaper fallback: the recorded process group, absent before
    /// the child exists.
    FreebsdReaper {
        /// The recorded process group.
        process_group: Option<ProcessGroupId>,
    },
    /// The macOS process-group plan: the recorded group, absent before the
    /// child exists.
    ProcessGroup {
        /// The recorded process group.
        process_group: Option<ProcessGroupId>,
    },
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

    /// Build a Windows job record.
    #[must_use]
    pub const fn windows_job() -> Self {
        Self::WindowsJob
    }

    /// Build a FreeBSD reaper record, with the group once it is known.
    #[must_use]
    pub const fn freebsd_reaper(process_group: Option<ProcessGroupId>) -> Self {
        Self::FreebsdReaper { process_group }
    }

    /// Build a macOS process-group record, with the group once it is known.
    #[must_use]
    pub const fn process_group(process_group: Option<ProcessGroupId>) -> Self {
        Self::ProcessGroup { process_group }
    }

    /// The recorded cgroup path, when this is a cgroup record.
    #[must_use]
    pub fn cgroup_path(&self) -> Option<&Path> {
        match self {
            Self::Cgroup { path } => Some(path),
            Self::Subreaper
            | Self::WindowsJob
            | Self::FreebsdReaper { .. }
            | Self::ProcessGroup { .. } => None,
        }
    }

    /// The recorded process group, when the mechanism records one.
    #[must_use]
    pub const fn recorded_process_group(&self) -> Option<ProcessGroupId> {
        match self {
            Self::FreebsdReaper { process_group } | Self::ProcessGroup { process_group } => {
                *process_group
            }
            Self::Cgroup { .. } | Self::Subreaper | Self::WindowsJob => None,
        }
    }

    /// Stable mechanism name.
    #[must_use]
    pub const fn mechanism(&self) -> &'static str {
        match self {
            Self::Cgroup { .. } => MECHANISM_CGROUP_V2,
            Self::Subreaper => MECHANISM_SUBREAPER,
            Self::WindowsJob => MECHANISM_WINDOWS_JOB,
            Self::FreebsdReaper { .. } => MECHANISM_FREEBSD_REAPER,
            Self::ProcessGroup { .. } => MECHANISM_PROCESS_GROUP,
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
    /// Each cell probes its own mechanism: Linux creates and removes a probe
    /// cgroup under the delegated root (a write that proves delegation rather
    /// than assuming it); Windows creates and drops a probe job object;
    /// FreeBSD acquires the `procctl(2)` reaper role; macOS needs no setup for
    /// its process-group plan. A failed probe returns
    /// [`ContainmentKind::Refused`] with [`ContainmentRefusal::UnsupportedPlatform`].
    #[must_use]
    pub fn probe() -> Self {
        self::plan::probe()
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

    /// Build a Windows job report for tests.
    #[must_use]
    pub fn windows_job(cell: PlatformCell) -> Self {
        Self {
            cell,
            kind: ContainmentKind::WindowsJob,
            refusal: None,
            fallback_reason: None,
            cgroup_root: None,
        }
    }

    /// Build a FreeBSD reaper report for tests.
    #[must_use]
    pub fn freebsd_reaper(cell: PlatformCell) -> Self {
        Self {
            cell,
            kind: ContainmentKind::FreebsdReaper,
            refusal: None,
            fallback_reason: None,
            cgroup_root: None,
        }
    }

    /// Build a macOS process-group report for tests.
    #[must_use]
    pub fn process_group(cell: PlatformCell) -> Self {
        Self {
            cell,
            kind: ContainmentKind::ProcessGroup,
            refusal: None,
            fallback_reason: None,
            cgroup_root: None,
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
        self.kind.cleanup_claim()
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
                format!("{MECHANISM_SUBREAPER} (enforced; cleanup swept when verified; {reason})")
            }
            ContainmentKind::WindowsJob => format!(
                "{MECHANISM_WINDOWS_JOB} (enforced; cleanup swept when the job verifies empty)"
            ),
            ContainmentKind::FreebsdReaper => format!(
                "{MECHANISM_FREEBSD_REAPER} (enforced; cleanup swept when the reaper verifies the group empty)"
            ),
            ContainmentKind::ProcessGroup => format!(
                "{MECHANISM_PROCESS_GROUP} (not enforced; cleanup {CLEANUP_GROUP_SIGNALLED}; a re-sessioned descendant is not provably covered)"
            ),
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

/// A spawn failure. Every variant refuses the spawn; none leaves an
/// uncontained child running.
#[derive(Debug)]
pub enum ContainmentError {
    /// The probe selected no mechanism.
    Refused(ContainmentRefusal),
    /// The cgroup plan failed before a child existed.
    #[cfg(target_os = "linux")]
    Cgroup(CgroupError),
    /// The subreaper fallback is unavailable.
    #[cfg(target_os = "linux")]
    Subreaper(SubreaperRefusal),
    /// `spawn` itself failed; no child was created.
    Spawn(io::Error),
    /// `exec` refused the program (`EACCES`, `ENOEXEC`, or `ENOENT`). No child
    /// ran, so this is a distinct typed refusal instead of an untyped spawn
    /// error (bitty#2790).
    Exec(ExecRefusal),
    /// The child's identity could not be observed. The child existed and was
    /// terminated; the carried class is what the cleanup could prove
    /// (`unverified` for the process-group plan).
    IdentityUnavailable(CleanupClass),
    /// The child ran but was outside the plan's recorded identity. The child
    /// existed and was terminated; the carried class is what the cleanup could
    /// prove (`swept` or `unverified`).
    Uncontained(CleanupClass),
}

impl ContainmentError {
    /// The cleanup class observed while the failed spawn was abandoned.
    ///
    /// A refusal, a pre-fork failure, an `exec` refusal, or an unavailable
    /// mechanism started nothing ([`CleanupClass::NotStarted`]); a failure
    /// after the fork reports what the plan could prove about the child it
    /// killed.
    #[must_use]
    pub const fn cleanup(&self) -> CleanupClass {
        match self {
            Self::Refused(_) | Self::Spawn(_) | Self::Exec(_) => CleanupClass::NotStarted,
            #[cfg(target_os = "linux")]
            Self::Cgroup(_) | Self::Subreaper(_) => CleanupClass::NotStarted,
            Self::IdentityUnavailable(cleanup) | Self::Uncontained(cleanup) => *cleanup,
        }
    }
}

impl fmt::Display for ContainmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(formatter, "containment refused: {refusal}"),
            #[cfg(target_os = "linux")]
            Self::Cgroup(error) => write!(formatter, "cgroup containment failed: {error}"),
            #[cfg(target_os = "linux")]
            Self::Subreaper(error) => write!(formatter, "subreaper unavailable: {error}"),
            Self::Spawn(error) => write!(formatter, "spawn failed: {error}"),
            Self::Exec(refusal) => write!(formatter, "{refusal}"),
            Self::IdentityUnavailable(cleanup) => write!(
                formatter,
                "spawned child identity could not be observed; cleanup {}",
                cleanup.as_str()
            ),
            Self::Uncontained(cleanup) => write!(
                formatter,
                "spawned child escaped the recorded containment identity; cleanup {}",
                cleanup.as_str()
            ),
        }
    }
}

impl std::error::Error for ContainmentError {}

#[cfg(target_os = "linux")]
impl From<CgroupError> for ContainmentError {
    fn from(error: CgroupError) -> Self {
        Self::Cgroup(error)
    }
}

impl From<io::Error> for ContainmentError {
    fn from(error: io::Error) -> Self {
        Self::Spawn(error)
    }
}

/// Turn a failed `Command::spawn` into the typed refusal it carries.
///
/// `std` reports the child-side `exec` errno through the close-on-exec pipe,
/// so an `EACCES`/`ENOEXEC`/`ENOENT` here means no child ever ran. Every other
/// errno (a fork refusal, a `pre_exec` failure) stays the untyped
/// [`ContainmentError::Spawn`]. On a cell with no `exec` (Windows) the
/// classifier returns `None` and every failure stays untyped.
pub(crate) fn classify_spawn_error(error: io::Error) -> ContainmentError {
    match ExecRefusal::from_spawn_error(&error) {
        Some(refusal) => ContainmentError::Exec(refusal),
        None => ContainmentError::Spawn(error),
    }
}

/// Poll `done` until it reports success or `window` elapses.
///
/// The poll interval is [`ContainmentLimits::poll`], clamped to the remaining
/// window so a short final slice does not oversleep.
pub(crate) fn wait_bounded(
    window: Duration,
    limits: ContainmentLimits,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + window;
    loop {
        if done() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(limits.poll().min(deadline.saturating_duration_since(now)));
    }
}

/// What a terminal cleanup could prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    class: CleanupClass,
    signalled: usize,
}

impl CleanupOutcome {
    /// The cleanup class (`swept`, `group-signalled`, `unverified`,
    /// `not_started`).
    #[must_use]
    pub const fn class(self) -> CleanupClass {
        self.class
    }

    /// How many recorded members received a signal. Never a pid list.
    #[must_use]
    pub const fn signalled(self) -> usize {
        self.signalled
    }
}

/// How the spawned child relates to its process group and session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// The daemon puts the child in a fresh process group
    /// (`setpgid(0, 0)`), which is the POSIX containment identity. Ignored by
    /// the Windows job plan, whose identity is the job itself.
    ProcessGroup,
    /// The caller's `pre_exec` establishes a new session (`setsid`), so the
    /// child is already a session and process-group leader. Used by the PTY
    /// spawn path, where `setsid` must run before `setpgid`.
    NewSession,
}

/// Spawn a command inside the selected containment plan.
///
/// `name` must be unique per live session and carry the
/// [`SESSION_CGROUP_PREFIX`] prefix; the caller derives it from the daemon
/// instance and session ids, never from user text. `limits` bounds the
/// escalation used by the failure path that abandons a child whose identity
/// could not be observed.
///
/// # Errors
///
/// Refuses a non-enforced plan, an invalid name, a cgroup creation/membership
/// failure, or a spawn failure. A child that fails the membership check is
/// killed through its recorded identity before the error returns.
pub fn spawn(
    report: &ContainmentReport,
    name: &str,
    command: &mut std::process::Command,
    limits: ContainmentLimits,
) -> Result<ContainedChild, ContainmentError> {
    spawn_with(report, name, command, limits, SessionMode::ProcessGroup)
}

/// Spawn with an explicit [`SessionMode`].
///
/// # Errors
///
/// The same refusals as [`spawn`].
pub fn spawn_with(
    report: &ContainmentReport,
    name: &str,
    command: &mut std::process::Command,
    limits: ContainmentLimits,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    if report.kind() == ContainmentKind::Refused {
        return Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        ));
    }
    self::plan::spawn_with(report, name, command, limits, mode)
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
    fn the_fallback_claims_a_verified_sweep_and_reports_the_reason() {
        let report =
            ContainmentReport::subreaper(PlatformCell::LinuxX86_64, Some("no delegation".into()));
        assert_eq!(report.kind(), ContainmentKind::Subreaper);
        assert_eq!(report.cleanup_claim(), "swept");
        assert_eq!(report.fallback_reason(), Some("no delegation"));
        assert!(report.summary().contains("subreaper_fallback"));
        assert!(report.summary().contains("swept when verified"));
    }

    #[test]
    fn both_enforced_plans_claim_a_swept_cleanup() {
        let report = ContainmentReport::cgroup_v2(
            PlatformCell::LinuxX86_64,
            PathBuf::from("/sys/fs/cgroup/probe"),
        );
        assert_eq!(report.kind(), ContainmentKind::CgroupV2);
        assert_eq!(report.cleanup_claim(), "swept");
        assert!(report.is_enforced());
    }

    #[test]
    fn the_windows_job_plan_claims_a_verified_sweep() {
        let report = ContainmentReport::windows_job(PlatformCell::WindowsX86_64);
        assert_eq!(report.kind(), ContainmentKind::WindowsJob);
        assert_eq!(report.kind().as_str(), MECHANISM_WINDOWS_JOB);
        assert_eq!(report.cleanup_claim(), CLEANUP_SWEPT);
        assert!(report.is_enforced());
        assert!(report.summary().contains("windows_job"));
        assert!(report.summary().contains("swept"));
    }

    #[test]
    fn the_freebsd_reaper_plan_claims_a_verified_sweep() {
        let report = ContainmentReport::freebsd_reaper(PlatformCell::FreeBsdX86_64);
        assert_eq!(report.kind(), ContainmentKind::FreebsdReaper);
        assert_eq!(report.kind().as_str(), MECHANISM_FREEBSD_REAPER);
        assert_eq!(report.cleanup_claim(), CLEANUP_SWEPT);
        assert!(report.is_enforced());
        assert!(report.summary().contains("freebsd_reaper"));
        assert!(report.summary().contains("swept"));
    }

    #[test]
    fn the_macos_process_group_plan_never_claims_a_sweep() {
        let report = ContainmentReport::process_group(PlatformCell::MacosArm64);
        assert_eq!(report.kind(), ContainmentKind::ProcessGroup);
        assert_eq!(report.kind().as_str(), MECHANISM_PROCESS_GROUP);
        assert_eq!(report.cleanup_claim(), CLEANUP_GROUP_SIGNALLED);
        assert_eq!(report.cleanup_claim(), "group-signalled");
        assert!(
            !report.is_enforced(),
            "a process group cannot prove a whole tree"
        );
        assert!(report.summary().contains("group-signalled"));
        assert!(report.summary().contains("not provably covered"));
    }

    #[test]
    fn only_the_linux_plans_verify_orphan_records() {
        assert!(ContainmentKind::CgroupV2.verifies_orphans());
        assert!(ContainmentKind::Subreaper.verifies_orphans());
        for kind in [
            ContainmentKind::WindowsJob,
            ContainmentKind::FreebsdReaper,
            ContainmentKind::ProcessGroup,
            ContainmentKind::Refused,
        ] {
            assert!(
                !kind.verifies_orphans(),
                "{kind} has no durable identity to re-check"
            );
        }
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
        assert_eq!(
            ContainmentRecord::windows_job().mechanism(),
            MECHANISM_WINDOWS_JOB
        );
        assert!(
            ContainmentRecord::windows_job()
                .recorded_process_group()
                .is_none()
        );

        let group = ProcessGroupId::new(4242).expect("nonzero group");
        let reaper = ContainmentRecord::freebsd_reaper(Some(group));
        assert_eq!(reaper.mechanism(), MECHANISM_FREEBSD_REAPER);
        assert_eq!(reaper.recorded_process_group(), Some(group));
        assert!(reaper.cgroup_path().is_none());
        let process_group = ContainmentRecord::process_group(Some(group));
        assert_eq!(process_group.mechanism(), MECHANISM_PROCESS_GROUP);
        assert_eq!(process_group.recorded_process_group(), Some(group));
        assert!(
            ContainmentRecord::process_group(None)
                .recorded_process_group()
                .is_none(),
            "the reserved form has no group yet"
        );
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "freebsd",
        target_os = "macos"
    )))]
    #[test]
    fn an_unsupported_cell_refuses_with_unsupported_platform() {
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
            ContainmentKind::Subreaper => assert_eq!(report.cleanup_claim(), "swept"),
            ContainmentKind::Refused => {
                assert!(report.refusal().is_some());
                assert!(!report.is_enforced());
            }
            other => panic!("Linux selected a foreign plan: {other}"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_live_probe_selects_the_job_plan_or_a_typed_refusal() {
        let report = ContainmentReport::probe();
        assert_eq!(report.cell(), PlatformCell::current());
        match report.kind() {
            ContainmentKind::WindowsJob => {
                assert_eq!(report.cleanup_claim(), CLEANUP_SWEPT);
                assert!(report.is_enforced());
            }
            ContainmentKind::Refused => {
                assert!(matches!(
                    report.refusal(),
                    Some(ContainmentRefusal::UnsupportedPlatform { cell }) if *cell == report.cell()
                ));
            }
            other => panic!("Windows selected a foreign plan: {other}"),
        }
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn the_live_probe_selects_the_reaper_plan_or_a_typed_refusal() {
        let report = ContainmentReport::probe();
        assert_eq!(report.cell(), PlatformCell::current());
        match report.kind() {
            ContainmentKind::FreebsdReaper => {
                assert_eq!(report.cleanup_claim(), CLEANUP_SWEPT);
                assert!(report.is_enforced());
            }
            ContainmentKind::Refused => {
                assert!(matches!(
                    report.refusal(),
                    Some(ContainmentRefusal::UnsupportedPlatform { cell }) if *cell == report.cell()
                ));
            }
            other => panic!("FreeBSD selected a foreign plan: {other}"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_live_probe_selects_the_process_group_plan() {
        let report = ContainmentReport::probe();
        assert_eq!(report.cell(), PlatformCell::current());
        assert_eq!(report.kind(), ContainmentKind::ProcessGroup);
        assert_eq!(report.cleanup_claim(), CLEANUP_GROUP_SIGNALLED);
        assert!(!report.is_enforced());
    }
}
