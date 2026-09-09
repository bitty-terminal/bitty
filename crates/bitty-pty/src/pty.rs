//! The [`Pty`] handle: lifecycle owner for a spawned child.
//!
//! Shutdown semantics, explicitly:
//!
//! - **Graceful:** drop or finish writing through [`PtyWriter`]; on Unix this
//!   sends an end-of-transmission sequence to the child, which typical
//!   line-oriented programs treat as EOF. Then call [`Pty::wait`].
//! - **Hard:** [`Pty::kill`] sends SIGKILL-equivalent termination;
//!   [`Pty::shutdown`] kills and reaps in one step.
//! - **Leak-free by default:** dropping [`Pty`] kills any unreaped child and
//!   blocks until it is reaped, so no zombie processes outlive the handle.
//!   Callers needing graceful shutdown must perform it before dropping.

use crate::error::PtyError;
use crate::platform::ExitStatus;
use crate::platform::Session;
use crate::reader::PtyReader;
use crate::reader::READ_CHUNK_SIZE;
use crate::reader::ReaderSource;
use crate::writer::PtyWriter;

/// Poll interval for [`Pty::wait_timeout`]: each poll is one non-blocking
/// kernel status check (`try_wait`), so a 5 ms cadence bounds CPU while
/// keeping reap latency negligible against second-scale timeouts.
const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// A child process running inside its own pseudo terminal.
///
/// Created exclusively through [`crate::PtyBuilder::spawn`]. The handle owns
/// the master end of the PTY and the child process; see the module docs for
/// shutdown semantics.
pub struct Pty {
    session: Session,
    reader_taken: bool,
    reaped: bool,
}

impl std::fmt::Debug for Pty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pty")
            .field("pid", &self.session.pid())
            .field("reader_taken", &self.reader_taken)
            .finish_non_exhaustive()
    }
}

impl Pty {
    pub(crate) fn new(session: Session) -> Self {
        Pty {
            session,
            reader_taken: false,
            reaped: false,
        }
    }

