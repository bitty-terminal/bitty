//! Shared numeric bounds and pure acquisition guards (CTX-1366 / R-004).
//!
//! This module owns the platform-independent half of bounded clipboard
//! acquisition so the Wayland transfer, the X11 `GetProperty`/INCR client, and
//! the Windows `HGLOBAL` scan all derive their windows from exactly one place
//! and can be exercised without a display server or a real clipboard:
//!
//! - [`AcquisitionLimits`] is the one absolute deadline and the decoded
//!   ceiling/window used by every bounded native read;
//! - [`AcquisitionError`] is the typed fail-closed surface those readers
//!   return, mapped once to [`PlatformError`] by [`platform_error`];
//! - the Windows [`WINDOWS_MAX_SCAN_UNITS`] / [`WINDOWS_MAX_ALLOCATION_BYTES`]
//!   gate and [`decode_windows_utf16`] scan a borrowed UTF-16 allocation
//!   without materializing more than the bounded window;
//! - [`x11_long_length_units`], [`x11_property_within_cap`],
//!   [`decode_x11_text`], and [`IncrAccumulator`] bound an X11 ordinary
//!   property reply and cap INCR accumulation;
//! - [`platform_error`] is the single public-error mapping shared by every
//!   bounded reader, so a failure reaches the paste/OSC 52 seams as one of
//!   `ClipboardAcquisitionTimeout`, `ClipboardPayloadTooLarge`,
//!   `ClipboardOperation`, or `ClipboardUnavailable` regardless of backend.
//!
//! Scope: these are in-process owned-memory bounds. They do not bound the
//! foreign producer, the X server, the compositor, or native framework
//! allocations that happen before the acquisition seam is entered.

use std::time::Duration;

use crate::clipboard::{BoundedText, CLIPBOARD_MAX_BYTES};
use crate::error::PlatformError;

/// Extra native bytes read beyond [`CLIPBOARD_MAX_BYTES`] so a producer that
/// exceeds the ceiling is *observed* to exceed it regardless of how it chunks
/// the stream, and so the full longest UTF-8 code point (four bytes) is
/// visible at the cut.
///
/// Used by the Wayland transfer window; X11 and Windows detect overflow from
/// the property/allocation length instead and do not need a probe.
pub(crate) const NATIVE_PROBE_BYTES: usize = 4;

/// Absolute deadline for one bounded native acquisition.
///
/// The numeric value is an unevidenced implementation default inherited from
/// the CTX-1315 review candidate (750 ms). It stays a named constant in
/// exactly one place and remains subject to owner/reviewer re-validation; it is
/// not a claim that the process completes within a hard real-time budget.
pub(crate) const CLIPBOARD_ACQUISITION_TIMEOUT: Duration = Duration::from_millis(750);

/// Per-acquisition byte and time limits.
///
/// Production uses [`Self::production`]; tests inject short deadlines and small
/// windows so the at-ceiling / one-over / deadline contracts are exercised
/// without a display server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AcquisitionLimits {
    /// Maximum decoded UTF-8 payload bytes returned to the caller.
    pub(crate) max_bytes: usize,
    /// Maximum native bytes retained from the transfer: `max_bytes` plus the
    /// oversize/char-boundary probe for streaming backends.
    #[cfg_attr(
        not(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        )),
        allow(dead_code)
    )]
    pub(crate) window_bytes: usize,
    /// Absolute wall-clock budget for the acquisition.
    #[cfg_attr(
        not(all(
            unix,
            not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
        )),
        allow(dead_code)
    )]
    pub(crate) deadline: Duration,
}

impl AcquisitionLimits {
    /// Production limits: [`CLIPBOARD_MAX_BYTES`], a four-byte streaming
    /// probe window, and [`CLIPBOARD_ACQUISITION_TIMEOUT`].
    pub(crate) const fn production() -> Self {
        Self {
            max_bytes: CLIPBOARD_MAX_BYTES,
            window_bytes: CLIPBOARD_MAX_BYTES + NATIVE_PROBE_BYTES,
            deadline: CLIPBOARD_ACQUISITION_TIMEOUT,
        }
    }
}

