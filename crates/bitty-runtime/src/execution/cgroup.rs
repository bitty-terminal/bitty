//! Per-job cgroup v2 leaves: the evidence source for `OomKilled`
//! (CTX-0880, #1537).
//!
//! `OomKilled` is asserted only when the host can determine it. On Linux
//! that is the `oom_kill` counter in a per-job cgroup's `memory.events`:
//! the supervisor reads it before the job starts and after the owned tree
//! is gone, and [`OomEvidence::from_counts`] turns the two readings into
//! evidence. Everything else (no delegated subtree, a counter that cannot
//! be read, macOS, Windows) keeps [`OomVerdict::Unknown`](super::OomVerdict)
//! and names the reason as an [`OomEvidenceGap`] on the job snapshot. A
//! signal number is never evidence.
//!
//! # Delegated subtree discovery
//!
//! [`JobCgroups::discover`] derives the subtree from this process's own
//! cgroup: the `0::` line of `/proc/self/cgroup` joined onto the cgroup2
//! mount point found in `/proc/self/mountinfo` (the mount is verified, never
//! assumed). Because of the cgroup v2 no-internal-processes rule, the
//! process's own (populated) cgroup cannot host job leaves itself, so the
//! parent that hosts them is the nearest of {own cgroup, its parent} whose
//! `cgroup.subtree_control` enables `memory` and where a directory can be
//! created. There a dedicated, empty job base (`bitty-jobs-<pid>-<seq>`) is
//! created with `+memory` in its own `cgroup.subtree_control`, and one leaf
//! per job (`job-<id>-<generation>`) is created under that base.
//! [`JobCgroups::under`] injects the hosting parent directly. Hosts without
//! a usable subtree get a typed [`CgroupUnavailable`] and run jobs without
//! leaves.
//!
//! # Placement window (documented residual)
//!
//! `bitty-pty` forbids `unsafe`, and `CommandExt::pre_exec` (like
//! `clone3(CLONE_INTO_CGROUP)`) needs it, so the child cannot be placed
//! before it runs. The supervisor instead writes the leader pid into the
//! leaf's `cgroup.procs` immediately after `spawn` returns, before the tree
//! is adopted and before the drains start. Residual window: a descendant
//! the program forks between its `exec` and that write stays in the
//! supervisor's cgroup, so its memory and any OOM kill of it are not
//! counted in the leaf. The leader itself is always in the leaf once the
//! placement succeeded, and a failed placement is recorded as
//! [`OomEvidenceGap::PlacementFailed`], never as `NotOom`.
//!
//! # Bounds and cleanup
//!
//! - At most [`MAX_JOB_CGROUP_LEAVES`] leaves exist per [`JobCgroups`]
//!   (live plus not yet removed); past that a job runs without a leaf and
//!   records [`OomEvidenceGap::LeafLimit`].
//! - Every read is bounded (`memory.events` by
//!   [`MAX_MEMORY_EVENTS_BYTES`], the control and `/proc` files by named
//!   constants); an over-bound read fails closed.
//! - A leaf is removed after the leader is reaped and the owned tree is
//!   killed, with a bounded number of `rmdir` attempts. A leaf that cannot
//!   be removed yet (a member escaped the process group and still runs) is
//!   kept on a bounded list, counted by [`JobCgroups::unremoved_leaves`],
//!   and retried at the next leaf creation and on drop. The job base is
//!   removed when the [`JobCgroups`] drops.
//! - Before the `rmdir`, `cgroup.kill` ends members that left the job's
//!   process group but stayed in its leaf (only this job's descendants can
//!   be there).

use std::fs;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use super::oom::{MAX_MEMORY_EVENTS_BYTES, OomEvidence, OomEvidenceGap, parse_oom_kill_count};

/// Maximum per-job leaves (live plus not yet removed) one [`JobCgroups`]
/// holds: the registry's default tracked-job bound, so a full registry
/// never runs out of leaves while a leaf leak cannot grow without bound.
pub const MAX_JOB_CGROUP_LEAVES: usize = super::registry::DEFAULT_MAX_JOBS;