    /// Resizes the terminal to `cols` x `rows`.
    ///
    /// The kernel updates the window size and delivers SIGWINCH to the
    /// child's foreground process group where applicable.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        if cols == 0 || rows == 0 {
            return Err(PtyError::InvalidSize { cols, rows });
        }
        self.session.resize(cols, rows)
    }

    /// Queries the current terminal size from the kernel.
    pub fn size(&self) -> Result<(u16, u16), PtyError> {
        self.session.size()
    }

    /// Terminal device name, when the platform exposes one (always `None`
    /// on Windows: ConPTY has no device path).
    pub fn tty_name(&self) -> Option<std::path::PathBuf> {
        self.session.tty_name()
    }

    /// Process id of the child, when applicable.
    pub fn pid(&self) -> Option<u32> {
        self.session.pid()
    }

    /// Takes exclusive ownership of the output side.
    ///
    /// The returned [`PtyReader`] pumps kernel reads into a bounded channel
    /// on a dedicated thread; see the [`reader`](crate::reader) module docs
    /// for the backpressure contract. May be called only once per PTY.
    pub fn take_reader(&mut self) -> Result<PtyReader, PtyError> {
        if self.reader_taken {
            return Err(PtyError::HalfAlreadyTaken("reader"));
        }
        let raw = self.session.try_clone_reader()?;
        self.reader_taken = true;
        Ok(PtyReader::spawn(ReaderSource::new(raw), READ_CHUNK_SIZE))
    }

    /// Takes exclusive ownership of the input side.
    ///
    /// Dropping the returned [`PtyWriter`] signals end-of-transmission to the
    /// child (see its type docs). May be called only once per PTY.
    pub fn take_writer(&mut self) -> Result<PtyWriter, PtyError> {
        let inner = self.session.take_writer()?;
        Ok(PtyWriter::new(inner))
    }

    /// Terminates the child immediately (SIGKILL on Unix,
    /// terminate-process on Windows).
    ///
    /// Does not reap; follow with [`Pty::wait`] or use [`Pty::shutdown`].
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.reaped {
            return Err(PtyError::ChildAlreadyReaped);
        }
        self.session.kill()
    }

    /// Kills and reaps the child, returning its final status.
    pub fn shutdown(&mut self) -> Result<ExitStatus, PtyError> {
        self.kill()?;
        self.wait()
    }

    /// Polls whether the child has exited without blocking.
    ///
    /// Errors with [`PtyError::ChildAlreadyReaped`] once a previous
    /// [`Pty::wait`] or [`Pty::shutdown`] consumed the status.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        if self.reaped {
            return Err(PtyError::ChildAlreadyReaped);
        }
        let status = self.session.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    /// Blocks until the child exits and reaps it.
    pub fn wait(&mut self) -> Result<ExitStatus, PtyError> {
        if self.reaped {
            return Err(PtyError::ChildAlreadyReaped);
        }
        let status = self.session.wait()?;
        self.reaped = true;
        Ok(status)
    }

    /// Blocks until the child exits or `timeout` elapses, then reaps on success.
    ///
    /// Returns `Ok(Some(status))` when the child exited in time (reaped,
    /// exactly like [`Pty::wait`]), or `Ok(None)` when the deadline passed
    /// with the child still alive (nothing is reaped; follow with
    /// [`Pty::kill`] or [`Pty::shutdown`]). Errors with
    /// [`PtyError::ChildAlreadyReaped`] once a previous wait consumed the
    /// status.
    ///
    /// This polls [`Pty::try_wait`] instead of blocking in the platform
    /// primitive, so no code path waits past the deadline: an unbounded
    /// [`Pty::wait`] hangs the caller when a child never signals exit (seen
    /// with a `cmd /C exit` child under ConPTY on Windows CI, where it held
    /// the whole test binary for ~23 minutes). Prefer this over [`Pty::wait`]
    /// wherever the child is not known to exit on its own.
    pub fn wait_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<ExitStatus>, PtyError> {
        if self.reaped {
            return Err(PtyError::ChildAlreadyReaped);
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let status = self.session.try_wait()?;
            if status.is_some() {
                self.reaped = true;
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(WAIT_POLL_INTERVAL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wait_timeout_returns_none_while_child_is_alive() {
        bitty_test_support::require_pty!();
        let mut pty = crate::PtyBuilder::new("/bin/cat")
            .spawn()
            .expect("spawn cat");
        // `cat` blocks on stdin: it cannot exit within the timeout.
        let outcome = pty
            .wait_timeout(Duration::from_millis(200))
            .expect("wait_timeout");
        assert!(outcome.is_none(), "live child must time out");
        // Nothing reaped: kill then reap through the same bounded path.
        pty.kill().expect("kill");
        let status = pty
            .wait_timeout(Duration::from_secs(10))
            .expect("reap after kill")
            .expect("killed child must exit");
        assert!(!status.is_success());
        assert!(matches!(
            pty.wait_timeout(Duration::from_secs(1)),
            Err(PtyError::ChildAlreadyReaped)
        ));
    }

    #[test]
    fn wait_timeout_reaps_fast_exiting_child_like_wait() {
        bitty_test_support::require_pty!();
        let mut pty = crate::PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("exit 3")
            .spawn()
            .expect("spawn sh");
        let status = pty
            .wait_timeout(Duration::from_secs(10))
            .expect("wait_timeout")
            .expect("fast child must exit in time");
        assert!(!status.is_success());
        assert_eq!(status.code(), 3);
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.reaped {
            // Kill first (deterministic), then block on reaping so no zombie
            // survives the handle. SIGKILL cannot be caught by a healthy
            // child, so the wait terminates even for misbehaving programs.
            let _ = self.session.kill();
            let _ = self.session.wait();
            self.reaped = true;
        }
    }
}