/// Typed fail-closed acquisition error.
///
/// [`Self::Setup`] is the only variant the Linux Wayland backend may treat as
/// "this backend produced no transfer": it is returned before any payload byte
/// is read. Every other variant aborts the read and returns no text, and the
/// Linux read funnel deliberately has no `arboard` fallback (CTX-1366).
#[cfg_attr(
    not(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    )),
    allow(dead_code)
)]
#[derive(Debug)]
pub(crate) enum AcquisitionError {
    /// Backend setup produced no transfer (Wayland: no seat, empty clipboard,
    /// no text MIME type, socket/roundtrip failure).
    Setup(String),
    /// The absolute deadline expired before completion.
    DeadlineExceeded {
        /// Acquisition stage that was waiting when the deadline expired.
        stage: &'static str,
        /// Configured budget in milliseconds.
        timeout_ms: u64,
    },
    /// The observed payload is not valid for its declared encoding.
    Malformed {
        /// Failure stage tag.
        stage: &'static str,
    },
    /// The transport or native object failed after acquisition started.
    Transport {
        /// Transfer stage that failed.
        stage: &'static str,
        /// Upstream diagnostic text.
        detail: String,
    },
    /// The producer exceeded the bounded window or the decoded ceiling.
    ///
    /// No partial value is delivered for the native X11/Windows paths: the
    /// bounded reader returns this typed error instead of a truncated prefix.
    OverLimit {
        /// Failure stage tag.
        stage: &'static str,
        /// Bytes observed or advertised once the bound was exceeded; a lower
        /// bound on the foreign payload, never necessarily its full length.
        observed_len: usize,
    },
    /// The clipboard cannot be read on this platform or contains no readable
    /// text. Carries the refusal/absence reason.
    Unavailable(String),
}

/// Maps a typed acquisition failure to the public platform error.
///
/// This is the single error surface for every bounded reader: the caller sees
/// a deadline as [`PlatformError::ClipboardAcquisitionTimeout`], an over-limit
/// producer as [`PlatformError::ClipboardPayloadTooLarge`], a missing/refused
/// backend as [`PlatformError::ClipboardUnavailable`], and any other transport
/// or decode failure as [`PlatformError::ClipboardOperation`].
pub(crate) fn platform_error(err: AcquisitionError) -> PlatformError {
    match err {
        AcquisitionError::Setup(detail) => PlatformError::ClipboardOperation(detail),
        AcquisitionError::DeadlineExceeded { stage, timeout_ms } => {
            PlatformError::ClipboardAcquisitionTimeout { stage, timeout_ms }
        }
        AcquisitionError::Malformed { stage } => PlatformError::ClipboardOperation(format!(
            "clipboard acquisition rejected malformed input during {stage}"
        )),
        AcquisitionError::Transport { stage, detail } => PlatformError::ClipboardOperation(
            format!("clipboard acquisition transport failure during {stage}: {detail}"),
        ),
        AcquisitionError::OverLimit {
            stage,
            observed_len,
        } => {
            // The public payload error has no stage field; bind it so the
            // diagnostic remains part of the variant's read surface.
            let _ = stage;
            PlatformError::ClipboardPayloadTooLarge {
                len: observed_len,
                max: CLIPBOARD_MAX_BYTES,
            }
        }
        AcquisitionError::Unavailable(detail) => PlatformError::ClipboardUnavailable(detail),
    }
}

// -- Windows -----------------------------------------------------------------

/// Maximum UTF-16 code units scanned from one `CF_UNICODETEXT` allocation.
///
/// Each scanned unit contributes at least one decoded output byte (a surrogate
/// pair contributes two bytes for two units), so `CLIPBOARD_MAX_BYTES` units
/// cover every value that can fit the decoded ceiling. Two extra units leave
/// room for the terminator and for the next (possibly surrogate-pair)
/// character that proves overflow without scanning an unbounded suffix.
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_MAX_SCAN_UNITS: usize = CLIPBOARD_MAX_BYTES + 2;

