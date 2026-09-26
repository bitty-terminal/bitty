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
//! # Bounds
//!
//! - [`KITTY_APC_LEDGER_CAP`] is the accepted IMG-1 4 MiB payload cap.
//!   Base64 is decoded incrementally into one bounded payload buffer, so
//!   pending chunks, current output, and decoder scratch never form a second
//!   large APC allocation. The control header has a separate 4 KiB bound.
//! - Raw `s`/`v` claims are checked before emission against the payload cap;
//!   PNG dimensions remain governed by the downstream decoder contract.
//! - Every growth and decoded-length check runs before the corresponding
//!   allocation or adapter hand-off.
//!
//! # Fail-closed behavior
//!
//! Every rejection warns via rate-limited `eprintln!` (diagnostic only, no
//! state change) and yields no completed transmission: bad base64 alphabet,
//! malformed control, missing `f`, oversize claim, ledger-cap overflow, or
//! decode-cap violation all store nothing and paint nothing. The caller
//! (`Parser`) emits no action on rejection. Chunked streams drop only the
//! offending stream on oversize, mirroring `KittyGraphicsStub` semantics.

use crate::diag::{RejectLog, warn_rejection};

/// Accepted IMG-1 parser payload cap.
pub const KITTY_APC_LEDGER_CAP: usize = 4 * 1024 * 1024;

pub(crate) const KITTY_APC_MAX_CONTROL_BYTES: usize = 4096;

const KITTY_APC_CODEC_SCRATCH_BYTES: usize = 4;

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
    /// Wire `C=` cursor movement (`0` moves cursor, `1` keeps it, default `0`).
    pub cursor_movement_C: u8,
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
    /// Growth would exceed the parser payload budget.
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
            Self::Oversize => write!(f, "kitty payload exceeds parser budget"),
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
    /// Wire `C=` cursor movement (`0` moves cursor, `1` keeps it).
    pub cursor_movement_C: u8,
    /// Base64-decoded payload bytes (assembled across `m=` chunks).
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

/// In-flight `m=1` stream: first-chunk params plus one decoded payload buffer.
#[derive(Debug, Clone)]
struct PendingKitty {
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    action_a: Option<char>,
    cols_c: u16,
    rows_r: u16,
    cursor_movement_C: u8,
    encoded_len: usize,
    decoder: Base64Stream,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Base64Stream {
    carry: [u8; 4],
    carry_len: usize,
    padding: usize,
    finished: bool,
}

#[derive(Debug, Clone, Copy)]
struct IntakeBudget {
    payload_limit: usize,
    total_limit: usize,
    current: usize,
    decoded: usize,
    peak_payload: usize,
    peak_total: usize,
}

impl IntakeBudget {
    fn new(payload_limit: usize) -> Self {
        Self {
            payload_limit,
            total_limit: payload_limit
                .saturating_add(KITTY_APC_MAX_CONTROL_BYTES)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES),
            current: 0,
            decoded: 0,
            peak_payload: 0,
            peak_total: KITTY_APC_CODEC_SCRATCH_BYTES,
        }
    }

    fn total(&self) -> usize {
        self.current
            .saturating_add(self.decoded)
            .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
    }

    fn reserve_current(&mut self, amount: usize) -> bool {
        let Some(next) = self.current.checked_add(amount) else {
            return false;
        };
        if next > KITTY_APC_MAX_CONTROL_BYTES
            || self
                .decoded
                .saturating_add(next)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
                > self.total_limit
        {
            return false;
        }
        self.current = next;
        self.peak_total = self.peak_total.max(self.total());
        true
    }

    fn release_current(&mut self, amount: usize) {
        self.current = self.current.saturating_sub(amount);
    }

    fn reserve_retained(&mut self, amount: usize) -> bool {
        if amount > self.payload_limit
            || self
                .current
                .saturating_add(amount)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
                > self.total_limit
        {
            return false;
        }
        self.decoded = amount;
        self.peak_payload = self.peak_payload.max(amount);
        self.peak_total = self.peak_total.max(self.total());
        true
    }

    fn clear_retained(&mut self) {
        self.decoded = 0;
    }
}

