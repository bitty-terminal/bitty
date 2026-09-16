//! Bounded PTY output pump and [`PtyReader`].
//!
//! # Backpressure contract
//!
//! PTY bytes are untrusted input; unbounded parsing or buffering is forbidden
//! by the security corpus. This module therefore enforces a hard memory
//! bound:
//!
//! - The pump reads at most [`READ_CHUNK_SIZE`] bytes per kernel read.
//! - Chunks travel through a `std::sync::mpsc` bounded channel holding at
//!   most [`CHANNEL_CAPACITY_CHUNKS`] slots.
//! - Total buffered payload inside this crate can therefore never exceed
//!   [`MAX_BUFFERED_BYTES`] (= chunk size x capacity), plus one in-flight
//!   chunk being read.
//!
//! # High-water behavior
//!
//! When a consumer stops draining [`PtyReader::recv`] while the child keeps
//! producing:
//!
//! 1. the channel fills to capacity;
//! 2. the pump thread blocks in `send`, stopping kernel-buffer drains;
//! 3. the kernel PTY buffer fills;
//! 4. the child's next `write()` to its terminal blocks — the operating
//!    system applies the backpressure end to end.
//!
//! No data is dropped and no memory grows; the child is simply suspended by
//! the kernel until the consumer catches up.
//!
//! # End-of-stream semantics
//!
//! A finished pump reports *why* it finished instead of folding every ending
//! into "no more data":
//!
//! - [`PtyReader::recv`] returns `Ok(None)` at clean EOF and `Err` when the
//!   pump stopped on an I/O failure (after every already-queued chunk was
//!   delivered).
//! - [`PtyReader::recv_timeout`] keeps `Ok(None)` for clean EOF and
//!   `Err(RecvTimeoutError::Disconnected)` for a pump failure.
//! - [`PtyReader::try_recv`] returns a [`PtyRecv`] that distinguishes a
//!   momentarily empty queue from clean EOF and from a failure.
//! - [`PtyReader::pump_error`] and [`PtyReader::join`] expose the recorded
//!   failure as owned data.

use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::TryRecvError;
use std::thread::JoinHandle;
use std::time::Duration;

/// Maximum payload bytes read from the PTY per kernel read.
pub const READ_CHUNK_SIZE: usize = 8 * 1024;

/// Number of chunk slots in the bounded channel between pump and consumer.
pub const CHANNEL_CAPACITY_CHUNKS: usize = 16;

/// Hard upper bound on buffered PTY payload inside this crate:
/// [`READ_CHUNK_SIZE`] x [`CHANNEL_CAPACITY_CHUNKS`] (128 KiB).
pub const MAX_BUFFERED_BYTES: usize = READ_CHUNK_SIZE * CHANNEL_CAPACITY_CHUNKS;

