//! Bounded mouse-report encoding for every xterm coordinate encoding
//! (CTX-0566, issue #1127).
//!
//! Before CTX-0566 the runtime emitted only SGR (`?1006`). This module adds
//! the legacy encodings so an application that enables X10 (default),
//! UTF-8 (`?1005`), or urxvt (`?1015`) receives reports instead of nothing.
//!
//! # Wire formats (xterm `ctlseqs.txt`, "Mouse Tracking")
//!
//! The mouse *protocol* (which events are reported) and the *encoding* (how
//! the parameters are framed) are independent. All four encodings share one
//! button/modifier code and add 32 to every legacy byte. Coordinates are
//! 1-based on the wire.
//!
//! - **X10 / legacy default** (no `?1005`/`?1006`/`?1015`): `CSI M Cb Cx Cy`,
//!   three raw bytes: `Cb = code + 32`, `Cx/Cy = coord_1based + 32`. The
//!   addressable range is 1..=223 per axis (`255 - 32`); larger positions
//!   are clamped to 223. Releases collapse to button 3, so the released
//!   button is not identified.
//! - **SGR** (`?1006`): `CSI < Cb;Cx;Cy M` for press and `... m` for
//!   release. `Cb` is decimal without the +32 offset; the `M`/`m` final
//!   keeps button identity (and modifiers) on release. No coordinate limit.
//! - **UTF-8** (`?1005`): `CSI M` followed by three UTF-8 codepoints, each
//!   `value + 32` (`Cb` as one or two bytes, `Cx/Cy` as one to three bytes).
//!   This is xterm's documented range extension from 223 to 2015.
//! - **urxvt** (`?1015`): `CSI Cb;Cx;Cy M` with `Cb = code + 32` and decimal
//!   `Cx/Cy`. Same button semantics as X10 (releases collapse to 3). xterm
//!   and rxvt-unicode place **no coordinate offset** on this encoding — the
//!   accepted wire format is plain 1-based decimals (xterm `button.c`
//!   `EmitMousePosition`/`EmitButtonCode`; rxvt-unicode `command.C`
//!   `mouse_report`: `tt_printf("\033[%d;%d;%dM", code, x, y)`). Some task
//!   descriptions mention a "2048 offset" for urxvt; no such bias exists in
//!   either reference implementation, so this module follows the accepted
//!   format and the "2048" wording is treated as an error. Coordinates are
//!   decimal, so the range is bounded only by the `u16` grid domain.
//!
//! # Bounds
//!
//! Encoding writes into a fixed [`MOUSE_REPORT_MAX_BYTES`] stack buffer; no
//! per-event heap allocation and no unbounded formatting. Coordinates are
//! clamped to the encoding's representable range before writing.

use bitty_vt::MouseCoordinateEncoding;

/// Maximum bytes any one mouse report can occupy.
///
/// Worst case is SGR with full five-digit coordinates
/// (`ESC [ <` + code + `;` + col + `;` + row + final) and the urxvt
/// decimal form, both well under this cap; the UTF-8 and X10 forms are
/// shorter still.
pub const MOUSE_REPORT_MAX_BYTES: usize = 32;

/// Largest 1-based X10/legacy coordinate (`255 - 32`).
const X10_COORD_MAX: u16 = 223;

/// Largest 0-based UTF-8-encoded coordinate. xterm's `EXT_MOUSE_LIMIT`
/// (`2047 - 32`): the codepoint `coord + 33` stays within three UTF-8 bytes.
const UTF8_COORD_MAX: u16 = 2015;

/// Effective coordinate encoding for a mouse report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseFormat {
    /// X10 byte framing (no extended encoding selected).
    X10,
    /// UTF-8 extended coordinates (`?1005`).
    Utf8,
    /// SGR decimal coordinates (`?1006`).
    Sgr,
    /// urxvt decimal coordinates (`?1015`).
    Urxvt,
}

impl MouseFormat {
    /// Resolves the effective format from the live mode register. An absent
    /// encoding is the X10 legacy default; SGR is only used when explicitly
    /// selected, never as an implicit upgrade.
    #[must_use]
    pub fn from_encoding(encoding: Option<MouseCoordinateEncoding>) -> Self {
        match encoding {
            None => Self::X10,
            Some(MouseCoordinateEncoding::Utf8) => Self::Utf8,
            Some(MouseCoordinateEncoding::Sgr) => Self::Sgr,
            Some(MouseCoordinateEncoding::Urxvt) => Self::Urxvt,
        }
    }
}

/// One normalized mouse report ready for framing.
#[derive(Debug, Clone, Copy)]
pub struct MouseReport {
    /// Button bits without modifiers: `0`/`1`/`2` left/middle/right, `8`/`9`
    /// back/forward, `64..=67` wheel up/down/left/right.
    pub button: u8,
    /// Modifier bits: shift `4`, alt `8`, control `16`.
    pub modifiers: u8,
    /// Motion event (adds the `+32` motion bit).
    pub motion: bool,
    /// Button release. SGR keeps [`Self::button`] identity; the legacy
    /// encodings collapse to button 3.
    pub release: bool,
    /// Zero-based grid column (clamped per encoding).
    pub col: u16,
    /// Zero-based grid row (clamped per encoding).
    pub row: u16,
}

