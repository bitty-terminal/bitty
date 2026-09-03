//! Bounded answers to standard terminal queries (CTX-0146, Issue #238).
//!
//! Live-observed in CTX-0142: `fish` stalls 10 s+ on Primary Device Attributes
//! at startup and the `tide` prompt needs ~1 min plus manual wakeups, because
//! standard queries go unanswered. CTX-0145 fixed the wakeup path, so replies
//! queued here now reach the screen promptly.
//!
//! Reply shapes follow the xterm control-sequence reference
//! (`recording/references/xterm/ctlseqs.txt`, read-only prior art per
//! DEC-0004; learned, never copied) and the secondary-DA shape of
//! `recording/references/alacritty` (`ESC[>0;<version>;1c`):
//!
//! | Query bytes | Reply bytes | Source of truth |
//! |---|---|---|
//! | `CSI c` / `CSI 0 c` (Primary DA) | `ESC[?6c` | `bitty-term-state` (unchanged) |
//! | `ESC Z` (legacy DECID) | `ESC[?6c` | mapped by `bitty-vt` to the existing `RequestDeviceStatus` action |
//! | `CSI > c` / `CSI > 0 c` (Secondary DA) | `ESC[>0;<ver>;1c` | [`secondary_da_reply`] |
//! | `CSI > q` / `CSI > 0 q` (XTVERSION) | `DCS > \| Bitty <ver> ST` | [`xterm_version_reply`] |
//! | `DCS + q <hex> ST` (XTGETTCAP) | `DCS 1 + r <hex>=<hex> ST` or `DCS 0 + r ST` | [`xtgettcap_reply`] |
//! | `CSI Ps $ p` / `CSI ? Ps $ p` (DECRQM) | `CSI [?] Ps ; Pm $ y` (`Pm`: 0 unknown, 1 set, 2 reset) | [`decrqm_value`] reads live `State` |
//!
//! Values are bitty's TRUE capabilities, never borrowed prestige:
//!
//! - XTGETTCAP answers only `TN`/`name` (= [`bitty_pty::DEFAULT_TERM`], the
//!   `TERM` children actually see), `Co`/`colors` (`256`: SGR `38;5` indexed
//!   color is parsed, stored, and rendered), and `RGB` (`8`: uniform 8-bit
//!   direct color via SGR `38;2`). Every other capability name — and any
//!   malformed hex — gets the well-formed negative `DCS 0 + r ST`, never
//!   silence (an invalid name ends processing, matching xterm).
//! - DECRQM reports the live mode register: ANSI `4` (insert) and `20`
//!   (line-feed/new-line) plus every private mode `bitty-vt` maps
//!   (cursor keys, 132-column request flag, reverse video, origin, autowrap,
//!   mouse levels/encodings, cursor visibility/blink, alt-screen, bracketed
//!   paste, focus events, Kitty `7727`). Anything else — including action-only
//!   pseudo-modes such as `1048` — reports `0` (not recognized).
//! - Tertiary DA (`CSI = c`) stays silent on purpose: the reference
//!   (`alacritty::identify_terminal`) answers primary/secondary only, and a
//!   fabricated unit-ID reply would claim identity bitty does not have.
//!
//! Scope note: only `bitty-vt` and `bitty-runtime` are touched by CTX-0146, so
//! no new [`bitty_vt::TerminalAction`] variant could be added (that would
//! break the exhaustive match and struct literals in `bitty-term-state`,
//! which is out of scope). Instead the runtime matches the parser's existing
//! [`bitty_vt::TerminalAction::Unknown`] triggers (precise
//! kind/final/intermediate triples, see [`is_secondary_da`] et al.) and, for
//! the two families that carry parameters, runs bounded raw scans over the
//! overlap-plus-new bytes with [`UnrecognizedSequence`] correlation, so bytes
//! buried inside OSC strings can never spoof a reply. Replies are queued
//! through the existing [`bitty_vt::TerminalAction::Reply`] action, so the
//! 4 KiB [`REPLY_CAP_BYTES`](bitty_term_state::replies::REPLY_CAP_BYTES) bound
//! and the `poll_pty -> write_replies` flush path apply unchanged.
//!
//! Bounds (threat T-01, fail-closed): overlap 512 B, DECRQM param run 64 B,
//! at most 16 modes and 32 queries per `handle_pty_bytes` call, XTGETTCAP
//! payload 512 B with at most 8 names, reply per query < 1 KiB. Malformed
//! queries yield silence (CSI families) or `DCS 0 + r ST` (XTGETTCAP), never
//! unbounded growth and never a panic.

