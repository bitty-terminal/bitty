//! Kitty `APC G` control parsing, base64 unwrap, and `m=` reassembly (CTX-0256).
//!
//! The VT state machine (`vte` 0.15) leaves `SOS/PM/APC` strings inert, so
//! [`crate::Parser`] pre-scans `APC` (`ESC _ ... ST`) and feeds complete raw
//! buffers here. This module parses the `G` graphics command, base64-decodes
//! with a fail-closed alphabet check, reassembles chunked `m=1`/`m=0`
//! streams, and hands assembled decoded bytes to the caller for routing to
//! `kitty_transmit`/`kitty_display_image`.
//!
//! # Wire shape
//!
//! `ESC _ G <control> ; <base64> ST` where `ST` is `ESC \` or `BEL` (C1 `ST`
//! `0x9C` also terminates inside the scanner). C1 `APC` (`0x9F`) is
//! intentionally not an introducer: it overlaps UTF-8 continuation bytes and
//! kitty/chafa always emit `ESC _`. `<control>` is a comma-separated
//! `key=value` list. Routing needs `f` (format), `s`/`v` (raw dimensions),
//! `a` (display action), `c`/`r` (cell spans), and `m` (more-chunks). All
//! other keys are ignored (future-proof).
//!
//! # Bounds (reuse, not reinvention)
//!
//! - Ledger cap [`KITTY_APC_LEDGER_CAP`] mirrors
//!   `bitty-rich::kitty::KITTY_LEDGER_MAX_BYTES` (320 MiB, Ghostty
//!   `total_limit` parity). Raw `APC` bytes and accumulated base64-encoded
//!   bytes are both capped here, checked with `checked_add` **before** any
//!   buffer growth, so hostile input can never force an over-cap allocation.
//! - Oversize `s`/`v` claims for raw formats are rejected against
//!   [`KITTY_APC_DECODE_MAX_DIMENSION`]/[`KITTY_APC_DECODE_MAX_PIXELS`]/
//!   [`KITTY_APC_DECODE_MAX_BYTES`], mirroring
//!   `bitty-rich::kitty_decode` caps, before any pixel buffer could exist.
//!   PNG (`f=100`) ignores `s`/`v` (`IHDR` governs); unknown `f` skips the
//!   claim check and lets the decoder fail closed.
//!
//! # Fail-closed behavior
//!
//! Every rejection warns via `eprintln!` (diagnostic only, no state change)
//! and yields no completed transmission: bad base64 alphabet, malformed
//! control, missing `f`, oversize claim, or ledger-cap overflow all store
//! nothing and paint nothing. The caller (`Parser`) emits no action on
//! rejection. Chunked streams drop only the offending stream on oversize,
//! mirroring `KittyGraphicsStub` semantics.

/// Ledger cap reused from `bitty-rich::kitty::KITTY_LEDGER_MAX_BYTES`.
///
/// Caps raw `APC` bytes per sequence and total base64-encoded bytes held
/// across an `m=` chunked stream (stored + in-flight parity). Conservative:
/// base64 expands ~33%, so capping encoded bytes here guarantees decoded
/// bytes stay under the same ceiling.
pub const KITTY_APC_LEDGER_CAP: usize = 320 * 1000 * 1000;

/// Side cap mirroring `bitty-rich::kitty_decode::KITTY_DECODE_MAX_DIMENSION`.
pub const KITTY_APC_DECODE_MAX_DIMENSION: u32 = 8192;

/// Area cap mirroring `bitty-rich::kitty_decode::KITTY_DECODE_MAX_PIXELS`.
pub const KITTY_APC_DECODE_MAX_PIXELS: u64 = 4096 * 4096;

/// Byte cap mirroring `bitty-rich::kitty_decode::KITTY_DECODE_MAX_BYTES`.
pub const KITTY_APC_DECODE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Parsed `G` control parameters needed for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyApcParams {
    /// Wire `f=` format value. Required; absent is rejected.
    pub format_f: u32,
    /// Wire `s=` width (`None` when absent).
    pub width_s: Option<u32>,
    /// Wire `v=` height (`None` when absent).
    pub height_v: Option<u32>,
    /// Wire `a=` action (`None` when absent means transmit-and-display).
    pub action_a: Option<char>,
    /// Wire `c=` columns (`0` when absent).
    pub cols_c: u16,
    /// Wire `r=` rows (`0` when absent).
    pub rows_r: u16,
    /// Wire `m=` more-chunks (`false` when absent, i.e. single-shot/final).
    pub more: bool,
}