/// Source of PTY output bytes. Abstracted so the pump logic is unit-testable
/// against a fake source without a real process or file descriptor.
pub(crate) trait ByteSource {
    /// Reads up to `buf.len()` bytes; `Ok(0)` signals EOF.
    fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

/// Adapter from any [`io::Read`] (the platform master-pty reader handle)
/// into a [`ByteSource`].
pub(crate) struct ReaderSource<R> {
    inner: R,
}

impl<R: io::Read> ReaderSource<R> {
    pub(crate) fn new(inner: R) -> Self {
        ReaderSource { inner }
    }
}

impl<R: io::Read> ByteSource for ReaderSource<R> {
    fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// Drains `source` into `tx` until EOF or error, one bounded chunk at a time.
///
/// `send` is blocking: that call is the backpressure point described in the
/// module docs. A disconnected receiver (consumer dropped) ends the pump with
/// [`io::ErrorKind::BrokenPipe`] instead of leaking the thread.
///
/// On Unix the master side reports `EIO` once the child has exited and all
/// slave descriptors closed; that condition is mapped to clean EOF because it
/// carries no error information for the consumer.
pub(crate) fn pump<S: ByteSource>(
    source: &mut S,
    tx: &SyncSender<Vec<u8>>,
    chunk_size: usize,
) -> io::Result<()> {
    debug_assert!(chunk_size > 0);
    let mut buf = vec![0u8; chunk_size];
    loop {
        let n = match source.read_chunk(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(err) if is_terminal_pty_eof(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        if tx.send(buf[..n].to_vec()).is_err() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
    }
}

/// POSIX `EIO` errno. Reading a PTY master whose slave end is fully closed
/// reports this on Linux, macOS, and the BSDs; the value is part of the
/// stable ABI on every supported Unix target (no `libc` dependency needed).
#[cfg(unix)]
const POSIX_EIO: i32 = 5;

/// Whether `err` is the POSIX PTY end-of-stream condition: `EIO` on a master
/// whose slave side closed. Portability note (CTX-0477): this is an errno
/// property, not a Linux property, so the whole Unix family maps it to clean
/// EOF. Windows has no such mapping (`5` there is `ERROR_ACCESS_DENIED`, a
/// real failure).
#[cfg(unix)]
fn is_terminal_pty_eof(err: &io::Error) -> bool {
    err.raw_os_error() == Some(POSIX_EIO)
}

#[cfg(not(unix))]
fn is_terminal_pty_eof(_err: &io::Error) -> bool {
    false
}

/// Non-blocking read outcome from the bounded pump.
///
/// `Option<Vec<u8>>` cannot express the difference between "nothing queued
/// yet" and "the stream is over", nor between a clean end and an I/O
/// failure; this type can.
#[derive(Debug)]
pub enum PtyRecv {
    /// A chunk of output bytes is ready.
    Chunk(Vec<u8>),
    /// Nothing is queued right now; the pump is still running.
    Empty,
    /// The pump finished cleanly; every byte was delivered.
    Eof,
    /// The pump finished after an I/O failure. Chunks produced before the
    /// failure were already delivered; this outcome is terminal.
    Error(io::Error),
}

/// Owned record of the pump's terminal failure. Stored instead of an
/// [`io::Error`] because errors are not `Clone` and consumers may need to
/// observe the outcome more than once.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PumpFailure {
    kind: io::ErrorKind,
    message: String,
}

impl PumpFailure {
    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

/// Consumer half of the bounded PTY output channel.
///
/// Exactly one instance exists per spawned PTY (see
/// [`crate::Pty::take_reader`]). The receive methods report clean EOF versus
/// a pump I/O failure; use [`PtyReader::join`] afterwards (or after dropping
/// the reader) to consume the pump thread.
#[derive(Debug)]
pub struct PtyReader {
    rx: Receiver<Vec<u8>>,
    handle: Option<JoinHandle<io::Result<()>>>,
    failure: Arc<Mutex<Option<PumpFailure>>>,
}

impl PtyReader {
    /// Spawns the pump thread for `source`.
    ///
    /// Fallible (CTX-0477): thread creation can fail under resource pressure,
    /// so this surfaces an [`io::Error`] instead of panicking. On failure the
    /// owned `source` is dropped with the unstarted closure.
    pub(crate) fn spawn<S: ByteSource + Send + 'static>(
        mut source: S,
        chunk_size: usize,
    ) -> io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CHANNEL_CAPACITY_CHUNKS);
        let failure = Arc::new(Mutex::new(None));
        let thread_failure = Arc::clone(&failure);
        let handle = std::thread::Builder::new()
            .name("bitty-pty-reader".to_owned())
            .spawn(move || {
                let outcome = pump(&mut source, &tx, chunk_size);
                if let Err(err) = &outcome {
                    // Poison-tolerant: a panicking peer must not turn a
                    // recorded I/O failure into a second panic.
                    let recorded = PumpFailure {
                        kind: err.kind(),
                        message: err.to_string(),
                    };
                    *thread_failure
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(recorded);
                }
                outcome
            })?;
        Ok(PtyReader {
            rx,
            handle: Some(handle),
            failure,
        })
    }

    /// Receives the next output chunk, blocking while the queue is empty.
    ///
    /// `Ok(Some(chunk))` yields data; `Ok(None)` means clean EOF after the
    /// queue drained; `Err` surfaces an I/O failure from the pump thread
    /// (queued chunks are delivered first, so no bytes are lost). While
    /// blocked, memory usage stays within [`MAX_BUFFERED_BYTES`]; see the
    /// module docs for the full high-water chain.
    pub fn recv(&self) -> io::Result<Option<Vec<u8>>> {
        match self.rx.recv() {
            Ok(chunk) => Ok(Some(chunk)),
            Err(_) => self.disconnected(),
        }
    }

