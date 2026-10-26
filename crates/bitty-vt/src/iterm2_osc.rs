//! Bounded iTerm2 inline-image (`OSC 1337 ; File=`) intake (CTX-1590, #2217).
//!
//! The iTerm2 wire shape is `ESC ] 1337 ; File=<attrs>:<base64> ST`, where
//! `<attrs>` is a `;`-separated `key=value` list and `ST` is `ESC \` or
//! `BEL`. The payload is the *entire* file (PNG is the only admitted
//! format here), so a single image can be megabytes of base64.
//!
//! # Why this is pre-scanned instead of dispatched by `vte`
//!
//! `vte` 0.15 collects an OSC string in a growable buffer before
//! `osc_dispatch` runs (the `std` build has no length cap), so a payload
//! bound applied at dispatch time would only run *after* the whole string
//! had been buffered. That is exactly the unbounded allocation the image
//! contract forbids. This module therefore mirrors the Kitty `APC` design
//! ([`crate::kitty_apc`]): [`crate::Parser`] detects `ESC ] 1337 ; File=`
//! itself, streams the body into a bounded buffer, and never hands those
//! bytes to `vte`. Every other OSC still follows the `vte` path unchanged:
//! a candidate whose prefix does not match is replayed byte-for-byte.
//!
//! C1 `OSC` (`0x9D`) is not an introducer for the same reason C1 `APC` is
//! not: it overlaps UTF-8 continuation bytes and `imgcat`/`it2copy` emit
//! `ESC ]`.
//!
//! # Bounds (all enforced before or during buffering)
//!
//! - [`ITERM2_OSC_MAX_WIRE_BYTES`]: base64 ASCII payload cap (IMG-1 parity
//!   with [`crate::KITTY_APC_LEDGER_CAP`]).
//! - [`ITERM2_OSC_MAX_DECODED_BYTES`]: decoded byte cap, checked inside the
//!   shared fail-closed base64 decoder while it appends.
//! - [`ITERM2_DECODE_MAX_DIMENSION`] / [`ITERM2_DECODE_MAX_PIXELS`]: the
//!   PNG `IHDR` dimensions are validated here, before the downstream
//!   decoder allocates anything, mirroring the Kitty `s`/`v` pre-check.
//! - [`ITERM2_OSC_MAX_ATTRIBUTE_BYTES`]: header cap; a longer header is a
//!   malformed request, never a reason to buffer.
//! - [`ITERM2_OSC_MAX_NAME_BYTES`]: decoded `name=` cap (the value is
//!   validated and then dropped: Bitty writes no files).
//! - [`ITERM2_OSC_MAX_CELL_SPAN`]: requested `width`/`height` cell span cap.
//! - [`crate::CONTROL_STRING_MAX_BYTES`]: the parser-wide control-string cap
//!   (CTX-1592) is sized from [`ITERM2_OSC_MAX_BODY_BYTES`] plus the `ESC ]`
//!   introducer and [`ITERM2_OSC_PREFIX`], so the diverted candidate shares
//!   one budget with every string `vte` collects.
//!
//! The body buffer is released (not just cleared) once an OSC terminates,
//! so a parser does not retain image-sized capacity between images. An
//! unterminated OSC can hold at most [`ITERM2_OSC_MAX_BODY_BYTES`] and is
//! otherwise inert, which bounds a stalled writer to one image budget per
//! parser.
//!
//! # Fail-closed behaviour
//!
//! Every rejection warns through the shared rate-limited diagnostic path
//! and yields [`crate::TerminalAction::Iterm2ImageRejected`] (an inert
//! telemetry action: it counts, stores nothing, paints nothing). A
//! rejected image never reaches the store, the placement layer, or the
//! render seam, and no partial payload is ever emitted.
//!
//! # Deliberate refusals (documented, not silently ignored)
//!
//! - `inline=0` and an absent `inline` (iTerm2's download form; the
//!   protocol default is `0`): storing an unreachable image would only
//!   consume the bounded store, and Bitty performs no file writes from
//!   protocol input. Refused, so only an explicit `inline=1` paints.
//! - `preserveAspectRatio=0` (stretch to the requested box): the agreed
//!   placement seam letterboxes a `cols`/`rows` box per the Kitty
//!   contract, so a distortion request cannot be honoured. Refused.
//! - `width`/`height` in `Npx` or `N%`: cell units and `auto` are the only
//!   forms admitted; pixel/percentage sizing needs viewport-dependent
//!   conversion at parse time. Refused.
//! - Every format except PNG: the decoder admits PNG only, so JPEG/GIF
//!   payloads are refused here instead of failing later.
//! - `name` and `size` are validated and dropped: neither reaches a file
//!   write nor the render path.

use crate::action::TerminalAction;
use crate::kitty_apc::{Base64Stream, KittyApcReject};

/// OSC introducer plus the only iTerm2 command Bitty admits.
pub const ITERM2_OSC_PREFIX: &[u8] = b"1337;File=";

