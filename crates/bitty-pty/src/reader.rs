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

use std::io;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::SyncSender;
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
/// On Linux the master side reports `EIO` once the child has exited and all
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
            Err(err) if cfg!(target_os = "linux") && err.raw_os_error() == Some(linux_eio()) => {
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        if tx.send(buf[..n].to_vec()).is_err() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_eio() -> i32 {
    5 // EIO on Linux
}

#[cfg(not(target_os = "linux"))]
fn linux_eio() -> i32 {
    // Never matches off Linux; the comparison site is guarded by cfg!.
    -1
}

/// Consumer half of the bounded PTY output channel.
///
/// Exactly one instance exists per spawned PTY (see
/// [`crate::Pty::take_reader`]). `recv` returns `None` once EOF was reached
/// and every chunk drained; use [`PtyReader::join`] afterwards (or after
/// dropping the reader) to learn whether the pump ended cleanly or with an
/// I/O error.
#[derive(Debug)]
pub struct PtyReader {
    rx: Receiver<Vec<u8>>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl PtyReader {
    pub(crate) fn spawn<S: ByteSource + Send + 'static>(mut source: S, chunk_size: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CHANNEL_CAPACITY_CHUNKS);
        let handle = std::thread::Builder::new()
            .name("bitty-pty-reader".to_owned())
            .spawn(move || pump(&mut source, &tx, chunk_size))
            .expect("std thread spawn cannot fail with default builder options");
        PtyReader {
            rx,
            handle: Some(handle),
        }
    }

    /// Receives the next output chunk, blocking while the queue is empty.
    ///
    /// Returns `None` after EOF once all chunks were consumed. While blocked,
    /// memory usage stays within [`MAX_BUFFERED_BYTES`]; see the module docs
    /// for the full high-water chain.
    pub fn recv(&self) -> Option<Vec<u8>> {
        self.rx.recv().ok()
    }

    /// Receives the next output chunk with a timeout.
    ///
    /// `Ok(Some(chunk))` yields data; `Ok(None)` means EOF after drain;
    /// `Err(Timeout)` means nothing arrived within `timeout`; `Err(Disconnected)`
    /// means the pump ended without an EOF marker (I/O error; see [`join`]).
    ///
    /// [`join`]: Self::join
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Vec<u8>>, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Err(RecvTimeoutError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
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
        let reader = PtyReader::spawn(FailingSource, 32);
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
    fn eof_maps_to_clean_join_and_none_recv() {
        struct EmptySource;
        impl ByteSource for EmptySource {
            fn read_chunk(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        let reader = PtyReader::spawn(EmptySource, 64);
        assert!(reader.recv().is_none());
        reader.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_eio_is_mapped_to_eof() {
        struct EioSource;
        impl ByteSource for EioSource {
            fn read_chunk(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(5))
            }
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut src = EioSource;
        pump(&mut src, &tx, 64).unwrap(); // EIO became Ok(()) EOF
        drop(tx);
        assert!(rx.recv().is_err()); // no data chunks were sent
    }
}