#![forbid(unsafe_code)]

use bitty_term_state::State;
use bitty_vt::UnrecognizedSequence;

/// Bytes of PTY history retained across [`Runtime::handle_pty_bytes`](crate::Runtime::handle_pty_bytes)
/// calls so a query split over two PTY reads is still recognized.
///
/// Queries are single short writes (tens of bytes); 512 B covers any
/// DECRQM/XTGETTCAP/secondary-DA shape with wide margin while staying
/// negligible per runtime.
pub(crate) const QUERY_OVERLAP_MAX: usize = 512;

/// Maximum DECRQM parameter characters scanned after `CSI[?]`.
const DECRQM_PARAM_MAX: usize = 64;

/// Maximum mode numbers honored per DECRQM query.
const DECRQM_MODES_MAX: usize = 16;

/// Maximum DECRQM/secondary-DA matches answered per `handle_pty_bytes` call.
const QUERY_MATCHES_MAX: usize = 32;

/// Maximum XTGETTCAP payload bytes between `DCS + q` and `ST`.
const TCAP_PAYLOAD_MAX: usize = 512;

/// Maximum capability names honored per XTGETTCAP query.
const TCAP_NAMES_MAX: usize = 8;

/// Maximum raw XTGETTCAP matches scanned per call.
const TCAP_MATCHES_MAX: usize = 8;

/// Maximum hex digits accepted per capability name token.
const TCAP_TOKEN_MAX: usize = 64;

/// Terminal name reported for XTGETTCAP `TN`/`name`: the `TERM` value children
/// actually observe, so the answer cannot drift from the environment.
fn term_name() -> &'static str {
    bitty_pty::DEFAULT_TERM
}

/// Numeric secondary-DA firmware version derived from this crate's semver.
///
/// Same positional scheme as the reference (`major*10000 + minor*100 +
/// patch`, pre-release suffix stripped) so newer releases report higher
/// numbers; bitty `0.0.1` reports `1`.
fn firmware_version() -> usize {
    let mut version = env!("CARGO_PKG_VERSION");
    if let Some(sep) = version.rfind('-') {
        version = &version[..sep];
    }
    let mut number = 0usize;
    for (i, part) in version.split('.').rev().enumerate() {
        number += usize::pow(100, i as u32).saturating_mul(part.parse::<usize>().unwrap_or(0));
    }
    number
}

/// Primary-DA reply (`ESC[?6c`, VT102 baseline): also used for legacy DECID.
///
/// Byte-identical to what `bitty-term-state` synthesizes for `CSI c`, so
/// `ESC Z` and `CSI c` are indistinguishable downstream.
#[must_use]
pub(crate) fn primary_da_reply() -> Vec<u8> {
    b"\x1b[?6c".to_vec()
}

/// Secondary-DA reply (`CSI > 0 ; <ver> ; 1 c`).
#[must_use]
pub(crate) fn secondary_da_reply() -> Vec<u8> {
    format!("\x1b[>0;{};1c", firmware_version()).into_bytes()
}

