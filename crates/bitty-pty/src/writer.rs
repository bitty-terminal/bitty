//! The [`PtyWriter`]: exclusive input half of a spawned PTY.

use std::io;

/// Write half of the PTY master, feeding the child's terminal input.
///
/// Implementations of [`io::Write`] forward bytes to the child exactly as
/// written; this crate performs no line buffering, translation, or shell
/// interpretation.
///
/// **Drop semantics (Unix):** dropping the writer writes an
/// end-of-transmission sequence (newline plus the terminal's EOF character)
/// before closing the descriptor, mirroring the wrapped upstream behavior.
/// Line-oriented children such as `cat` treat that as end of input and exit;
/// see [`crate::pty`] for how this composes into graceful shutdown.
pub struct PtyWriter {
    inner: Box<dyn io::Write + Send>,
}

impl PtyWriter {
    pub(crate) fn new(inner: Box<dyn io::Write + Send>) -> Self {
        PtyWriter { inner }
    }
}

impl io::Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl std::fmt::Debug for PtyWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyWriter").finish_non_exhaustive()
    }
}