/// Why an `APC G` buffer was rejected (fail-closed, warns, emits nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyApcReject {
    /// Buffer does not start with `G` (other APC command; inert, silent).
    NotGraphics,
    /// Control section malformed (bad `key=value` shape or bad numeric).
    MalformedControl,
    /// Required `f=` format value absent.
    MissingFormat,
    /// `a=` value longer than one character.
    BadAction,
    /// `m=` value other than `0`/`1`.
    BadMore,
    /// Base64 payload uses a non-alphabet byte or bad padding/length.
    BadBase64,
    /// Raw `s`/`v` claim exceeds decode side/area/byte caps.
    OversizeClaim,
    /// Growth would exceed the ledger cap.
    Oversize,
    /// Continuation (`m=` present, no `f=`) with no open stream.
    Orphan,
}

impl std::fmt::Display for KittyApcReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGraphics => write!(f, "not a kitty graphics command"),
            Self::MalformedControl => write!(f, "malformed kitty control parameters"),
            Self::MissingFormat => write!(f, "kitty transmission missing f= format"),
            Self::BadAction => write!(f, "malformed kitty a= action"),
            Self::BadMore => write!(f, "malformed kitty m= flag"),
            Self::BadBase64 => write!(f, "invalid kitty base64 payload"),
            Self::OversizeClaim => write!(f, "kitty s/v claim exceeds decode caps"),
            Self::Oversize => write!(f, "kitty stream exceeds ledger cap"),
            Self::Orphan => write!(f, "kitty chunk without an open stream"),
        }
    }
}

/// Completed transmission: routing params plus assembled decoded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCompleted {
    /// Wire `f=` format value.
    pub format_f: u32,
    /// Wire `s=` width (`None` when absent).
    pub width_s: Option<u32>,
    /// Wire `v=` height (`None` when absent).
    pub height_v: Option<u32>,
    /// Wire `a=` action (`None` when absent).
    pub action_a: Option<char>,
    /// Wire `c=` columns (`0` when absent).
    pub cols_c: u16,
    /// Wire `r=` rows (`0` when absent).
    pub rows_r: u16,
    /// Assembled base64-decoded bytes across `m=` chunks.
    pub payload: Box<[u8]>,
}

/// Outcome of feeding one `APC G` buffer to the assembler.
#[derive(Debug)]
pub enum KittyFeedOutcome {
    /// `m=1` buffered; more chunks expected. No action emitted.
    NeedMore {
        /// Total base64-encoded bytes held in flight.
        buffered_encoded: usize,
    },
    /// `m=0` completed the stream (or lone single-shot). Emit one action.
    Completed(KittyCompleted),
    /// Rejected fail-closed (warned, stored nothing). No action emitted.
    Rejected(KittyApcReject),
}

/// In-flight `m=1` stream: first-chunk params plus accumulated base64.
#[derive(Debug, Clone)]
struct PendingKitty {
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    action_a: Option<char>,
    cols_c: u16,
    rows_r: u16,
    encoded: Vec<u8>,
}

/// `m=` chunk reassembler with ledger-cap enforcement (CTX-0256).
///
/// Mirrors `KittyGraphicsStub` accumulation semantics but over base64-encoded
/// bytes (the wire splits base64 text, not binary): encoded chunks are
/// concatenated, then decoded once at the final `m=0`.
#[derive(Debug, Clone)]
pub struct KittyApcAssembler {
    pending: Option<PendingKitty>,
    ledger_cap: usize,
}

