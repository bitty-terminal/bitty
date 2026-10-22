//! The [`PtyWriter`]: exclusive input half of a spawned PTY.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};

/// Terminal end-of-file character (POSIX `VEOF`, `^D`): the canonical line
/// discipline treats it as end-of-transmission once the current line is
/// complete, which is why the drop sequence leads with a newline.
#[cfg(unix)]
const DEFAULT_VEOF: u8 = 0x04;

/// Largest single frame any producer may hand the queue in one write.
///
/// The composer is the widest producer: a submit carries at most the
/// 64 KiB content cap (`COMPOSER_MAX_BYTES`, owned by `bitty-rich`) plus the
/// byte-exact bracketed-paste framing `ESC[200~` (6 bytes) + `ESC[201~`
/// (6 bytes) + `CR` (1 byte) = 13 bytes, for 65,549 bytes total.
///
/// `bitty-pty` deliberately does not depend on `bitty-rich` (the composer
/// sits above the PTY boundary), so this mirrors the cap instead of
/// importing it. The cross-crate regression in `bitty-runtime` builds that
/// frame with `bitty_rich::host::frame_submit` and fails if the two drift.
const MAX_PRODUCER_FRAME_BYTES: usize = 64 * 1024 + 13;

/// Maximum bytes buffered in one queued writer (bitty#2446).
///
/// The queue exists so the terminal event loop never parks in a PTY write
/// when the child stops reading its input (a previewing TUI, a stopped job,
/// a shell blocked on its own output backpressure).
///
/// The bound must exceed [`MAX_PRODUCER_FRAME_BYTES`] (65,549 bytes), because
/// a max-size composer submit is offered as one write while the child may be
/// idle: bounding at the composer cap alone rejected that legitimate frame
/// after `terminal_submit` reported `Accepted`, silently losing the input
/// (bitty#2446 review F1). 128 KiB accepts the largest frame plus a
/// partially drained companion burst, so the worst-case queued-input memory
/// stays bounded at 128 KiB per pane (plus one in-flight chunk per writer
/// thread). Beyond it writes fail closed with [`io::ErrorKind::WouldBlock`]
/// and the caller's existing loss accounting reports the dropped bytes.
pub const QUEUED_WRITE_MAX_BYTES: usize = 128 * 1024;

// One max-size producer frame must fit an otherwise idle queue; otherwise a
// submit reported as accepted would be dropped instead of written.
const _: () = assert!(QUEUED_WRITE_MAX_BYTES > MAX_PRODUCER_FRAME_BYTES);

/// Maximum queued commands (data chunks plus flush/shutdown markers).
///
/// Each command carries at most the caller's buffer, so this bound only
/// keeps a flood of tiny writes from growing the deque without bound.
pub const QUEUED_WRITE_MAX_COMMANDS: usize = 512;

/// One queued writer command, processed in order by the writer thread.
enum WriteCommand {
    Data(Vec<u8>),
    Flush,
    Shutdown,
}

/// Shared queue state between [`PtyWriter`] and its writer thread.
struct QueueState {
    commands: VecDeque<WriteCommand>,
    bytes: usize,
    /// The writer thread observed a write error (child gone): every later
    /// write fails closed instead of queueing bytes that can never land.
    failed: bool,
    /// The producer dropped the writer: the thread drains, sends EOT, exits.
    closed: bool,
}

