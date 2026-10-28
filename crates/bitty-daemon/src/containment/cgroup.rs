//! The delegated cgroup v2 subtree (contract `D-06`, Linux x86_64).
//!
//! A per-session cgroup is created before the spawn, and the child writes its
//! own pid into `cgroup.procs` from a `pre_exec` hook, so no descendant can be
//! born outside the identity. Every operation is scoped to the session subtree:
//! signals iterate `cgroup.procs`, escalation writes `cgroup.kill`, and a
//! verified sweep requires `cgroup.procs` empty **and** `cgroup.events`
//! `populated 0`.
//!
//! The delegation probe proves the capability instead of assuming it: it
//! resolves this process's own cgroup from `/proc/self/cgroup`, requires a
//! domain cgroup owned by the effective uid, opens the control files the plan
//! needs, and creates and removes a temporary child subtree. A read-only
//! systemd scope therefore refuses with [`CgroupRefusal`] rather than failing
//! during the first spawn.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::containment::{SESSION_CGROUP_NAME_MAX, SESSION_CGROUP_PREFIX};

static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Kernel cgroup v2 mount point (a kernel ABI path, not a host path).
pub const CGROUP2_MOUNT: &str = "/sys/fs/cgroup";

/// This process's cgroup membership file.
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";

/// The cgroup v2 filesystem marker.
const CGROUP_CONTROLLERS: &str = "cgroup.controllers";

/// Why the delegated cgroup v2 plan is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupRefusal {
    /// `/proc/self/cgroup` has no `0::` (unified) entry.
    NotUnified,
    /// The compiled cell does not expose the cgroup v2 mount.
    NoMount,
    /// The process's own cgroup is not a domain cgroup.
    NotADomain,
    /// The delegated root is not owned by the effective uid.
    ForeignOwner {
        /// The observed owner uid.
        uid: u32,
    },
    /// The kernel lacks `cgroup.kill` (before Linux 5.14).
    NoKillFile,
    /// A control file could not be opened for the operation the plan needs.
    ControlFile {
        /// File name.
        file: &'static str,
        /// The OS error.
        kind: io::ErrorKind,
    },
    /// The probe subtree could not be created or removed.
    Probe {
        /// The OS error kind.
        kind: io::ErrorKind,
    },
}

impl std::fmt::Display for CgroupRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUnified => formatter.write_str("no unified cgroup hierarchy"),
            Self::NoMount => formatter.write_str("no cgroup v2 mount"),
            Self::NotADomain => formatter.write_str("own cgroup is not a domain cgroup"),
            Self::ForeignOwner { uid } => write!(formatter, "delegated root is owned by uid {uid}"),
            Self::NoKillFile => formatter.write_str("no cgroup.kill (needs Linux 5.14)"),
            Self::ControlFile { file, kind } => {
                write!(
                    formatter,
                    "cgroup control file {file} is not usable ({kind:?})"
                )
            }
            Self::Probe { kind } => write!(formatter, "cgroup delegation probe failed ({kind:?})"),
        }
    }
}

impl std::error::Error for CgroupRefusal {}

/// A cgroup operation failure while creating or using a session subtree.
#[derive(Debug)]
pub enum CgroupError {
    /// Filesystem failure.
    Io(io::Error),
    /// The session name is not a valid session cgroup name.
    InvalidName,
    /// The recorded path is not a direct child of this delegated root.
    OutsideRoot,
    /// The session subtree does not exist.
    Missing,
    /// The subtree is not owned by the daemon uid.
    ForeignOwner {
        /// The observed owner uid.
        uid: u32,
    },
    /// The subtree is still populated.
    Populated,
    /// A spawned child did not land in its recorded subtree.
    MembershipMismatch,
}