/// Accepted base64 payload cap in ASCII bytes (IMG-1 parity).
pub const ITERM2_OSC_MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;

/// Decoded payload cap (`3/4` of the wire cap, plus no slack).
pub const ITERM2_OSC_MAX_DECODED_BYTES: usize = 3 * 1024 * 1024;

/// Header (`key=value;...`) cap in bytes.
pub const ITERM2_OSC_MAX_ATTRIBUTE_BYTES: usize = 4096;

/// Total collected body cap: header, separator, and wire payload.
pub const ITERM2_OSC_MAX_BODY_BYTES: usize =
    ITERM2_OSC_MAX_ATTRIBUTE_BYTES + 1 + ITERM2_OSC_MAX_WIRE_BYTES;

/// Decoded `name=` cap in bytes.
pub const ITERM2_OSC_MAX_NAME_BYTES: usize = 256;

/// Requested cell span cap for `width`/`height`.
pub const ITERM2_OSC_MAX_CELL_SPAN: u16 = 4096;

/// PNG side cap mirroring `bitty-rich::kitty_place::KITTY_DECODE_MAX_DIMENSION`.
pub const ITERM2_DECODE_MAX_DIMENSION: u32 = 8192;

/// PNG area cap mirroring `bitty-rich::kitty_place::KITTY_DECODE_MAX_PIXELS`.
pub const ITERM2_DECODE_MAX_PIXELS: u64 = 4096 * 4096;

/// PNG signature every admitted payload starts with.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Bytes retained to replay a non-matching OSC candidate into `vte`
/// (`ESC ]` plus the longest matched prefix).
const MAX_REPLAY_BYTES: usize = ITERM2_OSC_PREFIX.len() + 2;

/// Smallest body-buffer growth step. Larger bodies grow geometrically from
/// here, so the retained capacity stays at the cap instead of the `2x` a bare
/// `Vec::push` doubling reaches (CTX-1590 review follow-up).
const BODY_RESERVE_MIN: usize = 4 * 1024;

/// Slack the capacity assertions allow above [`ITERM2_OSC_MAX_BODY_BYTES`].
///
/// `Vec::reserve_exact` documents only *at least* the requested capacity; the
/// tighter "never above the cap" bound is an implementation property of std's
/// `RawVec::grow_exact`, which records the request as the exact capacity. One
/// growth step of slack keeps the tests honest about the documented contract
/// while still failing a bare `Vec::push` doubling (~2x the cap).
#[cfg(test)]
pub(crate) const BODY_CAPACITY_SLACK: usize = BODY_RESERVE_MIN;

/// Why one `OSC 1337 ; File=` request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Iterm2Reject {
    /// The OSC ended early, or an interrupt (`CAN`/`SUB`) cut it off.
    Aborted,
    /// Header pair without `=`, or a header over [`ITERM2_OSC_MAX_ATTRIBUTE_BYTES`].
    MalformedAttribute,
    /// Attribute name outside the admitted set.
    UnknownAttribute,
    /// The same attribute appeared twice.
    DuplicateAttribute,
    /// Attribute value outside the admitted shape (`size` not numeric,
    /// malformed `name`, `inline`/`preserveAspectRatio` not `0`/`1`).
    BadValue,
    /// Dimension parsed as zero (or an `IHDR` axis of zero).
    ZeroDimension,
    /// Dimension in an unsupported unit (`Npx`, `N%`), negative, or over
    /// [`ITERM2_OSC_MAX_CELL_SPAN`].
    UnsupportedDimension,
    /// `preserveAspectRatio=0` (stretch) is not representable by the seam.
    UnsupportedAspect,
    /// `inline=0`, or no `inline` at all (the iTerm2 download form, whose
    /// default is `0`), is refused: no file writes, no unreachable store.
    TransmitOnly,
    /// No `:` separator, or an empty base64 payload.
    MissingPayload,
    /// Base64 alphabet/padding/length violation.
    BadBase64,
    /// Wire or decoded payload over the cap.
    OversizePayload,
    /// Payload is not a PNG (the only admitted format).
    UnsupportedFormat,
    /// PNG `IHDR` dimensions exceed the decode caps.
    OversizePixels,
}

impl Iterm2Reject {
    /// Stable short label used in diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Aborted => "truncated OSC",
            Self::MalformedAttribute => "malformed attribute",
            Self::UnknownAttribute => "unknown attribute",
            Self::DuplicateAttribute => "duplicate attribute",
            Self::BadValue => "bad attribute value",
            Self::ZeroDimension => "zero dimension",
            Self::UnsupportedDimension => "unsupported dimension",
            Self::UnsupportedAspect => "unsupported preserveAspectRatio",
            Self::TransmitOnly => "non-inline form refused",
            Self::MissingPayload => "missing base64 payload",
            Self::BadBase64 => "invalid base64 payload",
            Self::OversizePayload => "payload exceeds parser budget",
            Self::UnsupportedFormat => "unsupported image format",
            Self::OversizePixels => "declared pixels exceed decode caps",
        }
    }
}