impl Base64Stream {
    fn push(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), KittyApcReject> {
        for &byte in input {
            if self.finished {
                return Err(KittyApcReject::BadBase64);
            }
            if self.padding != 0 {
                if byte != b'=' {
                    return Err(KittyApcReject::BadBase64);
                }
                self.padding += 1;
                if self.padding > 2 {
                    return Err(KittyApcReject::BadBase64);
                }
                if self.padding == 2 {
                    self.finish_padded(output, limit)?;
                    self.finished = true;
                }
                continue;
            }
            if byte == b'=' {
                if self.carry_len < 2 {
                    return Err(KittyApcReject::BadBase64);
                }
                self.padding = 1;
                if self.carry_len == 3 {
                    self.finish_padded(output, limit)?;
                    self.finished = true;
                }
                continue;
            }
            if sextet(byte).is_none() {
                return Err(KittyApcReject::BadBase64);
            }
            self.carry[self.carry_len] = byte;
            self.carry_len += 1;
            if self.carry_len == 4 {
                self.finish_full(output, limit)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        if self.finished {
            return Ok(());
        }
        if self.padding != 0 {
            return Err(KittyApcReject::BadBase64);
        }
        match self.carry_len {
            0 => {}
            1 => return Err(KittyApcReject::BadBase64),
            2 => self.finish_tail(output, limit, 2)?,
            3 => self.finish_tail(output, limit, 3)?,
            _ => return Err(KittyApcReject::BadBase64),
        }
        self.finished = true;
        Ok(())
    }

    fn finish_full(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        let triple = (sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)? << 18)
            | (sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)? << 12)
            | (sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)? << 6)
            | sextet(self.carry[3]).ok_or(KittyApcReject::BadBase64)?;
        append_decoded(
            output,
            &[(triple >> 16) as u8, (triple >> 8) as u8, triple as u8],
            limit,
        )?;
        self.carry_len = 0;
        Ok(())
    }

    fn finish_padded(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        let expected = if self.padding == 1 { 3 } else { 2 };
        if self.carry_len != expected {
            return Err(KittyApcReject::BadBase64);
        }
        let first = sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)?;
        let second = sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)?;
        if expected == 3 {
            let third = sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)?;
            let bits = (first << 18) | (second << 12) | (third << 6);
            append_decoded(output, &[(bits >> 16) as u8, (bits >> 8) as u8], limit)?;
        } else {
            let bits = (first << 18) | (second << 12);
            append_decoded(output, &[(bits >> 16) as u8], limit)?;
        }
        self.carry_len = 0;
        Ok(())
    }

    fn finish_tail(
        &mut self,
        output: &mut Vec<u8>,
        limit: usize,
        len: usize,
    ) -> Result<(), KittyApcReject> {
        let first = sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)?;
        let second = sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)?;
        if len == 2 {
            append_decoded(output, &[((first << 18 | second << 12) >> 16) as u8], limit)
        } else {
            let third = sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)?;
            append_decoded(
                output,
                &[
                    ((first << 18 | second << 12 | third << 6) >> 16) as u8,
                    ((first << 18 | second << 12 | third << 6) >> 8) as u8,
                ],
                limit,
            )
        }
    }
}

impl PendingKitty {
    fn push(
        &mut self,
        input: &[u8],
        available: usize,
        budget: &mut IntakeBudget,
    ) -> Result<(), KittyApcReject> {
        let encoded_len = self
            .encoded_len
            .checked_add(input.len())
            .ok_or(KittyApcReject::Oversize)?;
        if encoded_len > max_encoded_len_for_decode_cap(available) {
            return Err(KittyApcReject::Oversize);
        }
        self.encoded_len = encoded_len;
        self.decoder.push(input, &mut self.payload, available)?;
        if !budget.reserve_retained(self.payload.capacity()) {
            return Err(KittyApcReject::Oversize);
        }
        Ok(())
    }
}