/// XTVERSION reply (`DCS > | Bitty <ver> ST`, 7-bit `ESC \` terminator).
///
/// Names bitty truthfully: never `XTerm`, so hosts cannot enable xterm-only
/// workarounds against us.
#[must_use]
pub(crate) fn xterm_version_reply() -> Vec<u8> {
    format!("\x1bP>|Bitty {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
}

/// DECRPM reply (`CSI [?] Ps ; Pm $ y`).
#[must_use]
pub(crate) fn decrpm_reply(private: bool, mode: u16, value: u8) -> Vec<u8> {
    if private {
        format!("\x1b[?{mode};{value}$y").into_bytes()
    } else {
        format!("\x1b[{mode};{value}$y").into_bytes()
    }
}

/// DECRQM mode value for one queried mode number against live state.
///
/// `1` set, `2` reset, `0` not recognized (ANSI modes other than 4/20,
/// action-only pseudo-modes such as 1048, and anything unmapped).
/// Maps a set flag to its DECRPM value: `1` set, `2` reset.
fn pm(set: bool) -> u8 {
    if set { 1 } else { 2 }
}

#[must_use]
pub(crate) fn decrqm_value(state: &State, private: bool, mode: u16) -> u8 {
    if !private {
        return match mode {
            4 => pm(state.modes().insert),
            20 => pm(state.modes().line_feed_new_line),
            _ => 0,
        };
    }
    let set = match mode {
        1 => Some(state.modes().application_cursor_keys),
        3 => Some(state.modes().column_132_requested),
        5 => Some(state.modes().reverse_video),
        6 => Some(state.modes().origin),
        7 => Some(state.modes().auto_wrap),
        9 => Some(state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::X10)),
        12 => Some(state.modes().cursor_blinking),
        25 => Some(state.cursor().visible),
        47 | 1047 | 1049 => Some(state.alt_screen_active()),
        1000 => Some(state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::Normal)),
        1002 => Some(state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::Button)),
        1003 => Some(state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::Any)),
        1004 => Some(state.modes().focus_events),
        1005 => Some(
            state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Utf8),
        ),
        1006 => Some(
            state.modes().mouse_coordinate_encoding == Some(bitty_vt::MouseCoordinateEncoding::Sgr),
        ),
        1015 => Some(
            state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Urxvt),
        ),
        2004 => Some(state.modes().bracketed_paste),
        7727 => Some(state.modes().kitty_keyboard != 0),
        _ => None,
    };
    match set {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    }
}

/// One parsed DECRQM query: privacy plus bounded mode list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecrqmQuery {
    /// Whether the query used the `?` private prefix.
    pub private: bool,
    /// Queried mode numbers (at most [`DECRQM_MODES_MAX`]).
    pub modes: Vec<u16>,
}

