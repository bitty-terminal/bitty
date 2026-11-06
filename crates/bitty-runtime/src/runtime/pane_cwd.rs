//! Terminal-truth pane working directory (issue #2911).
//!
//! `bitty.process.spawn { cwd = "pane" }` must not be steerable by anything a
//! pane prints. `OSC 7` is PTY output: any program running in the pane — a
//! remote shell, `cat` of a crafted file, a malicious prompt — can report an
//! arbitrary existing local directory, and the allowlisted child would then
//! start there. This module resolves the directory from the kernel instead:
//! the PTY child's own working directory, pinned as a `(dev, ino)` identity so
//! a later path swap cannot silently retarget the spawn.
//!
//! # Platform rules
//!
//! - **Linux:** `/proc/<pid>/cwd` is the child's live kernel cwd. It is
//!   captured by opening the magic link `O_PATH | O_DIRECTORY` (which pins the
//!   inode in one step) and reading the resulting descriptor's path, so the
//!   identity and the path come from the same instant.
//! - **macOS and the BSDs:** there is no `/proc`, and a PTY handle exposes no
//!   kernel cwd query, so [`PaneCwd::from_pty_child`] returns
//!   [`PaneCwdError::Unsupported`]. The host then refuses the `pane` semantic
//!   with the typed `E_SPAWN_CWD_UNAVAILABLE`; it never falls back to the
//!   `OSC 7` report.
//! - **Windows:** ConPTY exposes no cwd query for a running child and this
//!   module does not retain a process handle, so the same refusal applies.
//!
//! Where the platform *can* name a directory, [`PaneCwd::revalidate`] re-opens
//! the pinned path and requires the same inode. A swap between observation and
//! the spawn boundary therefore refuses instead of following the new inode;
//! the value handed to the child is the path that named the pinned inode at
//! the boundary check.

use std::path::{Path, PathBuf};

/// Identity of one directory inode, captured when the pane cwd was pinned.
///
/// Unix keeps `(st_dev, st_ino)`: a path that resolves to a different pair is
/// a different directory, whatever its spelling. Windows has no portable
/// stable file identity in `std` at this MSRV, so the field is absent there and
/// [`PaneCwd::revalidate`] degrades to an existence check — which is sound
/// because Windows never constructs a [`PaneCwd`] from a PTY child (see the
/// module docs).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirIdent {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl DirIdent {
    fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// One pane directory the host resolved from terminal truth.
///
/// Produced only by [`PaneCwd::from_pty_child`] (the production source: the
/// focused pane's kernel cwd) or [`PaneCwd::from_path`] (a host-owned source
/// that already names a directory; never plugin input). The type carries no
/// terminal bytes, so nothing a pane prints can construct one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCwd {
    path: PathBuf,
    #[cfg(unix)]
    ident: DirIdent,
}

/// Why a pane directory could not be pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneCwdError {
    /// This platform cannot query a process's working directory, so the host
    /// refuses `cwd = "pane"` instead of trusting the `OSC 7` report.
    Unsupported,
    /// The PTY child is gone (no such pid) or its cwd could not be read.
    MissingProcess,
    /// The candidate path is not an absolute existing directory.
    NotADirectory,
    /// The kernel reported a deleted directory (`/proc` appends
    /// `" (deleted)"`); a deleted cwd is not usable and must not be handed to
    /// a child.
    Deleted,
}

impl PaneCwd {
    /// Pins an already-known host directory (composition roots and tests).
    ///
    /// The path must be absolute and name an existing directory; its inode
    /// identity is recorded so [`Self::revalidate`] can catch a swap. This is
    /// a *host* constructor: no plugin-facing surface reaches it, and it is
    /// never fed `OSC 7` text.
    ///
    /// # Errors
    ///
    /// [`PaneCwdError::NotADirectory`] for a relative, missing, or
    /// non-directory path.
    pub fn from_path(path: &Path) -> Result<Self, PaneCwdError> {
        if !path.is_absolute() {
            return Err(PaneCwdError::NotADirectory);
        }
        let metadata = std::fs::metadata(path).map_err(|_| PaneCwdError::NotADirectory)?;
        if !metadata.is_dir() {
            return Err(PaneCwdError::NotADirectory);
        }
        Ok(Self {
            path: path.to_path_buf(),
            #[cfg(unix)]
            ident: DirIdent::of(&metadata),
        })
    }