/// Maximum `GlobalSize` extent (bytes) that may be copied for a bounded
/// `CF_UNICODETEXT` read: two bytes per scanned UTF-16 code unit.
///
/// The size query happens before the copy; a larger allocation is rejected
/// without materializing it (CTX-1366).
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_MAX_ALLOCATION_BYTES: usize = 2 * WINDOWS_MAX_SCAN_UNITS;

/// Pure Windows size gate: `true` when a `GlobalSize` extent fits the bounded
/// scan window.
///
/// `GlobalSize` reports the allocation extent, which can include padding after
/// the NUL terminator; it is never used as the logical text length.
#[cfg(any(windows, test))]
pub(crate) const fn windows_allocation_within_bound(size: usize) -> bool {
    size <= WINDOWS_MAX_ALLOCATION_BYTES
}

/// Decodes a copied little-endian UTF-16 `CF_UNICODETEXT` extent.
///
/// Contract:
///
/// - only whole code units are read; an odd byte length is malformed;
/// - a NUL code unit terminates the text, so allocation padding after it is
///   ignored; no NUL inside the extent is malformed (unterminated);
/// - lone or mismatched surrogates become U+FFFD, matching the Windows
///   `WideCharToMultiByte(CP_UTF8, 0, ...)` policy used by the previous path;
/// - the decoded value is retained only while it fits `max_bytes`; the first
///   character that would exceed the ceiling is a typed
///   [`AcquisitionError::OverLimit`] and no partial value is returned.
///
/// The caller must have rejected an extent above
/// [`WINDOWS_MAX_ALLOCATION_BYTES`] before copying.
#[cfg(any(windows, test))]
pub(crate) fn decode_windows_utf16(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<BoundedText, AcquisitionError> {
    const STAGE: &str = "windows-utf16-scan";
    if bytes.len() % 2 != 0 {
        return Err(AcquisitionError::Malformed { stage: STAGE });
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut out = String::with_capacity(max_bytes.min(units.len()));
    let mut index = 0;
    let mut terminated = false;
    while index < units.len() {
        let unit = units[index];
        index += 1;
        if unit == 0 {
            terminated = true;
            break;
        }
        let ch = if (0xD800..=0xDBFF).contains(&unit) {
            match units.get(index) {
                Some(&low) if (0xDC00..=0xDFFF).contains(&low) => {
                    index += 1;
                    let scalar =
                        0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                    char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER)
                }
                _ => char::REPLACEMENT_CHARACTER,
            }
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            char::REPLACEMENT_CHARACTER
        } else {
            char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)
        };
        if out.len() + ch.len_utf8() > max_bytes {
            return Err(AcquisitionError::OverLimit {
                stage: STAGE,
                observed_len: bytes.len(),
            });
        }
        out.push(ch);
    }
    if !terminated {
        return Err(AcquisitionError::Malformed { stage: STAGE });
    }
    Ok(BoundedText::Complete(out))
}

// -- X11 ---------------------------------------------------------------------

/// `GetProperty.long_length` (four-byte units) that keeps one reply inside
/// `window_bytes` rounded up to the next four-byte unit.
///
/// X11 property requests count four-byte units, so the wire window is at most
/// three bytes above the caller's window; that rounding slack is the only
/// allocation a conforming server can add. `u32::MAX` is the protocol field
/// limit and is never reached by the clipboard window in practice.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
pub(crate) const fn x11_long_length_units(window_bytes: usize) -> u32 {
    let units = window_bytes.saturating_add(3) / 4;
    if units > u32::MAX as usize {
        u32::MAX
    } else {
        units as u32
    }
}

/// Wire bytes a bounded `GetProperty` request may return.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
pub(crate) const fn x11_window_bytes(max_bytes: usize) -> usize {
    4 * x11_long_length_units(max_bytes) as usize
}

/// Pure ordinary-property gate.
///
/// A reply is acceptable only when it is entirely inside the ceiling and the
/// property has no unread remainder. `bytes_after` is the server-reported
/// continuation length, so a partial reply (or a property just over the
/// ceiling) is an over-limit observation rather than a silent truncation.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
pub(crate) const fn x11_property_within_cap(
    value_len: usize,
    bytes_after: usize,
    max_bytes: usize,
) -> bool {
    value_len <= max_bytes && bytes_after == 0
}