/// Scan `combined` (overlap ++ new bytes) for fresh DECRQM queries.
///
/// Returns queries whose final `p` lands strictly after `new_start` (queries
/// ending inside the overlap were answered on the earlier call). Shapes are
/// exactly `CSI Ps $ p` / `CSI ? Ps $ p` with 0-64 param characters from
/// `[0-9;]`; anything else is malformed and skipped (fail-closed).
pub(crate) fn find_decrqm(combined: &[u8], new_start: usize) -> Vec<DecrqmQuery> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= combined.len() && out.len() < QUERY_MATCHES_MAX {
        if combined[i] != 0x1b || *combined.get(i + 1).unwrap_or(&0) != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        let mut private = false;
        if *combined.get(j).unwrap_or(&0) == b'?' {
            private = true;
            j += 1;
        }
        let params_start = j;
        while j < combined.len()
            && j - params_start < DECRQM_PARAM_MAX
            && (combined[j].is_ascii_digit() || combined[j] == b';')
        {
            j += 1;
        }
        let params_len = j - params_start;
        if j + 2 <= combined.len()
            && combined[j] == b'$'
            && combined[j + 1] == b'p'
            && params_len <= DECRQM_PARAM_MAX
            && j + 2 > new_start
        {
            let mut modes = Vec::new();
            // Empty params mean Ps 0 (not recognized downstream).
            if params_len == 0 {
                modes.push(0);
            } else {
                for token in combined[params_start..j].split(|b| *b == b';') {
                    if modes.len() >= DECRQM_MODES_MAX {
                        break;
                    }
                    if token.is_empty() {
                        modes.push(0);
                        continue;
                    }
                    if token.len() > 5 {
                        modes.push(0);
                        continue;
                    }
                    let mut value: u32 = 0;
                    for &d in token {
                        value = value.saturating_mul(10).saturating_add(u32::from(d - b'0'));
                    }
                    modes.push(u16::try_from(value).unwrap_or(u16::MAX));
                }
            }
            out.push(DecrqmQuery { private, modes });
            i = j + 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Scan for fresh secondary-DA queries in request form (`CSI > c` with empty
/// or `0` params).
///
/// Echoes of our own reply (`CSI > 0 ; <ver> ; 1 c`) carry extra params and
/// are ignored, which closes the reply-echo loop on echoing PTYs.
pub(crate) fn find_secondary_da(combined: &[u8], new_start: usize) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;
    while i + 4 <= combined.len() && count < QUERY_MATCHES_MAX {
        if combined[i] != 0x1b || *combined.get(i + 1).unwrap_or(&0) != b'[' {
            i += 1;
            continue;
        }
        if *combined.get(i + 2).unwrap_or(&0) != b'>' {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let params_start = j;
        while j < combined.len() && j - params_start < 16 && combined[j].is_ascii_digit() {
            j += 1;
        }
        let params = &combined[params_start..j.min(combined.len())];
        if j < combined.len()
            && combined[j] == b'c'
            && j + 1 > new_start
            && (params.is_empty() || params == b"0")
        {
            count += 1;
            i = j + 1;
            continue;
        }
        i += 1;
    }
    count
}

/// One parsed XTGETTCAP query: raw hex payload between `DCS + q` and `ST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XtgettcapQuery {
    /// Hex-encoded capability names (`;`-separated), at most [`TCAP_PAYLOAD_MAX`] bytes.
    pub payload: Vec<u8>,
}

/// Scan for fresh XTGETTCAP queries (`DCS + q <payload> ST`).
///
/// Accepts 7-bit `ESC \` and 8-bit `9C` terminators. A payload containing a
/// bare `ESC` (not part of the terminator), or no terminator within
/// [`TCAP_PAYLOAD_MAX`] bytes, is malformed/oversized and skipped.
pub(crate) fn find_xtgettcap(combined: &[u8], new_start: usize) -> Vec<XtgettcapQuery> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= combined.len() && out.len() < TCAP_MATCHES_MAX {
        if combined[i] != 0x1b || *combined.get(i + 1).unwrap_or(&0) != b'P' {
            i += 1;
            continue;
        }
        if *combined.get(i + 2).unwrap_or(&0) != b'+' || *combined.get(i + 3).unwrap_or(&0) != b'q'
        {
            i += 1;
            continue;
        }
        let payload_start = i + 4;
        let mut j = payload_start;
        let mut end: Option<(usize, usize)> = None;
        while j < combined.len() && j - payload_start <= TCAP_PAYLOAD_MAX {
            if combined[j] == 0x1b {
                if *combined.get(j + 1).unwrap_or(&0) == b'\\' {
                    end = Some((j, j + 2));
                    break;
                }
                // Bare ESC inside payload: malformed.
                break;
            }
            if combined[j] == 0x9c {
                end = Some((j, j + 1));
                break;
            }
            j += 1;
        }
        if let Some((term_start, term_end)) = end {
            if term_end > new_start {
                out.push(XtgettcapQuery {
                    payload: combined[payload_start..term_start].to_vec(),
                });
            }
            i = term_end;
            continue;
        }
        i += 1;
    }
    out
}

/// Decode one even-length ASCII hex token to bytes; `None` when malformed.
fn decode_hex(token: &[u8]) -> Option<Vec<u8>> {
    if token.is_empty() || token.len() > TCAP_TOKEN_MAX || token.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(token.len() / 2);
    for pair in token.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Uppercase hex encoding of raw bytes (reply values per ctlseqs).
fn encode_hex(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0F) as usize]);
    }
    out
}

/// Truthful value for a decoded XTGETTCAP capability name; `None` when bitty
/// does not implement it (caller ends processing with a negative reply).
fn tcap_value(name: &[u8]) -> Option<&'static str> {
    match name {
        b"TN" | b"name" => Some(term_name()),
        b"Co" | b"colors" => Some("256"),
        b"RGB" => Some("8"),
        _ => None,
    }
}

