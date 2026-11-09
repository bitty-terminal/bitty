//! The typed `exec` refusal shared by every platform cell.
//!
//! `exec` failures used to be an untyped `ContainmentError::Spawn(io::Error)`,
//! which made them indistinguishable from a PTY allocation failure on the wire
//! (bitty#2790 gap 3). The class is defined here, outside the Linux-only
//! containment child, because the worker error, the registry classification and
//! the wire rendering are compiled on every cell.

use std::fmt;
use std::io;

/// Why `exec` refused a program, from the errno `Command::spawn` reported.
///
/// Only errnos `execve` itself produces are classified. A fork refusal or a
/// `pre_exec` failure keeps the untyped containment spawn error, so the typed
/// case never mislabels a refusal that happened before `exec` (bitty#2790).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRefusalKind {
    /// `EACCES`: the file exists but the session user may not execute it, or a
    /// path component denies search.
    NotPermitted,
    /// `ENOEXEC`: the kernel recognises no executable format for the file, so
    /// a malformed `#!` line lands here too.
    NotExecutable,
    /// `ENOENT`: the program is missing, or the interpreter its `#!` line
    /// names is. The kernel cannot tell those apart at `exec` time.
    NotFound,
}

impl ExecRefusalKind {
    /// Classify an `exec` errno, or `None` when `exec` does not report it.
    #[cfg(unix)]
    #[must_use]
    pub fn from_errno(errno: i32) -> Option<Self> {
        match errno {
            nix::libc::EACCES => Some(Self::NotPermitted),
            nix::libc::ENOEXEC => Some(Self::NotExecutable),
            nix::libc::ENOENT => Some(Self::NotFound),
            _ => None,
        }
    }

    /// Classify a Windows `CreateProcessW` OS error code, or `None` when the
    /// code is not an executable-launch refusal.
    #[cfg(windows)]
    #[must_use]
    pub fn from_errno(errno: i32) -> Option<Self> {
        // Win32 error codes from `winerror.h`:
        // - 2 (`ERROR_FILE_NOT_FOUND`), 3 (`ERROR_PATH_NOT_FOUND`),
        //   267 (`ERROR_DIRECTORY`): missing executable or path component.
        // - 5 (`ERROR_ACCESS_DENIED`): file or directory access denied.
        // - 191 (`ERROR_INVALID_EXE_SIGNATURE`), 192 (`ERROR_EXE_MARKED_INVALID`),
        //   193 (`ERROR_BAD_EXE_FORMAT`), 216 (`ERROR_EXE_MACHINE_TYPE_MISMATCH`):
        //   not a valid executable image for this cell.
        match errno {
            5 => Some(Self::NotPermitted),
            191..=193 | 216 => Some(Self::NotExecutable),
            2 | 3 | 267 => Some(Self::NotFound),
            _ => None,
        }
    }

    /// Classify an `exec` errno, or `None` when `exec` does not report it.
    #[cfg(not(any(unix, windows)))]
    #[must_use]
    pub fn from_errno(_errno: i32) -> Option<Self> {
        None
    }

    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotPermitted => "permission_denied",
            Self::NotExecutable => "not_executable",
            Self::NotFound => "not_found",
        }
    }
}

/// One refused `exec`: the class plus the raw errno it was classified from.
///
/// No child ran. `Command::spawn` reports the child's `exec` failure
/// synchronously through its close-on-exec pipe, so this refusal always leaves
/// nothing to clean up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecRefusal {
    kind: ExecRefusalKind,
    errno: i32,
}

impl ExecRefusal {
    /// Build a refusal from an already-classified errno.
    #[must_use]
    pub const fn new(kind: ExecRefusalKind, errno: i32) -> Self {
        Self { kind, errno }
    }

    /// Classify the error `Command::spawn` returned, or `None` when the errno
    /// is not one `exec` reports.
    #[must_use]
    pub fn from_spawn_error(error: &io::Error) -> Option<Self> {
        let errno = error.raw_os_error()?;
        ExecRefusalKind::from_errno(errno).map(|kind| Self::new(kind, errno))
    }

    /// The refusal class.
    #[must_use]
    pub const fn kind(self) -> ExecRefusalKind {
        self.kind
    }

    /// The raw errno the kernel reported.
    #[must_use]
    pub const fn errno(self) -> i32 {
        self.errno
    }
}

impl fmt::Display for ExecRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "exec refused the program: {} (errno {})",
            self.kind.as_str(),
            self.errno
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn exec_errnos_are_classified_and_other_spawn_errnos_are_not() {
        for (errno, kind) in [
            (nix::libc::EACCES, ExecRefusalKind::NotPermitted),
            (nix::libc::ENOEXEC, ExecRefusalKind::NotExecutable),
            (nix::libc::ENOENT, ExecRefusalKind::NotFound),
        ] {
            let refusal = ExecRefusal::from_spawn_error(&io::Error::from_raw_os_error(errno))
                .expect("an exec errno is classified");
            assert_eq!(refusal.kind(), kind);
            assert_eq!(refusal.errno(), errno);
        }
        // A fork refusal and a `pre_exec` failure are not exec refusals, so
        // they must not borrow the typed case.
        for errno in [nix::libc::EAGAIN, nix::libc::EIO, nix::libc::EPIPE] {
            assert!(
                ExecRefusal::from_spawn_error(&io::Error::from_raw_os_error(errno)).is_none(),
                "errno {errno} is not an exec refusal"
            );
        }
        assert!(
            ExecRefusal::from_spawn_error(&io::Error::from(io::ErrorKind::NotFound)).is_none(),
            "a synthetic error carries no errno to classify"
        );
    }

    #[test]
    fn an_exec_refusal_keeps_a_stable_spelling() {
        assert_eq!(
            ExecRefusal::new(ExecRefusalKind::NotPermitted, nix::libc::EACCES).to_string(),
            "exec refused the program: permission_denied (errno 13)"
        );
        assert_eq!(
            ExecRefusal::new(ExecRefusalKind::NotExecutable, nix::libc::ENOEXEC).to_string(),
            "exec refused the program: not_executable (errno 8)"
        );
        assert_eq!(
            ExecRefusal::new(ExecRefusalKind::NotFound, nix::libc::ENOENT).to_string(),
            "exec refused the program: not_found (errno 2)"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_create_process_errors_are_classified_and_others_are_not() {
        for (code, kind) in [
            (2, ExecRefusalKind::NotFound),
            (3, ExecRefusalKind::NotFound),
            (267, ExecRefusalKind::NotFound),
            (5, ExecRefusalKind::NotPermitted),
            (191, ExecRefusalKind::NotExecutable),
            (192, ExecRefusalKind::NotExecutable),
            (193, ExecRefusalKind::NotExecutable),
            (216, ExecRefusalKind::NotExecutable),
        ] {
            let refusal = ExecRefusal::from_spawn_error(&io::Error::from_raw_os_error(code))
                .expect("a CreateProcessW exec error is classified");
            assert_eq!(refusal.kind(), kind);
            assert_eq!(refusal.errno(), code);
        }
        for code in [8, 87, 109] {
            assert!(
                ExecRefusal::from_spawn_error(&io::Error::from_raw_os_error(code)).is_none(),
                "Win32 error {code} is not an exec refusal"
            );
        }
        assert!(
            ExecRefusal::from_spawn_error(&io::Error::from(io::ErrorKind::NotFound)).is_none(),
            "a synthetic error carries no OS error code to classify"
        );
    }
}