impl MouseReport {
    /// Button/modifier code with the `+32` motion bit for the legacy
    /// encodings, where a release collapses to button 3.
    fn legacy_code(&self) -> u8 {
        let base = if self.release { 3 } else { self.button };
        let mut code = base | self.modifiers;
        if self.motion {
            code |= 32;
        }
        code
    }

    /// SGR button code: identity preserved, no coordinate offset.
    fn sgr_code(&self) -> u8 {
        let mut code = self.button | self.modifiers;
        if self.motion {
            code |= 32;
        }
        code
    }
}

/// A byte buffer holding one encoded report.
///
/// Returned by value so callers can pass it straight to
/// [`Runtime::push_input_bytes`](super::Runtime::push_input_bytes) without a
/// heap allocation.
pub struct MouseBytes {
    buf: [u8; MOUSE_REPORT_MAX_BYTES],
    len: usize,
}

impl MouseBytes {
    /// The encoded report.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Encodes one mouse report in `format` into a fixed stack buffer.
///
/// X10/UTF-8 coordinates are clamped to their representable range; SGR and
/// urxvt use unbounded decimal within the `u16` grid domain. The output is
/// always `<= MOUSE_REPORT_MAX_BYTES`.
#[must_use]
pub fn encode_mouse(format: MouseFormat, report: MouseReport) -> MouseBytes {
    let mut out = MouseBytes {
        buf: [0; MOUSE_REPORT_MAX_BYTES],
        len: 0,
    };
    match format {
        MouseFormat::X10 => encode_x10(&mut out, &report),
        MouseFormat::Utf8 => encode_utf8(&mut out, &report),
        MouseFormat::Sgr => encode_sgr(&mut out, &report),
        MouseFormat::Urxvt => encode_urxvt(&mut out, &report),
    }
    out
}

fn push(out: &mut MouseBytes, byte: u8) {
    debug_assert!(out.len < MOUSE_REPORT_MAX_BYTES, "mouse report overflow");
    if out.len < MOUSE_REPORT_MAX_BYTES {
        out.buf[out.len] = byte;
        out.len += 1;
    }
}

fn push_slice(out: &mut MouseBytes, bytes: &[u8]) {
    for &b in bytes {
        push(out, b);
    }
}

/// Writes unsigned decimal, returning bytes written. Bounded by `u16`.
fn push_decimal(out: &mut MouseBytes, mut value: u32) -> usize {
    let mut digits = [0u8; 5];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        push(out, digits[i]);
    }
    n
}

fn encode_x10(out: &mut MouseBytes, r: &MouseReport) {
    // `Cb` includes the +32 offset (legacy range 32..=127 for buttons but
    // the wheel/extra codes fit too); coordinates are 1-based, clamped to
    // 223, then offset by 32.
    let cb = r.legacy_code().wrapping_add(32);
    let col = (r.col.saturating_add(1)).min(X10_COORD_MAX) + 32;
    let row = (r.row.saturating_add(1)).min(X10_COORD_MAX) + 32;
    push_slice(out, b"\x1b[M");
    push(out, cb);
    push(out, col as u8);
    push(out, row as u8);
}

fn encode_sgr(out: &mut MouseBytes, r: &MouseReport) {
    push_slice(out, b"\x1b[<");
    push_decimal(out, u32::from(r.sgr_code()));
    push(out, b';');
    push_decimal(out, u32::from(r.col) + 1);
    push(out, b';');
    push_decimal(out, u32::from(r.row) + 1);
    push(out, if r.release { b'm' } else { b'M' });
}

fn encode_utf8(out: &mut MouseBytes, r: &MouseReport) {
    push_slice(out, b"\x1b[M");
    push_utf8_codepoint(out, u32::from(r.legacy_code().wrapping_add(32)));
    // xterm's UTF-8 extension: clamp the zero-based coordinate to 2015 and
    // encode `coord + 33` as a codepoint (1..=3 bytes).
    let col = r.col.min(UTF8_COORD_MAX) + 33;
    let row = r.row.min(UTF8_COORD_MAX) + 33;
    push_utf8_codepoint(out, u32::from(col));
    push_utf8_codepoint(out, u32::from(row));
}

fn encode_urxvt(out: &mut MouseBytes, r: &MouseReport) {
    push(out, 0x1b);
    push(out, b'[');
    // Decimal button code keeps the legacy +32 bias; coordinates are
    // 1-based decimals with no offset (see module docs).
    push_decimal(out, u32::from(r.legacy_code().wrapping_add(32)));
    push(out, b';');
    push_decimal(out, u32::from(r.col) + 1);
    push(out, b';');
    push_decimal(out, u32::from(r.row) + 1);
    push(out, b'M');
}

