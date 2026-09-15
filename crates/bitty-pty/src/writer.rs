//! The [`PtyWriter`]: exclusive input half of a spawned PTY.

use std::io;

/// Terminal end-of-file character (POSIX `VEOF`, `^D`): the canonical line
/// discipline treats it as end-of-transmission once the current line is
/// complete, which is why the drop sequence leads with a newline.
#[cfg(unix)]
const DEFAULT_VEOF: u8 = 0x04;

/// Write half of the PTY master, feeding the child's terminal input.
///
/// Implementations of [`io::Write`] forward bytes to the child exactly as
/// written; this crate performs no line buffering, translation, or shell
/// interpretation.
///
/// **Drop semantics (Unix):** dropping the writer writes an
/// end-of-transmission sequence (newline plus the default EOF character
/// `^D`) before closing the descriptor. EOF is only interpreted by the line
/// discipline after a newline, so both bytes are sent together; line-oriented
/// children such as `cat` treat that as end of input and exit. The write is
/// best-effort: a child that already exited makes it fail, which the dropper
/// ignores. A terminal with a non-default `VEOF` is covered by the wrapped
/// upstream writer, which reads the configured EOF character on close. See
/// [`crate::pty`] for how this composes into graceful shutdown.
///
/// **Drop semantics (Windows):** ConPTY has no line-discipline EOT; dropping
/// the writer closes the input handle, which is that backend's end-of-input
/// signal.
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

impl Drop for PtyWriter {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // EOF is only interpreted after a newline, so send both before the
            // descriptor closes (CTX-0477: the wrapper owns this sequence; the
            // wrapped upstream writer closing the descriptor must not be the
            // only thing standing between a line-oriented child and EOF).
            let _ = self.inner.write_all(&[b'\n', DEFAULT_VEOF]);
            let _ = self.inner.flush();
        }
    }
}

impl std::fmt::Debug for PtyWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyWriter").finish_non_exhaustive()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// `io::Write` sink that records every byte written into shared state.
    #[derive(Clone)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("recording mutex")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn drop_sends_newline_then_eof_character() {
        // CTX-0477: the wrapper must own the documented Unix drop-EOT
        // sequence instead of relying on the wrapped upstream writer.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::new(Box::new(RecordingWriter(Arc::clone(&sink))));
        drop(writer);
        assert_eq!(
            &*sink.lock().expect("recording mutex"),
            b"\n\x04",
            "drop must emit newline followed by the EOF character"
        );
    }
}