/// X11 text encodings a bounded read may decode.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X11Encoding {
    /// `UTF8_STRING`: raw UTF-8 bytes.
    Utf8,
    /// `STRING`: ISO 8859-1 (Latin-1), one character per octet.
    Latin1,
}

/// Decodes an X11 property value under the decoded ceiling.
///
/// Over-cap input is a typed [`AcquisitionError::OverLimit`] for both
/// encodings; no partial value is returned (CTX-1366 path B). Latin-1
/// expansion is checked per character, so the retained `String` never exceeds
/// `max_bytes` even though one octet can encode as two UTF-8 bytes.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
pub(crate) fn decode_x11_text(
    bytes: &[u8],
    encoding: X11Encoding,
    max_bytes: usize,
) -> Result<BoundedText, AcquisitionError> {
    const STAGE: &str = "x11-property";
    match encoding {
        X11Encoding::Utf8 => {
            if bytes.len() > max_bytes {
                return Err(AcquisitionError::OverLimit {
                    stage: STAGE,
                    observed_len: bytes.len(),
                });
            }
            std::str::from_utf8(bytes)
                .map(|text| BoundedText::Complete(text.to_owned()))
                .map_err(|_| AcquisitionError::Malformed { stage: STAGE })
        }
        X11Encoding::Latin1 => {
            let mut out = String::with_capacity(max_bytes.min(bytes.len()));
            for &octet in bytes {
                let ch = char::from(octet);
                if out.len() + ch.len_utf8() > max_bytes {
                    return Err(AcquisitionError::OverLimit {
                        stage: STAGE,
                        observed_len: bytes.len(),
                    });
                }
                out.push(ch);
            }
            Ok(BoundedText::Complete(out))
        }
    }
}

/// Bounded INCR accumulation.
///
/// The ICCCM INCR advertisement is untrusted metadata, never an allocation
/// instruction: the buffer is pre-sized to the rounded ceiling and a chunk
/// that would cross either the rounded window or the decoded ceiling is
/// rejected as [`AcquisitionError::OverLimit`] **before** it is retained.
/// The caller requests only [`Self::remaining_units`] from the next property
/// read, so a conforming server can never hand back more than the window.
#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
#[derive(Debug)]
pub(crate) struct IncrAccumulator {
    max_bytes: usize,
    window_bytes: usize,
    bytes: Vec<u8>,
}

#[cfg(any(
    all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ),
    test
))]
impl IncrAccumulator {
    /// Creates an accumulator whose retained window is `max_bytes` rounded up
    /// to a four-byte X11 unit boundary.
    pub(crate) fn new(max_bytes: usize) -> Self {
        let window_bytes = x11_window_bytes(max_bytes);
        Self {
            max_bytes,
            window_bytes,
            bytes: Vec::with_capacity(window_bytes),
        }
    }

    /// Maximum native bytes this accumulator may ever retain.
    #[cfg(test)]
    pub(crate) fn window_bytes(&self) -> usize {
        self.window_bytes
    }

    /// Native bytes retained so far.
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `long_length` for the next `GetProperty` request: only the remaining
    /// window, rounded down to whole four-byte units.
    pub(crate) fn remaining_units(&self) -> u32 {
        let remaining = self.window_bytes.saturating_sub(self.bytes.len());
        x11_long_length_units(remaining)
    }