impl std::fmt::Display for Iterm2Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One admitted request: requested cell span plus decoded PNG bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iterm2Request {
    /// Requested width in cells (`0` = derive from the PNG pixel width).
    pub cols: u16,
    /// Requested height in cells (`0` = derive from the PNG pixel height).
    pub rows: u16,
    /// Decoded payload bytes (PNG, bounded by the caps above).
    pub payload: Box<[u8]>,
}

/// Where the byte-level pre-scan currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ScanState {
    /// Not inside a candidate OSC.
    #[default]
    Idle,
    /// Saw `ESC ]`; matching [`ITERM2_OSC_PREFIX`] byte by byte.
    Probe,
    /// Prefix matched; collecting the bounded body.
    Collect,
    /// Saw `ESC` inside a collected body; the next byte decides `ST`.
    AwaitSt,
}

/// What [`Iterm2OscScanner::push`] wants the caller to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OscOutcome {
    /// Byte consumed; keep scanning.
    Consumed,
    /// Feed [`Iterm2OscScanner::replay`] to `vte`, then process the current
    /// byte through the normal path (prefix mismatch).
    PassThrough,
    /// The OSC finished; take the result from
    /// [`Iterm2OscScanner::take_result`].
    Finished,
    /// Finished *and* the current byte must be reprocessed through the
    /// normal path after replaying the held `ESC`.
    FinishedThenPassThrough,
}

/// Byte-level detector/collector for `OSC 1337 ; File=`.
///
/// Owns no diagnostics: the caller emits the action and the warning, so
/// the same rate-limited log covers the whole parser.
#[derive(Debug, Default)]
pub(crate) struct Iterm2OscScanner {
    state: ScanState,
    matched: usize,
    body: Vec<u8>,
    rejecting: Option<Iterm2Reject>,
    replay: [u8; MAX_REPLAY_BYTES],
    replay_len: usize,
    finished: Option<Result<Iterm2Request, Iterm2Reject>>,
}

impl Iterm2OscScanner {
    /// Whether a candidate OSC is open (bytes must keep flowing here).
    pub(crate) const fn is_active(&self) -> bool {
        !matches!(self.state, ScanState::Idle)
    }

    /// Whether the prefix matched and the scanner is collecting the body
    /// (the only diversion bytes that count against the shared budget).
    pub(crate) const fn is_collecting(&self) -> bool {
        matches!(self.state, ScanState::Collect | ScanState::AwaitSt)
    }

    /// Bytes currently collected in the body buffer (cap-boundary tests).
    #[cfg(test)]
    pub(crate) fn buffered_body_len(&self) -> usize {
        self.body.len()
    }

    /// Capacity currently retained by the body buffer (cap-boundary tests).
    #[cfg(test)]
    pub(crate) fn buffered_body_capacity(&self) -> usize {
        self.body.capacity()
    }

    /// Opens a candidate after the caller consumed `ESC ]`.
    pub(crate) fn begin(&mut self) {
        self.state = ScanState::Probe;
        self.matched = 0;
        self.body.clear();
        self.rejecting = None;
        self.replay_len = 0;
        self.finished = None;
    }

    /// Bytes that must be replayed into `vte` before the current byte.
    pub(crate) fn replay(&self) -> &[u8] {
        &self.replay[..self.replay_len]
    }

    /// Drops the replay buffer after the caller fed it.
    pub(crate) fn clear_replay(&mut self) {
        self.replay_len = 0;
    }

    /// Takes the finished result exactly once.
    pub(crate) fn take_result(&mut self) -> Option<Result<Iterm2Request, Iterm2Reject>> {
        self.finished.take()
    }

    /// Feeds one byte of the candidate OSC.
    pub(crate) fn push(&mut self, byte: u8) -> OscOutcome {
        match self.state {
            ScanState::Idle => OscOutcome::Consumed,
            ScanState::Probe => {
                if byte == ITERM2_OSC_PREFIX[self.matched] {
                    self.matched += 1;
                    if self.matched == ITERM2_OSC_PREFIX.len() {
                        self.state = ScanState::Collect;
                    }
                    OscOutcome::Consumed
                } else {
                    self.fill_replay_prefix();
                    self.state = ScanState::Idle;
                    OscOutcome::PassThrough
                }
            }
            ScanState::Collect => match byte {
                0x07 => {
                    self.finish(None);
                    OscOutcome::Finished
                }
                0x1B => {
                    self.state = ScanState::AwaitSt;
                    OscOutcome::Consumed
                }
                0x18 | 0x1A => {
                    let reason = self.rejecting.unwrap_or(Iterm2Reject::Aborted);
                    self.finish(Some(reason));
                    OscOutcome::Finished
                }
                _ => {
                    self.push_body(byte);
                    OscOutcome::Consumed
                }
            },
            ScanState::AwaitSt => {
                if byte == b'\\' {
                    self.finish(None);
                    OscOutcome::Finished
                } else {
                    // `vte` ends an OSC at any `ESC`; do the same, refuse the
                    // truncated image, and hand the `ESC` back so the next
                    // sequence is parsed exactly as it would have been.
                    let reason = self.rejecting.unwrap_or(Iterm2Reject::Aborted);
                    self.finish(Some(reason));
                    self.replay[0] = 0x1B;
                    self.replay_len = 1;
                    OscOutcome::FinishedThenPassThrough
                }
            }
        }
    }

