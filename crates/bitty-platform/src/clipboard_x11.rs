//! Bounded X11 text acquisition (CTX-1366 path B).
//!
//! The locked `arboard 3.6.1` X11 read requests a property with
//! `u32::MAX / 4` four-byte units, materializes every ordinary reply, and
//! appends complete INCR chunks while refreshing an inactivity timer. This
//! module replaces that read with a small dedicated `x11rb` client whose
//! acquisition stage is bounded:
//!
//! - **Bounded size:** every `GetProperty` asks for at most
//!   [`x11_window_bytes`](crate::clipboard_limits::x11_window_bytes)
//!   four-byte units (the decoded ceiling rounded up), and the INCR
//!   accumulator refuses a chunk before retaining it when it would cross the
//!   ceiling ([`IncrAccumulator`]);
//! - **Over cap is a typed rejection:** an ordinary reply with `bytes_after >
//!   0`, or any accumulation above the ceiling, returns
//!   [`AcquisitionError::OverLimit`] and no text is delivered (no partial
//!   value);
//! - **Bounded wait:** one absolute monotonic deadline covers connection setup,
//!   atom/window/selection requests, the `SelectionNotify` wait, every
//!   `GetProperty` reply, and every INCR `PropertyNotify` wait. The connection
//!   uses a [`DeadlineStream`] wrapper that bounds x11rb's internal `poll`
//!   retry loops **and** its packet-reader `read` loop (which otherwise runs
//!   until the socket would block), so no single call can wait past the
//!   deadline;
//! - **No unbounded fallback:** the Linux read funnel never falls back to
//!   `arboard` for a failed X11 read.
//!
//! Trust and residual scope: the requestor treats the clipboard owner as
//! untrusted behind a protocol-conforming session X server. Requested reply
//! lengths bound a conforming server's property reply; they do not bound a
//! hostile X server's packet/event/FD queues or the server's own memory. The
//! initial socket `connect` and local name resolution happen before the
//! deadline stream exists and are therefore not covered by the deadline (they
//! are local, address-family operations in the normal case). Teardown is
//! best-effort `DeleteProperty`/`DestroyWindow` without waiting for a peer
//! acknowledgement.
//!
//! Auth/connection flow: this reuses the same x11rb primitives `x11rb::connect`
//! uses (`parse_display`, `DefaultStream::connect`, `xauth::get_auth`, and
//! `RustConnection::connect_to_stream_with_auth_info`) so `DISPLAY` and
//! `XAUTHORITY` semantics are unchanged; only the stream is wrapped to enforce
//! the deadline.

use std::io;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, GetPropertyReply, Property,
    SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::reexports::x11rb_protocol::xauth::get_auth;
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use crate::clipboard::BoundedText;
use crate::clipboard_limits::{
    AcquisitionError, AcquisitionLimits, IncrAccumulator, IncrReply, X11Encoding,
    classify_incr_reply, decode_x11_text, x11_long_length_units, x11_property_within_cap,
};

const STAGE_CONNECT: &str = "x11-connect";
const STAGE_SELECTION: &str = "x11-selection";
const STAGE_PROPERTY: &str = "x11-property";
const STAGE_INCR: &str = "x11-incr";

/// Which X11 selection a bounded read targets.
#[derive(Debug, Clone, Copy)]
pub(crate) enum X11Selection {
    /// Regular clipboard (`CLIPBOARD`).
    Clipboard,
    /// Primary selection (`PRIMARY`, middle-click).
    Primary,
}

impl X11Selection {
    const fn atom_name(self) -> &'static [u8] {
        match self {
            Self::Clipboard => b"CLIPBOARD",
            Self::Primary => b"PRIMARY",
        }
    }
}

/// One absolute acquisition deadline plus its configured budget.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: Instant,
    timeout_ms: u64,
}

impl Deadline {
    fn new(budget: Duration) -> Result<Self, AcquisitionError> {
        let timeout_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
        let at = Instant::now()
            .checked_add(budget)
            .ok_or(AcquisitionError::DeadlineExceeded {
                stage: STAGE_CONNECT,
                timeout_ms,
            })?;
        Ok(Self { at, timeout_ms })
    }

    fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    fn error(&self, stage: &'static str) -> AcquisitionError {
        AcquisitionError::DeadlineExceeded {
            stage,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// Waits for `flags` on `fd` no longer than the absolute `deadline`.
///
/// Returns [`deadline_io_error`] when the deadline expires first. `poll(2)` is
/// retried after `EINTR` with the recomputed remaining time; the `Stream`
/// contract forbids surfacing `Interrupted` to x11rb.
fn wait_for_fd(fd: impl AsFd, flags: PollFlags, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(deadline_io_error());
        }
        let mut fds = [PollFd::new(fd.as_fd(), flags)];
        // Round down: `poll` must never sleep past the absolute deadline. A
        // sub-millisecond remainder becomes a zero timeout; if it elapses
        // before the deadline the helper reports a spurious wake and the
        // caller retries with the recomputed remainder.
        let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
        match poll(&mut fds, timeout) {
            // The floor-rounded timeout elapsed. If the absolute deadline is
            // still ahead, report a spurious wake and let the caller retry
            // with the recomputed remaining time; otherwise fail typed.
            Ok(0) if Instant::now() >= deadline => return Err(deadline_io_error()),
            Ok(0) => return Ok(()),
            Ok(_) => return Ok(()),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => return Err(io::Error::from(errno)),
        }
    }
}

/// A `DefaultStream` wrapper that enforces the acquisition deadline in
/// `poll` **and** in `read`.
///
/// `DefaultStream` is already non-blocking for reads/writes but its
/// `Stream::poll` uses an infinite `poll(2)`, so the wrapper bounds `poll` to
/// the remaining time for x11rb's handshake/write/read retry loops.
///
/// Bounding `poll` alone is not enough: x11rb's `PacketReader::try_read_packets`
/// loops over `Stream::read` until `WouldBlock`, EOF, or an error and consults
/// no time source. A server (or local client keeping the socket readable) can
/// therefore make one `poll_for_event()` call outlive the caller's deadline
/// without ever calling `poll` again. `read` consequently checks the absolute
/// deadline first, waits for readability only for the remaining time, and
/// re-checks after the read; the deadline error propagates out of
/// `try_read_packets` and `map_request_error` keeps it typed. Writes stay
/// non-blocking and are bounded by x11rb's `poll` retry loop.
#[derive(Debug)]
struct DeadlineStream<S> {
    inner: S,
    deadline: Instant,
}

impl<S: Stream + AsFd> Stream for DeadlineStream<S> {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::POLLIN;
        }
        if mode.writable() {
            flags |= PollFlags::POLLOUT;
        }
        wait_for_fd(self.inner.as_fd(), flags, self.deadline)
    }

    fn read(&self, buf: &mut [u8], fd_storage: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        // Top-of-read check: `try_read_packets` never consults a clock, so
        // this is the only place that can stop its loop once the budget ends.
        wait_for_fd(self.inner.as_fd(), PollFlags::POLLIN, self.deadline)?;
        let read = self.inner.read(buf, fd_storage)?;
        // Re-check after the read: even a single syscall that returned data
        // just past the deadline must not let the packet loop continue.
        if Instant::now() >= self.deadline {
            return Err(deadline_io_error());
        }
        Ok(read)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.inner.write(buf, fds)
    }

    fn write_vectored(
        &self,
        bufs: &[io::IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.inner.write_vectored(bufs, fds)
    }
}

fn deadline_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "clipboard acquisition deadline exceeded",
    )
}

type BoundedConnection = RustConnection<DeadlineStream<DefaultStream>>;

/// Attempts one bounded X11 text read with production limits.
///
/// Every failure is typed and final for this read: the Linux funnel does not
/// retry through `arboard`. `Unavailable` means no text target was offered or
/// the connection could not be established; `OverLimit` means the producer
/// exceeded the ceiling and no partial value is returned.
pub(crate) fn try_read_x11_text(selection: X11Selection) -> Result<BoundedText, AcquisitionError> {
    try_read_x11_text_with(selection, AcquisitionLimits::production())
}