    /// Pins the live kernel working directory of PTY child `pid`.
    ///
    /// Linux only: opens `/proc/<pid>/cwd` with `O_PATH | O_DIRECTORY`, which
    /// resolves the magic link and pins the inode in one step, then records
    /// the descriptor's path and `(st_dev, st_ino)`. Every other platform
    /// returns [`PaneCwdError::Unsupported`]; see the module docs.
    ///
    /// # Errors
    ///
    /// [`PaneCwdError::Unsupported`] off Linux,
    /// [`PaneCwdError::MissingProcess`] when the child or its cwd is gone, and
    /// [`PaneCwdError::Deleted`] for a deleted cwd.
    pub fn from_pty_child(pid: u32) -> Result<Self, PaneCwdError> {
        platform::from_pty_child(pid)
    }

    /// The pinned path (the value the host resolves and validates).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the pinned path still names the pinned inode.
    ///
    /// Called at the spawn boundary, after the source observation and before
    /// the directory reaches a child. `false` means a swap, a rename, or a
    /// deletion happened in between: the caller must refuse, never re-resolve
    /// by name. On Unix this re-stats the path and compares `(st_dev,
    /// st_ino)`; on Windows it checks that the path is still a directory (no
    /// [`PaneCwd`] is ever captured from a PTY child there, so the weaker
    /// check never decides a spawn).
    #[must_use]
    pub fn revalidate(&self) -> bool {
        #[cfg(unix)]
        {
            identity_of(&self.path) == Some(self.ident)
        }
        #[cfg(not(unix))]
        {
            self.path.is_dir()
        }
    }
}

/// Identity of `path` when it names an existing directory.
///
/// Symlinks are followed: the identity compared is the directory the path
/// actually resolves to, which is exactly the question a spawn boundary asks.
#[cfg(unix)]
fn identity_of(path: &Path) -> Option<DirIdent> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    Some(DirIdent::of(&metadata))
}

/// Linux `/proc` capture. Off Linux both entry points are stubs that refuse,
/// so the same call site compiles everywhere and no platform silently degrades
/// to the untrusted report.
#[cfg(target_os = "linux")]
mod platform {
    use super::{DirIdent, PaneCwd, PaneCwdError};
    use std::os::fd::AsRawFd;
    use std::path::{Path, PathBuf};

    use rustix::fs::{Mode, OFlags};

    /// `O_PATH` pins the directory inode without read permission; `O_DIRECTORY`
    /// rejects a non-directory; `O_CLOEXEC` keeps the descriptor out of any
    /// child this host later spawns.
    fn pin_flags() -> OFlags {
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
    }

    /// Opens `path` as a pinned directory descriptor.
    fn open_pinned(path: &Path) -> Option<(std::os::fd::OwnedFd, DirIdent)> {
        let fd = rustix::fs::open(path, pin_flags(), Mode::empty()).ok()?;
        let stat = rustix::fs::fstat(&fd).ok()?;
        let ident = DirIdent {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        };
        Some((fd, ident))
    }

    /// Current path naming the descriptor's inode (`/proc/self/fd/<n>` link
    /// text). A deleted directory keeps the suffix the kernel appends.
    fn path_of(fd: &std::os::fd::OwnedFd) -> Option<PathBuf> {
        let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
        Some(link)
    }

    /// Whether the kernel's link text names a deleted directory.
    fn is_deleted(path: &Path) -> bool {
        path.to_str()
            .is_some_and(|text| text.ends_with(" (deleted)"))
    }

    pub(super) fn from_pty_child(pid: u32) -> Result<PaneCwd, PaneCwdError> {
        let proc_cwd = PathBuf::from(format!("/proc/{pid}/cwd"));
        let (fd, ident) = open_pinned(&proc_cwd).ok_or(PaneCwdError::MissingProcess)?;
        let path = path_of(&fd).ok_or(PaneCwdError::MissingProcess)?;
        if is_deleted(&path) {
            return Err(PaneCwdError::Deleted);
        }
        if !path.is_absolute() {
            return Err(PaneCwdError::NotADirectory);
        }
        Ok(PaneCwd { path, ident })
    }
}

