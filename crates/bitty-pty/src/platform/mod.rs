//! Platform backend compile seams.
//!
//! ADR-0002 makes Unix the Tier-1 launch platform with Windows following in a
//! later Tier-1 slice. The public API in this crate is therefore
//! cfg-independent, while the actual PTY mechanics live behind exactly one
//! internal module per platform:
//!
//! - `unix`: real implementation wrapping `portable-pty`'s Unix/POSIX PTY.
//! - `windows`: ConPTY compile seam. Types and signatures exist so dependent
//!   code compiles, but every operation reports
//!   [`PtyError::Unsupported`] until the dedicated Windows slice implements
//!   it (task CTX-0011 scope: implement Unix only).

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(unix)]
pub(crate) use unix as imp;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as imp;

#[cfg(not(any(unix, windows)))]
compile_error!("bitty-pty supports unix and windows targets; no backend exists for this platform");

use std::path::PathBuf;

use crate::builder::SpawnConfig;
use crate::error::PtyError;

/// Owned exit status of a reaped child, converted from the upstream status
/// representation inside the platform module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    success: bool,
    code: u32,
    signal: Option<String>,
}

impl ExitStatus {
    /// Whether the child exited successfully.
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// Raw exit code (meaningless when the child was killed by a signal).
    pub fn code(&self) -> u32 {
        self.code
    }

    /// Signal name if the child was terminated by a signal.
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }
}

/// Platform PTY session: the master end plus the spawned child.
pub(crate) struct Session {
    master: imp::Master,
    child: imp::Child,
}

/// Entry point used by [`crate::PtyBuilder::spawn`].
pub(crate) fn spawn_session(config: &SpawnConfig) -> Result<crate::pty::Pty, PtyError> {
    Session::open(config).map(crate::pty::Pty::new)
}

impl Session {
    /// Opens a fresh PTY of the configured size and spawns `config` into it.
    pub(crate) fn open(config: &SpawnConfig) -> Result<Self, PtyError> {
        let (master, child) = imp::open_pty_and_spawn(config)?;
        Ok(Session { master, child })
    }

    /// Resizes the terminal; the kernel delivers SIGWINCH to the foreground
    /// process group where applicable.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        imp::resize(&self.master, cols, rows)
    }

    /// Queries the current terminal size as known by the kernel.
    pub(crate) fn size(&self) -> Result<(u16, u16), PtyError> {
        imp::size(&self.master)
    }

    /// Terminal device name, when the platform exposes one.
    pub(crate) fn tty_name(&self) -> Option<PathBuf> {
        imp::tty_name(&self.master)
    }

    /// Process id of the child, when applicable.
    pub(crate) fn pid(&self) -> Option<u32> {
        imp::child_pid(&self.child)
    }

    /// Takes the readable master side. The returned reader may be moved to
    /// another thread; it feeds this crate's bounded pump.
    pub(crate) fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, PtyError> {
        imp::try_clone_reader(&self.master)
    }

    /// Takes the writable master side (once).
    pub(crate) fn take_writer(&mut self) -> Result<Box<dyn std::io::Write + Send>, PtyError> {
        imp::take_writer(&mut self.master)
    }

    /// Kills the child (SIGKILL-equivalent on Unix).
    pub(crate) fn kill(&mut self) -> Result<(), PtyError> {
        imp::kill(&mut self.child)
    }

    /// Polls for child exit without blocking.
    pub(crate) fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        imp::try_wait(&mut self.child)
    }

    /// Blocks until the child exits and reaps it.
    pub(crate) fn wait(&mut self) -> Result<ExitStatus, PtyError> {
        imp::wait(&mut self.child)
    }
}