impl std::fmt::Display for CgroupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cgroup I/O error: {error}"),
            Self::InvalidName => formatter.write_str("invalid session cgroup name"),
            Self::OutsideRoot => formatter.write_str("cgroup path is outside the delegated root"),
            Self::Missing => formatter.write_str("session cgroup is missing"),
            Self::ForeignOwner { uid } => write!(formatter, "session cgroup is owned by uid {uid}"),
            Self::Populated => formatter.write_str("session cgroup is still populated"),
            Self::MembershipMismatch => {
                formatter.write_str("spawned child is not in its recorded cgroup")
            }
        }
    }
}

impl std::error::Error for CgroupError {}

impl From<io::Error> for CgroupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The delegated cgroup v2 root this daemon may create session subtrees under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupRoot {
    path: PathBuf,
    relative: PathBuf,
    uid: u32,
}

impl CgroupRoot {
    /// Resolve and prove this process's delegated cgroup v2 root.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupRefusal`] when the hierarchy is not unified, the root is
    /// missing or foreign-owned, `cgroup.kill` is absent, a needed control file
    /// is not writable/readable, or a probe subtree cannot be created and
    /// removed.
    pub fn detect() -> Result<Self, CgroupRefusal> {
        let unified =
            fs::read_to_string(PROC_SELF_CGROUP).map_err(|_| CgroupRefusal::NotUnified)?;
        let relative = unified
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or(CgroupRefusal::NotUnified)?
            .trim()
            .to_owned();
        let relative = PathBuf::from(relative.trim_start_matches('/'));

        let mount = Path::new(CGROUP2_MOUNT);
        fs::metadata(mount.join(CGROUP_CONTROLLERS)).map_err(|_| CgroupRefusal::NoMount)?;
        let path = if relative.as_os_str().is_empty() {
            mount.to_path_buf()
        } else {
            mount.join(&relative)
        };
        let metadata = fs::metadata(&path).map_err(|_| CgroupRefusal::NoMount)?;
        let uid = nix::unistd::geteuid().as_raw();
        if metadata.uid() != uid {
            return Err(CgroupRefusal::ForeignOwner {
                uid: metadata.uid(),
            });
        }
        let cgroup_type = fs::read_to_string(path.join("cgroup.type")).map_err(|error| {
            CgroupRefusal::ControlFile {
                file: "cgroup.type",
                kind: error.kind(),
            }
        })?;
        if cgroup_type.trim() != "domain" {
            return Err(CgroupRefusal::NotADomain);
        }
        for file in ["cgroup.procs", "cgroup.kill"] {
            fs::OpenOptions::new()
                .write(true)
                .open(path.join(file))
                .map_err(|error| CgroupRefusal::ControlFile {
                    file,
                    kind: error.kind(),
                })?;
        }
        fs::File::open(path.join("cgroup.events")).map_err(|error| CgroupRefusal::ControlFile {
            file: "cgroup.events",
            kind: error.kind(),
        })?;
        if !path.join("cgroup.kill").exists() {
            return Err(CgroupRefusal::NoKillFile);
        }

        let sequence = PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let probe_name = format!(
            "{SESSION_CGROUP_PREFIX}probe-{}-{sequence}",
            std::process::id()
        );
        let probe = path.join(&probe_name);
        match fs::create_dir(&probe) {
            Ok(()) => {
                fs::remove_dir(&probe)
                    .map_err(|error| CgroupRefusal::Probe { kind: error.kind() })?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CgroupRefusal::Probe {
                    kind: io::ErrorKind::AlreadyExists,
                });
            }
            Err(error) => {
                return Err(CgroupRefusal::Probe { kind: error.kind() });
            }
        }

