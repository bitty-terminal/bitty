//! Bounded native clipboard acquisition (Linux Wayland) for CTX-1315/CTX-1366.
//!
//! The locked `arboard 3.6.1` text read materializes the complete clipboard
//! value before any Bitty-owned bound is applied: Wayland drains the transfer
//! pipe to EOF into a growing `Vec`, and X11 requests a property with
//! `u32::MAX / 4` four-byte units and appends complete INCR chunks (see
//! `recording/release-security-0.0.24/clipboard-acquisition-three-platform-design.md`).
//! [`CLIPBOARD_MAX_BYTES`](crate::clipboard::CLIPBOARD_MAX_BYTES) is therefore
//! only a retained/inspected/pasted bound, not a peak-memory bound.
//!
//! This module adds a native acquisition path whose **transfer stage** never
//! owns more than a fixed window of the foreign payload:
//!
//! - the Wayland data-control transfer pipe is consumed incrementally through
//!   [`read_bounded_utf8`], which reads at most
//!   [`AcquisitionLimits::window_bytes`] native bytes into a pre-sized buffer;
//! - every wait is bounded by one absolute monotonic deadline
//!   ([`CLIPBOARD_ACQUISITION_TIMEOUT`](crate::clipboard_limits::CLIPBOARD_ACQUISITION_TIMEOUT))
//!   that per-chunk progress never resets;
//! - a deadline, transport failure, or invalid retained UTF-8 fails closed with
//!   a typed [`AcquisitionError`]; nothing is returned to the paste/OSC 52
//!   seams on failure;
//! - an over-window producer yields the bounded UTF-8 prefix (the existing
//!   truncated-paste contract) rather than a full value.
//!
//! Scope, stated precisely (this is not R-004 closure):
//!
//! - **Bounded here:** the in-process transfer payload for Linux Wayland text
//!   reads (regular and primary), the decoded UTF-8 output, and the transfer
//!   wall-clock deadline.
//! - **Bounded elsewhere (CTX-1366):** Linux X11 via the bounded `x11rb`
//!   `GetProperty`/INCR client in [`crate::clipboard_x11`], and Windows via the
//!   `GlobalSize` pre-check plus bounded UTF-16 scan in
//!   [`crate::clipboard_windows`]; macOS reads are refused with a typed error.
//! - **Not bounded here:** `wl-clipboard-rs`'s blocking setup roundtrips before
//!   the pipe is handed to us (registry/seat/offer/MIME discovery), Wayland
//!   backend pre-callback metadata/message/event/FD queues, and all
//!   compositor-side allocations. Setup failure is a typed failure; the Linux
//!   read funnel has no unbounded `arboard` fallback (CTX-1366).
//!
//! The numeric deadline is an implementation default, not an accepted policy
//! value: the design record keeps `750 ms` / `1500 ms` as unevidenced
//! candidates and forbids presenting either as authorized. It stays a named
//! constant in exactly one place ([`crate::clipboard_limits`]) and must be
//! re-validated by the owning task.

use std::io::Read;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

use crate::clipboard::BoundedText;
use crate::clipboard_limits::{AcquisitionError, AcquisitionLimits};

/// Read chunk size. Only affects syscall count, not the retained window.
const READ_CHUNK_BYTES: usize = 4096;

/// Which Linux selection a bounded Wayland read targets.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WaylandSelection {
    /// Regular clipboard.
    Clipboard,
    /// Primary selection (middle-click).
    Primary,
}

/// Attempts one bounded Wayland text read with production limits.
///
/// [`AcquisitionError::Setup`] means no transfer pipe was produced and the
/// caller keeps the previous `arboard` behavior for that case. Once a pipe
/// exists the read is final: it never falls back to an unbounded backend.
pub(crate) fn try_read_wayland_text(
    selection: WaylandSelection,
) -> Result<BoundedText, AcquisitionError> {
    try_read_wayland_text_with(selection, AcquisitionLimits::production())
}

/// [`try_read_wayland_text`] with explicit limits (tests inject short
/// deadlines and small windows).
pub(crate) fn try_read_wayland_text_with(
    selection: WaylandSelection,
    limits: AcquisitionLimits,
) -> Result<BoundedText, AcquisitionError> {
    let clipboard = match selection {
        WaylandSelection::Clipboard => ClipboardType::Regular,
        WaylandSelection::Primary => ClipboardType::Primary,
    };
    let (mut pipe, _mime) = get_contents(clipboard, Seat::Unspecified, MimeType::Text)
        .map_err(|err| AcquisitionError::Setup(err.to_string()))?;
    read_bounded_utf8(&mut pipe, limits)
}