struct WriteQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl WriteQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                commands: VecDeque::new(),
                bytes: 0,
                failed: false,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        // A panicking writer thread must not poison the terminal hot path.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enqueue one data chunk without ever blocking the caller.
    fn push(&self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.lock();
        if state.failed || state.closed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        if state.bytes.saturating_add(buf.len()) > QUEUED_WRITE_MAX_BYTES
            || state.commands.len() >= QUEUED_WRITE_MAX_COMMANDS
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pty writer queue is full",
            ));
        }
        state.bytes += buf.len();
        state.commands.push_back(WriteCommand::Data(buf.to_vec()));
        drop(state);
        self.ready.notify_one();
        Ok(buf.len())
    }

    /// Queue a flush marker without blocking.
    fn flush(&self) -> io::Result<()> {
        let mut state = self.lock();
        if state.failed || state.closed {
            return Ok(());
        }
        if state.commands.len() < QUEUED_WRITE_MAX_COMMANDS {
            state.commands.push_back(WriteCommand::Flush);
            drop(state);
            self.ready.notify_one();
        }
        Ok(())
    }

    /// Ask the thread to drain, emit the Unix EOT sequence, and exit.
    ///
    /// Never blocks: a thread parked in `write(2)` against a child that
    /// stopped reading will observe the shutdown only after that write
    /// returns. Detaching is deliberate — the terminal event loop must not
    /// wait for a wedged child; [`crate::Pty`] teardown kills the child,
    /// which unblocks the write.
    fn shutdown(&self) {
        let mut state = self.lock();
        if state.closed || state.failed {
            return;
        }
        state.commands.push_back(WriteCommand::Shutdown);
        state.closed = true;
        drop(state);
        self.ready.notify_one();
    }

    /// Record a terminal write failure and release queued bytes.
    fn fail(&self) {
        let mut state = self.lock();
        state.failed = true;
        state.commands.clear();
        state.bytes = 0;
        drop(state);
        self.ready.notify_one();
    }

    /// Bytes currently queued (test/diagnostic observation).
    #[must_use]
    fn queued_bytes(&self) -> usize {
        self.lock().bytes
    }
}

/// Drain commands in order on the writer thread.
fn writer_loop(mut inner: Box<dyn io::Write + Send>, queue: Arc<WriteQueue>) {
    loop {
        let command = {
            let mut state = queue.lock();
            while state.commands.is_empty() && !state.closed && !state.failed {
                state = queue
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let Some(command) = state.commands.pop_front() else {
                return;
            };
            if let WriteCommand::Data(bytes) = &command {
                state.bytes = state.bytes.saturating_sub(bytes.len());
            }
            command
        };
        match command {
            WriteCommand::Data(bytes) => {
                if inner.write_all(&bytes).is_err() {
                    queue.fail();
                    return;
                }
            }
            WriteCommand::Flush => {
                let _ = inner.flush();
            }
            WriteCommand::Shutdown => {
                #[cfg(unix)]
                {
                    // EOF is only interpreted after a newline, so send both
                    // before the descriptor closes (CTX-0477).
                    let _ = inner.write_all(&[b'\n', DEFAULT_VEOF]);
                }
                let _ = inner.flush();
                return;
            }
        }
    }
}

/// Which write path this handle uses.
enum Sink {
    /// Direct synchronous writes (unit tests and explicit callers that want
    /// the historical semantics).
    Direct(Box<dyn io::Write + Send>),
    /// Bounded FIFO drained by one writer thread (production PTYs).
    Queued(Arc<WriteQueue>),
}

/// Write half of the PTY master, feeding the child's terminal input.
///
/// Implementations of [`io::Write`] forward bytes to the child exactly as
/// written; this crate performs no line buffering, translation, or shell
/// interpretation.
///
/// **Blocking contract:** the terminal event loop owns every production
/// writer. `PtyWriter::new_queued` (used by [`crate::Pty::take_writer_async`])
/// copies input into a bounded FIFO and returns immediately, so a child that
/// stops reading its stdin — a previewing TUI, a stopped process, a shell
/// blocked on its own output — can never park the UI thread in `write(2)`.
/// The queue is `QUEUED_WRITE_MAX_BYTES`; overflow fails closed with
/// [`io::ErrorKind::WouldBlock`] and the caller's loss counters report it.
/// `PtyWriter::new` keeps the historical synchronous sink for tests that
/// assert immediate child-side effects.
///
/// **Drop semantics (Unix):** dropping the writer queues an
/// end-of-transmission sequence (newline plus the default EOF character
/// `^D`) before closing the descriptor. EOF is only interpreted by the line
/// discipline after a newline, so both bytes are sent together; line-oriented
/// children such as `cat` treat that as end of input and exit. The write is
/// best-effort: a child that already exited makes it fail, which the dropper
/// ignores. A terminal with a non-default `VEOF` is covered by the wrapped
/// upstream writer, which reads the configured EOF character on close. See
/// `crate::pty` for how this composes into graceful shutdown. The queued
/// sink enqueues the sequence instead of writing it on the dropping thread,
/// so a wedged child cannot stall teardown either.
///
/// **Drop semantics (Windows):** ConPTY has no line-discipline EOT; dropping
/// the writer closes the input handle, which is that backend's end-of-input
/// signal.
pub struct PtyWriter {
    sink: Sink,
}

impl PtyWriter {
    pub(crate) fn new(inner: Box<dyn io::Write + Send>) -> Self {
        PtyWriter {
            sink: Sink::Direct(inner),
        }
    }

    /// Wrap `inner` in a bounded queue drained by a named writer thread.
    ///
    /// If the thread cannot be spawned the handle falls back to direct
    /// writes rather than failing the PTY: thread exhaustion is already a
    /// degraded state, and preserving input at the historical blocking risk
    /// is better than removing the input path entirely. The fallback keeps
    /// the same public type, so callers cannot observe which sink is active.
    pub(crate) fn new_queued(inner: Box<dyn io::Write + Send>) -> Self {
        let queue = Arc::new(WriteQueue::new());
        // Park the sink so a refused spawn hands it back instead of dropping
        // it (same recovery shape as the read forwarder, CTX-0473).
        let slot = Arc::new(Mutex::new(Some(inner)));
        let thread_slot = Arc::clone(&slot);
        let thread_queue = Arc::clone(&queue);
        let spawned = std::thread::Builder::new()
            .name("bitty-pty-writer".to_owned())
            .spawn(move || {
                let inner = thread_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(inner) = inner {
                    writer_loop(inner, thread_queue);
                }
            });
        match spawned {
            Ok(_) => PtyWriter {
                sink: Sink::Queued(queue),
            },
            Err(_) => {
                let inner = slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                match inner {
                    Some(inner) => PtyWriter {
                        sink: Sink::Direct(inner),
                    },
                    // Unreachable: the worker only takes the slot when it is
                    // running, which requires a successful spawn. Keep total.
                    None => PtyWriter {
                        sink: Sink::Queued(queue),
                    },
                }
            }
        }
    }

    /// Bytes accepted but not yet written to the child (queued sink only).
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        match &self.sink {
            Sink::Direct(_) => 0,
            Sink::Queued(queue) => queue.queued_bytes(),
        }
    }
}