/// [`try_read_x11_text`] with explicit limits (tests inject short deadlines).
fn try_read_x11_text_with(
    selection: X11Selection,
    limits: AcquisitionLimits,
) -> Result<BoundedText, AcquisitionError> {
    let deadline = Deadline::new(limits.deadline)?;
    let (conn, screen) = connect(&deadline)?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .map(|screen| screen.root)
        .ok_or_else(|| AcquisitionError::Transport {
            stage: STAGE_CONNECT,
            detail: format!("X11 screen {screen} is not present in the setup reply"),
        })?;

    let selection_atom = intern(&conn, selection.atom_name(), &deadline)?;
    let utf8_atom = intern(&conn, b"UTF8_STRING", &deadline)?;
    let string_atom = Atom::from(AtomEnum::STRING);
    let incr_atom = intern(&conn, b"INCR", &deadline)?;
    let property_atom = intern(&conn, b"BITTY_CLIPBOARD", &deadline)?;

    let window = conn
        .generate_id()
        .map_err(|err| transport(STAGE_SELECTION, err))?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|err| transport(STAGE_SELECTION, err))?;
    let _requestor = RequestorWindow {
        conn: &conn,
        window,
        property: property_atom,
    };

    // Prefer UTF8_STRING and fall back to STRING (Latin-1) exactly once. A
    // refusal is an immediate SelectionNotify with property == None.
    let (encoding, reply) = match request_selection(
        &conn,
        window,
        selection_atom,
        utf8_atom,
        property_atom,
        &deadline,
    )? {
        true => (
            X11Encoding::Utf8,
            read_property(&conn, window, property_atom, limits.max_bytes, &deadline)?,
        ),
        false => match request_selection(
            &conn,
            window,
            selection_atom,
            string_atom,
            property_atom,
            &deadline,
        )? {
            true => (
                X11Encoding::Latin1,
                read_property(&conn, window, property_atom, limits.max_bytes, &deadline)?,
            ),
            false => {
                return Err(AcquisitionError::Unavailable(String::from(
                    "X11 selection owner offered no UTF8_STRING or STRING target",
                )));
            }
        },
    };

    if reply.type_ == incr_atom {
        read_incr(&conn, window, property_atom, encoding, limits, &deadline)
    } else {
        decode_ordinary(reply, encoding, limits.max_bytes)
    }
}

/// Opens a dedicated X11 connection with the acquisition deadline.
///
/// Mirrors `x11rb::connect` (`DISPLAY`, address iteration, Xauthority lookup)
/// but wraps the stream so x11rb's internal waits obey the deadline.
fn connect(deadline: &Deadline) -> Result<(BoundedConnection, usize), AcquisitionError> {
    let parsed = parse_display(None).map_err(|err| {
        AcquisitionError::Unavailable(format!("X11 display could not be parsed: {err}"))
    })?;
    let screen: usize = parsed.screen.into();
    let mut last_error = None;
    for address in parsed.connect_instruction() {
        if deadline.expired() {
            return Err(deadline.error(STAGE_CONNECT));
        }
        let (stream, (family, address_bytes)) = match DefaultStream::connect(&address) {
            Ok(connected) => connected,
            Err(err) => {
                last_error = Some(err.to_string());
                continue;
            }
        };
        // Same auth lookup as `x11rb::connect`: an auth lookup failure tries
        // without auth instead of failing the connection.
        let (auth_name, auth_data) = get_auth(family, &address_bytes, parsed.display)
            .unwrap_or(None)
            .unwrap_or_default();
        let stream = DeadlineStream {
            inner: stream,
            deadline: deadline.at,
        };
        match RustConnection::connect_to_stream_with_auth_info(stream, screen, auth_name, auth_data)
        {
            Ok(connection) => return Ok((connection, screen)),
            Err(err) => {
                if deadline.expired() {
                    return Err(deadline.error(STAGE_CONNECT));
                }
                last_error = Some(err.to_string());
            }
        }
    }
    Err(AcquisitionError::Unavailable(format!(
        "X11 connection unavailable: {}",
        last_error.unwrap_or_else(|| String::from("no connect address"))
    )))
}

/// Interns one atom under the deadline.
fn intern(
    conn: &BoundedConnection,
    name: &[u8],
    deadline: &Deadline,
) -> Result<Atom, AcquisitionError> {
    conn.intern_atom(false, name)
        .map_err(|err| transport(STAGE_SELECTION, err))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|err| map_request_error(err, deadline, STAGE_SELECTION))
}

/// Asks the owner for `target`; returns whether the property was produced.
///
/// A `SelectionNotify` with `property == None` is a refusal, not a transport
/// failure, so the caller can try the next supported target within the same
/// deadline.
fn request_selection(
    conn: &BoundedConnection,
    window: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    deadline: &Deadline,
) -> Result<bool, AcquisitionError> {
    conn.convert_selection(window, selection, target, property, x11rb::CURRENT_TIME)
        .map_err(|err| transport(STAGE_SELECTION, err))?;
    conn.flush()
        .map_err(|err| map_request_error(err, deadline, STAGE_SELECTION))?;
    let notify = wait_for_selection(conn, window, deadline)?;
    Ok(notify.property != x11rb::NONE)
}