/// Largest `/proc/self/cgroup` payload accepted (64 KiB).
#[cfg(target_os = "linux")]
const MAX_PROC_CGROUP_BYTES: usize = 64 * 1024;

/// Largest `/proc/self/mountinfo` payload accepted (1 MiB).
#[cfg(target_os = "linux")]
const MAX_MOUNTINFO_BYTES: usize = 1024 * 1024;

/// Largest cgroup control file (`cgroup.subtree_control`) accepted (4 KiB).
const MAX_CONTROL_FILE_BYTES: usize = 4 * 1024;

/// `rmdir` attempts for one leaf or base before it is kept as unremoved.
const RMDIR_ATTEMPTS: u32 = 50;

/// Pause between `rmdir` attempts (members of a killed tree exit
/// asynchronously; the total bound is `RMDIR_ATTEMPTS * RMDIR_BACKOFF`).
const RMDIR_BACKOFF: Duration = Duration::from_millis(10);

/// Name prefix of the dedicated job base (`<prefix>-<pid>-<seq>`).
const JOB_BASE_PREFIX: &str = "bitty-jobs";

/// Name prefix of one job's leaf (`<prefix>-<id>-<generation>`).
const LEAF_PREFIX: &str = "job";

/// The controller whose `memory.events` carries the evidence.
const MEMORY_CONTROLLER: &str = "memory";

/// Control file that enables controllers for children.
const SUBTREE_CONTROL_FILE: &str = "cgroup.subtree_control";

/// Membership file a pid is written into.
const PROCS_FILE: &str = "cgroup.procs";

/// Kill switch for every member of a cgroup (Linux 5.14+).
const KILL_FILE: &str = "cgroup.kill";

/// Event counters file holding `oom_kill`.
const MEMORY_EVENTS_FILE: &str = "memory.events";

/// Filesystem type of the unified hierarchy in mountinfo.
const CGROUP2_FSTYPE: &str = "cgroup2";

/// This process's cgroup membership (kernel ABI path).
#[cfg(target_os = "linux")]
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";

/// This process's mount table (kernel ABI path).
#[cfg(target_os = "linux")]
const PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";

/// Sequence that keeps job bases of one process distinct.
static BASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Why no per-job cgroup subtree is usable on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CgroupUnavailable {
    /// The platform has no cgroup v2 (macOS, Windows, BSD).
    UnsupportedPlatform,
    /// No unified (`0::`) membership or no verified cgroup2 mount.
    NoUnifiedHierarchy,
    /// No candidate parent enables the `memory` controller for children.
    NoMemoryController,
    /// The job base could not be created (subtree not delegated to us).
    NotWritable,
}

impl CgroupUnavailable {
    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::NoUnifiedHierarchy => "no_unified_hierarchy",
            Self::NoMemoryController => "no_memory_controller",
            Self::NotWritable => "not_writable",
        }
    }
}

impl std::fmt::Display for CgroupUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for CgroupUnavailable {}

/// A delegated cgroup v2 job base plus the bounded book of its leaves.
///
/// Hand one to [`JobRegistry::with_job_cgroups`](super::JobRegistry::with_job_cgroups); the registry creates one
/// leaf per started job under [`JobCgroups::base`]. Dropping the value (with
/// the last registry clone) removes the base.
#[derive(Debug)]
pub struct JobCgroups {
    base: PathBuf,
    max_leaves: usize,
    book: Mutex<LeafBook>,
}

#[derive(Debug, Default)]
struct LeafBook {
    /// Leaves created and not yet released.
    live: usize,
    /// Released leaves whose `rmdir` has not succeeded yet.
    unremoved: Vec<PathBuf>,
}

