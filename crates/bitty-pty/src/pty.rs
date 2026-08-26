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

    /// Terminal device name (Unix), when available.
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

    /// Terminates the child immediately (SIGKILL-equivalent on Unix).
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