    /// Receives the next output chunk with a timeout.
    ///
    /// `Ok(Some(chunk))` yields data; `Ok(None)` means clean EOF after drain;
    /// `Err(Timeout)` means nothing arrived within `timeout`;
    /// `Err(Disconnected)` means the pump ended with an I/O failure. The
    /// failure itself is available from [`PtyReader::pump_error`] or
    /// [`PtyReader::join`].
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Vec<u8>>, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Err(RecvTimeoutError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                if self.pump_error().is_some() {
                    Err(RecvTimeoutError::Disconnected)
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Non-blocking attempt to receive the next chunk.
    ///
    /// Returns [`PtyRecv::Chunk`] when data is immediately available,
    /// [`PtyRecv::Empty`] when the queue is momentarily empty, and a terminal
    /// variant ([`PtyRecv::Eof`] or [`PtyRecv::Error`]) once the pump has
    /// finished. Use [`recv`](Self::recv) for blocking or
    /// [`recv_timeout`](Self::recv_timeout) when a deadline is needed. This
    /// helper exists so embedders (e.g. `bitty-runtime::Runtime::poll_pty`)
    /// can drain without blocking the render thread while preserving the
    /// bounded channel backpressure contract.
    pub fn try_recv(&self) -> PtyRecv {
        match self.rx.try_recv() {
            Ok(chunk) => PtyRecv::Chunk(chunk),
            Err(TryRecvError::Empty) => PtyRecv::Empty,
            Err(TryRecvError::Disconnected) => match self.pump_error() {
                Some(err) => PtyRecv::Error(err),
                None => PtyRecv::Eof,
            },
        }
    }

    /// The pump thread's I/O failure, when the stream ended with one.
    ///
    /// `None` while the stream is live or when it ended at clean EOF. The
    /// value is owned and may be queried repeatedly; [`join`](Self::join)
    /// returns the same failure through the thread result.
    pub fn pump_error(&self) -> Option<io::Error> {
        self.failure
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(PumpFailure::to_io_error)
    }

    /// Classifies a disconnected channel as clean EOF or recorded failure.
    fn disconnected(&self) -> io::Result<Option<Vec<u8>>> {
        match self.pump_error() {
            Some(err) => Err(err),
            None => Ok(None),
        }
    }

    /// Joins the pump thread and returns its terminal outcome.
    ///
    /// `Ok(())` means clean EOF; `Err` surfaces read failures (or the
    /// broken pipe caused by dropping this reader's receiver early). Does not
    /// require the queue to be drained first.
    pub fn join(mut self) -> io::Result<()> {
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    /// Fake byte source replaying canned chunks (each smaller than any test
    /// chunk size) and recording how many reads produced data.
    struct FakeSource {
        chunks: Vec<Vec<u8>>,
        next: usize,
        produced: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn new(chunks: Vec<Vec<u8>>) -> (Self, Arc<AtomicUsize>) {
            let produced = Arc::new(AtomicUsize::new(0));
            (
                FakeSource {
                    chunks,
                    next: 0,
                    produced: Arc::clone(&produced),
                },
                produced,
            )
        }
    }

    impl ByteSource for FakeSource {
        fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.next >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.next];
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            self.next += 1;
            self.produced.fetch_add(1, Ordering::SeqCst);
            Ok(n)
        }
    }

    /// Fake byte source failing immediately, emulating a kernel read error.
    struct FailingSource;
    impl ByteSource for FailingSource {
        fn read_chunk(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("fake read failure"))
        }
    }

    #[test]
    fn pump_delivers_chunks_in_order_until_eof() {
        let payload: Vec<Vec<u8>> =
            vec![b"hello ".to_vec(), b"bounded ".to_vec(), b"world".to_vec()];
        let expected: Vec<u8> = payload.iter().flatten().copied().collect();

        let (mut source, _) = FakeSource::new(payload);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        pump(&mut source, &tx, 64).unwrap();
        drop(tx);

        let mut got = Vec::new();
        while let Ok(chunk) = rx.recv() {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn pump_respects_channel_bound_with_idle_consumer() {
        // Infinite producer; an unbounded implementation would run away.
        let infinite: Vec<Vec<u8>> = (0u64..10_000).map(|i| i.to_le_bytes().to_vec()).collect();
        let (mut source, produced) = FakeSource::new(infinite);

        const CAPACITY: usize = 2;
        const CHUNK: usize = 32;
        let (tx, rx) = std::sync::mpsc::sync_channel(CAPACITY);
        let handle = std::thread::spawn(move || {
            let _ = pump(&mut source, &tx, CHUNK);
        });

        // Give the pump ample time to saturate the channel. With capacity 2
        // it may hold at most CAPACITY queued chunks plus one blocked inside
        // `send`: 3 produced chunks total. A runaway pump would have produced
        // orders of magnitude more within this window.
        std::thread::sleep(Duration::from_millis(150));
        let high_water = produced.load(Ordering::SeqCst);
        assert!(
            high_water <= CAPACITY + 1,
            "backpressure violated: {high_water} chunks produced with idle consumer"
        );

        // Drain everything; the pump must finish cleanly afterwards.
        drop(rx);
        handle.join().ok();
    }

    #[test]
    fn pumped_chunks_never_exceed_declared_chunk_size() {
        // One huge logical payload; the pump must split it into <= CHUNK
        // sized pieces because the fake source honors the provided buffer.
        struct BigSource {
            remaining: usize,
        }
        impl ByteSource for BigSource {
            fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Ok(0);
                }
                let n = buf.len().min(self.remaining);
                buf[..n].fill(b'x');
                self.remaining -= n;
                Ok(n)
            }
        }
        const CHUNK: usize = 64;
        let total = CHUNK * 5 + 17;
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let mut src = BigSource { remaining: total };
        pump(&mut src, &tx, CHUNK).unwrap();
        drop(tx);

        let mut received = 0usize;
        let mut max_chunk = 0usize;
        while let Ok(chunk) = rx.recv() {
            max_chunk = max_chunk.max(chunk.len());
            assert!(chunk.len() <= CHUNK);
            received += chunk.len();
        }
        assert_eq!(received, total);
        assert!(max_chunk > 0 && max_chunk <= CHUNK);
    }