impl JobCgroups {
    /// Discovers the delegated subtree from this process's own cgroup (see
    /// the module docs) and creates a fresh job base in it.
    ///
    /// # Errors
    ///
    /// Returns the [`CgroupUnavailable`] reason when no usable subtree
    /// exists; callers keep running jobs with `Unknown` OOM evidence.
    pub fn discover() -> Result<Self, CgroupUnavailable> {
        #[cfg(target_os = "linux")]
        {
            let membership = read_bounded(Path::new(PROC_SELF_CGROUP), MAX_PROC_CGROUP_BYTES)
                .map_err(|_| CgroupUnavailable::NoUnifiedHierarchy)?;
            let relative = parse_unified_cgroup_path(&membership)
                .ok_or(CgroupUnavailable::NoUnifiedHierarchy)?;
            let mountinfo = read_bounded(Path::new(PROC_SELF_MOUNTINFO), MAX_MOUNTINFO_BYTES)
                .map_err(|_| CgroupUnavailable::NoUnifiedHierarchy)?;
            let mount = parse_cgroup2_mount_point(&mountinfo)
                .ok_or(CgroupUnavailable::NoUnifiedHierarchy)?;
            Self::discover_from(&mount, &relative)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(CgroupUnavailable::UnsupportedPlatform)
        }
    }

    /// Creates a fresh job base under the injected hosting `parent`, which
    /// must enable `memory` in its `cgroup.subtree_control`.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupUnavailable::NoMemoryController`] when `parent` does
    /// not delegate `memory`, [`CgroupUnavailable::NotWritable`] when the
    /// base cannot be created, and
    /// [`CgroupUnavailable::UnsupportedPlatform`] off Linux.
    pub fn under(parent: impl Into<PathBuf>) -> Result<Self, CgroupUnavailable> {
        #[cfg(target_os = "linux")]
        {
            Self::create_base(parent.into(), MAX_JOB_CGROUP_LEAVES)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = parent.into();
            Err(CgroupUnavailable::UnsupportedPlatform)
        }
    }

    /// The job base directory (the parent of every job leaf).
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Leaves whose removal has not succeeded yet (a diagnostic counter:
    /// nonzero means a job's tree member outlived its kill).
    #[must_use]
    pub fn unremoved_leaves(&self) -> usize {
        self.lock_book().unremoved.len()
    }

    /// Tries `own` and then its parent (inside `mount`) as the hosting
    /// parent of the job base.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn discover_from(mount: &Path, relative: &Path) -> Result<Self, CgroupUnavailable> {
        let own = mount.join(relative);
        let mut candidates = vec![own.clone()];
        if let Some(parent) = own.parent() {
            if parent.starts_with(mount) {
                candidates.push(parent.to_path_buf());
            }
        }
        let mut last = CgroupUnavailable::NoMemoryController;
        for candidate in candidates {
            match Self::create_base(candidate, MAX_JOB_CGROUP_LEAVES) {
                Ok(cgroups) => return Ok(cgroups),
                Err(reason) => last = reason,
            }
        }
        Err(last)
    }