/// XTGETTCAP reply for one query payload.
///
/// `DCS 1 + r name=value[;...] ST` over the valid prefix; `DCS 0 + r ST`
/// when the first name is unknown or malformed (an invalid name ends
/// processing, matching xterm). Names echo in their original hex; values are
/// hex-encoded.
#[must_use]
pub(crate) fn xtgettcap_reply(payload: &[u8]) -> Vec<u8> {
    let mut reply: Vec<u8> = b"\x1bP1+r".to_vec();
    let mut answered = 0usize;
    for token in payload.split(|b| *b == b';').take(TCAP_NAMES_MAX + 1) {
        if answered >= TCAP_NAMES_MAX {
            break;
        }
        let Some(name) = decode_hex(token) else {
            break;
        };
        let Some(value) = tcap_value(&name) else {
            break;
        };
        if answered > 0 {
            reply.push(b';');
        }
        reply.extend_from_slice(token);
        reply.push(b'=');
        reply.extend_from_slice(&encode_hex(value.as_bytes()));
        answered += 1;
    }
    if answered == 0 {
        return b"\x1bP0+r\x1b\\".to_vec();
    }
    reply.extend_from_slice(b"\x1b\\");
    reply
}

// Precise [`UnrecognizedSequence`] triggers (kind/final/intermediates).
// The parser maps every query below to `Unknown` today; matching the full
// triple keeps unrelated sequences silent.

/// `CSI > c` / `CSI > 0 c` (Secondary DA request form).
#[must_use]
pub(crate) fn is_secondary_da(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Csi
        && seq.final_byte == b'c'
        && seq.intermediates == [b'>', 0]
}

/// `CSI > q` (XTVERSION request).
#[must_use]
pub(crate) fn is_xterm_version(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Csi
        && seq.final_byte == b'q'
        && seq.intermediates == [b'>', 0]
}

/// `ESC Z` (legacy DECID).
#[must_use]
pub(crate) fn is_legacy_decid(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Esc && seq.final_byte == b'Z' && seq.intermediates == [0, 0]
}

/// `CSI Ps $ p` (ANSI DECRQM).
#[must_use]
pub(crate) fn is_decrqm_ansi(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Csi
        && seq.final_byte == b'p'
        && seq.intermediates == [b'$', 0]
}

/// `CSI ? Ps $ p` (private DECRQM).
#[must_use]
pub(crate) fn is_decrqm_private(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Csi && seq.final_byte == b'p' && seq.intermediates == *b"?$"
}