        Ok(Self {
            path,
            relative,
            uid,
        })
    }

    /// The absolute delegated root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path relative to the cgroup v2 mount, as `/proc/<pid>/cgroup`
    /// reports it.
    #[must_use]
    pub fn relative(&self) -> &Path {
        &self.relative
    }

    /// The owner uid this root was probed with.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    #[cfg(test)]
    pub(crate) fn from_test_parts(path: PathBuf, relative: PathBuf) -> Self {
        Self {
            path,
            relative,
            uid: nix::unistd::geteuid().as_raw(),
        }
    }

    /// Re-open a root path recorded by an earlier probe in this process.
    ///
    /// # Errors
    ///
    /// Refuses a path outside the cgroup v2 mount or a root that is not a
    /// domain cgroup owned by this uid.
    pub(crate) fn from_absolute(path: PathBuf) -> Result<Self, CgroupError> {
        let mount = Path::new(CGROUP2_MOUNT);
        let relative = path
            .strip_prefix(mount)
            .map_err(|_| CgroupError::OutsideRoot)?;
        let relative = relative.to_path_buf();
        let metadata = fs::metadata(&path)?;
        if !metadata.is_dir() {
            return Err(CgroupError::Missing);
        }
        let uid = nix::unistd::geteuid().as_raw();
        if metadata.uid() != uid {
            return Err(CgroupError::ForeignOwner {
                uid: metadata.uid(),
            });
        }
        let cgroup_type = fs::read_to_string(path.join("cgroup.type"))?;
        if cgroup_type.trim() != "domain" {
            return Err(CgroupError::OutsideRoot);
        }
        Ok(Self {
            path,
            relative,
            uid,
        })
    }

    /// Create one session subtree.
    ///
    /// # Errors
    ///
    /// Refuses an invalid name or a filesystem failure.
    pub fn create_session(&self, name: &str) -> Result<SessionCgroup, CgroupError> {
        validate_name(name)?;
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        match SessionCgroup::open_existing(path.clone(), self.relative.join(name), self.uid) {
            Ok(cgroup) => Ok(cgroup),
            Err(error) => {
                let _ = fs::remove_dir(&path);
                Err(error)
            }
        }
    }

    /// Whether a recorded path is exactly a session subtree of this root.
    #[must_use]
    pub fn owns_recorded_path(&self, path: &Path) -> bool {
        if path.parent() != Some(self.path.as_path()) {
            return false;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        validate_name(name).is_ok()
    }

    /// Open a recorded session subtree after validating its owner and location.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupError::OutsideRoot`] for a path this root does not own,
    /// [`CgroupError::Missing`] when it is gone, and
    /// [`CgroupError::ForeignOwner`] when the directory owner differs.
    pub fn open_recorded(&self, path: &Path) -> Result<SessionCgroup, CgroupError> {
        if !self.owns_recorded_path(path) {
            return Err(CgroupError::OutsideRoot);
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(CgroupError::InvalidName)?;
        SessionCgroup::open_existing(path.to_path_buf(), self.relative.join(name), self.uid)
    }
}

fn validate_name(name: &str) -> Result<(), CgroupError> {
    if name.len() > SESSION_CGROUP_NAME_MAX
        || !name.starts_with(SESSION_CGROUP_PREFIX)
        || name.len() == SESSION_CGROUP_PREFIX.len()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(CgroupError::InvalidName);
    }
    Ok(())
}

/// One per-session cgroup v2 subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCgroup {
    path: PathBuf,
    relative: PathBuf,
    uid: u32,
}

impl SessionCgroup {
    fn open_existing(path: PathBuf, relative: PathBuf, uid: u32) -> Result<Self, CgroupError> {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CgroupError::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() {
            return Err(CgroupError::Missing);
        }
        if metadata.uid() != uid {
            return Err(CgroupError::ForeignOwner {
                uid: metadata.uid(),
            });
        }
        Ok(Self {
            path,
            relative,
            uid,
        })
    }

    /// The absolute subtree path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The owner uid recorded at creation.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// Read the pid list.
    ///
    /// # Errors
    ///
    /// Propagates a `cgroup.procs` read failure.
    pub fn pids(&self) -> Result<Vec<u32>, CgroupError> {
        let text = fs::read_to_string(self.path.join("cgroup.procs"))?;
        let mut pids = Vec::new();
        for line in text.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }

    /// Whether the subtree is empty, using both `cgroup.procs` and the
    /// `populated` event.
    ///
    /// # Errors
    ///
    /// Propagates a control-file read failure.
    pub fn verify_empty(&self) -> Result<bool, CgroupError> {
        if !self.pids()?.is_empty() {
            return Ok(false);
        }
        let events = fs::read_to_string(self.path.join("cgroup.events"))?;
        for line in events.lines() {
            if let Some(value) = line.strip_prefix("populated ") {
                return Ok(value.trim() == "0");
            }
        }
        Ok(true)
    }

    /// Signal every recorded member and return the pids that accepted the
    /// signal.
    ///
    /// Membership is the recorded containment identity: a pid is signalled
    /// only because the kernel reports it inside this session's subtree, and
    /// only through a pidfd that pins that exact process.
    ///
    /// # Errors
    ///
    /// Propagates a member-read failure. A pid that exits between the read and
    /// the signal is not an error.
    pub fn signal_members(
        &self,
        signal: nix::sys::signal::Signal,
    ) -> Result<Vec<u32>, CgroupError> {
        let mut signalled = Vec::new();
        for pid in self.pids()? {
            // A pidfd pins the exact member, so a pid that exits and is reused
            // between the read and the signal cannot receive it.
            if let Ok(pinned) = super::linux_sys::PidFd::open(pid) {
                if pinned.send_signal(signal).is_ok() {
                    signalled.push(pid);
                }
            }
        }
        Ok(signalled)
    }

    /// Kill every member through `cgroup.kill` (kernel 5.14+).
    ///
    /// # Errors
    ///
    /// Propagates a write failure.
    pub fn kill_all(&self) -> Result<(), CgroupError> {
        fs::write(self.path.join("cgroup.kill"), b"1\n")?;
        Ok(())
    }

    /// Verify that one pid is a member of this subtree, using the kernel's
    /// `/proc/<pid>/cgroup` report.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupError::MembershipMismatch`] for a mismatch and
    /// [`CgroupError::Missing`] when the process is already gone.
    pub fn verify_member(&self, pid: u32) -> Result<(), CgroupError> {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        let expected = format!("0::/{}", self.relative.display());
        if text.lines().any(|line| line.trim_end() == expected) {
            Ok(())
        } else {
            Err(CgroupError::MembershipMismatch)
        }
    }

    /// Remove the subtree. It must be empty.
    ///
    /// # Errors
    ///
    /// Returns [`CgroupError::Populated`] when removal fails with `EBUSY`.
    pub fn remove(&self) -> Result<(), CgroupError> {
        match fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(nix::libc::EBUSY) => {
                Err(CgroupError::Populated)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_prefixed_and_charset_bounded() {
        assert!(validate_name("bittyd-abc-123").is_ok());
        assert!(matches!(
            validate_name("bittyd-"),
            Err(CgroupError::InvalidName)
        ));
        assert!(matches!(
            validate_name("other-abc"),
            Err(CgroupError::InvalidName)
        ));
        assert!(matches!(
            validate_name("bittyd-abc/../x"),
            Err(CgroupError::InvalidName)
        ));
        assert!(matches!(
            validate_name(&format!("bittyd-{}", "a".repeat(SESSION_CGROUP_NAME_MAX))),
            Err(CgroupError::InvalidName)
        ));
    }

    #[test]
    fn a_recorded_path_outside_the_root_is_refused_without_touching_it() {
        let root = CgroupRoot {
            path: PathBuf::from("/sys/fs/cgroup/bittyd-root"),
            relative: PathBuf::from("bittyd-root"),
            uid: 1000,
        };
        assert!(!root.owns_recorded_path(Path::new("/sys/fs/cgroup/other/bittyd-x")));
        assert!(!root.owns_recorded_path(Path::new("/sys/fs/cgroup/bittyd-root/../x")));
        assert!(root.owns_recorded_path(Path::new("/sys/fs/cgroup/bittyd-root/bittyd-x")));
    }
}