    /// Creates `<parent>/bitty-jobs-<pid>-<seq>` with `+memory` enabled for
    /// its children. The base itself never holds a process (cgroup v2
    /// no-internal-processes rule).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn create_base(parent: PathBuf, max_leaves: usize) -> Result<Self, CgroupUnavailable> {
        if !delegates_memory(&parent) {
            return Err(CgroupUnavailable::NoMemoryController);
        }
        let sequence = BASE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let base = parent.join(format!(
            "{JOB_BASE_PREFIX}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&base).map_err(|_| CgroupUnavailable::NotWritable)?;
        if fs::write(
            base.join(SUBTREE_CONTROL_FILE),
            format!("+{MEMORY_CONTROLLER}"),
        )
        .is_err()
        {
            let _ = fs::remove_dir(&base);
            return Err(CgroupUnavailable::NoMemoryController);
        }
        Ok(Self {
            base,
            max_leaves,
            book: Mutex::new(LeafBook::default()),
        })
    }

    /// Creates the leaf `name` under the base, within the leaf bound.
    /// Unremoved leaves get one more removal attempt first.
    fn create_leaf(&self, name: &str) -> Result<PathBuf, OomEvidenceGap> {
        let mut book = self.lock_book();
        book.unremoved
            .retain(|leaf| fs::remove_dir(leaf).is_err() && leaf.exists());
        if book.live.saturating_add(book.unremoved.len()) >= self.max_leaves {
            return Err(OomEvidenceGap::LeafLimit);
        }
        let leaf = self.base.join(name);
        fs::create_dir(&leaf).map_err(|_| OomEvidenceGap::LeafCreateFailed)?;
        book.live += 1;
        Ok(leaf)
    }

    /// Removes a released leaf with bounded retries; a leaf that is still
    /// busy is kept on the unremoved list. Returns whether it is gone.
    fn release_leaf(&self, leaf: PathBuf) -> bool {
        let removed = remove_dir_bounded(&leaf);
        let mut book = self.lock_book();
        book.live = book.live.saturating_sub(1);
        if !removed {
            book.unremoved.push(leaf);
        }
        removed
    }

    fn lock_book(&self) -> MutexGuard<'_, LeafBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for JobCgroups {
    fn drop(&mut self) {
        let unremoved = std::mem::take(&mut self.lock_book().unremoved);
        for leaf in unremoved {
            remove_dir_bounded(&leaf);
        }
        remove_dir_bounded(&self.base);
    }
}

/// `rmdir` with [`RMDIR_ATTEMPTS`] bounded attempts; a missing directory
/// counts as removed.
fn remove_dir_bounded(dir: &Path) -> bool {
    for attempt in 0..RMDIR_ATTEMPTS {
        match fs::remove_dir(dir) {
            Ok(()) => return true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return true,
            Err(_) if attempt + 1 < RMDIR_ATTEMPTS => thread::sleep(RMDIR_BACKOFF),
            Err(_) => {}
        }
    }
    false
}

/// Reads at most `max` bytes of `path` as text; a longer payload is
/// rejected (fail closed), never truncated into a plausible prefix.
fn read_bounded(path: &Path, max: usize) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let mut text = String::new();
    file.take(limit).read_to_string(&mut text)?;
    if text.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cgroup file exceeds its read bound",
        ));
    }
    Ok(text)
}

/// Whether `dir`'s `cgroup.subtree_control` enables `memory`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn delegates_memory(dir: &Path) -> bool {
    read_bounded(&dir.join(SUBTREE_CONTROL_FILE), MAX_CONTROL_FILE_BYTES).is_ok_and(|text| {
        text.split_ascii_whitespace()
            .any(|controller| controller == MEMORY_CONTROLLER)
    })
}

/// The unified-hierarchy path from `/proc/self/cgroup` text, relative to
/// the cgroup2 mount root.
///
/// Only the `0::` line counts. The path must be absolute and contain only
/// normal components: a `..` path (a cgroup outside this namespace) is
/// rejected rather than resolved.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_unified_cgroup_path(text: &str) -> Option<PathBuf> {
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = path.strip_prefix('/')?;
    let relative = Path::new(relative);
    relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then(|| relative.to_path_buf())
}

/// The mount point of the cgroup2 filesystem from `/proc/self/mountinfo`
/// text.
///
/// Only a mount exposing the hierarchy root (`root` field `/`) qualifies,
/// so joining the `0::` path onto it is exact. Octal escapes (`\040`) in the
/// mount point are decoded.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cgroup2_mount_point(text: &str) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let (mount_fields, fs_fields) = line.split_once(" - ")?;
        if fs_fields.split_ascii_whitespace().next()? != CGROUP2_FSTYPE {
            return None;
        }
        let mut fields = mount_fields.split(' ');
        let root = fields.nth(3)?;
        let mount_point = fields.next()?;
        if root != "/" {
            return None;
        }
        let decoded = unescape_mount_field(mount_point)?;
        let path = PathBuf::from(decoded);
        path.is_absolute().then_some(path)
    })
}