    /// Buffers `ESC ]` plus the matched prefix for replay.
    fn fill_replay_prefix(&mut self) {
        self.replay[0] = 0x1B;
        self.replay[1] = b']';
        self.replay[2..2 + self.matched].copy_from_slice(&ITERM2_OSC_PREFIX[..self.matched]);
        self.replay_len = 2 + self.matched;
    }

    /// Collects one body byte under the body cap.
    ///
    /// Capacity grows in bounded steps (`max(len, BODY_RESERVE_MIN)`, capped
    /// by the remaining budget), so the *retained* capacity never exceeds the
    /// cap (a realloc can transiently hold the old and new blocks side by
    /// side) instead of the ~`2x` cap a plain `Vec::push` doubling retains.
    fn push_body(&mut self, byte: u8) {
        if self.rejecting.is_some() {
            return;
        }
        if self.body.len() >= ITERM2_OSC_MAX_BODY_BYTES {
            self.rejecting = Some(Iterm2Reject::OversizePayload);
            // Release the accumulated body now: the remaining bytes are
            // discarded without buffering.
            self.body = Vec::new();
            return;
        }
        if self.body.len() == self.body.capacity() {
            let step = self
                .body
                .len()
                .max(BODY_RESERVE_MIN)
                .min(ITERM2_OSC_MAX_BODY_BYTES - self.body.len());
            self.body.reserve_exact(step);
        }
        self.body.push(byte);
    }

    /// Parses (or records the forced rejection) and resets to idle.
    fn finish(&mut self, forced: Option<Iterm2Reject>) {
        let result = match forced.or(self.rejecting) {
            Some(reason) => Err(reason),
            None => parse_body(&self.body),
        };
        self.finished = Some(result);
        self.state = ScanState::Idle;
        self.matched = 0;
        self.rejecting = None;
        // Release image-sized capacity instead of retaining it per parser.
        self.body = Vec::new();
    }
}

/// Parses the collected `<attrs>:<base64>` body.
fn parse_body(body: &[u8]) -> Result<Iterm2Request, Iterm2Reject> {
    let separator = body
        .iter()
        .position(|&byte| byte == b':')
        .ok_or(Iterm2Reject::MissingPayload)?;
    let (header, encoded) = body.split_at(separator);
    // `split_at` leaves the `:` at the head of `encoded`.
    let encoded = encoded.get(1..).unwrap_or(&[]);
    let (cols, rows) = parse_attributes(header)?;
    let payload = decode_payload(encoded)?;
    validate_png(&payload)?;
    Ok(Iterm2Request {
        cols,
        rows,
        payload: payload.into_boxed_slice(),
    })
}

/// Parses the `;`-separated attribute list into the admitted cell span.
fn parse_attributes(header: &[u8]) -> Result<(u16, u16), Iterm2Reject> {
    if header.len() > ITERM2_OSC_MAX_ATTRIBUTE_BYTES {
        return Err(Iterm2Reject::MalformedAttribute);
    }
    if header.is_empty() {
        // No attributes at all: the download form with every default,
        // including `inline=0`. Refuse before any payload work.
        return Err(Iterm2Reject::TransmitOnly);
    }
    let mut cols = 0u16;
    let mut rows = 0u16;
    let mut seen = 0u8;
    let mut inline_admitted = false;
    for pair in header.split(|&byte| byte == b';') {
        let eq = pair
            .iter()
            .position(|&byte| byte == b'=')
            .ok_or(Iterm2Reject::MalformedAttribute)?;
        let (key, value) = pair.split_at(eq);
        let value = value.get(1..).unwrap_or(&[]);
        match key {
            b"name" => {
                mark_seen(&mut seen, 1 << 0)?;
                validate_name(value)?;
            }
            b"size" => {
                mark_seen(&mut seen, 1 << 1)?;
                validate_size(value)?;
            }
            b"width" => {
                mark_seen(&mut seen, 1 << 2)?;
                cols = parse_dimension(value)?;
            }
            b"height" => {
                mark_seen(&mut seen, 1 << 3)?;
                rows = parse_dimension(value)?;
            }
            b"preserveAspectRatio" => {
                mark_seen(&mut seen, 1 << 4)?;
                match value {
                    b"1" => {}
                    b"0" => return Err(Iterm2Reject::UnsupportedAspect),
                    _ => return Err(Iterm2Reject::BadValue),
                }
            }
            b"inline" => {
                mark_seen(&mut seen, 1 << 5)?;
                match value {
                    b"1" => inline_admitted = true,
                    b"0" => return Err(Iterm2Reject::TransmitOnly),
                    _ => return Err(Iterm2Reject::BadValue),
                }
            }
            _ => return Err(Iterm2Reject::UnknownAttribute),
        }
    }
    // `inline` defaults to `0` in the iTerm2 protocol, so only an explicit
    // `inline=1` is an inline-image request; the download form is refused.
    if !inline_admitted {
        return Err(Iterm2Reject::TransmitOnly);
    }
    Ok((cols, rows))
}