impl io::Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.sink {
            Sink::Direct(inner) => inner.write(buf),
            Sink::Queued(queue) => queue.push(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match &mut self.sink {
            Sink::Direct(inner) => inner.write_all(buf),
            // One copy, one queue push: `write_all` must not loop and block.
            Sink::Queued(queue) => queue.push(buf).map(|_| ()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.sink {
            Sink::Direct(inner) => inner.flush(),
            Sink::Queued(queue) => queue.flush(),
        }
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        match &mut self.sink {
            Sink::Direct(inner) => {
                #[cfg(unix)]
                {
                    // EOF is only interpreted after a newline, so send both
                    // before the descriptor closes (CTX-0477: the wrapper
                    // owns this sequence; the wrapped upstream writer closing
                    // the descriptor must not be the only thing standing
                    // between a line-oriented child and EOF).
                    let _ = inner.write_all(&[b'\n', DEFAULT_VEOF]);
                    let _ = inner.flush();
                }
                #[cfg(not(unix))]
                let _ = inner;
            }
            Sink::Queued(queue) => queue.shutdown(),
        }
    }
}

impl std::fmt::Debug for PtyWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyWriter")
            .field("queued", &matches!(self.sink, Sink::Queued(_)))
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

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

    /// Sink whose writes park until the test signals the gate.
    struct GatedWriter {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        recorded: Arc<Mutex<Vec<u8>>>,
        /// Gate only the first write; drop's EOT must pass through.
        gate: std::sync::atomic::AtomicBool,
    }

    impl io::Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.gate.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = self.entered.send(());
                let _ = self.release.recv();
            }
            self.recorded
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