/// Non-Linux capture: the platform cannot prove a PTY child's cwd, so the host
/// refuses `cwd = "pane"` with a typed error instead of trusting `OSC 7`.
#[cfg(not(target_os = "linux"))]
mod platform {
    use super::{PaneCwd, PaneCwdError};

    pub(super) fn from_pty_child(_pid: u32) -> Result<PaneCwd, PaneCwdError> {
        Err(PaneCwdError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory under the platform temp dir.
    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bitty-pane-cwd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ))
    }

    #[test]
    fn from_path_pins_an_absolute_existing_directory() {
        let root = scratch("from-path");
        std::fs::create_dir_all(&root).expect("fixture dir");
        let pinned = PaneCwd::from_path(&root).expect("absolute dir pins");
        assert_eq!(pinned.path(), root.as_path());
        assert!(pinned.revalidate(), "an untouched directory still matches");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn from_path_refuses_relative_missing_and_non_directory_paths() {
        let root = scratch("from-path-bad");
        std::fs::create_dir_all(&root).expect("fixture dir");
        let file = root.join("file");
        std::fs::write(&file, b"x").expect("fixture file");
        for (tag, path) in [
            ("relative", PathBuf::from("relative/dir")),
            ("missing", root.join("missing")),
            ("file", file.clone()),
        ] {
            assert_eq!(
                PaneCwd::from_path(&path),
                Err(PaneCwdError::NotADirectory),
                "{tag}: only an absolute existing directory pins"
            );
        }
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn revalidate_refuses_a_path_swapped_for_another_inode() {
        // The defect behind #2911: a path is not an identity. After the pin,
        // the directory is moved away and a different one takes its name; the
        // boundary check must refuse instead of following the new inode.
        let root = scratch("swap");
        let pane = root.join("pane");
        let moved = root.join("pane-moved");
        std::fs::create_dir_all(&pane).expect("fixture dir");
        let pinned = PaneCwd::from_path(&pane).expect("pin");
        std::fs::rename(&pane, &moved).expect("move the pinned directory aside");
        std::fs::create_dir_all(&pane).expect("impostor at the pinned path");

        assert!(
            !pinned.revalidate(),
            "a different inode at the pinned path must not revalidate"
        );
        assert_eq!(pinned.path(), pane.as_path(), "the pin keeps its path");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn revalidate_refuses_a_deleted_directory() {
        let root = scratch("deleted");
        let pane = root.join("pane");
        std::fs::create_dir_all(&pane).expect("fixture dir");
        let pinned = PaneCwd::from_path(&pane).expect("pin");
        std::fs::remove_dir_all(&root).expect("remove the pinned directory");
        assert!(!pinned.revalidate(), "a deleted directory has no identity");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn from_pty_child_reads_the_kernel_cwd_of_a_live_child() {
        let root = scratch("pty-child");
        let dir = root.join("real");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        // `cd` then `exec` keeps the pid and its kernel cwd: /proc reports the
        // directory the child actually sits in, with no terminal bytes
        // involved.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("cd '{}' && exec sleep 60", dir.display()))
            .spawn()
            .expect("spawn fixture child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut pinned = None;
        while std::time::Instant::now() < deadline {
            if let Ok(candidate) = PaneCwd::from_pty_child(child.id()) {
                if candidate.path() == dir.as_path() {
                    pinned = Some(candidate);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pinned = pinned.expect("the child's kernel cwd is readable");
        assert!(pinned.revalidate(), "the live cwd still matches");
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn from_pty_child_refuses_a_gone_process() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        let _ = child.wait();
        assert_eq!(
            PaneCwd::from_pty_child(pid),
            Err(PaneCwdError::MissingProcess),
            "a reaped child has no kernel cwd to pin"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn from_pty_child_refuses_on_platforms_without_a_kernel_cwd_query() {
        // Documented limitation: no /proc on macOS/BSD, no cwd query through
        // ConPTY on Windows. The host must refuse `pane` rather than read the
        // untrusted report.
        assert_eq!(
            PaneCwd::from_pty_child(std::process::id()),
            Err(PaneCwdError::Unsupported)
        );
    }
}