/// Records one attribute bit, refusing duplicates.
fn mark_seen(seen: &mut u8, bit: u8) -> Result<(), Iterm2Reject> {
    if *seen & bit != 0 {
        return Err(Iterm2Reject::DuplicateAttribute);
    }
    *seen |= bit;
    Ok(())
}

/// Validates the base64 `name=` value and drops it (no file writes).
fn validate_name(value: &[u8]) -> Result<(), Iterm2Reject> {
    if value.is_empty() {
        return Ok(());
    }
    let mut decoded = Vec::new();
    let mut decoder = Base64Stream::default();
    decode_with(&mut decoder, value, &mut decoded, ITERM2_OSC_MAX_NAME_BYTES)
        .map_err(|_| Iterm2Reject::BadValue)
}

/// Validates the numeric `size=` value and drops it (informational only).
fn validate_size(value: &[u8]) -> Result<(), Iterm2Reject> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(Iterm2Reject::BadValue);
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .map(|_| ())
        .ok_or(Iterm2Reject::BadValue)
}

/// Parses `auto` or a cell count; other units are refused.
fn parse_dimension(value: &[u8]) -> Result<u16, Iterm2Reject> {
    if value == b"auto" {
        return Ok(0);
    }
    let unsupported_unit = value.ends_with(b"px") || value.ends_with(b"%");
    if unsupported_unit {
        return Err(Iterm2Reject::UnsupportedDimension);
    }
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(Iterm2Reject::UnsupportedDimension);
    }
    let cells = std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
        .ok_or(Iterm2Reject::UnsupportedDimension)?;
    if cells == 0 {
        return Err(Iterm2Reject::ZeroDimension);
    }
    if cells > u32::from(ITERM2_OSC_MAX_CELL_SPAN) {
        return Err(Iterm2Reject::UnsupportedDimension);
    }
    u16::try_from(cells).map_err(|_| Iterm2Reject::UnsupportedDimension)
}

/// Decodes the payload with the shared fail-closed base64 decoder.
fn decode_payload(encoded: &[u8]) -> Result<Vec<u8>, Iterm2Reject> {
    if encoded.is_empty() {
        return Err(Iterm2Reject::MissingPayload);
    }
    if encoded.len() > ITERM2_OSC_MAX_WIRE_BYTES {
        return Err(Iterm2Reject::OversizePayload);
    }
    let mut decoded = Vec::new();
    let mut decoder = Base64Stream::default();
    decode_with(
        &mut decoder,
        encoded,
        &mut decoded,
        ITERM2_OSC_MAX_DECODED_BYTES,
    )?;
    Ok(decoded)
}

/// Runs the shared decoder over one complete field, mapping its rejections.
fn decode_with(
    decoder: &mut Base64Stream,
    encoded: &[u8],
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), Iterm2Reject> {
    decoder
        .push(encoded, output, limit)
        .map_err(decode_reject)?;
    decoder.finish(output, limit).map_err(decode_reject)
}

/// Maps the Kitty base64 rejection taxonomy onto the iTerm2 one.
fn decode_reject(reject: KittyApcReject) -> Iterm2Reject {
    match reject {
        KittyApcReject::Oversize => Iterm2Reject::OversizePayload,
        _ => Iterm2Reject::BadBase64,
    }
}

/// Admits PNG only and validates `IHDR` before any decoder allocation.
fn validate_png(payload: &[u8]) -> Result<(), Iterm2Reject> {
    // Magic (8) + length (4) + `IHDR` (4) + width/height (8) + depth/color
    // (2) + compression/filter/interlace (3) + CRC (4) = 33 bytes.
    if payload.len() < 33 || payload[..8] != PNG_MAGIC || &payload[12..16] != b"IHDR" {
        return Err(Iterm2Reject::UnsupportedFormat);
    }
    // `IHDR` is the first chunk and its payload is exactly 13 bytes; a
    // mismatched declared length means the dimensions below are not the ones
    // the decoder will read (`bitty-rich::kitty_decode::sniff_png_ihdr`).
    if u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]) != 13 {
        return Err(Iterm2Reject::UnsupportedFormat);
    }
    let width = u32::from_be_bytes([payload[16], payload[17], payload[18], payload[19]]);
    let height = u32::from_be_bytes([payload[20], payload[21], payload[22], payload[23]]);
    if width == 0 || height == 0 {
        return Err(Iterm2Reject::ZeroDimension);
    }
    if width > ITERM2_DECODE_MAX_DIMENSION
        || height > ITERM2_DECODE_MAX_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > ITERM2_DECODE_MAX_PIXELS
    {
        return Err(Iterm2Reject::OversizePixels);
    }
    Ok(())
}