/// Decodes mountinfo's `\ooo` octal escapes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_mount_field(field: &str) -> Option<String> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let digits = bytes.get(index + 1..index + 4)?;
            let text = std::str::from_utf8(digits).ok()?;
            out.push(u8::from_str_radix(text, 8).ok()?);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Leaf name for one job: derived from its id and execution generation,
/// so two registries (or a restart) never collide on a name.
fn leaf_name(id: u64, generation: u64) -> String {
    format!("{LEAF_PREFIX}-{id}-{generation:016x}")
}

/// Reads the leaf's `oom_kill` counter within [`MAX_MEMORY_EVENTS_BYTES`].
fn read_oom_kill_count(leaf: &Path) -> Option<u64> {
    read_bounded(&leaf.join(MEMORY_EVENTS_FILE), MAX_MEMORY_EVENTS_BYTES)
        .ok()
        .as_deref()
        .and_then(parse_oom_kill_count)
}

/// Where a registry's per-job leaves come from.
#[derive(Debug, Clone)]
pub(crate) enum CgroupSource {
    /// The registry was built without cgroup accounting.
    NotConfigured,
    /// Accounting was requested, but no usable subtree exists.
    Unavailable(CgroupUnavailable),
    /// A delegated job base.
    Available(Arc<JobCgroups>),
}

impl CgroupSource {
    /// Source from a discovery (or injection) result.
    pub(crate) fn from_result(result: Result<Arc<JobCgroups>, CgroupUnavailable>) -> Self {
        match result {
            Ok(cgroups) => Self::Available(cgroups),
            Err(reason) => Self::Unavailable(reason),
        }
    }

    /// The gap a job records when this source yields no leaf.
    const fn gap(&self) -> Option<OomEvidenceGap> {
        match self {
            Self::Available(_) => None,
            Self::NotConfigured if cfg!(target_os = "linux") => Some(OomEvidenceGap::NotConfigured),
            Self::NotConfigured | Self::Unavailable(CgroupUnavailable::UnsupportedPlatform) => {
                Some(OomEvidenceGap::UnsupportedPlatform)
            }
            Self::Unavailable(_) => Some(OomEvidenceGap::Undelegated),
        }
    }
}

/// One job's cgroup accounting, owned by its supervisor thread.
///
/// `prepare` creates the leaf and takes the "before" reading before the
/// spawn, `place` moves the leader in right after it, and `finish` takes
/// the "after" reading once the tree is gone and removes the leaf. Every
/// state that cannot yield both readings carries a typed gap.
pub(crate) struct JobAccounting {
    cgroups: Option<Arc<JobCgroups>>,
    leaf: Option<PathBuf>,
    before: Option<u64>,
    placed: bool,
    gap: Option<OomEvidenceGap>,
}

impl JobAccounting {
    /// Creates the job's leaf and reads its starting counter.
    pub(crate) fn prepare(source: CgroupSource, id: u64, generation: u64) -> Self {
        let gap = source.gap();
        let cgroups = match source {
            CgroupSource::Available(cgroups) => Some(cgroups),
            CgroupSource::NotConfigured | CgroupSource::Unavailable(_) => None,
        };
        let mut accounting = Self {
            cgroups: cgroups.clone(),
            leaf: None,
            before: None,
            placed: false,
            gap,
        };
        let Some(cgroups) = cgroups else {
            return accounting;
        };
        match cgroups.create_leaf(&leaf_name(id, generation)) {
            Err(gap) => accounting.gap = Some(gap),
            Ok(leaf) => match read_oom_kill_count(&leaf) {
                Some(before) => {
                    accounting.before = Some(before);
                    accounting.leaf = Some(leaf);
                }
                None => {
                    cgroups.release_leaf(leaf);
                    accounting.gap = Some(OomEvidenceGap::CounterUnreadable);
                }
            },
        }
        accounting
    }

    /// Moves the freshly spawned leader into the leaf (see the module docs
    /// for the residual window).
    pub(crate) fn place(&mut self, pid: u32) {
        let Some(leaf) = &self.leaf else {
            return;
        };
        if fs::write(leaf.join(PROCS_FILE), pid.to_string()).is_ok() {
            self.placed = true;
        } else {
            self.gap = Some(OomEvidenceGap::PlacementFailed);
        }
    }