/// Waits for the `SelectionNotify` addressed to this requestor window.
fn wait_for_selection(
    conn: &BoundedConnection,
    window: Window,
    deadline: &Deadline,
) -> Result<SelectionNotifyEvent, AcquisitionError> {
    drain_until(
        deadline,
        STAGE_SELECTION,
        || {
            conn.poll_for_event()
                .map_err(|err| map_request_error(err, deadline, STAGE_SELECTION))
        },
        |event| match event {
            Event::SelectionNotify(notify) if notify.requestor == window => Some(notify),
            _ => None,
        },
    )
}

/// Reads one property reply bounded to `max_bytes` (rounded up to four bytes).
fn read_property(
    conn: &BoundedConnection,
    window: Window,
    property: Atom,
    max_bytes: usize,
    deadline: &Deadline,
) -> Result<GetPropertyReply, AcquisitionError> {
    conn.get_property(
        true,
        window,
        property,
        AtomEnum::ANY,
        0,
        x11_long_length_units(max_bytes),
    )
    .map_err(|err| transport(STAGE_PROPERTY, err))?
    .reply()
    .map_err(|err| map_request_error(err, deadline, STAGE_PROPERTY))
}

/// Reads one property reply for the INCR continuation, bounded to the
/// accumulator's remaining window.
fn read_incr_property(
    conn: &BoundedConnection,
    window: Window,
    property: Atom,
    accumulator: &IncrAccumulator,
    deadline: &Deadline,
) -> Result<GetPropertyReply, AcquisitionError> {
    conn.get_property(
        true,
        window,
        property,
        AtomEnum::ANY,
        0,
        accumulator.remaining_units(),
    )
    .map_err(|err| transport(STAGE_INCR, err))?
    .reply()
    .map_err(|err| map_request_error(err, deadline, STAGE_INCR))
}

/// Validates and decodes an ordinary (non-INCR) property reply.
fn decode_ordinary(
    reply: GetPropertyReply,
    requested: X11Encoding,
    max_bytes: usize,
) -> Result<BoundedText, AcquisitionError> {
    if reply.format != 8 {
        return Err(AcquisitionError::Malformed {
            stage: STAGE_PROPERTY,
        });
    }
    let encoding = if reply.type_ == Atom::from(AtomEnum::STRING) {
        X11Encoding::Latin1
    } else {
        // UTF8_STRING and any other text target we requested are decoded as
        // the requested encoding (the server may alias the target atom).
        requested
    };
    let value_len = reply.value.len();
    let bytes_after = usize::try_from(reply.bytes_after).unwrap_or(usize::MAX);
    if !x11_property_within_cap(value_len, bytes_after, max_bytes) {
        return Err(AcquisitionError::OverLimit {
            stage: STAGE_PROPERTY,
            observed_len: value_len.saturating_add(bytes_after),
        });
    }
    decode_x11_text(&reply.value, encoding, max_bytes)
}

/// Consumes an INCR transfer under the same deadline and decoded ceiling.
///
/// The advertisement (the initial `INCR` typed property) is never treated as
/// an allocation size. After the initial property is deleted, each
/// `PropertyNotify`/`GetProperty` pair appends at most the remaining window;
/// a chunk that would cross the ceiling, or a reply that still has
/// `bytes_after`, is a typed over-limit rejection with no delivered text.
fn read_incr(
    conn: &BoundedConnection,
    window: Window,
    property: Atom,
    encoding: X11Encoding,
    limits: AcquisitionLimits,
    deadline: &Deadline,
) -> Result<BoundedText, AcquisitionError> {
    let mut accumulator = IncrAccumulator::new(limits.max_bytes);
    // The initial `GetProperty(delete = true)` already acknowledged the
    // announcement; the owner starts sending chunks after that delete
    // notification (ICCCM INCR contract), so no second delete is sent.
    loop {
        wait_for_property(conn, window, property, deadline)?;
        let reply = read_incr_property(conn, window, property, &accumulator, deadline)?;
        let bytes_after = usize::try_from(reply.bytes_after).unwrap_or(usize::MAX);
        match classify_incr_reply(reply.format, reply.value.len(), bytes_after)? {
            IncrReply::Complete => return accumulator.finish(encoding),
            IncrReply::Append => accumulator.push(&reply.value)?,
            IncrReply::Reject { observed_len } => {
                return Err(AcquisitionError::OverLimit {
                    stage: STAGE_INCR,
                    observed_len: accumulator.len().saturating_add(observed_len),
                });
            }
        }
    }
}