    /// Retains one INCR chunk or rejects it whole.
    ///
    /// The chunk is never partially appended: a chunk that would cross the
    /// decoded ceiling returns a typed error and leaves the retained bytes
    /// unchanged. The ceiling is the stricter bound; [`Self::window_bytes`]
    /// only sizes the allocation and the next property request. A chunk that
    /// lands exactly on `max_bytes` is accepted; completion is signalled
    /// separately by the zero-length property.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), AcquisitionError> {
        const STAGE: &str = "x11-incr";
        let total = self.bytes.len().saturating_add(chunk.len());
        if total > self.max_bytes {
            return Err(AcquisitionError::OverLimit {
                stage: STAGE,
                observed_len: total,
            });
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    /// Consumes the accumulator and decodes its retained bytes.
    pub(crate) fn finish(self, encoding: X11Encoding) -> Result<BoundedText, AcquisitionError> {
        decode_x11_text(&self.bytes, encoding, self.max_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_limits_use_the_shared_ceiling_and_probe() {
        let limits = AcquisitionLimits::production();
        assert_eq!(limits.max_bytes, CLIPBOARD_MAX_BYTES);
        assert_eq!(
            limits.window_bytes,
            CLIPBOARD_MAX_BYTES + NATIVE_PROBE_BYTES
        );
        assert_eq!(limits.deadline, Duration::from_millis(750));
    }

    // -- Windows: pure size gate and UTF-16 scan ---------------------------

    #[test]
    fn windows_size_gate_accepts_the_bound_and_rejects_one_over() {
        assert!(windows_allocation_within_bound(0));
        assert!(windows_allocation_within_bound(
            WINDOWS_MAX_ALLOCATION_BYTES
        ));
        assert!(!windows_allocation_within_bound(
            WINDOWS_MAX_ALLOCATION_BYTES + 1
        ));
        assert!(!windows_allocation_within_bound(usize::MAX));
        assert_eq!(
            WINDOWS_MAX_ALLOCATION_BYTES,
            2 * (CLIPBOARD_MAX_BYTES + 2),
            "the window is derived from the decoded ceiling, not a new policy"
        );
    }

    #[test]
    fn windows_utf16_at_the_ceiling_is_complete() {
        let mut bytes = Vec::with_capacity((CLIPBOARD_MAX_BYTES + 1) * 2);
        for _ in 0..CLIPBOARD_MAX_BYTES {
            bytes.extend_from_slice(&u16::from(b'a').to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        let result = decode_windows_utf16(&bytes, CLIPBOARD_MAX_BYTES).expect("at-cap text");
        match result {
            BoundedText::Complete(text) => assert_eq!(text.len(), CLIPBOARD_MAX_BYTES),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn windows_utf16_one_over_the_ceiling_is_rejected_without_a_prefix() {
        let mut bytes = Vec::with_capacity((CLIPBOARD_MAX_BYTES + 2) * 2);
        for _ in 0..=CLIPBOARD_MAX_BYTES {
            bytes.extend_from_slice(&u16::from(b'a').to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        let error = decode_windows_utf16(&bytes, CLIPBOARD_MAX_BYTES)
            .expect_err("over-cap text must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::OverLimit {
                stage: "windows-utf16-scan",
                ..
            }
        ));
    }

    #[test]
    fn windows_utf16_padding_after_nul_is_ignored() {
        let mut bytes = Vec::new();
        for unit in "ok".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        // GlobalSize padding beyond the terminator is not text.
        bytes.extend_from_slice(&[0xAA; 32]);
        let result = decode_windows_utf16(&bytes, CLIPBOARD_MAX_BYTES).expect("terminated text");
        assert!(matches!(result, BoundedText::Complete(ref text) if text == "ok"));
    }

    #[test]
    fn windows_utf16_without_a_terminator_is_malformed() {
        let bytes = vec![b'a'; 4];
        let error = decode_windows_utf16(&bytes, CLIPBOARD_MAX_BYTES)
            .expect_err("unterminated text must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::Malformed {
                stage: "windows-utf16-scan"
            }
        ));
    }

    #[test]
    fn windows_utf16_odd_byte_length_is_malformed() {
        let error = decode_windows_utf16(&[0, 1, 2], CLIPBOARD_MAX_BYTES)
            .expect_err("a partial code unit must fail closed");
        assert!(matches!(error, AcquisitionError::Malformed { .. }));
    }

    #[test]
    fn windows_utf16_lone_surrogates_become_replacement_characters() {
        // Matches the locked WideCharToMultiByte(CP_UTF8, 0, ...) replacement
        // policy for observed invalid UTF-16 (not a new policy).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xD800_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::from(b'x').to_le_bytes());
        bytes.extend_from_slice(&0xDC00_u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        let result = decode_windows_utf16(&bytes, CLIPBOARD_MAX_BYTES).expect("scan");
        match result {
            BoundedText::Complete(text) => {
                assert_eq!(text, "\u{FFFD}x\u{FFFD}");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn windows_utf16_surrogate_pair_decodes_to_one_character() {
        let mut bytes = Vec::new();
        for unit in "😀".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        let result = decode_windows_utf16(&bytes, 4).expect("pair fits four bytes");
        assert!(matches!(result, BoundedText::Complete(ref text) if text == "😀"));
        // The same pair cannot be delivered into a three-byte ceiling.
        assert!(matches!(
            decode_windows_utf16(&bytes, 3),
            Err(AcquisitionError::OverLimit { .. })
        ));
    }

    #[test]
    fn windows_utf16_empty_value_is_complete_and_empty() {
        let result = decode_windows_utf16(&[0, 0], CLIPBOARD_MAX_BYTES).expect("empty");
        assert!(matches!(result, BoundedText::Complete(ref text) if text.is_empty()));
    }

    // -- X11: request window, ordinary property, INCR ----------------------

    #[test]
    fn x11_request_units_never_exceed_the_window_plus_rounding() {
        for max in [1_usize, 3, 4, 63, 64, 8191, CLIPBOARD_MAX_BYTES] {
            let wire = x11_window_bytes(max);
            assert!(wire >= max, "must request enough for the ceiling");
            assert!(
                wire - max < 4,
                "rounding slack must stay below one four-byte unit"
            );
            assert_eq!(wire, 4 * x11_long_length_units(wire) as usize);
        }
        assert_eq!(x11_long_length_units(0), 0);
        assert_eq!(x11_long_length_units(usize::MAX), u32::MAX);
    }

    #[test]
    fn x11_ordinary_property_gate_rejects_a_partial_or_over_cap_reply() {
        assert!(x11_property_within_cap(0, 0, 8192));
        assert!(x11_property_within_cap(8192, 0, 8192));
        assert!(!x11_property_within_cap(8193, 0, 8192));
        assert!(
            !x11_property_within_cap(8192, 1, 8192),
            "a remainder proves the property exceeded the requested window"
        );
    }

    #[test]
    fn x11_utf8_at_the_ceiling_is_complete() {
        let bytes = vec![b'a'; 8192];
        let result = decode_x11_text(&bytes, X11Encoding::Utf8, 8192).expect("at cap");
        assert!(matches!(result, BoundedText::Complete(ref text) if text.len() == 8192));
    }

    #[test]
    fn x11_utf8_over_the_ceiling_is_typed_rejection_without_a_prefix() {
        let bytes = vec![b'a'; 8193];
        let error = decode_x11_text(&bytes, X11Encoding::Utf8, 8192)
            .expect_err("over-cap UTF-8 must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::OverLimit {
                stage: "x11-property",
                observed_len: 8193
            }
        ));
    }

    #[test]
    fn x11_utf8_invalid_bytes_are_malformed() {
        let error = decode_x11_text(&[0xFF, 0xFE], X11Encoding::Utf8, 8192)
            .expect_err("invalid UTF-8 must fail closed");
        assert!(matches!(
            error,
            AcquisitionError::Malformed {
                stage: "x11-property"
            }
        ));
    }

    #[test]
    fn x11_latin1_expansion_is_bounded_by_the_decoded_ceiling() {
        // 0xE9 is 'é' (two UTF-8 bytes). Four octets fit an 8-byte ceiling,
        // five do not, even though the native window is only five bytes.
        let at_cap = [0xE9_u8; 4];
        let result = decode_x11_text(&at_cap, X11Encoding::Latin1, 8).expect("fits");
        assert!(matches!(result, BoundedText::Complete(ref text) if text.len() == 8));
        let over = [0xE9_u8; 5];
        assert!(matches!(
            decode_x11_text(&over, X11Encoding::Latin1, 8),
            Err(AcquisitionError::OverLimit {
                stage: "x11-property",
                ..
            })
        ));
    }

    #[test]
    fn incr_accumulator_retains_at_most_the_rounded_window() {
        let mut acc = IncrAccumulator::new(CLIPBOARD_MAX_BYTES);
        assert_eq!(acc.window_bytes(), CLIPBOARD_MAX_BYTES);
        assert!(
            acc.bytes.capacity() <= acc.window_bytes(),
            "the buffer is pre-sized to the bounded window"
        );
        acc.push(&vec![b'a'; 4096]).expect("first chunk");
        acc.push(&vec![b'b'; 4096])
            .expect("second chunk reaches the cap");
        assert_eq!(acc.len(), CLIPBOARD_MAX_BYTES);
        assert!(
            acc.bytes.capacity() <= acc.window_bytes(),
            "no reallocation beyond the window"
        );
        let error = acc
            .push(b"c")
            .expect_err("one byte over the cap must be rejected");
        assert!(matches!(
            error,
            AcquisitionError::OverLimit {
                stage: "x11-incr",
                observed_len: 8193
            }
        ));
        assert_eq!(
            acc.len(),
            CLIPBOARD_MAX_BYTES,
            "rejected chunk is not retained"
        );
    }

    #[test]
    fn incr_accumulator_rejects_a_chunk_above_the_ceiling_before_retaining_it() {
        let mut acc = IncrAccumulator::new(10);
        assert_eq!(acc.window_bytes(), 12);
        let error = acc.push(&[b'x'; 13]).expect_err("ceiling overrun");
        assert!(matches!(
            error,
            AcquisitionError::OverLimit {
                stage: "x11-incr",
                observed_len: 13
            }
        ));
        assert_eq!(acc.len(), 0);
        acc.push(&[b'x'; 10]).expect("decoded ceiling is inclusive");
        assert!(matches!(
            acc.push(&[b'y'; 2]),
            Err(AcquisitionError::OverLimit { .. })
        ));
        assert_eq!(acc.len(), 10);
    }

    #[test]
    fn incr_accumulator_requests_only_the_remaining_window() {
        let mut acc = IncrAccumulator::new(64);
        assert_eq!(acc.remaining_units(), 16);
        acc.push(&[b'a'; 12]).expect("chunk");
        assert_eq!(acc.remaining_units(), 13);
        acc.push(&[b'a'; 52]).expect("reaches the cap");
        assert_eq!(acc.remaining_units(), 0);
    }

    #[test]
    fn incr_accumulator_finishes_with_the_requested_encoding() {
        let mut acc = IncrAccumulator::new(8192);
        acc.push(b"hello").expect("chunk");
        let result = acc.finish(X11Encoding::Utf8).expect("decode");
        assert!(matches!(result, BoundedText::Complete(ref text) if text == "hello"));
    }

    #[test]
    fn public_error_mapping_is_unified() {
        assert_eq!(
            platform_error(AcquisitionError::DeadlineExceeded {
                stage: "x11-selection",
                timeout_ms: 750
            }),
            PlatformError::ClipboardAcquisitionTimeout {
                stage: "x11-selection",
                timeout_ms: 750
            }
        );
        assert_eq!(
            platform_error(AcquisitionError::OverLimit {
                stage: "x11-incr",
                observed_len: 9000
            }),
            PlatformError::ClipboardPayloadTooLarge {
                len: 9000,
                max: CLIPBOARD_MAX_BYTES
            }
        );
        assert_eq!(
            platform_error(AcquisitionError::Unavailable(String::from("no text"))),
            PlatformError::ClipboardUnavailable(String::from("no text"))
        );
        assert!(matches!(
            platform_error(AcquisitionError::Transport {
                stage: "x11-property",
                detail: String::from("closed")
            }),
            PlatformError::ClipboardOperation(_)
        ));
        assert!(matches!(
            platform_error(AcquisitionError::Malformed {
                stage: "x11-property"
            }),
            PlatformError::ClipboardOperation(_)
        ));
    }
}