    /// Evidence while the job runs.
    pub(crate) fn running_evidence(&self) -> OomEvidence {
        match (self.gap, self.leaf.is_some(), self.placed) {
            (Some(gap), _, _) => OomEvidence::Missing(gap),
            (None, true, true) => OomEvidence::Tracked,
            (None, true, false) => OomEvidence::Missing(OomEvidenceGap::PlacementFailed),
            (None, false, _) => OomEvidence::Missing(OomEvidenceGap::CounterUnreadable),
        }
    }

    /// Takes the final reading and removes the leaf. Call only after the
    /// leader is reaped and the owned tree killed.
    pub(crate) fn finish(mut self) -> OomEvidence {
        let evidence = match self.running_evidence() {
            OomEvidence::Tracked => {
                let after = self.leaf.as_deref().and_then(read_oom_kill_count);
                OomEvidence::from_counts(self.before, after)
            }
            other => other,
        };
        self.release();
        evidence
    }

    /// The job never started a process: removes the leaf.
    pub(crate) fn abandon(mut self) -> OomEvidence {
        self.release();
        OomEvidence::Missing(OomEvidenceGap::NotStarted)
    }

    /// Kills whatever is left in the leaf, then removes it.
    ///
    /// Runs only after the leader is reaped and the owned tree killed (or
    /// on an unwinding supervisor), so the leaf holds nothing but members
    /// of this job that left its process group: `cgroup.kill` ends them so
    /// the leaf can be removed. The file is opened without `create`, so a
    /// kernel without `cgroup.kill` (before 5.14) just skips the step.
    fn release(&mut self) {
        if let (Some(cgroups), Some(leaf)) = (&self.cgroups, self.leaf.take()) {
            if let Ok(mut kill) = fs::OpenOptions::new()
                .write(true)
                .open(leaf.join(KILL_FILE))
            {
                let _ = io::Write::write_all(&mut kill, b"1");
            }
            cgroups.release_leaf(leaf);
        }
    }
}