/// Waits for a `PropertyNotify` (`NewValue`) for this window/property.
fn wait_for_property(
    conn: &BoundedConnection,
    window: Window,
    property: Atom,
    deadline: &Deadline,
) -> Result<(), AcquisitionError> {
    drain_until(
        deadline,
        STAGE_INCR,
        || {
            conn.poll_for_event()
                .map_err(|err| map_request_error(err, deadline, STAGE_INCR))
        },
        |event| match event {
            Event::PropertyNotify(notify)
                if notify.window == window
                    && notify.atom == property
                    && notify.state == Property::NEW_VALUE =>
            {
                Some(())
            }
            _ => None,
        },
    )
}

/// Drains queued events until `accept` matches, under one absolute deadline.
///
/// `next_event` is the real `RustConnection::poll_for_event`: it pops one
/// queued event when the protocol queue is non-empty, otherwise it calls
/// `read_packet_and_enqueue(NonBlocking)`, which runs
/// `PacketReader::try_read_packets`. That function is **not** one non-blocking
/// read; it loops `stream.read()` until `WouldBlock`, EOF, or an error and
/// consults no clock. [`DeadlineStream::read`] enforces the absolute deadline
/// inside that loop (pre-read wait for the remaining time, top-of-read check,
/// post-read re-check), and this loop additionally re-checks the deadline
/// before every observed event and on every empty queue, not only after the
/// queue drains. An untrusted local X11 client can flood `PropertyNotify` or
/// send synthetic `SelectionNotify`/`PropertyNotify` events to the requestor
/// window; such a flood still ends in the typed deadline error, with no text
/// and no teardown wait.
fn drain_until<T, E>(
    deadline: &Deadline,
    stage: &'static str,
    mut next_event: impl FnMut() -> Result<Option<E>, AcquisitionError>,
    mut accept: impl FnMut(E) -> Option<T>,
) -> Result<T, AcquisitionError> {
    loop {
        match next_event()? {
            Some(event) => {
                if deadline.expired() {
                    return Err(deadline.error(stage));
                }
                if let Some(value) = accept(event) {
                    return Ok(value);
                }
            }
            None => {
                if deadline.expired() {
                    return Err(deadline.error(stage));
                }
                pause_until_next_poll(deadline);
            }
        }
    }
}

/// Sleeps for a short bounded interval while waiting for an X event.
///
/// x11rb's public `RustConnection` does not expose its stream, so the event
/// loops cannot poll the connection FD directly; the wrapper's `read` blocks
/// inside `poll(2)` for the remaining budget, and this pause only covers a
/// spurious wake or a raced empty read so the synchronous paste path cannot
/// busy-loop. Bounded by the remaining time.
fn pause_until_next_poll(deadline: &Deadline) {
    const POLL_PAUSE: Duration = Duration::from_millis(1);
    let pause = POLL_PAUSE.min(deadline.remaining());
    if !pause.is_zero() {
        std::thread::sleep(pause);
    }
}

/// Maps an x11rb request/reply failure, preferring the typed deadline when the
/// budget expired while waiting.
fn map_request_error(
    err: impl std::fmt::Display,
    deadline: &Deadline,
    stage: &'static str,
) -> AcquisitionError {
    if deadline.expired() {
        deadline.error(stage)
    } else {
        AcquisitionError::Transport {
            stage,
            detail: err.to_string(),
        }
    }
}

fn transport(stage: &'static str, err: impl std::fmt::Display) -> AcquisitionError {
    AcquisitionError::Transport {
        stage,
        detail: err.to_string(),
    }
}

/// Best-effort local teardown of the requestor window and property.
///
/// Both requests are void and are flushed without waiting for a reply. A
/// failure while tearing down is intentionally ignored: the acquisition
/// result (or its typed error) is already decided and no peer acknowledgement
/// is awaited.
struct RequestorWindow<'a> {
    conn: &'a BoundedConnection,
    window: Window,
    property: Atom,
}