fn sextet(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a' + 26)),
        b'0'..=b'9' => Some(u32::from(byte - b'0' + 52)),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn append_decoded(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), KittyApcReject> {
    let required = output
        .len()
        .checked_add(bytes.len())
        .ok_or(KittyApcReject::Oversize)?;
    if required > limit {
        return Err(KittyApcReject::Oversize);
    }
    if output.capacity() < required {
        let quantum = if limit >= 4096 { 4096 } else { 1 };
        let target = required
            .saturating_add(quantum - 1)
            .checked_div(quantum)
            .and_then(|value| value.checked_mul(quantum))
            .unwrap_or(limit)
            .min(limit);
        if target < required {
            return Err(KittyApcReject::Oversize);
        }
        output
            .try_reserve_exact(target - output.len())
            .map_err(|_| KittyApcReject::Oversize)?;
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn max_encoded_len_for_decode_cap(decode_cap: usize) -> usize {
    let full = decode_cap / 3;
    let remainder = decode_cap % 3;
    full.saturating_mul(4)
        .saturating_add(if remainder == 0 { 0 } else { 4 })
}

#[derive(Debug, Clone)]
pub struct KittyApcAssembler {
    pending: Option<PendingKitty>,
    current_final: Option<bool>,
    ledger_cap: usize,
    decode_cap: usize,
    budget: IntakeBudget,
    log: RejectLog,
}

impl KittyApcAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::with_caps(KITTY_APC_LEDGER_CAP, KITTY_APC_DECODE_MAX_BYTES)
    }

    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self::with_caps(ledger_cap, KITTY_APC_DECODE_MAX_BYTES)
    }

    #[must_use]
    pub fn with_caps(ledger_cap: usize, decode_cap: usize) -> Self {
        Self {
            pending: None,
            current_final: None,
            ledger_cap,
            decode_cap,
            budget: IntakeBudget::new(ledger_cap.min(decode_cap)),
            log: RejectLog::default(),
        }
    }

    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.ledger_cap
    }

    #[must_use]
    pub const fn decode_cap(&self) -> usize {
        self.decode_cap
    }

    #[must_use]
    fn effective_cap(&self) -> usize {
        self.ledger_cap.min(self.decode_cap)
    }

    pub(crate) fn reserve_header_byte(&mut self) -> bool {
        self.budget.reserve_current(1)
    }

    pub(crate) fn release_header(&mut self, amount: usize) {
        self.budget.release_current(amount);
    }

    #[cfg(test)]
    pub(crate) fn peak_memory(&self) -> usize {
        self.budget.peak_payload
    }

    #[cfg(test)]
    pub(crate) fn peak_total_memory(&self) -> usize {
        self.budget.peak_total
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn pending_encoded_len(&self) -> usize {
        self.pending
            .as_ref()
            .map_or(0, |pending| pending.encoded_len)
    }

    pub fn abort(&mut self) -> bool {
        self.current_final = None;
        self.budget.clear_retained();
        self.pending.take().is_some()
    }

    pub fn feed(&mut self, raw: &[u8]) -> KittyFeedOutcome {
        let (control, payload) = match raw.iter().position(|&byte| byte == b';') {
            Some(semi) => (&raw[..semi], &raw[semi + 1..]),
            None => (raw, &[][..]),
        };
        if let Err(reason) = self.begin_control(control) {
            return KittyFeedOutcome::Rejected(reason);
        }
        if let Err(reason) = self.push_payload(payload) {
            return KittyFeedOutcome::Rejected(reason);
        }
        self.finish()
    }

    pub(crate) fn begin_control(&mut self, control: &[u8]) -> Result<(), KittyApcReject> {
        if self.current_final.is_some() {
            let reason = KittyApcReject::Orphan;
            self.warn_reject(reason, "nested APC chunk");
            return Err(reason);
        }
        if control.len() > KITTY_APC_MAX_CONTROL_BYTES {
            let reason = KittyApcReject::Oversize;
            self.warn_reject(reason, "control");
            return Err(reason);
        }
        let Some(after_g) = control.strip_prefix(b"G") else {
            return Err(KittyApcReject::NotGraphics);
        };
        if self.pending.is_some() {
            let more = match more_flag(after_g) {
                Ok(Some(more)) => more,
                Ok(None) => {
                    let reason = KittyApcReject::Orphan;
                    self.warn_reject(reason, "missing m= with open stream");
                    return Err(reason);
                }
                Err(reason) => {
                    self.warn_reject(reason, "continuation m=");
                    return Err(reason);
                }
            };
            self.current_final = Some(!more);
        } else {
            let params = match parse_control(after_g) {
                Ok(params) => params,
                Err(reason) => {
                    self.warn_reject(reason, "control");
                    return Err(reason);
                }
            };
            let more = match more_flag(after_g) {
                Ok(Some(m)) => m,
                Ok(None) => false, // No m= means single-shot (m=0)
                Err(reason) => {
                    self.warn_reject(reason, "m= in new transmission");
                    return Err(reason);
                }
            };
            self.pending = Some(PendingKitty {
                format_f: params.format_f,
                width_s: params.width_s,
                height_v: params.height_v,
                action_a: params.action_a,
                cols_c: params.cols_c,
                rows_r: params.rows_r,
                cursor_movement_C: params.cursor_movement_C,
                encoded_len: 0,
                decoder: Base64Stream::default(),
                payload: Vec::new(),
            });
            self.current_final = Some(!more);
        }
        Ok(())
    }

    pub(crate) fn push_payload(&mut self, payload: &[u8]) -> Result<(), KittyApcReject> {
        if self.current_final.is_none() {
            return Err(KittyApcReject::Orphan);
        }
        let available = self.budget.payload_limit;
        let result = match self.pending.as_mut() {
            Some(pending) => pending.push(payload, available, &mut self.budget),
            None => Err(KittyApcReject::Orphan),
        };
        if let Err(reason) = result {
            self.abort();
            self.warn_reject(reason, "payload");
            return Err(reason);
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> KittyFeedOutcome {
        let Some(final_chunk) = self.current_final.take() else {
            return KittyFeedOutcome::Rejected(KittyApcReject::Orphan);
        };
        let Some(mut pending) = self.pending.take() else {
            return KittyFeedOutcome::Rejected(KittyApcReject::Orphan);
        };
        if !final_chunk {
            if pending.decoder.finished {
                self.budget.clear_retained();
                self.warn_reject(KittyApcReject::BadBase64, "non-final chunk");
                return KittyFeedOutcome::Rejected(KittyApcReject::BadBase64);
            }
            self.pending = Some(pending);
            return KittyFeedOutcome::NeedMore {
                buffered_encoded: self.pending_encoded_len(),
            };
        }
        let available = self.budget.payload_limit;
        let result = pending
            .decoder
            .finish(&mut pending.payload, available)
            .and_then(|()| {
                if !self.budget.reserve_retained(pending.payload.capacity()) {
                    return Err(KittyApcReject::Oversize);
                }
                validate_raw_claim(
                    pending.format_f,
                    pending.width_s,
                    pending.height_v,
                    self.effective_cap(),
                )
            });
        if let Err(reason) = result {
            self.budget.clear_retained();
            self.warn_reject(reason, "final payload");
            return KittyFeedOutcome::Rejected(reason);
        }
        self.budget.clear_retained();
        KittyFeedOutcome::Completed(KittyCompleted {
            format_f: pending.format_f,
            width_s: pending.width_s,
            height_v: pending.height_v,
            action_a: pending.action_a,
            cols_c: pending.cols_c,
            rows_r: pending.rows_r,
            cursor_movement_C: pending.cursor_movement_C,
            payload: pending.payload.into_boxed_slice(),
        })
    }
}

impl Default for KittyApcAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyApcAssembler {
    /// Rate-limited diagnostic warn on fail-closed rejection (no state
    /// change, no paint). `NotGraphics` stays silent.
    fn warn_reject(&mut self, reason: KittyApcReject, context: &str) {
        if reason == KittyApcReject::NotGraphics {
            return;
        }
        if let Some(occurrence) = self.log.record() {
            warn_rejection(
                occurrence,
                &format!("bitty: rejecting kitty APC G ({reason} in {context}): stored nothing"),
            );
        }
    }
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
    let mut cursor_movement_C: u8 = 0;
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
            b'C' => {
                cursor_movement_C = parse_u8(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            _ => {
                // Unknown single-letter keys (i, p, q, d, e, t, o, X, Y, w,
                // h, x, y, z, R, ...): ignored for transmit/display.
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
        cursor_movement_C,
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
    payload_cap: usize,
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
        .filter(|&n| n <= payload_cap)
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

fn parse_u8(value: &[u8]) -> Option<u8> {
    if value.is_empty() || value.len() > 3 {
        return None;
    }
    let mut acc: u8 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(b - b'0')?;
    }
    Some(acc)
}

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
    fn raw_claim_over_img1_payload_budget_is_rejected_before_emit() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2048,v=2048,m=0;AA=="),
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim)
        ));
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
    fn decode_cap_default_mirrors_rich_decode() {
        assert_eq!(
            KittyApcAssembler::new().decode_cap(),
            KITTY_APC_DECODE_MAX_BYTES
        );
    }

    #[test]
    fn single_shot_decode_is_capped_before_decode() {
        // A single packet may not bypass the decode cap via the larger
        // ledger: the decoded size bound applies to lone transmissions too.
        let mut assembler = KittyApcAssembler::with_caps(4096, 9);
        // 16 base64 chars decode to 12 bytes > cap 9: rejected.
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        // Encoded length beyond what the decode cap can yield is refused
        // before any decode allocation (20 chars > 16 allowed for cap 9).
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        // Exactly at the cap still completes (12 chars -> 9 bytes).
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAA") {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), 9),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn chunked_assembly_respects_decode_cap() {
        let mut assembler = KittyApcAssembler::with_caps(4096, 9);
        match assembler.feed(b"Gf=24,s=3,v=1,m=1;AAAAAAAA") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // 8 + 8 encoded chars decode to 12 bytes > cap 9: drop, warn, no emit.
        match assembler.feed(b"Gm=0;AAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        // Reusable afterwards: a capped lone single-shot fits.
        match assembler.feed(b"Gf=24,s=1,v=1,m=0;AAAAAAAAAAAA") {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), 9),
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

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            out.push(ALPHABET[(first >> 2) as usize] as char);
            out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(third & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn img1_payload_below_at_and_above_boundary_is_bounded() {
        let cap = KITTY_APC_LEDGER_CAP;
        for (len, should_complete) in [(cap - 1, true), (cap, true), (cap + 1, false)] {
            let payload = vec![0x5a; len];
            let encoded = base64_encode(&payload);
            let mut raw = Vec::with_capacity(encoded.len() + 16);
            raw.extend_from_slice(b"Gf=100,m=0;");
            raw.extend_from_slice(encoded.as_bytes());
            let mut assembler = KittyApcAssembler::new();
            match assembler.feed(&raw) {
                KittyFeedOutcome::Completed(done) if should_complete => {
                    assert_eq!(done.payload.len(), len);
                }
                KittyFeedOutcome::Rejected(KittyApcReject::Oversize) if !should_complete => {}
                other => panic!("unexpected boundary outcome for {len}: {other:?}"),
            }
            assert!(assembler.peak_memory() <= cap);
            assert!(
                assembler.peak_total_memory()
                    <= cap + KITTY_APC_MAX_CONTROL_BYTES + KITTY_APC_CODEC_SCRATCH_BYTES
            );
        }
    }

    #[test]
    fn small_cap_continuation_is_exact_and_over_budget_is_rejected() {
        let cap = 9;
        let encoded = base64_encode(&vec![0x33; cap]);
        let split = encoded.len() - 4;
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        let mut first = b"Gf=100,m=1;".to_vec();
        first.extend_from_slice(&encoded.as_bytes()[..split]);
        match assembler.feed(&first) {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        let mut final_chunk = b"Gm=0;".to_vec();
        final_chunk.extend_from_slice(&encoded.as_bytes()[split..]);
        match assembler.feed(&final_chunk) {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), cap),
            other => panic!("expected completion, got {other:?}"),
        }
        assert!(assembler.peak_memory() <= cap);

        let first = base64_encode(&[0x33; 6]);
        let tail = base64_encode(&[0x33; 4]);
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        let mut first_chunk = b"Gf=100,m=1;".to_vec();
        first_chunk.extend_from_slice(first.as_bytes());
        assert!(matches!(
            assembler.feed(&first_chunk),
            KittyFeedOutcome::NeedMore { .. }
        ));
        let mut final_chunk = b"Gm=0;".to_vec();
        final_chunk.extend_from_slice(tail.as_bytes());
        assert!(matches!(
            assembler.feed(&final_chunk),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(!assembler.has_pending());
        assert!(assembler.peak_memory() <= cap);
    }

    #[test]
    fn first_chunk_over_budget_is_rejected_before_decode() {
        let cap = 9;
        let encoded = base64_encode(&vec![0x33; cap + 1]);
        let mut raw = b"Gf=100,m=1;".to_vec();
        raw.extend_from_slice(encoded.as_bytes());
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        assert!(matches!(
            assembler.feed(&raw),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(!assembler.has_pending());
        assert!(assembler.peak_memory() <= cap);
    }

    #[test]
    fn caps_mirror_canonical_crates() {
        assert_eq!(KITTY_APC_LEDGER_CAP, 4 * 1024 * 1024);
        assert_eq!(KITTY_APC_DECODE_MAX_DIMENSION, 8192);
        assert_eq!(KITTY_APC_DECODE_MAX_PIXELS, 4096 * 4096);
        assert_eq!(KITTY_APC_DECODE_MAX_BYTES, 64 * 1024 * 1024);
    }
}