impl KittyApcAssembler {
    /// Empty assembler with the default ledger cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: None,
            ledger_cap: KITTY_APC_LEDGER_CAP,
        }
    }

    /// Empty assembler with a custom ledger cap (tests exercise cap behavior
    /// without allocating hundreds of megabytes).
    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self {
            pending: None,
            ledger_cap,
        }
    }

    /// Ledger cap (max encoded bytes held in flight).
    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.ledger_cap
    }

    /// Whether an `m=1` stream is open.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Base64-encoded bytes currently buffered (`0` when idle).
    #[must_use]
    pub fn pending_encoded_len(&self) -> usize {
        self.pending.as_ref().map_or(0, |p| p.encoded.len())
    }

    /// Abandons the open stream without emitting. Returns `true` when a
    /// stream was actually discarded.
    pub fn abort(&mut self) -> bool {
        self.pending.take().is_some()
    }

    /// Feeds one complete `APC` raw buffer (after `ESC _`, before `ST`).
    ///
    /// Non-`G` buffers are inert (`Rejected(NotGraphics)`, silent: the
    /// caller emits nothing and does not warn, preserving pre-existing APC
    /// inert behavior for other commands).
    pub fn feed(&mut self, raw: &[u8]) -> KittyFeedOutcome {
        let Some(after_g) = raw.strip_prefix(b"G") else {
            return KittyFeedOutcome::Rejected(KittyApcReject::NotGraphics);
        };
        let (control, payload_b64) = match after_g.iter().position(|&b| b == b';') {
            Some(semi) => (&after_g[..semi], &after_g[semi + 1..]),
            None => (after_g, &[][..]),
        };
        if self.pending.is_none() {
            self.feed_new(control, payload_b64)
        } else {
            self.feed_continuation(control, payload_b64)
        }
    }

    /// Handles an `APC G` buffer with no open stream: new begin (`m=1`) or
    /// lone single-shot (`m=0`/absent).
    fn feed_new(&mut self, control: &[u8], payload_b64: &[u8]) -> KittyFeedOutcome {
        let params = match parse_control(control) {
            Ok(params) => params,
            Err(reason) => {
                warn_reject(reason, "control");
                return KittyFeedOutcome::Rejected(reason);
            }
        };
        if params.more {
            if payload_b64.len() > self.ledger_cap {
                warn_reject(KittyApcReject::Oversize, "first chunk");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            let buffered = payload_b64.len();
            self.pending = Some(PendingKitty {
                format_f: params.format_f,
                width_s: params.width_s,
                height_v: params.height_v,
                action_a: params.action_a,
                cols_c: params.cols_c,
                rows_r: params.rows_r,
                encoded: payload_b64.to_vec(),
            });
            KittyFeedOutcome::NeedMore {
                buffered_encoded: buffered,
            }
        } else {
            // Lone single-shot: decode now, enforce caps, validate claim.
            if payload_b64.len() > self.ledger_cap {
                warn_reject(KittyApcReject::Oversize, "single-shot");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            let decoded = match base64_decode_standard(payload_b64) {
                Ok(bytes) => bytes,
                Err(_) => {
                    warn_reject(KittyApcReject::BadBase64, "single-shot");
                    return KittyFeedOutcome::Rejected(KittyApcReject::BadBase64);
                }
            };
            if decoded.len() > self.ledger_cap {
                warn_reject(KittyApcReject::Oversize, "decoded single-shot");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            if let Err(reason) =
                validate_raw_claim(params.format_f, params.width_s, params.height_v)
            {
                warn_reject(reason, "claim");
                return KittyFeedOutcome::Rejected(reason);
            }
            KittyFeedOutcome::Completed(KittyCompleted {
                format_f: params.format_f,
                width_s: params.width_s,
                height_v: params.height_v,
                action_a: params.action_a,
                cols_c: params.cols_c,
                rows_r: params.rows_r,
                payload: decoded.into_boxed_slice(),
            })
        }
    }

    /// Handles an `APC G` buffer with an open stream: continuation (`m=1`)
    /// appends, final (`m=0`/absent-with-pending) assembles and decodes.
    ///
    /// Continuation control params other than `m` are ignored (the first
    /// chunk is authoritative). A missing `m` while a stream is open keeps
    /// the open stream and drops the newcomer (fail-closed, no merge).
    fn feed_continuation(&mut self, control: &[u8], payload_b64: &[u8]) -> KittyFeedOutcome {
        let more = match more_flag(control) {
            Ok(more) => more,
            Err(reason) => {
                warn_reject(reason, "continuation m=");
                return KittyFeedOutcome::Rejected(reason);
            }
        };
        let Some(more) = more else {
            // No `m` while a stream is open: keep the open stream, drop the
            // newcomer (it is likely an unrelated single-shot that must not
            // corrupt the in-flight assembly).
            warn_reject(KittyApcReject::Orphan, "missing m= with open stream");
            return KittyFeedOutcome::Rejected(KittyApcReject::Orphan);
        };
        if more {
            let buffered = self.pending_encoded_len();
            let needed = buffered.saturating_add(payload_b64.len());
            if needed > self.ledger_cap {
                self.pending = None;
                warn_reject(KittyApcReject::Oversize, "chunk growth");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            let pending = self.pending.as_mut().expect("open stream");
            pending.encoded.extend_from_slice(payload_b64);
            KittyFeedOutcome::NeedMore {
                buffered_encoded: pending.encoded.len(),
            }
        } else {
            let buffered = self.pending_encoded_len();
            let needed = buffered.saturating_add(payload_b64.len());
            if needed > self.ledger_cap {
                self.pending = None;
                warn_reject(KittyApcReject::Oversize, "final growth");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            let pending = self.pending.take().expect("open stream");
            let mut encoded = pending.encoded;
            encoded.extend_from_slice(payload_b64);
            let decoded = match base64_decode_standard(&encoded) {
                Ok(bytes) => bytes,
                Err(_) => {
                    warn_reject(KittyApcReject::BadBase64, "assembled");
                    return KittyFeedOutcome::Rejected(KittyApcReject::BadBase64);
                }
            };
            if decoded.len() > self.ledger_cap {
                warn_reject(KittyApcReject::Oversize, "decoded assembly");
                return KittyFeedOutcome::Rejected(KittyApcReject::Oversize);
            }
            if let Err(reason) =
                validate_raw_claim(pending.format_f, pending.width_s, pending.height_v)
            {
                warn_reject(reason, "assembled claim");
                return KittyFeedOutcome::Rejected(reason);
            }
            KittyFeedOutcome::Completed(KittyCompleted {
                format_f: pending.format_f,
                width_s: pending.width_s,
                height_v: pending.height_v,
                action_a: pending.action_a,
                cols_c: pending.cols_c,
                rows_r: pending.rows_r,
                payload: decoded.into_boxed_slice(),
            })
        }
    }
}

impl Default for KittyApcAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Diagnostic warn on fail-closed rejection (no state change, no paint).
fn warn_reject(reason: KittyApcReject, context: &str) {
    if reason == KittyApcReject::NotGraphics {
        return;
    }
    eprintln!("bitty: rejecting kitty APC G ({reason} in {context}): stored nothing");
}

/// Parses the `G` control section (`key=value` pairs separated by `,`).
///
/// Unknown keys are ignored. Known keys are strictly validated: any malformed
/// known value rejects the whole transmission fail-closed.
fn parse_control(control: &[u8]) -> Result<KittyApcParams, KittyApcReject> {
    if control.is_empty() {
        return Err(KittyApcReject::MissingFormat);
    }
    let mut format_f: Option<u32> = None;
    let mut width_s: Option<u32> = None;
    let mut height_v: Option<u32> = None;
    let mut action_a: Option<char> = None;
    let mut cols_c: u16 = 0;
    let mut rows_r: u16 = 0;
    let mut more = false;
    for piece in control.split(|&b| b == b',') {
        if piece.is_empty() {
            continue;
        }
        let eq = piece
            .iter()
            .position(|&b| b == b'=')
            .ok_or(KittyApcReject::MalformedControl)?;
        let (key, value) = (&piece[..eq], &piece[eq + 1..]);
        if key.len() != 1 {
            // Multi-letter keys are unknown future extensions: ignore.
            continue;
        }
        match key[0] {
            b'f' => {
                format_f = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b's' => {
                width_s = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b'v' => {
                height_v = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b'a' => {
                action_a = match value {
                    [] => None,
                    [single] => Some(char::from(*single)),
                    _ => return Err(KittyApcReject::BadAction),
                };
            }
            b'c' => {
                cols_c = parse_u16(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'r' => {
                rows_r = parse_u16(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'm' => {
                more = match value {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(KittyApcReject::BadMore),
                };
            }
            _ => {
                // Unknown single-letter keys (i, p, q, d, e, t, o, X, Y, w,
                // h, x, y, z, C, R, ...): ignored for transmit/display.
            }
        }
    }
    let Some(format_f) = format_f else {
        return Err(KittyApcReject::MissingFormat);
    };
    Ok(KittyApcParams {
        format_f,
        width_s,
        height_v,
        action_a,
        cols_c,
        rows_r,
        more,
    })
}

/// Extracts the `m=` flag from a continuation buffer (`None` when absent).
fn more_flag(control: &[u8]) -> Result<Option<bool>, KittyApcReject> {
    for piece in control.split(|&b| b == b',') {
        if piece.is_empty() {
            continue;
        }
        let Some(eq) = piece.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&piece[..eq], &piece[eq + 1..]);
        if key == b"m" {
            return match value {
                b"0" => Ok(Some(false)),
                b"1" => Ok(Some(true)),
                _ => Err(KittyApcReject::BadMore),
            };
        }
    }
    Ok(None)
}

/// Rejects oversize raw `s`/`v` claims before any pixel buffer could exist.
///
/// Only raw formats (`f=24` RGB, `f=32` RGBA) with both dimensions present
/// are checked. PNG (`f=100`) ignores `s`/`v` (`IHDR` governs); unknown
/// formats skip the check and let the decoder fail closed. Zero/missing
/// dimensions pass through to the decoder (`ZeroDimension`/`MissingDimensions`).
fn validate_raw_claim(
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
) -> Result<(), KittyApcReject> {
    let channels: usize = match format_f {
        24 => 3,
        32 => 4,
        _ => return Ok(()),
    };
    let (Some(w), Some(h)) = (width_s, height_v) else {
        return Ok(());
    };
    if w == 0 || h == 0 {
        return Ok(());
    }
    if w > KITTY_APC_DECODE_MAX_DIMENSION || h > KITTY_APC_DECODE_MAX_DIMENSION {
        return Err(KittyApcReject::OversizeClaim);
    }
    let pixels = u64::from(w) * u64::from(h);
    if pixels > KITTY_APC_DECODE_MAX_PIXELS {
        return Err(KittyApcReject::OversizeClaim);
    }
    (pixels as usize)
        .checked_mul(channels)
        .filter(|&n| n <= KITTY_APC_DECODE_MAX_BYTES)
        .map(|_| ())
        .ok_or(KittyApcReject::OversizeClaim)
}

/// Strict ASCII decimal `u32` (no sign, no whitespace, no empty).
fn parse_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() || value.len() > 10 {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

/// Strict ASCII decimal `u16` (no sign, no whitespace, no empty).
fn parse_u16(value: &[u8]) -> Option<u16> {
    if value.is_empty() || value.len() > 5 {
        return None;
    }
    let mut acc: u16 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u16::from(b - b'0'))?;
    }
    Some(acc)
}

/// Minimal standard-base64 decoder (RFC 4648 §4, `+/` with `=` padding).
///
/// Mirrors `bitty-runtime` OSC 52 `base64_decode_standard` without a new
/// dependency: accepts padded and unpadded input; rejects non-alphabet
/// bytes, misplaced padding, and lengths congruent to 1 mod 4. Empty input
/// decodes to empty. Time O(n), space O(n) in the input length.
fn base64_decode_standard(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    fn sextet(byte: u8) -> Result<u32, &'static str> {
        match byte {
            b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
            b'a'..=b'z' => Ok(u32::from(byte - b'a' + 26)),
            b'0'..=b'9' => Ok(u32::from(byte - b'0' + 52)),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid base64 character"),
        }
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if input.len() % 4 == 1 {
        return Err("invalid base64 length");
    }
    let mut pad = 0_usize;
    for &byte in input.iter().rev() {
        if byte == b'=' {
            pad += 1;
        } else {
            break;
        }
    }
    if pad > 2 {
        return Err("invalid base64 padding");
    }
    let body_len = input.len() - pad;
    if input[..body_len].contains(&b'=') {
        return Err("misplaced base64 padding");
    }
    if pad == 1 && body_len % 4 != 3 {
        return Err("invalid base64 padding");
    }
    if pad == 2 && body_len % 4 != 2 {
        return Err("invalid base64 padding");
    }
    let body = &input[..body_len];
    let (full, tail) = body.split_at(body.len() / 4 * 4);
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    for chunk in full.chunks_exact(4) {
        let triple = (sextet(chunk[0])? << 18)
            | (sextet(chunk[1])? << 12)
            | (sextet(chunk[2])? << 6)
            | sextet(chunk[3])?;
        out.push((triple >> 16) as u8);
        out.push((triple >> 8) as u8);
        out.push(triple as u8);
    }
    match tail.len() {
        0 => {}
        2 => {
            let bits = (sextet(tail[0])? << 18) | (sextet(tail[1])? << 12);
            out.push((bits >> 16) as u8);
        }
        3 => {
            let bits =
                (sextet(tail[0])? << 18) | (sextet(tail[1])? << 12) | (sextet(tail[2])? << 6);
            out.push((bits >> 16) as u8);
            out.push((bits >> 8) as u8);
        }
        _ => return Err("invalid base64 length"),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(raw: &[u8]) -> KittyCompleted {
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(raw) {
            KittyFeedOutcome::Completed(done) => done,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn control_parses_routing_fields() {
        // 2x2 RGBA red (16 bytes) -> 24-char base64.
        let raw = b"Gf=32,s=2,v=2,a=T,c=2,r=2,m=0;/wAA//8AAP//AAD//wAA/w==";
        let done = completed(raw);
        assert_eq!(done.format_f, 32);
        assert_eq!(done.width_s, Some(2));
        assert_eq!(done.height_v, Some(2));
        assert_eq!(done.action_a, Some('T'));
        assert_eq!(done.cols_c, 2);
        assert_eq!(done.rows_r, 2);
        assert_eq!(done.payload.len(), 16);
        assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(4));
    }

    #[test]
    fn absent_action_and_spans_default() {
        let done = completed(b"Gf=100,m=0;aGk=");
        assert_eq!(done.format_f, 100);
        assert_eq!(done.action_a, None);
        assert_eq!(done.cols_c, 0);
        assert_eq!(done.rows_r, 0);
        assert_eq!(&*done.payload, b"hi");
    }

    #[test]
    fn unknown_keys_ignored() {
        let done = completed(b"Gf=32,s=1,v=1,i=7,p=1,q=2,X=0,Y=0,z=5,m=0;/wAA/w==");
        assert_eq!(done.format_f, 32);
        assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn non_g_is_inert_not_graphics() {
        let mut assembler = KittyApcAssembler::new();
        for raw in [b"Thello".as_slice(), b"".as_slice(), b" f=32".as_slice()] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(KittyApcReject::NotGraphics) => {}
                other => panic!("expected NotGraphics for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn malformed_control_rejected() {
        let mut assembler = KittyApcAssembler::new();
        for raw in [
            b"Gf".as_slice(),
            b"Gf=abc".as_slice(),
            b"Gs=2".as_slice(),
            b"Gf=32,a=TT".as_slice(),
            b"Gf=32,m=2".as_slice(),
            b"G".as_slice(),
        ] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(_) => {}
                other => panic!("expected Rejected for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn bad_base64_rejected() {
        let mut assembler = KittyApcAssembler::new();
        // `!` is outside the standard alphabet; `=` mid-body is misplaced.
        for raw in [
            b"Gf=32,s=1,v=1,m=0;!!!!".as_slice(),
            b"Gf=32,s=1,v=1,m=0;/w=A/w==".as_slice(),
            b"Gf=32,s=1,v=1,m=0;abcde".as_slice(),
        ] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(KittyApcReject::BadBase64) => {}
                other => panic!("expected BadBase64 for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn oversize_claim_rejected_before_emit() {
        let mut assembler = KittyApcAssembler::new();
        // 9000px side exceeds 8192; tiny payload proves no large alloc.
        match assembler.feed(b"Gf=32,s=9000,v=1,m=0;AA==") {
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim) => {}
            other => panic!("expected OversizeClaim, got {other:?}"),
        }
        // 5000x5000 area exceeds 4096^2.
        match assembler.feed(b"Gf=32,s=5000,v=5000,m=0;AA==") {
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim) => {}
            other => panic!("expected OversizeClaim, got {other:?}"),
        }
        // PNG ignores s/v: same claim passes the claim gate (decodes empty
        // PNG file bytes, which are not a valid PNG, but the claim itself
        // must not reject; the decoder fails closed downstream).
        match assembler.feed(b"Gf=100,s=9000,v=9000,m=0;AA==") {
            KittyFeedOutcome::Completed(_) | KittyFeedOutcome::Rejected(_) => {}
            KittyFeedOutcome::NeedMore { .. } => panic!("unexpected NeedMore"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_reassembly_is_exact() {
        let mut assembler = KittyApcAssembler::new();
        // Split the 24-char red_2x2 base64 across three APCs.
        let full = b"/wAA//8AAP//AAD//wAA/w==";
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 8,
            } => {}
            other => panic!("expected NeedMore(8), got {other:?}"),
        }
        assert!(assembler.has_pending());
        match assembler.feed(b"Gm=1;AP//AAD/") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 16,
            } => {}
            other => panic!("expected NeedMore(16), got {other:?}"),
        }
        match assembler.feed(b"Gm=0;/wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.format_f, 32);
                assert_eq!(done.width_s, Some(2));
                assert_eq!(done.height_v, Some(2));
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(4));
                let _ = full;
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_empty_edge_chunks_assemble_chafa_shape() {
        // Real `chafa --format kitty` shape: params-only first `m=1` (empty
        // payload) and params-only final `m=0` (no `;`), data in middles.
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(b"Gf=32,s=2,v=2,a=T,c=2,r=2,m=1,q=2") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 0,
            } => {}
            other => panic!("expected NeedMore(0), got {other:?}"),
        }
        match assembler.feed(b"Gm=1;/wAA//8AAP//AAD/") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 16,
            } => {}
            other => panic!("expected NeedMore(16), got {other:?}"),
        }
        match assembler.feed(b"Gm=0") {
            KittyFeedOutcome::Completed(done) => {
                // `q=2` ignored; first-chunk params authoritative.
                assert_eq!(done.format_f, 32);
                assert_eq!(done.action_a, Some('T'));
                assert_eq!(done.cols_c, 2);
                assert_eq!(done.rows_r, 2);
                // 16 encoded chars -> 12 decoded bytes (3 red pixels); the
                // tail `/wAA/w==` (4B) is intentionally absent here to prove
                // empty-final assembly is exact for what was sent.
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(3));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_oversize_drops_stream_and_rejects() {
        let mut assembler = KittyApcAssembler::with_ledger_cap(16);
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // 8 + 16 > 16: drops the stream, stores nothing.
        match assembler.feed(b"Gm=1;AAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        // Reusable afterwards: lone single-shot fits.
        match assembler.feed(b"Gf=32,s=1,v=1,m=0;/wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF]);
            }
            other => panic!("expected Completed after drop, got {other:?}"),
        }
    }

    #[test]
    fn missing_m_with_open_stream_keeps_stream() {
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // No `m=`: newcomer dropped, open stream kept.
        match assembler.feed(b"Gf=32,s=1,v=1;/wAA/w==") {
            KittyFeedOutcome::Rejected(KittyApcReject::Orphan) => {}
            other => panic!("expected Orphan, got {other:?}"),
        }
        assert!(assembler.has_pending());
        // True tail still completes exactly.
        match assembler.feed(b"Gm=0;AP//AAD//wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(4));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn caps_mirror_canonical_crates() {
        assert_eq!(KITTY_APC_LEDGER_CAP, 320 * 1000 * 1000);
        assert_eq!(KITTY_APC_DECODE_MAX_DIMENSION, 8192);
        assert_eq!(KITTY_APC_DECODE_MAX_PIXELS, 4096 * 4096);
        assert_eq!(KITTY_APC_DECODE_MAX_BYTES, 64 * 1024 * 1024);
    }
}