impl Drop for JobAccounting {
    /// Safety net for an unwinding supervisor: the leaf never leaks.
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh fake cgroupfs directory under the system temp dir.
    fn fake_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx0880-cgroup-{tag}-{}-{}",
            std::process::id(),
            BASE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("fake cgroupfs root");
        dir
    }

    fn fake_cgroups(tag: &str, subtree: &str, max_leaves: usize) -> (PathBuf, JobCgroups) {
        let root = fake_root(tag);
        fs::write(root.join(SUBTREE_CONTROL_FILE), subtree).expect("subtree_control");
        let cgroups = JobCgroups::create_base(root.clone(), max_leaves).expect("fake base");
        (root, cgroups)
    }

    #[test]
    fn unified_path_is_the_zero_line_and_rejects_escapes() {
        let text = "12:pids:/legacy\n0::/user.slice/app.slice/a.scope\n";
        assert_eq!(
            parse_unified_cgroup_path(text),
            Some(PathBuf::from("user.slice/app.slice/a.scope"))
        );
        assert_eq!(parse_unified_cgroup_path("0::/\n"), Some(PathBuf::new()));
        assert_eq!(parse_unified_cgroup_path("0::/../../outside\n"), None);
        assert_eq!(parse_unified_cgroup_path("0::relative\n"), None);
        assert_eq!(parse_unified_cgroup_path("1:name=systemd:/x\n"), None);
        assert_eq!(parse_unified_cgroup_path(""), None);
    }

    #[test]
    fn cgroup2_mount_is_verified_from_mountinfo() {
        let text = "\
22 1 0:21 / /proc rw,nosuid - proc proc rw
30 25 0:26 / /sys/fs/cgroup/legacy rw - cgroup cgroup rw,memory
31 25 0:27 /nested /elsewhere rw - cgroup2 cgroup2 rw
32 25 0:28 / /mnt/cg\\040two rw,nosuid shared:7 - cgroup2 cgroup2 rw,nsdelegate
";
        assert_eq!(
            parse_cgroup2_mount_point(text),
            Some(PathBuf::from("/mnt/cg two"))
        );
        assert_eq!(
            parse_cgroup2_mount_point("22 1 0:21 / /proc rw - proc proc rw\n"),
            None
        );
        assert_eq!(unescape_mount_field("a\\04"), None, "truncated escape");
    }

    #[test]
    fn leaf_names_derive_from_ids() {
        assert_eq!(leaf_name(7, 0xab), "job-7-00000000000000ab");
        assert_ne!(leaf_name(1, 2), leaf_name(1, 3));
    }

    #[test]
    fn a_parent_without_memory_delegation_is_refused() {
        let root = fake_root("nomem");
        fs::write(root.join(SUBTREE_CONTROL_FILE), "cpu pids").expect("subtree_control");
        assert_eq!(
            JobCgroups::create_base(root.clone(), MAX_JOB_CGROUP_LEAVES).map(drop),
            Err(CgroupUnavailable::NoMemoryController)
        );
        let missing = root.join("absent");
        assert_eq!(
            JobCgroups::create_base(missing, MAX_JOB_CGROUP_LEAVES).map(drop),
            Err(CgroupUnavailable::NoMemoryController)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_tries_the_own_cgroup_then_its_parent() {
        let mount = fake_root("discover");
        let own = mount.join("slice").join("own.scope");
        fs::create_dir_all(&own).expect("own cgroup");
        // Only the parent delegates memory (the own cgroup holds processes).
        fs::write(
            mount.join("slice").join(SUBTREE_CONTROL_FILE),
            "memory pids",
        )
        .expect("parent subtree_control");
        let cgroups = JobCgroups::discover_from(&mount, Path::new("slice/own.scope"))
            .expect("parent hosts the base");
        assert_eq!(cgroups.base().parent(), Some(mount.join("slice").as_path()));
        assert_eq!(
            fs::read_to_string(cgroups.base().join(SUBTREE_CONTROL_FILE)).expect("enabled"),
            "+memory"
        );
        drop(cgroups);
        let none = fake_root("discover-none");
        fs::create_dir_all(none.join("a")).expect("own cgroup");
        assert_eq!(
            JobCgroups::discover_from(&none, Path::new("a")).map(drop),
            Err(CgroupUnavailable::NoMemoryController)
        );
        let _ = fs::remove_dir_all(mount);
        let _ = fs::remove_dir_all(none);
    }

    #[test]
    fn leaves_are_bounded_and_unremoved_leaves_count_until_removed() {
        let (root, cgroups) = fake_cgroups("bound", "memory", 2);
        let first = cgroups.create_leaf("job-1-a").expect("first leaf");
        let second = cgroups.create_leaf("job-2-b").expect("second leaf");
        assert_eq!(
            cgroups.create_leaf("job-3-c"),
            Err(OomEvidenceGap::LeafLimit)
        );
        assert!(cgroups.release_leaf(second), "an empty leaf is removed");
        // A leaf that cannot be removed yet stays counted against the bound.
        fs::write(first.join(PROCS_FILE), "1").expect("busy leaf");
        assert!(!cgroups.release_leaf(first.clone()));
        assert_eq!(cgroups.unremoved_leaves(), 1);
        let third = cgroups.create_leaf("job-3-c").expect("room for one");
        assert_eq!(
            cgroups.create_leaf("job-4-d"),
            Err(OomEvidenceGap::LeafLimit)
        );
        // Once it can go, the next creation reclaims it.
        fs::remove_file(first.join(PROCS_FILE)).expect("unbusy");
        let fourth = cgroups.create_leaf("job-4-d").expect("reclaimed slot");
        assert_eq!(cgroups.unremoved_leaves(), 0);
        assert!(!first.exists());
        assert_eq!(
            cgroups.create_leaf("job-4-d"),
            Err(OomEvidenceGap::LeafLimit)
        );
        assert!(cgroups.release_leaf(third));
        assert_eq!(
            cgroups.create_leaf("job-4-d"),
            Err(OomEvidenceGap::LeafCreateFailed),
            "a name collision is a typed create failure"
        );
        assert!(cgroups.release_leaf(fourth));
        drop(cgroups);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn counters_are_read_within_the_bound() {
        let (root, cgroups) = fake_cgroups("read", "memory", 4);
        let leaf = cgroups.create_leaf("job-1-a").expect("leaf");
        assert_eq!(read_oom_kill_count(&leaf), None, "absent file");
        fs::write(leaf.join(MEMORY_EVENTS_FILE), "oom 1\noom_kill 3\n").expect("events");
        assert_eq!(read_oom_kill_count(&leaf), Some(3));
        let mut huge = "x".repeat(MAX_MEMORY_EVENTS_BYTES);
        huge.push_str("\noom_kill 1\n");
        fs::write(leaf.join(MEMORY_EVENTS_FILE), huge).expect("events");
        assert_eq!(read_oom_kill_count(&leaf), None, "over-bound fails closed");
        fs::remove_file(leaf.join(MEMORY_EVENTS_FILE)).expect("cleanup");
        assert!(cgroups.release_leaf(leaf));
        drop(cgroups);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accounting_without_cgroups_names_the_gap() {
        let unconfigured = if cfg!(target_os = "linux") {
            OomEvidenceGap::NotConfigured
        } else {
            OomEvidenceGap::UnsupportedPlatform
        };
        for (source, gap) in [
            (CgroupSource::NotConfigured, unconfigured),
            (
                CgroupSource::Unavailable(CgroupUnavailable::NotWritable),
                OomEvidenceGap::Undelegated,
            ),
            (
                CgroupSource::Unavailable(CgroupUnavailable::UnsupportedPlatform),
                OomEvidenceGap::UnsupportedPlatform,
            ),
        ] {
            let accounting = JobAccounting::prepare(source.clone(), 1, 1);
            assert_eq!(accounting.running_evidence(), OomEvidence::Missing(gap));
            assert_eq!(accounting.finish(), OomEvidence::Missing(gap));
            assert_eq!(
                JobAccounting::prepare(source, 1, 1).abandon(),
                OomEvidence::Missing(OomEvidenceGap::NotStarted)
            );
        }
    }

    #[test]
    fn accounting_without_a_counter_removes_the_leaf() {
        let (root, cgroups) = fake_cgroups("nocounter", "memory", 4);
        let cgroups = Arc::new(cgroups);
        let accounting =
            JobAccounting::prepare(CgroupSource::Available(Arc::clone(&cgroups)), 1, 2);
        assert_eq!(
            accounting.running_evidence(),
            OomEvidence::Missing(OomEvidenceGap::CounterUnreadable)
        );
        assert!(!cgroups.base().join(leaf_name(1, 2)).exists());
        drop(accounting);
        drop(cgroups);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accounting_never_reuses_an_existing_leaf_name() {
        let (root, cgroups) = fake_cgroups("account", "memory", 4);
        let cgroups = Arc::new(cgroups);
        let leaf = cgroups.base().join(leaf_name(5, 6));
        fs::create_dir(&leaf).expect("pre-existing leaf");
        let accounting =
            JobAccounting::prepare(CgroupSource::Available(Arc::clone(&cgroups)), 5, 6);
        assert_eq!(
            accounting.running_evidence(),
            OomEvidence::Missing(OomEvidenceGap::LeafCreateFailed)
        );
        assert_eq!(
            accounting.finish(),
            OomEvidence::Missing(OomEvidenceGap::LeafCreateFailed)
        );
        fs::remove_dir(&leaf).expect("cleanup");
        assert_eq!(cgroups.unremoved_leaves(), 0);
        drop(cgroups);
        let _ = fs::remove_dir_all(root);
    }
}
