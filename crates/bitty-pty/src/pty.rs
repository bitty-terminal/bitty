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

/// Maximum bytes of a process name reported in [`ForegroundJob::name`]
/// (CTX-0370). The kernel interface (`/proc/<pid>/comm`) already caps names
/// at 16 bytes; this is a defensive display bound.
pub const MAX_JOB_NAME_BYTES: usize = 32;

/// A foreground job observed on a PTY: the kernel's foreground process-group
/// leader when it differs from the spawned child (the idle shell).
///
/// CTX-0370 busy definition: "the foreground process is not the shell
/// itself". An interactive shell at its prompt is the foreground group
/// leader == the spawned child pid, so it is *not* a job; a foreground
/// pipeline's leader is a distinct pid, so it *is*. Read-only observation,
/// bounded and cheap enough for a close-gesture path (never the input hot
/// path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    /// Foreground process-group leader pid (`tcgetpgrp`).
    pub pid: u32,
    /// Bounded process name when the platform exposes one cheaply
    /// (`/proc/<pid>/comm` on Linux); `None` elsewhere.
    pub name: Option<String>,
}

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

    /// Kernel foreground process-group leader pid, when the platform exposes
    /// one (Unix `tcgetpgrp` on the master fd; always `None` on Windows
    /// ConPTY). Read-only, non-blocking, best-effort.
    pub fn foreground_pgid(&self) -> Option<u32> {
        self.session.process_group_leader()
    }

    /// Foreground job running beyond the spawned shell, when detectable.
    ///
    /// CTX-0370 busy detection: the kernel foreground process group differs
    /// from the spawned child pid. The child is the session leader and starts
    /// as its own foreground group, so an idle shell reports `None`; a
    /// foreground pipeline/program (job control moves each job into its own
    /// process group) reports `Some` with a bounded name when the platform
    /// exposes one. `None` also means "cannot determine" (Windows ConPTY, or
    /// a dead PTY) — callers must treat it as *not busy*, never invent a
    /// guess.
    pub fn foreground_job(&self) -> Option<ForegroundJob> {
        let child = self.pid()?;
        let fg = foreground_job_pid(Some(child), self.foreground_pgid())?;
        Some(ForegroundJob {
            pid: fg,
            name: process_name(fg),
        })
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

/// Pure busy classifier shared by [`Pty::foreground_job`] and its tests:
/// returns the foreground job pid only when the kernel reports a foreground
/// process group that is not the spawned child (the idle shell).
///
/// `None` covers every "not busy / cannot determine" case: no child, no
/// foreground group, the shell itself in front, or a non-positive group id.
fn foreground_job_pid(child_pid: Option<u32>, fg_pgid: Option<u32>) -> Option<u32> {
    let child = child_pid?;
    let fg = fg_pgid?;
    (fg != child).then_some(fg)
}

/// Bounded process name for a job pid, when the platform exposes one.
///
/// Linux reads `/proc/<pid>/comm` (a kernel interface; 16-byte name), trims
/// the trailing newline, strips control characters, and truncates to
/// [`MAX_JOB_NAME_BYTES`]. Any read failure, non-UTF-8 content, or empty
/// result is `None` (fail-soft: the confirmation still names the pid).
/// Other platforms return `None` (no new dependency, no `/proc`).
fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        let cleaned: String = raw
            .trim()
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .take(MAX_JOB_NAME_BYTES)
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        Some(cleaned)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn busy_classifier_is_shell_identity_relative() {
        // CTX-0370: only a foreground group distinct from the child pid is a
        // job; every indeterminate case is "not busy" (never a false prompt).
        assert_eq!(foreground_job_pid(None, Some(2)), None, "no child");
        assert_eq!(foreground_job_pid(Some(1), None), None, "no fg group");
        assert_eq!(foreground_job_pid(Some(1), Some(1)), None, "shell in front");
        assert_eq!(
            foreground_job_pid(Some(1), Some(2)),
            Some(2),
            "job in front"
        );
    }

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

    /// Polls `check` until it returns `Some`, or panics past `timeout`.
    fn wait_until<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(std::time::Instant::now() < deadline, "poll timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn idle_shell_is_not_busy_and_running_job_is_detected() {
        // CTX-0370 live busy detection: a shell at its prompt reports no
        // foreground job; a foreground pipeline reports one; interrupting it
        // returns to idle. Real `/bin/sh` + kernel pgid state, bounded polls.
        bitty_test_support::require_pty!();
        use std::io::Write as _;

        let mut pty = crate::PtyBuilder::new("/bin/sh")
            .size(80, 24)
            .spawn()
            .expect("spawn sh");
        let mut writer = pty.take_writer().expect("writer half");
        let reader = pty.take_reader().expect("reader half");
        // Drain asynchronously? The bounded pump keeps startup output small;
        // the shell's prompt fits far under the 128 KiB channel cap.
        let _keep_reader = reader;

        // The shell must own the foreground before "idle" is meaningful.
        wait_until(Duration::from_secs(10), || {
            pty.foreground_pgid().map(|_| ())
        });
        assert_eq!(
            pty.foreground_job(),
            None,
            "idle shell is the foreground process, not a job"
        );

        writer.write_all(b"sleep 30\n").expect("write job");
        writer.flush().expect("flush job");
        let job = wait_until(Duration::from_secs(10), || pty.foreground_job());
        assert_ne!(
            job.pid,
            pty.pid().expect("child pid"),
            "job is not the shell"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(job.name.as_deref(), Some("sleep"), "bounded job name");

        // Ctrl-C through the line discipline interrupts the foreground job;
        // the shell takes the foreground back and busy clears.
        writer.write_all(b"\x03").expect("write intr");
        writer.flush().expect("flush intr");
        wait_until(Duration::from_secs(10), || {
            pty.foreground_job().is_none().then_some(())
        });

        // Clean teardown: shutdown kills + reaps the shell (SIGHUP reaches
        // the foreground group as the session dies).
        pty.shutdown().expect("shutdown");
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