    #[test]
    fn queued_write_returns_while_the_sink_is_blocked_issue_2446() {
        // bitty#2446: a child that stops reading stdin must not park the
        // event loop in `write(2)`. The queued writer returns after copying,
        // even while the wrapped sink is parked.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut writer = PtyWriter::new_queued(Box::new(GatedWriter {
            entered: entered_tx,
            release: release_rx,
            recorded: Arc::clone(&recorded),
            gate: std::sync::atomic::AtomicBool::new(true),
        }));

        let start = Instant::now();
        writer
            .write_all(b"focus-report")
            .expect("queued write must be accepted");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "queued write blocked for {:?}",
            start.elapsed()
        );
        // The worker reached the wrapped sink and is now parked there.
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer thread must enter the sink");
        release_tx.send(()).expect("release the sink");
        drop(writer);
        // Drop queues EOT on the same worker; it lands after the released
        // write. Poll briefly instead of assuming thread scheduling.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !recorded
                .lock()
                .expect("recording mutex")
                .ends_with(b"\n\x04")
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            recorded
                .lock()
                .expect("recording mutex")
                .ends_with(b"\n\x04"),
            "drop must still deliver EOT through the queue"
        );
    }

    /// Byte-exact max-size composer frame — `ESC[200~`, 64 KiB of payload,
    /// `ESC[201~`, and `CR` — mirroring `bitty-rich`'s `frame_submit` without
    /// depending on it (the composer sits above this crate's boundary).
    fn max_composer_frame() -> Vec<u8> {
        let mut frame = Vec::with_capacity(MAX_PRODUCER_FRAME_BYTES);
        frame.extend_from_slice(b"\x1b[200~");
        frame.extend(std::iter::repeat_n(b'q', 64 * 1024));
        frame.extend_from_slice(b"\x1b[201~");
        frame.push(b'\r');
        frame
    }

    #[test]
    fn queued_write_accepts_one_max_composer_frame_issue_2446_f1() {
        // bitty#2446 review F1: a max-size composer submit is 64 KiB of
        // content plus 13 bytes of bracketed-paste framing, offered as one
        // write while the child is idle. When the bound equalled the composer
        // cap the frame was rejected and `terminal_submit` still reported
        // `Accepted`, silently dropping the input.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut writer = PtyWriter::new_queued(Box::new(RecordingWriter(Arc::clone(&sink))));
        let frame = max_composer_frame();
        assert_eq!(frame.len(), MAX_PRODUCER_FRAME_BYTES);
        writer
            .write_all(&frame)
            .expect("an idle queue must accept the largest producer frame");

        // The worker copies the whole frame through (one command, one sink
        // write) and the queued-bytes accounting returns to zero.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let written = sink.lock().expect("recording mutex").len();
            if written >= frame.len() {
                assert_eq!(written, frame.len(), "frame must not be truncated");
                break;
            }
            assert!(Instant::now() < deadline, "frame never reached the sink");
            std::thread::sleep(Duration::from_millis(5));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while writer.queued_bytes() != 0 {
            assert!(Instant::now() < deadline, "queued bytes never drained");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Drop queues EOT after the frame; the exact-length check above
        // already proved every frame byte was offered verbatim.
        drop(writer);
    }

    #[test]
    fn queued_write_fails_closed_when_the_queue_is_full_issue_2446() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (_release_tx, release_rx) = std::sync::mpsc::channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut writer = PtyWriter::new_queued(Box::new(GatedWriter {
            entered: entered_tx,
            release: release_rx,
            recorded,
            gate: std::sync::atomic::AtomicBool::new(true),
        }));

        // Park the worker, then fill the bounded queue. Every call must
        // return; the overflow is a typed error, never a blocking write.
        writer.write_all(b"first").expect("first write");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer thread must enter the sink");
        let chunk = vec![b'x'; 4096];
        let start = Instant::now();
        let mut overflowed = false;
        for _ in 0..64 {
            match writer.write_all(&chunk) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    overflowed = true;
                    break;
                }
                Err(err) => panic!("unexpected queue error: {err}"),
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "queue fill blocked for {:?}",
            start.elapsed()
        );
        assert!(overflowed, "queue must fail closed instead of growing");
        assert!(writer.queued_bytes() <= QUEUED_WRITE_MAX_BYTES);
    }
}