    #[test]
    fn read_errors_surface_through_join() {
        let reader = PtyReader::spawn(FailingSource, 32).expect("spawn pump");
        let outcome = reader.join().unwrap_err();
        assert_eq!(outcome.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn dropping_receiver_unblocks_pump_with_broken_pipe() {
        struct InfiniteSource;
        impl ByteSource for InfiniteSource {
            fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'z');
                Ok(buf.len())
            }
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx);
        let mut infinite = InfiniteSource;
        let err = pump(&mut infinite, &tx, 64).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn clean_eof_is_distinguished_from_failure() {
        // CTX-0477: a clean end reports Ok(None)/Eof with no recorded error,
        // while the receive methods no longer fold that into "no data".
        struct EmptySource;
        impl ByteSource for EmptySource {
            fn read_chunk(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        let reader = PtyReader::spawn(EmptySource, 64).expect("spawn pump");
        assert!(matches!(reader.recv(), Ok(None)));
        assert!(matches!(reader.try_recv(), PtyRecv::Eof));
        assert!(matches!(
            reader.recv_timeout(Duration::from_millis(10)),
            Ok(None)
        ));
        assert!(reader.pump_error().is_none());
        reader.join().unwrap();
    }

    #[test]
    fn pump_failure_is_distinguished_from_clean_eof() {
        // CTX-0477: the same terminal signal (channel disconnect) must say
        // *why* the pump stopped, not pretend the stream reached EOF.
        let reader = PtyReader::spawn(FailingSource, 32).expect("spawn pump");
        assert!(reader.recv().is_err());
        assert!(matches!(reader.try_recv(), PtyRecv::Error(_)));
        assert!(matches!(
            reader.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected)
        ));
        let error = reader.pump_error().expect("recorded pump failure");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(reader.join().is_err());
    }

    #[test]
    fn queued_chunks_are_delivered_before_the_terminal_failure() {
        struct DataThenFail {
            first: bool,
        }
        impl ByteSource for DataThenFail {
            fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    buf[0] = b'z';
                    return Ok(1);
                }
                Err(io::Error::other("boom after data"))
            }
        }
        let reader = PtyReader::spawn(DataThenFail { first: true }, 8).expect("spawn pump");
        assert!(matches!(reader.recv(), Ok(Some(chunk)) if chunk == b"z"));
        assert!(reader.recv().is_err());
        assert!(matches!(reader.try_recv(), PtyRecv::Error(_)));
        assert!(reader.join().is_err());
    }

    #[test]
    fn try_recv_reports_empty_while_the_pump_is_live() {
        struct GatedSource {
            release: Arc<AtomicBool>,
            sent: bool,
        }
        impl ByteSource for GatedSource {
            fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.sent {
                    return Ok(0);
                }
                while !self.release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.sent = true;
                buf[0] = b'x';
                Ok(1)
            }
        }
        let release = Arc::new(AtomicBool::new(false));
        let reader = PtyReader::spawn(
            GatedSource {
                release: Arc::clone(&release),
                sent: false,
            },
            8,
        )
        .expect("spawn pump");
        assert!(matches!(reader.try_recv(), PtyRecv::Empty));
        release.store(true, Ordering::SeqCst);
        assert!(matches!(reader.recv(), Ok(Some(_))));
        assert!(matches!(reader.recv(), Ok(None)));
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn eio_is_mapped_to_eof_on_unix() {
        // CTX-0477: the master read after the slave side closes reports EIO
        // on Linux, macOS, and the BSDs alike; that is a Unix-wide property
        // rather than a Linux-only guard.
        struct EioSource;
        impl ByteSource for EioSource {
            fn read_chunk(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(POSIX_EIO))
            }
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut src = EioSource;
        pump(&mut src, &tx, 64).unwrap(); // EIO became Ok(()) EOF
        drop(tx);
        assert!(rx.recv().is_err()); // no data chunks were sent
    }
}
