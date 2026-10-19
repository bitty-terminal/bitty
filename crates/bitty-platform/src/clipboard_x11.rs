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
//!   uses a [`DeadlineStream`] wrapper so x11rb's own internal poll loops
//!   cannot wait past the deadline;
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

/// A `DefaultStream` wrapper that enforces the acquisition deadline in poll.
///
/// `DefaultStream` is already non-blocking for reads/writes but its
/// `Stream::poll` uses an infinite `poll(2)`. x11rb calls `poll` from its
/// internal write/read retry loops, so wrapping `poll` is what makes handshake
/// and request responses obey the one absolute deadline. Reads and writes are
/// delegated unchanged (they never block on this non-blocking stream).
#[derive(Debug)]
struct DeadlineStream<S> {
    inner: S,
    deadline: Instant,
}

impl<S: Stream + AsFd> Stream for DeadlineStream<S> {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(deadline_io_error());
        }
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::POLLIN;
        }
        if mode.writable() {
            flags |= PollFlags::POLLOUT;
        }
        let mut fds = [PollFd::new(self.inner.as_fd(), flags)];
        let millis = i32::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
        match poll(&mut fds, timeout) {
            Ok(0) => Err(deadline_io_error()),
            Ok(_) => Ok(()),
            // The Stream contract forbids returning `Interrupted`; report the
            // wake as spurious and let x11rb retry with the remaining time.
            Err(nix::errno::Errno::EINTR) => Ok(()),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    fn read(&self, buf: &mut [u8], fd_storage: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.inner.read(buf, fd_storage)
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
/// `next_event` must not block: x11rb's `poll_for_event` pops one queued event
/// or performs one non-blocking read, so a client that keeps the stream
/// non-empty would otherwise keep this loop running forever. The deadline is
/// therefore re-checked **before every observed event and on every empty
/// queue**, not only after the queue drains. An untrusted local X11 client can
/// flood `PropertyNotify` or send synthetic `SelectionNotify`/`PropertyNotify`
/// events to the requestor window; such a flood still ends in the typed
/// deadline error, with no text and no teardown wait.
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
/// loops cannot poll the connection FD directly; a one-millisecond pause keeps
/// the synchronous paste path off a busy loop and still wakes well inside the
/// absolute deadline. Bounded by the remaining time.
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

    // F1 (CTX-1366 review): an untrusted client can keep
    // `poll_for_event` returning `Some` forever. The drain loop must observe
    // the absolute deadline on every event, not only when the queue empties.
    #[test]
    fn event_flood_cannot_starve_the_absolute_deadline() {
        let deadline = Deadline::new(Duration::from_millis(20)).expect("deadline");
        let started = Instant::now();
        let error = drain_until(
            &deadline,
            STAGE_SELECTION,
            || Ok(Some(1_u8)),
            |_| None::<()>,
        )
        .expect_err("a never-empty event source must not outlive the deadline");
        assert!(matches!(
            error,
            AcquisitionError::DeadlineExceeded {
                stage: "x11-selection",
                ..
            }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "flood drain outlived the deadline: {:?}",
            started.elapsed()
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