impl Drop for RequestorWindow<'_> {
    fn drop(&mut self) {
        let _ = self.conn.delete_property(self.window, self.property);
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use nix::sys::socket::{setsockopt, sockopt};
    use x11rb::protocol::xproto::{BackingStore, ImageOrder, Screen, Setup};
    use x11rb::x11_utils::Serialize;

    use super::*;

    #[test]
    fn selection_atom_names_match_the_icccm_selections() {
        assert_eq!(X11Selection::Clipboard.atom_name(), b"CLIPBOARD");
        assert_eq!(X11Selection::Primary.atom_name(), b"PRIMARY");
    }

    #[test]
    fn ordinary_decoding_rejects_an_over_cap_reply() {
        // The pure reply-shape gate and decoding are covered in
        // `clipboard_limits`; this pins the x11 module's wiring of a format-8
        // STRING reply.
        let reply = GetPropertyReply {
            format: 8,
            sequence: 0,
            length: 0,
            type_: Atom::from(AtomEnum::STRING),
            bytes_after: 0,
            value_len: 2,
            value: b"hi".to_vec(),
        };
        let decoded = decode_ordinary(reply, X11Encoding::Utf8, 8192).expect("decode");
        assert!(matches!(decoded, BoundedText::Complete(ref text) if text == "hi"));
    }

    #[test]
    fn ordinary_decoding_rejects_a_non_byte_format() {
        let reply = GetPropertyReply {
            format: 32,
            sequence: 0,
            length: 0,
            type_: Atom::from(AtomEnum::STRING),
            bytes_after: 0,
            value_len: 1,
            value: vec![0; 4],
        };
        assert!(matches!(
            decode_ordinary(reply, X11Encoding::Utf8, 8192),
            Err(AcquisitionError::Malformed { .. })
        ));
    }

    #[test]
    fn ordinary_decoding_rejects_an_over_cap_reply_without_a_prefix() {
        let reply = GetPropertyReply {
            format: 8,
            sequence: 0,
            length: 0,
            type_: Atom::from(AtomEnum::STRING),
            bytes_after: 0,
            value_len: 8193,
            value: vec![b'a'; 8193],
        };
        assert!(matches!(
            decode_ordinary(reply, X11Encoding::Utf8, 8192),
            Err(AcquisitionError::OverLimit {
                stage: "x11-property",
                ..
            })
        ));
    }

    #[test]
    fn ordinary_decoding_rejects_a_partial_reply() {
        let reply = GetPropertyReply {
            format: 8,
            sequence: 0,
            length: 0,
            type_: Atom::from(AtomEnum::STRING),
            bytes_after: 1,
            value_len: 2,
            value: b"hi".to_vec(),
        };
        assert!(matches!(
            decode_ordinary(reply, X11Encoding::Utf8, 8192),
            Err(AcquisitionError::OverLimit { .. })
        ));
    }

    /// Serialized minimal `Setup` reply for a fake X server.
    ///
    /// `RustConnection::connect_to_stream` sends the setup request first and
    /// blocks for this reply, so the test peer writes it before any flood.
    fn x11_setup_reply() -> Vec<u8> {
        let setup = Setup {
            status: 1,
            protocol_major_version: 11,
            protocol_minor_version: 0,
            length: 0,
            release_number: 1,
            resource_id_base: 0x200000,
            resource_id_mask: 0x1fffff,
            motion_buffer_size: 0,
            maximum_request_length: 4096,
            image_byte_order: ImageOrder::LSB_FIRST,
            bitmap_format_bit_order: ImageOrder::LSB_FIRST,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 8,
            max_keycode: 255,
            vendor: Vec::new(),
            pixmap_formats: Vec::new(),
            roots: vec![Screen {
                root: 0x1234,
                default_colormap: 0,
                white_pixel: 0x00ff_ffff,
                black_pixel: 0,
                current_input_masks: EventMask::NO_EVENT,
                width_in_pixels: 1,
                height_in_pixels: 1,
                width_in_millimeters: 1,
                height_in_millimeters: 1,
                min_installed_maps: 1,
                max_installed_maps: 1,
                root_visual: 0,
                backing_stores: BackingStore::NOT_USEFUL,
                save_unders: false,
                root_depth: 24,
                allowed_depths: Vec::new(),
            }],
        };
        let mut bytes = setup.serialize();
        // `length` counts four-byte units after the fixed eight-byte header.
        let length_units = u16::try_from((bytes.len() - 8) / 4).expect("setup length fits u16");
        bytes[6..8].copy_from_slice(&length_units.to_ne_bytes());
        bytes
    }

    /// One `PropertyNotify` (`PropertyNewValue`) for an arbitrary window and
    /// atom. `drain_until` never accepts it here, so only the event fetch path
    /// is under test.
    fn property_notify_packet() -> [u8; 32] {
        let mut packet = [0_u8; 32];
        packet[0] = 28; // PropertyNotify
        packet[1] = 1; // Property::NEW_VALUE
        packet[4..8].copy_from_slice(&0x1234_u32.to_ne_bytes()); // window
        packet[8..12].copy_from_slice(&0x99_u32.to_ne_bytes()); // property atom
        packet
    }

    /// Requested socket buffer size for the flood test.
    ///
    /// A full buffer is what makes the mutation (read-level bound removed)
    /// deterministic: draining it takes far longer than the deadline even if
    /// the producer never gets scheduled again. The kernel clamps the value to
    /// `net.core.{r,w}mem_max`; the test tolerates a smaller effective buffer.
    const FLOOD_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

    /// Grows both directions of a Unix socket so the fake server can queue a
    /// large event backlog before the drain starts. Best-effort: a clamped
    /// buffer still preserves the deadline contract, only the mutation margin
    /// shrinks.
    fn grow_socket_buffers(stream: &UnixStream) {
        let _ = setsockopt(stream, sockopt::RcvBuf, &FLOOD_SOCKET_BUFFER_BYTES);
        let _ = setsockopt(stream, sockopt::SndBuf, &FLOOD_SOCKET_BUFFER_BYTES);
    }

    /// Fake X server peer: writes the setup reply, then either floods valid
    /// events forever or stays idle with the socket open.
    ///
    /// In flood mode `filled` is signalled once the first non-blocking write
    /// reports `WouldBlock`, i.e. the socket buffer is full, so the caller can
    /// start the drain against a deterministic backlog.
    fn spawn_fake_x_server(
        mut server: UnixStream,
        flood: bool,
        stop: Arc<AtomicBool>,
        filled: mpsc::Sender<()>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            server
                .write_all(&x11_setup_reply())
                .expect("write fake X11 setup reply");
            if !flood {
                // Keep the peer open with no readable data so the control run
                // observes the bare deadline instead of an EOF/transport
                // error.
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(1));
                }
                return;
            }
            server
                .set_nonblocking(true)
                .expect("non-blocking fake X server");
            let packet = property_notify_packet();
            let mut burst = Vec::with_capacity(64 * 1024);
            while burst.len() < 64 * 1024 {
                burst.extend_from_slice(&packet);
            }
            let mut announced = false;
            while !stop.load(Ordering::Relaxed) {
                match server.write(&burst) {
                    Ok(_) => {}
                    Err(err)
                        if err.kind() == io::ErrorKind::WouldBlock
                            || err.kind() == io::ErrorKind::Interrupted =>
                    {
                        if !announced {
                            announced = true;
                            filled.send(()).ok();
                        }
                        thread::yield_now();
                    }
                    Err(_) => break,
                }
            }
        })
    }

    /// Runs the real `drain_until` caller shape over a real
    /// `RustConnection<DeadlineStream<DefaultStream>>`.
    ///
    /// Returns the typed error, how long the call took, and how many event
    /// fetches it performed. The drain runs on its own thread behind a
    /// watchdog so a missing read-level deadline fails the test instead of
    /// hanging it.
    fn run_event_wait(
        budget: Duration,
        flood: bool,
        stage: &'static str,
    ) -> (AcquisitionError, Duration, u64) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        grow_socket_buffers(&client);
        grow_socket_buffers(&server);
        let stop = Arc::new(AtomicBool::new(false));
        let (filled_tx, filled_rx) = mpsc::channel();
        let server_thread = spawn_fake_x_server(server, flood, Arc::clone(&stop), filled_tx);
        let (inner, _peer) = DefaultStream::from_unix_stream(client).expect("default stream");
        // Same production shape: one absolute deadline covers the handshake
        // and the event wait, so timing starts before the handshake.
        let started = Instant::now();
        let deadline = Deadline::new(budget).expect("deadline");
        let stream = DeadlineStream {
            inner,
            deadline: deadline.at,
        };
        let conn = RustConnection::connect_to_stream(stream, 0).expect("x11 handshake");
        if flood {
            filled_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("fake server never filled the socket buffer");
        }

        let (send, recv) = mpsc::channel();
        let drain = thread::spawn(move || {
            let mut calls = 0_u64;
            let mut next_event = || {
                calls += 1;
                conn.poll_for_event()
                    .map_err(|err| map_request_error(err, &deadline, stage))
            };
            let result = drain_until(&deadline, stage, &mut next_event, |_| None::<()>);
            send.send((result, started.elapsed(), calls)).ok();
        });

        let received = recv.recv_timeout(budget + Duration::from_secs(2));
        stop.store(true, Ordering::Relaxed);
        drain.join().expect("drain thread");
        server_thread.join().expect("fake server thread");
        let (result, elapsed, calls) = received
            .expect("event wait did not return; x11rb's read loop is not bounded by the deadline");
        let error = result.expect_err("a never-matching event wait must time out");
        (error, elapsed, calls)
    }

    // F1 (CTX-1366 review2): x11rb's `poll_for_event` loops `Stream::read`
    // until `WouldBlock`; a socket that stays readable makes one call
    // unbounded unless `DeadlineStream::read` enforces the deadline. This is
    // the INCR `PropertyNotify` fetch (`wait_for_property`'s caller shape),
    // over a real socket rather than a closure stub.
    #[test]
    fn incr_event_flood_is_cut_off_by_the_read_level_deadline() {
        let budget = Duration::from_millis(20);
        let (error, elapsed, calls) = run_event_wait(budget, true, STAGE_INCR);
        assert!(
            matches!(
                error,
                AcquisitionError::DeadlineExceeded {
                    stage: "x11-incr",
                    ..
                }
            ),
            "unexpected error: {error:?}"
        );
        assert!(calls > 0, "the drain never fetched an event");
        assert!(
            elapsed + Duration::from_millis(2) >= budget,
            "returned before the deadline: {elapsed:?}"
        );
        assert!(
            elapsed < budget + Duration::from_millis(100),
            "the read loop outlived the deadline by draining the flood backlog: {elapsed:?}"
        );
    }

    // The same event-fetch bound serves the `SelectionNotify` wait.
    #[test]
    fn selection_event_flood_is_cut_off_by_the_read_level_deadline() {
        let budget = Duration::from_millis(20);
        let (error, elapsed, calls) = run_event_wait(budget, true, STAGE_SELECTION);
        assert!(
            matches!(
                error,
                AcquisitionError::DeadlineExceeded {
                    stage: "x11-selection",
                    ..
                }
            ),
            "unexpected error: {error:?}"
        );
        assert!(calls > 0, "the drain never fetched an event");
        assert!(
            elapsed < budget + Duration::from_millis(100),
            "the read loop outlived the deadline by draining the flood backlog: {elapsed:?}"
        );
    }

    // Control: with no readable data the same caller observes the deadline
    // through the bounded wait rather than through the flood path. Without
    // this run the flood assertions could pass for the wrong reason (e.g. an
    // immediate transport error).
    #[test]
    fn event_wait_observes_the_deadline_without_a_flood() {
        let budget = Duration::from_millis(20);
        let (error, elapsed, _calls) = run_event_wait(budget, false, STAGE_INCR);
        assert!(
            matches!(
                error,
                AcquisitionError::DeadlineExceeded {
                    stage: "x11-incr",
                    ..
                }
            ),
            "unexpected error: {error:?}"
        );
        assert!(
            elapsed + Duration::from_millis(2) >= budget,
            "returned before the deadline: {elapsed:?}"
        );
        assert!(
            elapsed < budget + Duration::from_secs(1),
            "empty wait outlived the deadline: {elapsed:?}"
        );
    }

    #[test]
    fn drain_returns_a_matching_event_before_the_deadline() {
        let deadline = Deadline::new(Duration::from_secs(1)).expect("deadline");
        let value = drain_until(
            &deadline,
            STAGE_INCR,
            || Ok(Some(7_u8)),
            |event| (event == 7).then_some(event),
        )
        .expect("matching event");
        assert_eq!(value, 7);
    }

    #[test]
    fn drain_times_out_on_an_empty_queue() {
        let deadline = Deadline::new(Duration::from_millis(5)).expect("deadline");
        let error = drain_until(&deadline, STAGE_INCR, || Ok(None::<u8>), |_| None::<()>)
            .expect_err("an empty queue must time out");
        assert!(matches!(
            error,
            AcquisitionError::DeadlineExceeded {
                stage: "x11-incr",
                ..
            }
        ));
    }

    #[test]
    fn deadline_stream_reports_timeout_after_the_deadline() {
        use std::os::unix::net::UnixStream;
        let (left, _right) = UnixStream::pair().expect("socketpair");
        let (inner, _peer) = DefaultStream::from_unix_stream(left).expect("default stream");
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("past instant");
        let stream = DeadlineStream { inner, deadline };
        let error = stream
            .poll(PollMode::Readable)
            .expect_err("an expired deadline must fail the poll");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