/// Builds the inert telemetry action for one refusal.
#[must_use]
pub(crate) fn rejected_action(reason: Iterm2Reject) -> TerminalAction {
    TerminalAction::Iterm2ImageRejected { reason }
}

/// Builds the admitted-image action.
#[must_use]
pub(crate) fn image_action(request: Iterm2Request) -> TerminalAction {
    TerminalAction::Iterm2Image {
        cols: request.cols,
        rows: request.rows,
        payload: request.payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 opaque red PNG: the same fixture the runtime suite uses.
    const PNG_1X1: [u8; 70] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// Minimal standard-alphabet base64 encoder for fixtures.
    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut triple = 0u32;
            for (shift, byte) in chunk.iter().enumerate() {
                triple |= u32::from(*byte) << (16 - 8 * shift);
            }
            out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(triple >> 6) as usize & 0x3F] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[triple as usize & 0x3F] as char
            } else {
                '='
            });
        }
        out
    }

    /// Builds `<header>:<base64>` for the 1x1 PNG fixture.
    fn body(header: &str) -> Vec<u8> {
        let mut raw = header.as_bytes().to_vec();
        raw.push(b':');
        raw.extend_from_slice(base64_encode(&PNG_1X1).as_bytes());
        raw
    }

    /// Runs the scanner over one body terminated by `BEL`.
    fn scan(raw: &[u8]) -> (Option<Result<Iterm2Request, Iterm2Reject>>, Vec<u8>) {
        let mut terminated = raw.to_vec();
        terminated.push(0x07);
        scan_stream(&terminated)
    }

    /// Runs the scanner over an explicit body (no implicit terminator),
    /// feeding the admitted prefix first exactly as `Parser` does.
    fn scan_stream(raw: &[u8]) -> (Option<Result<Iterm2Request, Iterm2Reject>>, Vec<u8>) {
        let mut scanner = Iterm2OscScanner::default();
        scanner.begin();
        for &byte in ITERM2_OSC_PREFIX {
            assert_eq!(scanner.push(byte), OscOutcome::Consumed);
        }
        drive(&mut scanner, raw)
    }

    /// Drives the scanner from the introducer arm (prefix still to match).
    fn scan_from_start(raw: &[u8]) -> (Option<Result<Iterm2Request, Iterm2Reject>>, Vec<u8>) {
        let mut scanner = Iterm2OscScanner::default();
        scanner.begin();
        drive(&mut scanner, raw)
    }

    fn drive(
        scanner: &mut Iterm2OscScanner,
        raw: &[u8],
    ) -> (Option<Result<Iterm2Request, Iterm2Reject>>, Vec<u8>) {
        let mut replay = Vec::new();
        let mut finished = None;
        for &byte in raw {
            match scanner.push(byte) {
                OscOutcome::Consumed => {}
                OscOutcome::Finished => {
                    finished = scanner.take_result();
                    break;
                }
                OscOutcome::PassThrough | OscOutcome::FinishedThenPassThrough => {
                    replay.extend_from_slice(scanner.replay());
                    scanner.clear_replay();
                    if let Some(result) = scanner.take_result() {
                        finished = Some(result);
                    }
                }
            }
        }
        (finished, replay)
    }

    /// Asserts one body is refused with `expected`.
    fn expect_reject(raw: &[u8], expected: Iterm2Reject) {
        let (result, _) = scan(raw);
        assert_eq!(
            result.expect("terminated").expect_err("must be refused"),
            expected,
            "body {raw:?}"
        );
    }

    #[test]
    fn admitted_inline_image_parses_cells_and_payload() {
        let raw = body("width=2;height=3;preserveAspectRatio=1;inline=1;size=70;name=aW1n");
        let (result, replay) = scan(&raw);
        assert!(replay.is_empty(), "admitted image must not replay");
        let request = result.expect("terminated").expect("admitted");
        assert_eq!((request.cols, request.rows), (2, 3));
        assert_eq!(&*request.payload, PNG_1X1.as_slice());
    }

    #[test]
    fn auto_dimensions_keep_natural_size() {
        let (result, _) = scan(&body("inline=1"));
        let request = result.expect("terminated").expect("admitted");
        assert_eq!((request.cols, request.rows), (0, 0));

        let (result, _) = scan(&body("inline=1;width=auto;height=auto"));
        let request = result.expect("terminated").expect("admitted");
        assert_eq!((request.cols, request.rows), (0, 0));
    }

    #[test]
    fn absent_inline_is_the_download_form_and_is_refused() {
        // iTerm2 defaults `inline` to 0: no explicit `inline=1`, no paint.
        expect_reject(&body(""), Iterm2Reject::TransmitOnly);
        expect_reject(&body("width=2;height=2"), Iterm2Reject::TransmitOnly);
    }

    #[test]
    fn bel_and_st_terminators_are_accepted() {
        let (result, replay) = scan(&body("inline=1"));
        assert!(result.expect("terminated").is_ok());
        assert!(replay.is_empty());

        let mut stream = body("inline=1");
        stream.extend_from_slice(b"\x1b\\");
        let (result, replay) = scan_stream(&stream);
        assert!(result.expect("terminated").is_ok());
        assert!(replay.is_empty(), "ST is consumed, never replayed");
    }

    #[test]
    fn truncated_base64_is_refused() {
        // Drop three characters: the tail quantum is a single sextet, which
        // encodes no complete byte (`len % 4 == 1`).
        let mut raw = b"inline=1:".to_vec();
        let encoded = base64_encode(&PNG_1X1);
        raw.extend_from_slice(&encoded.as_bytes()[..encoded.len() - 3]);
        expect_reject(&raw, Iterm2Reject::BadBase64);

        // A non-alphabet byte is refused as well (fail-closed alphabet).
        let mut raw = b"inline=1:".to_vec();
        raw.extend_from_slice(b"iVBORw0KGgo!");
        expect_reject(&raw, Iterm2Reject::BadBase64);
    }

    #[test]
    fn body_cap_and_missing_inline_still_refuse_cleanly() {
        // A body with no `inline=1` is refused before any payload work.
        let mut raw = b"width=2:".to_vec();
        raw.extend_from_slice(base64_encode(&PNG_1X1).as_bytes());
        expect_reject(&raw, Iterm2Reject::TransmitOnly);
    }

    #[test]
    fn oversize_decoded_payload_is_refused() {
        // The wire cap is checked before the decoder runs.
        let encoded = vec![b'A'; ITERM2_OSC_MAX_WIRE_BYTES + 4];
        assert_eq!(decode_payload(&encoded), Err(Iterm2Reject::OversizePayload));
    }

    #[test]
    fn oversize_body_is_dropped_without_buffering() {
        let mut scanner = Iterm2OscScanner::default();
        scanner.begin();
        for _ in 0..ITERM2_OSC_PREFIX.len() {
            assert_eq!(
                scanner.push(ITERM2_OSC_PREFIX[scanner.matched]),
                OscOutcome::Consumed
            );
        }
        for _ in 0..=ITERM2_OSC_MAX_BODY_BYTES {
            assert_eq!(scanner.push(b'A'), OscOutcome::Consumed);
        }
        // Peak retention is the cap, never the whole hostile stream.
        assert!(scanner.body.is_empty(), "overflow body must be released");
        assert_eq!(scanner.push(0x07), OscOutcome::Finished);
        assert_eq!(
            scanner.take_result().expect("terminated"),
            Err(Iterm2Reject::OversizePayload)
        );
    }

    #[test]
    fn body_growth_stays_within_the_cap() {
        let mut scanner = Iterm2OscScanner::default();
        scanner.begin();
        for _ in 0..ITERM2_OSC_PREFIX.len() {
            assert_eq!(
                scanner.push(ITERM2_OSC_PREFIX[scanner.matched]),
                OscOutcome::Consumed
            );
        }
        for _ in 0..ITERM2_OSC_MAX_BODY_BYTES {
            assert_eq!(scanner.push(b'A'), OscOutcome::Consumed);
        }
        assert_eq!(scanner.buffered_body_len(), ITERM2_OSC_MAX_BODY_BYTES);
        // `Vec::push` doubling alone would reach ~2x the cap here; the bound
        // checked is the cap plus the documented slack (see
        // `BODY_CAPACITY_SLACK`).
        let bound = ITERM2_OSC_MAX_BODY_BYTES + BODY_CAPACITY_SLACK;
        assert!(
            scanner.buffered_body_capacity() <= bound,
            "body capacity {} must stay within the {bound}-byte bound",
            scanner.buffered_body_capacity()
        );
    }

    #[test]
    fn malformed_and_unknown_attributes_are_refused() {
        expect_reject(&body("width"), Iterm2Reject::MalformedAttribute);
        expect_reject(&body("=2"), Iterm2Reject::UnknownAttribute);
        expect_reject(&body("bogus=1"), Iterm2Reject::UnknownAttribute);
        expect_reject(&body("width=2;width=3"), Iterm2Reject::DuplicateAttribute);
    }

    #[test]
    fn transmit_only_form_is_refused() {
        expect_reject(&body("inline=0"), Iterm2Reject::TransmitOnly);
        expect_reject(&body("inline=2"), Iterm2Reject::BadValue);
        expect_reject(&body("inline=1;inline=1"), Iterm2Reject::DuplicateAttribute);
    }

    #[test]
    fn stretch_request_is_refused() {
        expect_reject(
            &body("preserveAspectRatio=0"),
            Iterm2Reject::UnsupportedAspect,
        );
        expect_reject(&body("preserveAspectRatio=yes"), Iterm2Reject::BadValue);
    }

    #[test]
    fn zero_and_negative_dimensions_are_refused() {
        expect_reject(&body("width=0"), Iterm2Reject::ZeroDimension);
        expect_reject(&body("height=0"), Iterm2Reject::ZeroDimension);
        expect_reject(&body("width=-1"), Iterm2Reject::UnsupportedDimension);
        expect_reject(&body("height=1.5"), Iterm2Reject::UnsupportedDimension);
    }

    #[test]
    fn pixel_and_percent_dimensions_are_refused() {
        expect_reject(&body("width=40px"), Iterm2Reject::UnsupportedDimension);
        expect_reject(&body("height=50%"), Iterm2Reject::UnsupportedDimension);
        expect_reject(&body("width=99999"), Iterm2Reject::UnsupportedDimension);
    }

    #[test]
    fn huge_png_dimensions_with_tiny_payload_are_refused() {
        // Valid magic and `IHDR`, but the header claims 100000 x 100000 px.
        let mut payload = PNG_1X1.to_vec();
        payload[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        payload[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let mut raw = b"inline=1:".to_vec();
        raw.extend_from_slice(base64_encode(&payload).as_bytes());
        expect_reject(&raw, Iterm2Reject::OversizePixels);
    }

    #[test]
    fn ihdr_chunk_length_must_be_thirteen() {
        // Same valid PNG bytes, but the first chunk's declared length is not
        // `IHDR`'s 13: the dimensions read here would not be the ones the
        // decoder reads, so the request is refused before either allocates.
        let mut payload = PNG_1X1.to_vec();
        payload[8..12].copy_from_slice(&16u32.to_be_bytes());
        let mut raw = b"inline=1:".to_vec();
        raw.extend_from_slice(base64_encode(&payload).as_bytes());
        expect_reject(&raw, Iterm2Reject::UnsupportedFormat);
    }

    #[test]
    fn non_png_payload_is_refused() {
        let mut raw = b"inline=1:".to_vec();
        // JPEG SOI + JFIF marker: a real format, just not an admitted one.
        raw.extend_from_slice(base64_encode(b"\xFF\xD8\xFF\xE0not-a-png").as_bytes());
        expect_reject(&raw, Iterm2Reject::UnsupportedFormat);

        // A short PNG-prefixed body cannot carry a valid `IHDR`.
        let mut raw = b"inline=1:".to_vec();
        raw.extend_from_slice(base64_encode(&PNG_MAGIC).as_bytes());
        expect_reject(&raw, Iterm2Reject::UnsupportedFormat);
    }

    #[test]
    fn missing_payload_separator_and_empty_payload_are_refused() {
        expect_reject(b"inline=1", Iterm2Reject::MissingPayload);
        expect_reject(b"inline=1:", Iterm2Reject::MissingPayload);
    }

    #[test]
    fn name_and_size_are_validated_then_dropped() {
        expect_reject(&body("size=abc"), Iterm2Reject::BadValue);
        expect_reject(&body("name=!!!"), Iterm2Reject::BadValue);
        // An oversized decoded name is refused, not truncated.
        let long_name = base64_encode(&[b'a'; ITERM2_OSC_MAX_NAME_BYTES + 1]);
        expect_reject(&body(&format!("name={long_name}")), Iterm2Reject::BadValue);
        // Admitting a numeric size and a valid name changes nothing.
        let (result, _) = scan(&body("inline=1;size=18446744073709551615;name=aW1n"));
        assert!(result.expect("terminated").is_ok());
    }

    #[test]
    fn interrupted_osc_is_refused() {
        let mut raw = body("inline=1");
        raw.push(0x18);
        expect_reject(&raw, Iterm2Reject::Aborted);
    }

    #[test]
    fn non_file_osc_candidate_replays_to_the_caller() {
        // A partial prefix match is replayed byte-for-byte, including the
        // introducer, so `vte` sees exactly the input it always saw.
        let (result, replay) = scan_from_start(b"1337;FilX");
        assert!(result.is_none());
        assert_eq!(replay, b"\x1b]1337;Fil");
    }

    #[test]
    fn esc_that_is_not_st_ends_the_image_and_replays_the_esc() {
        let mut raw = body("inline=1");
        // `ESC [` starts a CSI: the image is refused and the ESC goes back
        // to the normal path, which parses the CSI normally.
        raw.extend_from_slice(b"\x1b[1m");
        let (result, replay) = scan_stream(&raw);
        assert_eq!(
            result.expect("terminated"),
            Err(Iterm2Reject::Aborted),
            "truncated image must refuse"
        );
        assert_eq!(replay, b"\x1b");
    }
}