/// Reads at most [`AcquisitionLimits::window_bytes`] native bytes from
/// `reader` under one absolute deadline and returns the bounded UTF-8 value.
///
/// Contract:
///
/// - EOF at or below `max_bytes` yields [`BoundedText::Complete`];
/// - [`BoundedText::Truncated`] is returned **only** when EOF arrives above
///   `max_bytes`, or when the full `max_bytes + 4` native window has been
///   observed; the prefix is then cut at the last complete code point at or
///   below `max_bytes`;
/// - a value of `max_bytes + 1 ..= max_bytes + 3` bytes whose producer stalls
///   before EOF fails closed at the deadline rather than being assumed
///   truncated, and reaching `max_bytes` exactly then stalling likewise fails;
/// - invalid UTF-8 inside the retained prefix fails closed
///   ([`AcquisitionError::Malformed`]); bytes after the retained prefix are
///   never validated or read (`prefix validation`, not full-source
///   validation);
/// - the deadline is absolute: progress does not extend it.
pub(crate) fn read_bounded_utf8<R: Read + AsFd>(
    reader: &mut R,
    limits: AcquisitionLimits,
) -> Result<BoundedText, AcquisitionError> {
    const STAGE: &str = "wayland-transfer";
    let timeout_ms = u64::try_from(limits.deadline.as_millis()).unwrap_or(u64::MAX);
    let Some(deadline) = Instant::now().checked_add(limits.deadline) else {
        return Err(AcquisitionError::DeadlineExceeded {
            stage: STAGE,
            timeout_ms,
        });
    };
    let mut window: Vec<u8> = Vec::with_capacity(limits.window_bytes);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        if window.len() >= limits.window_bytes {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AcquisitionError::DeadlineExceeded {
                stage: STAGE,
                timeout_ms,
            });
        }
        if !wait_readable(reader, remaining, STAGE)? {
            return Err(AcquisitionError::DeadlineExceeded {
                stage: STAGE,
                timeout_ms,
            });
        }
        // Only ever ask for the remaining window; the Vec never grows past it.
        let want = READ_CHUNK_BYTES.min(limits.window_bytes - window.len());
        match reader.read(&mut chunk[..want]) {
            Ok(0) => {
                // EOF: complete only when the whole value fit the ceiling.
                if window.len() <= limits.max_bytes {
                    return std::str::from_utf8(&window)
                        .map(|text| BoundedText::Complete(text.to_owned()))
                        .map_err(|_| AcquisitionError::Malformed { stage: STAGE });
                }
                return clip_prefix(&window, limits.max_bytes, STAGE);
            }
            Ok(read) => window.extend_from_slice(&chunk[..read]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                return Err(AcquisitionError::Transport {
                    stage: STAGE,
                    detail: err.to_string(),
                });
            }
        }
    }
    // The window filled before EOF: the producer exceeded what this process
    // retains. A valid observed prefix is a successful truncation.
    clip_prefix(&window, limits.max_bytes, STAGE)
}

/// Cuts `window` to the longest valid UTF-8 prefix at or below `max_bytes`.
///
/// `window` is longer than `max_bytes` on every call site. Invalid bytes inside
/// the retained prefix fail closed; a code point crossing the cut is dropped
/// whole rather than split.
///
/// The cut is validated with `Utf8Error::valid_up_to`: an incomplete sequence
/// ending on either a lead byte or a continuation byte backs off to the last
/// complete code point, and a valid character before an orphan continuation is
/// not silently dropped (an orphan continuation is invalid and fails closed).
/// This is a refactor of the previous continuation-byte scan, **not** a fix for
/// a valid-stream `Malformed`: the scan already returned the correct truncated
/// prefix for `63 × 'a' + U+1F600`, whose cut lands on the emoji's second byte.
fn clip_prefix(
    window: &[u8],
    max_bytes: usize,
    stage: &'static str,
) -> Result<BoundedText, AcquisitionError> {
    let mut end = max_bytes.min(window.len());
    loop {
        match std::str::from_utf8(&window[..end]) {
            Ok(prefix) => {
                return Ok(BoundedText::Truncated {
                    prefix: prefix.to_owned(),
                    observed_len: window.len(),
                });
            }
            // Incomplete sequence at the cut: `valid_up_to` is the boundary of
            // the last complete code point. Strictly decreases `end`.
            Err(err) if err.error_len().is_none() => end = err.valid_up_to(),
            // Invalid bytes inside the retained prefix.
            Err(_) => return Err(AcquisitionError::Malformed { stage }),
        }
    }
}