/// Writes `cp` as raw UTF-8 (one to three bytes for our bounded range).
fn push_utf8_codepoint(out: &mut MouseBytes, cp: u32) {
    let mut buf = [0u8; 4];
    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
    let encoded = ch.encode_utf8(&mut buf);
    push_slice(out, encoded.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(button: u8, col: u16, row: u16) -> MouseReport {
        MouseReport {
            button,
            modifiers: 0,
            motion: false,
            release: false,
            col,
            row,
        }
    }

    #[test]
    fn from_encoding_defaults_to_x10() {
        assert_eq!(MouseFormat::from_encoding(None), MouseFormat::X10);
        assert_eq!(
            MouseFormat::from_encoding(Some(MouseCoordinateEncoding::Sgr)),
            MouseFormat::Sgr
        );
        assert_eq!(
            MouseFormat::from_encoding(Some(MouseCoordinateEncoding::Utf8)),
            MouseFormat::Utf8
        );
        assert_eq!(
            MouseFormat::from_encoding(Some(MouseCoordinateEncoding::Urxvt)),
            MouseFormat::Urxvt
        );
    }

    #[test]
    fn x10_shape_and_clamp() {
        assert_eq!(
            encode_mouse(MouseFormat::X10, report(0, 0, 0)).as_slice(),
            b"\x1b[M\x20\x21\x21"
        );
        // Coordinates beyond 223 clamp to 223 (+32 = 255).
        assert_eq!(
            encode_mouse(MouseFormat::X10, report(0, 10_000, 10_000)).as_slice(),
            b"\x1b[M\x20\xff\xff"
        );
        // Legacy release collapses to button 3.
        let mut rel = report(2, 0, 0);
        rel.release = true;
        assert_eq!(
            encode_mouse(MouseFormat::X10, rel).as_slice(),
            b"\x1b[M\x23\x21\x21"
        );
    }

    #[test]
    fn utf8_small_matches_single_bytes_and_large_uses_multibyte() {
        assert_eq!(
            encode_mouse(MouseFormat::Utf8, report(2, 0, 0)).as_slice(),
            b"\x1b[M\x22\x21\x21"
        );
        // 300 -> codepoint 333 -> two UTF-8 bytes; 400 -> 433.
        let bytes = encode_mouse(MouseFormat::Utf8, report(0, 300, 400));
        let out = bytes.as_slice();
        assert_eq!(&out[..3], b"\x1b[M");
        let decoded: Vec<char> = std::str::from_utf8(&out[3..])
            .expect("valid utf8")
            .chars()
            .collect();
        assert_eq!(decoded, vec!['\u{20}', '\u{14d}', '\u{1b1}']);
    }

    #[test]
    fn utf8_clamps_to_2015() {
        let bytes = encode_mouse(MouseFormat::Utf8, report(0, 60_000, 60_000));
        let out = bytes.as_slice();
        let text = std::str::from_utf8(&out[3..]).expect("valid utf8");
        let mut it = text.chars();
        let _code = it.next();
        // 2015 + 33 = 2048 = U+0800.
        assert_eq!(it.next(), Some('\u{800}'));
        assert_eq!(it.next(), Some('\u{800}'));
    }

    #[test]
    fn urxvt_shape_and_release() {
        let mut m = report(1, 0, 0);
        m.modifiers = 4; // shift
        assert_eq!(
            encode_mouse(MouseFormat::Urxvt, m).as_slice(),
            b"\x1b[37;1;1M"
        );
        let mut rel = report(1, 0, 0);
        rel.release = true;
        assert_eq!(
            encode_mouse(MouseFormat::Urxvt, rel).as_slice(),
            b"\x1b[35;1;1M"
        );
        // Large coordinates are unbounded decimal.
        assert_eq!(
            encode_mouse(MouseFormat::Urxvt, report(0, 300, 400)).as_slice(),
            b"\x1b[32;301;401M"
        );
    }

    #[test]
    fn sgr_keeps_identity_and_modifiers() {
        let mut m = report(2, 4, 5);
        m.modifiers = 4 | 8; // shift+alt
        assert_eq!(
            encode_mouse(MouseFormat::Sgr, m).as_slice(),
            b"\x1b[<14;5;6M"
        );
        let mut rel = report(2, 4, 5);
        rel.release = true;
        assert_eq!(
            encode_mouse(MouseFormat::Sgr, rel).as_slice(),
            b"\x1b[<2;5;6m"
        );
    }

    #[test]
    fn wheel_and_motion_codes() {
        assert_eq!(
            encode_mouse(MouseFormat::Sgr, report(64, 0, 0)).as_slice(),
            b"\x1b[<64;1;1M"
        );
        let mut motion = report(0, 0, 0);
        motion.motion = true;
        assert_eq!(
            encode_mouse(MouseFormat::Sgr, motion).as_slice(),
            b"\x1b[<32;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseFormat::Urxvt, report(65, 0, 0)).as_slice(),
            b"\x1b[97;1;1M"
        );
    }

    #[test]
    fn every_encoding_stays_within_the_buffer_cap() {
        for format in [
            MouseFormat::X10,
            MouseFormat::Utf8,
            MouseFormat::Sgr,
            MouseFormat::Urxvt,
        ] {
            let bytes = encode_mouse(format, report(67, u16::MAX, u16::MAX));
            assert!(bytes.as_slice().len() <= MOUSE_REPORT_MAX_BYTES);
        }
    }
}