/// `DCS + q ... ST` (XTGETTCAP).
#[must_use]
pub(crate) fn is_xtgettcap(seq: &UnrecognizedSequence) -> bool {
    seq.kind == bitty_vt::SequenceKind::Dcs
        && seq.final_byte == b'q'
        && seq.intermediates == [b'+', 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_da_reply_carries_numeric_version() {
        let reply = secondary_da_reply();
        assert!(reply.starts_with(b"\x1b[>0;"), "got {reply:?}");
        assert!(reply.ends_with(b";1c"), "got {reply:?}");
    }

    #[test]
    fn xterm_version_reply_names_bitty_not_xterm() {
        let reply = xterm_version_reply();
        assert!(reply.starts_with(b"\x1bP>|"), "got {reply:?}");
        assert!(reply.ends_with(b"\x1b\\"), "got {reply:?}");
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("Bitty"), "got {text}");
        assert!(!text.contains("XTerm"), "must not claim xterm, got {text}");
    }

    #[test]
    fn decrpm_reply_shapes_match_spec() {
        assert_eq!(decrpm_reply(false, 4, 1), b"\x1b[4;1$y");
        assert_eq!(decrpm_reply(true, 2004, 2), b"\x1b[?2004;2$y");
        assert_eq!(decrpm_reply(true, 9999, 0), b"\x1b[?9999;0$y");
    }

    #[test]
    fn decrqm_defaults_report_reset_and_autowrap_set() {
        let state = State::new();
        assert_eq!(decrqm_value(&state, false, 4), 2);
        assert_eq!(decrqm_value(&state, false, 20), 2);
        assert_eq!(decrqm_value(&state, false, 12), 0);
        assert_eq!(decrqm_value(&state, true, 7), 1, "DECAWM defaults on");
        assert_eq!(
            decrqm_value(&state, true, 25),
            1,
            "cursor visible by default"
        );
        assert_eq!(decrqm_value(&state, true, 2004), 2);
        assert_eq!(
            decrqm_value(&state, true, 1048),
            0,
            "action-only pseudo-mode"
        );
        assert_eq!(
            decrqm_value(&state, true, 2026),
            0,
            "unimplemented sync mode"
        );
    }

    #[test]
    fn find_decrqm_parses_private_and_ansi_forms() {
        let fresh = find_decrqm(b"\x1b[?2004$p", 0);
        assert_eq!(
            fresh,
            vec![DecrqmQuery {
                private: true,
                modes: vec![2004]
            }]
        );
        let fresh = find_decrqm(b"\x1b[4$p", 0);
        assert_eq!(
            fresh,
            vec![DecrqmQuery {
                private: false,
                modes: vec![4]
            }]
        );
        // Overlap region matches are stale (answered on the earlier call).
        let combined = b"\x1b[?7$p\x1b[?25$p";
        assert!(find_decrqm(combined, 7).iter().all(|q| q.modes == vec![25]));
        assert_eq!(find_decrqm(combined, 7).len(), 1);
    }

    #[test]
    fn find_decrqm_rejects_malformed_shapes() {
        assert!(find_decrqm(b"\x1b[?2004p", 0).is_empty(), "missing $");
        assert!(find_decrqm(b"\x1b[?2004$y", 0).is_empty(), "wrong final");
        assert!(
            find_decrqm(b"\x1b[?2 0$p", 0).is_empty(),
            "space fails closed"
        );
    }

    #[test]
    fn find_secondary_da_accepts_request_form_only() {
        assert_eq!(find_secondary_da(b"\x1b[>c", 0), 1);
        assert_eq!(find_secondary_da(b"\x1b[>0c", 0), 1);
        // Echo of our own reply must not retrigger (loop guard).
        assert_eq!(find_secondary_da(b"\x1b[>0;1;1c", 0), 0);
        assert_eq!(find_secondary_da(b"\x1b[>4c", 0), 0);
    }

    #[test]
    fn xtgettcap_answers_true_caps_and_rejects_unknown() {
        // TN -> xterm-256color
        let reply = xtgettcap_reply(b"544E");
        let mut expected: Vec<u8> = b"\x1bP1+r544E=".to_vec();
        expected.extend_from_slice(&encode_hex(b"xterm-256color"));
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(reply, expected);
        // Co -> 256
        assert_eq!(xtgettcap_reply(b"436F"), b"\x1bP1+r436F=323536\x1b\\");
        // Unknown first name -> well-formed negative, never silence.
        assert_eq!(xtgettcap_reply(b"5A5A"), b"\x1bP0+r\x1b\\");
        // Malformed hex -> negative.
        assert_eq!(xtgettcap_reply(b"544"), b"\x1bP0+r\x1b\\");
        assert_eq!(xtgettcap_reply(b""), b"\x1bP0+r\x1b\\");
        // Valid prefix then invalid: valid pairs kept, processing ends.
        assert_eq!(xtgettcap_reply(b"544E;5A5A"), expected);
    }

    #[test]
    fn xtgettcap_multi_name_replies_all_valid() {
        // TN;Co in one query.
        let reply = xtgettcap_reply(b"544E;436F");
        let mut expected: Vec<u8> = b"\x1bP1+r544E=".to_vec();
        expected.extend_from_slice(&encode_hex(b"xterm-256color"));
        expected.extend_from_slice(b";436F=323536\x1b\\");
        assert_eq!(reply, expected);
    }

    #[test]
    fn find_xtgettcap_finds_st_terminated_query() {
        let bytes = b"\x1bP+q544E\x1b\\";
        let found = find_xtgettcap(bytes, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payload, b"544E");
        // Stale (fully in overlap) matches are skipped.
        assert!(find_xtgettcap(bytes, bytes.len()).is_empty());
        // Unterminated DCS fails closed.
        assert!(find_xtgettcap(b"\x1bP+q544E", 0).is_empty());
    }
}