/// Waits until `reader` is readable or `timeout` expires; `Ok(false)` means
/// the deadline won. `POLLHUP` counts as readable so the following `read`
/// observes EOF.
fn wait_readable<R: AsFd>(
    reader: &R,
    timeout: Duration,
    stage: &'static str,
) -> Result<bool, AcquisitionError> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let mut fds = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
    // Round up so a sub-millisecond remainder is not turned into "expired";
    // the deadline is at most i32::MAX milliseconds in practice (750 ms).
    let millis = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
    let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
    match poll(&mut fds, timeout) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(errno) => Err(AcquisitionError::Transport {
            stage,
            detail: errno.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard_limits::NATIVE_PROBE_BYTES;
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use std::thread::JoinHandle;

    /// Small-window limits for fast tests: 64-byte ceiling, 4-byte probe.
    fn limits(max_bytes: usize, deadline: Duration) -> AcquisitionLimits {
        AcquisitionLimits {
            max_bytes,
            window_bytes: max_bytes + NATIVE_PROBE_BYTES,
            deadline,
        }
    }

    /// Writes `bytes` on a background thread (a large payload exceeds the
    /// socket buffer, so the write cannot run inline) and closes the writer.
    fn spawn_writer(bytes: Vec<u8>) -> (UnixStream, JoinHandle<()>) {
        let (reader, mut writer) = UnixStream::pair().expect("socketpair");
        let handle = std::thread::spawn(move || {
            // The reader stops at its window; EPIPE is the expected outcome.
            let _ = writer.write_all(&bytes);
        });
        (reader, handle)
    }

    #[test]
    fn at_ceiling_with_eof_is_accepted() {
        let (mut reader, writer) = spawn_writer(vec![b'a'; 64]);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("at-ceiling value must be accepted");
        match result {
            BoundedText::Complete(text) => assert_eq!(text.len(), 64),
            other => panic!("expected Complete, got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn one_over_ceiling_is_a_truncated_prefix() {
        let (mut reader, writer) = spawn_writer(vec![b'b'; 65]);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("one-over value must clip, not reject");
        match result {
            BoundedText::Truncated {
                prefix,
                observed_len,
            } => {
                assert_eq!(prefix.len(), 64);
                assert_eq!(observed_len, 65, "observed bytes are a lower bound");
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn far_over_ceiling_retains_only_the_window() {
        // 1 MiB foreign value: the reader must stop at the 68-byte window and
        // never own the full payload.
        let (mut reader, writer) = spawn_writer(vec![b'c'; 1024 * 1024]);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("oversized value must clip");
        match result {
            BoundedText::Truncated {
                prefix,
                observed_len,
            } => {
                assert_eq!(prefix.len(), 64);
                assert_eq!(observed_len, 68, "window is max + probe bytes");
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
        drop(reader);
        let _ = writer.join();
    }

    #[test]
    fn emoji_crossing_the_ceiling_is_cut_on_a_char_boundary() {
        // 17 * 4 = 68 bytes of 4-byte code points, ceiling 64, window 68: the
        // cut must land on 64 (a char boundary). This cut is already aligned,
        // so it does NOT exercise the incomplete-sequence backoff; see
        // `truncated_prefix_backs_off_an_incomplete_sequence_at_the_cut`.
        let payload: Vec<u8> = "😀".repeat(17).into_bytes();
        let (mut reader, writer) = spawn_writer(payload);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("emoji value must clip");
        match result {
            BoundedText::Truncated { prefix, .. } => {
                assert_eq!(prefix.len(), 64);
                assert_eq!(prefix.len() % 4, 0);
                assert!(prefix.chars().all(|ch| ch == '😀'));
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn truncated_prefix_backs_off_an_incomplete_sequence_at_the_cut() {
        // Test-coverage addition, not a regression fix: 63 ASCII bytes +
        // U+1F600 is 67 bytes with ceiling 64 and window 68. The cut index
        // (64) lands on the emoji's *second* byte; the lead byte is at 63. The
        // previous continuation-byte scan already backed off to 63 and
        // returned the CTX-0478 truncated prefix, so this payload cannot
        // distinguish the two scans. This test pins that contract and makes
        // the recorded naive-cut mutation fail (non-vacuous coverage).
        let mut payload = vec![b'a'; 63];
        payload.extend_from_slice("😀".as_bytes());
        assert_eq!(payload.len(), 67, "63 ASCII + 4-byte emoji");
        let (mut reader, writer) = spawn_writer(payload);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("oversized value must clip, not fail as malformed");
        match result {
            BoundedText::Truncated {
                prefix,
                observed_len,
            } => {
                assert_eq!(prefix.len(), 63, "cut before the 4-byte code point");
                assert!(prefix.bytes().all(|byte| byte == b'a'));
                assert!(
                    std::str::from_utf8(prefix.as_bytes()).is_ok(),
                    "truncated prefix must be valid UTF-8"
                );
                assert_eq!(observed_len, 67);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn exact_ceiling_without_eof_fails_closed_at_the_deadline() {
        // The design contract: exactly `max_bytes` observed and then a stall
        // before EOF is not success — it is a deadline failure.
        let (mut reader, mut writer) = UnixStream::pair().expect("socketpair");
        writer.write_all(&[b'd'; 64]).expect("write ceiling");
        let started = Instant::now();
        let error = read_bounded_utf8(&mut reader, limits(64, Duration::from_millis(60)))
            .expect_err("exact-ceiling stall must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::DeadlineExceeded {
                stage: "wayland-transfer",
                ..
            }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "deadline must be enforced promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn silent_producer_fails_closed_at_the_deadline() {
        let (mut reader, _writer) = UnixStream::pair().expect("socketpair");
        let error = read_bounded_utf8(&mut reader, limits(64, Duration::from_millis(40)))
            .expect_err("silent producer must fail closed");
        match error {
            AcquisitionError::DeadlineExceeded { timeout_ms, .. } => assert_eq!(timeout_ms, 40),
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
    }

    #[test]
    fn deadline_is_absolute_and_progress_does_not_reset_it() {
        // A trickle producer keeps making progress; the one absolute deadline
        // must still fire.
        let (mut reader, mut writer) = UnixStream::pair().expect("socketpair");
        let trickle = std::thread::spawn(move || {
            for _ in 0..100 {
                if writer.write_all(b"e").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error = read_bounded_utf8(&mut reader, limits(64, Duration::from_millis(60)))
            .expect_err("trickle must not extend the deadline");
        assert!(matches!(error, AcquisitionError::DeadlineExceeded { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "trickle read outlived the deadline: {:?}",
            started.elapsed()
        );
        drop(reader);
        let _ = trickle.join();
    }

    #[test]
    fn invalid_utf8_in_the_retained_prefix_fails_closed() {
        let (mut reader, writer) = spawn_writer(vec![0xFF, 0xFE, 0xFD]);
        let error = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect_err("invalid UTF-8 must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::Malformed {
                stage: "wayland-transfer"
            }
        ));
        drop(writer);
    }

    #[test]
    fn invalid_utf8_after_the_ceiling_is_not_validated() {
        // Prefix validation, not full-source validation: the unread suffix is
        // never inspected, so an invalid byte beyond the window cannot fail a
        // successful truncation.
        let mut payload = vec![b'f'; 64];
        payload.push(0xFF);
        let (mut reader, writer) = spawn_writer(payload);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("suffix bytes past the window are not validated");
        assert!(matches!(result, BoundedText::Truncated { .. }));
        drop(writer);
    }

    #[test]
    fn invalid_utf8_inside_the_truncated_prefix_fails_closed() {
        // The cut path validates its prefix: invalid bytes before the cut are
        // not silently dropped or replacement-decoded, so no paste occurs.
        let (mut reader, writer) = spawn_writer(vec![0xFF; 70]);
        let error = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect_err("invalid bytes in the cut prefix must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::Malformed {
                stage: "wayland-transfer"
            }
        ));
        drop(writer);
    }

    #[test]
    fn empty_value_is_complete_and_empty() {
        let (mut reader, writer) = spawn_writer(Vec::new());
        match read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5))) {
            Ok(BoundedText::Complete(text)) => assert!(text.is_empty()),
            other => panic!("expected empty Complete, got {other:?}"),
        }
        drop(writer);
    }

    #[test]
    fn bounded_text_len_never_exceeds_the_ceiling() {
        let (mut reader, writer) = spawn_writer(vec![b'g'; 4096]);
        let result = read_bounded_utf8(&mut reader, limits(64, Duration::from_secs(5)))
            .expect("oversized value must clip");
        assert!(result.len() <= 64);
        drop(writer);
    }
}
