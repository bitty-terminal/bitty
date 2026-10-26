//! Byte-stream VT parser producing [`TerminalAction`] values.
//!
//! Wraps the adopted `vte` state machine (ADR-0004) behind this crate's own
//! API: `vte` types never appear in the public surface. The [`Perform`]
//! implementation maps every callback onto the RFC action interface; UTF-8
//! collection is delegated to `vte`, whose decoder replaces invalid bytes
//! with U+FFFD, matching the single specified replacement policy of the
//! RFC's parser obligations.
//!
//! Bounded-parsing obligations are satisfied jointly: `vte` bounds parameter
//! count (extra parameters are dropped and flagged via `ignore`),
//! parameter magnitude (`u16` saturation), and OSC payload size (fixed
//! buffer); this module additionally bounds every materialized string or
//! byte payload through the [`crate::bounded`] types. Exceeding any limit
//! therefore yields a well-defined truncated action rather than unbounded
//! growth (threat T-01).
//!
//! The crate holds no terminal state: the only memory retained across input
//! chunks is the `vte` machine itself plus a pending device-control-string
//! marker needed to report terminated-but-unmapped string sequences.

use crate::action::TerminalAction;
use crate::diag::{RejectLog, warn_rejection};
use crate::iterm2_osc::{Iterm2OscScanner, OscOutcome};
use crate::kitty_apc::{KITTY_APC_MAX_CONTROL_BYTES, KittyApcAssembler, KittyFeedOutcome};
use crate::kitty_font_query::{KITTY_FONT_QUERY_WIRE_MAX_BYTES, parse_kitty_font_query};
use vte::Params;

mod dispatch;
mod sgr;

#[cfg(test)]
mod tests;

/// Stateful byte-stream parser: wraps a `vte::Parser` and translates its
/// callbacks into semantic [`TerminalAction`] values via the `emit` sink.
///
/// No terminal state lives here; see the crate-level documentation for the
/// parser/state split mandated by ADR-0003.
///
/// Kitty `APC G` is pre-scanned here because `vte` 0.15 leaves
/// `SOS/PM/APC` strings inert with no callback. The control header is bounded
/// and payload bytes are streamed into [`KittyApcAssembler`] under the IMG-1
/// parser budget; completed transmissions emit
/// [`TerminalAction::KittyGraphics`]. All other `APC` (and `PM`/`SOS`,
/// which stay with `vte`) remain inert.
///
/// `OSC 1337 ; File=` is likewise pre-scanned (see [`crate::iterm2_osc`]).
/// Because that diversion removes the sequence's `ESC` from `vte`'s input,
/// a private `VteString` tracker records whether a string `vte` owns is
/// still open, and the diversion feeds a terminating `ESC \` first
/// (CTX-1590 review MF-1).
pub struct Parser {
    state_machine: vte::Parser,
    dcs: PendingDcs,
    kitty: KittyApcAssembler,
    iterm2: Iterm2OscScanner,
    apc_buf: Vec<u8>,
    apc_payload: bool,
    in_apc: bool,
    apc_discarding: bool,
    held_esc: bool,
    held_in_apc: bool,
    vte_string: VteString,
    reject_log: RejectLog,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("dcs", &self.dcs)
            .field("in_apc", &self.in_apc)
            .field("kitty_pending", &self.kitty.has_pending())
            .field("iterm2_pending", &self.iterm2.is_active())
            .field("vte_string", &self.vte_string)
            .finish_non_exhaustive()
    }
}

/// Marker for a device-control string opened by `hook` and closed by
/// `unhook`, so unmapped strings can be reported once, deterministically.
///
/// DCS `ESC P +q ... ST` is also the kitty font-query envelope (CTX-1370),
/// so the hook records that header and `put` captures a fixed-cap payload
/// for classification at `unhook`. Every other DCS keeps its payload inert
/// and is reported as [`UnrecognizedSequence`] exactly as before.
#[derive(Debug)]
struct PendingDcs {
    active: bool,
    final_byte: u8,
    intermediates: [u8; 2],
    font_query: bool,
    font_payload: [u8; KITTY_FONT_QUERY_WIRE_MAX_BYTES],
    font_len: usize,
    font_overflow: bool,
}

impl Default for PendingDcs {
    fn default() -> Self {
        Self {
            active: false,
            final_byte: 0,
            intermediates: [0; 2],
            font_query: false,
            font_payload: [0; KITTY_FONT_QUERY_WIRE_MAX_BYTES],
            font_len: 0,
            font_overflow: false,
        }
    }
}

/// Tracks the reachable string state of the wrapped `vte` machine so a
/// diversion can close it before taking bytes off the wire (CTX-1590 review
/// MF-1/MF-2).
///
/// `vte` ends an `OSC` on `BEL`, `CAN`, `SUB`, or `ESC`, but a
/// `DCS`/`SOS`/`PM` only on `CAN`, `SUB`, or `ESC`; the kind is kept so a
/// forwarded `BEL` never clears a string that is still open. The tracked
/// state only has to answer "would the next forwarded byte land inside a
/// string?", so non-string sequence states collapse into [`Self::Ground`]
/// (nothing to close) and every string state into [`Self::Other`] (only
/// terminators can leave it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum VteString {
    /// Not inside a string (`vte` Ground, or mid-CSI/mid-dispatch).
    #[default]
    Ground,
    /// A bare `ESC` was forwarded; the next forwarded byte decides.
    Escape,
    /// An `OSC` string, collected in `vte`'s uncapped buffer.
    Osc,
    /// A `DCS`, `SOS`, or `PM` string, whose payload stays with `vte`.
    Other,
}

impl VteString {
    /// Tracked state after `byte` is forwarded to `vte`.
    fn after(self, byte: u8) -> Self {
        match self {
            // `vte` Ground prints until `ESC` enters the sequence layer.
            Self::Ground => {
                if byte == 0x1B {
                    Self::Escape
                } else {
                    Self::Ground
                }
            }
            // `advance_esc`: `ESC` stays, C0/CAN/SUB behave as anywhere,
            // `]` opens an OSC, `P`/`X`/`^`/`_` open a string, and every
            // other byte dispatches, starts a CSI/charset, or collects an
            // intermediate, none of which a string can start from.
            Self::Escape => match byte {
                0x1B => Self::Escape,
                0x18 | 0x1A => Self::Ground,
                b']' => Self::Osc,
                b'P' | b'X' | b'^' | b'_' => Self::Other,
                0x00..=0x17 | 0x19 | 0x1C..=0x2F => Self::Escape,
                _ => Self::Ground,
            },
            // `advance_osc_string`: collected until `BEL`/`CAN`/`SUB`/`ESC`.
            Self::Osc => match byte {
                0x07 | 0x18 | 0x1A => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Osc,
            },
            // `advance_dcs_passthrough` / `anywhere`: `CAN`/`SUB`/`ESC` end
            // it. C1 `ST` (0x9C) is left open on purpose: it only unhooks an
            // already-hooked DCS, so a stale flag can only add one inert
            // `ESC \` later.
            Self::Other => match byte {
                0x18 | 0x1A => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Other,
            },
        }
    }

    /// Whether `vte` holds state that `ESC \` closes without an action:
    /// an open string, or the pending `ESC` of an abandoned sequence.
    const fn needs_close(self) -> bool {
        !matches!(self, Self::Ground)
    }
}

impl Parser {
    /// Creates a fresh parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
            kitty: KittyApcAssembler::new(),
            iterm2: Iterm2OscScanner::default(),
            apc_buf: Vec::new(),
            apc_payload: false,
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
            vte_string: VteString::default(),
            reject_log: RejectLog::default(),
        }
    }

    /// Creates a parser with a custom IMG-1 payload cap.
    ///
    /// The cap bounds compressed (`f=100`, `o=`) and undeclared-size kitty
    /// streams. Uncompressed raw `f=24`/`f=32` streams with an `s`/`v` claim
    /// are bounded by their declared size under the decode caps instead.
    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
            kitty: KittyApcAssembler::with_ledger_cap(ledger_cap),
            iterm2: Iterm2OscScanner::default(),
            apc_buf: Vec::new(),
            apc_payload: false,
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
            vte_string: VteString::default(),
            reject_log: RejectLog::default(),
        }
    }

    /// Kitty IMG-1 payload cap in effect for compressed or undeclared-size
    /// streams.
    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.kitty.ledger_cap()
    }

    /// Whether a kitty `m=1` stream is open awaiting more chunks.
    #[must_use]
    pub fn has_pending_kitty(&self) -> bool {
        self.kitty.has_pending()
    }

    /// Returns the number of encoded bytes currently buffered in a pending
    /// Kitty graphics stream (the base64-encoded input), plus any held APC
    /// header/payload bytes. Used by the runtime to charge in-flight streams
    /// against IMG-4 (256 MiB total across all parsers). Returns 0 when idle.
    pub fn pending_kitty_encoded_bytes(&self) -> usize {
        let kitty_bytes = self.kitty.pending_encoded_len();
        let apc_bytes = if self.in_apc { self.apc_buf.len() } else { 0 };
        kitty_bytes + apc_bytes
    }

    #[cfg(test)]
    pub(crate) fn kitty_peak_memory(&self) -> usize {
        self.kitty.peak_memory()
    }

    #[cfg(test)]
    pub(crate) fn kitty_peak_total_memory(&self) -> usize {
        self.kitty.peak_total_memory()
    }

    /// Feeds raw PTY bytes into the parser, emitting one [`TerminalAction`]
    /// per resolved semantic event into `emit`.
    ///
    /// Parsing may be resumed across arbitrary chunk boundaries; splitting
    /// the same byte stream differently does not change the emitted action
    /// sequence. `APC` (`ESC _ ... ST`) never reaches `vte`: complete
    /// buffers are routed to the kitty assembler, and completed `G`
    /// transmissions emit [`TerminalAction::KittyGraphics`] in stream order.
    /// Unterminated `APC` and held trailing `ESC` are buffered for the next
    /// call.
    pub fn advance<F>(&mut self, bytes: &[u8], emit: F)
    where
        F: FnMut(TerminalAction),
    {
        let Self {
            state_machine,
            dcs,
            kitty,
            iterm2,
            apc_buf,
            apc_payload,
            in_apc,
            apc_discarding,
            held_esc,
            held_in_apc,
            vte_string,
            reject_log,
        } = self;
        let mut bridge = Bridge { emit, dcs };
        let mut i = 0;

        if *held_esc {
            if bytes.is_empty() {
                return;
            }
            let next = bytes[0];
            if *held_in_apc {
                *held_esc = false;
                if next == b'\\' || (*apc_discarding && (next == 0x07 || next == 0x9C)) {
                    i = 1;
                    terminate_apc(
                        &mut bridge,
                        kitty,
                        apc_buf,
                        apc_payload,
                        in_apc,
                        apc_discarding,
                    );
                } else if next == 0x18 || next == 0x1A {
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    *apc_discarding = false;
                    kitty.abort();
                    feed_vte(state_machine, &mut bridge, vte_string, &[next]);
                    i = 1;
                } else if *apc_discarding {
                    // Held ESC is discarded. A following ESC is re-examined
                    // by the main loop (it may start `ESC \`), matching the
                    // in-buffer path; any other byte is discarded with it.
                    if next == 0x1B {
                        kitty.note_interleaved(1);
                        i = 0;
                    } else {
                        kitty.note_interleaved(2);
                        i = 1;
                    }
                } else if next == b'_' {
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                    i = 1;
                } else if next == b']' {
                    // Aborted APC followed by `OSC 1337 ; File=` (CTX-1590
                    // review MF-2): the aborting `ESC` must not hand the body
                    // to `vte`'s uncapped OSC collector. Route into the same
                    // diversion the ground path uses.
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    close_vte_string(state_machine, &mut bridge, vte_string);
                    kitty.note_interleaved(1);
                    iterm2.begin();
                    i = 1;
                } else {
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    feed_vte(state_machine, &mut bridge, vte_string, &[0x1B]);
                    i = 0;
                }
            } else {
                *held_esc = false;
                if next == b'_' {
                    begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                    i = 1;
                } else if next == b']' {
                    // Possible `OSC 1337 ; File=` (CTX-1590): the introducer
                    // is consumed here and the scanner decides whether the
                    // prefix matches or must be replayed into `vte`.
                    close_vte_string(state_machine, &mut bridge, vte_string);
                    kitty.note_interleaved(1);
                    iterm2.begin();
                    i = 1;
                } else {
                    feed_vte(state_machine, &mut bridge, vte_string, &[0x1B]);
                    i = 0;
                }
            }
        }

        while i < bytes.len() {
            if iterm2.is_active() {
                let byte = bytes[i];
                match iterm2.push(byte) {
                    OscOutcome::Consumed => {
                        kitty.note_interleaved(1);
                        i += 1;
                        continue;
                    }
                    OscOutcome::PassThrough => {
                        feed_vte(state_machine, &mut bridge, vte_string, iterm2.replay());
                        iterm2.clear_replay();
                        // Fall through: the current byte takes the normal
                        // path now that `vte` has the whole OSC prefix.
                    }
                    OscOutcome::Finished => {
                        finish_iterm2(&mut bridge, iterm2, reject_log);
                        i += 1;
                        continue;
                    }
                    OscOutcome::FinishedThenPassThrough => {
                        finish_iterm2(&mut bridge, iterm2, reject_log);
                        feed_vte(state_machine, &mut bridge, vte_string, iterm2.replay());
                        iterm2.clear_replay();
                    }
                }
            }
            if *in_apc {
                let b = bytes[i];
                if b == 0x1B {
                    if i + 1 >= bytes.len() {
                        *held_esc = true;
                        *held_in_apc = true;
                        break;
                    }
                    let nxt = bytes[i + 1];
                    if nxt == b'\\' {
                        i += 2;
                        terminate_apc(
                            &mut bridge,
                            kitty,
                            apc_buf,
                            apc_payload,
                            in_apc,
                            apc_discarding,
                        );
                    } else if *apc_discarding {
                        // A discarded ESC still counts toward the stall
                        // bound, so an ESC flood cannot evade it.
                        kitty.note_interleaved(1);
                        i += 1;
                    } else {
                        clear_apc_header(kitty, apc_buf);
                        *in_apc = false;
                        *apc_payload = false;
                        kitty.abort();
                        if nxt == b'_' {
                            begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                            i += 2;
                        } else if nxt == b']' {
                            // Aborted APC followed by `OSC 1337 ; File=`
                            // (CTX-1590 review MF-2): divert the body before
                            // `vte` can collect it as an OSC.
                            close_vte_string(state_machine, &mut bridge, vte_string);
                            kitty.note_interleaved(1);
                            iterm2.begin();
                            i += 2;
                        } else {
                            feed_vte(state_machine, &mut bridge, vte_string, &[0x1B]);
                            i += 1;
                        }
                    }
                } else if b == 0x07 || b == 0x9C {
                    i += 1;
                    terminate_apc(
                        &mut bridge,
                        kitty,
                        apc_buf,
                        apc_payload,
                        in_apc,
                        apc_discarding,
                    );
                } else if b == 0x18 || b == 0x1A {
                    i += 1;
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    *apc_discarding = false;
                    kitty.abort();
                    feed_vte(state_machine, &mut bridge, vte_string, &[b]);
                } else if *apc_discarding {
                    // Discarded APC bytes (non-`G` commands, rejected
                    // headers) count toward an open stream's stall bound.
                    kitty.note_interleaved(1);
                    i += 1;
                } else if *apc_payload {
                    if kitty.push_payload(std::slice::from_ref(&b)).is_err() {
                        *apc_payload = false;
                        *apc_discarding = true;
                    }
                    i += 1;
                } else if b == b';' && !apc_buf.starts_with(b"+q") {
                    match kitty.begin_control(apc_buf) {
                        Ok(()) => {
                            *apc_payload = true;
                            clear_apc_header(kitty, apc_buf);
                        }
                        Err(_) => {
                            *apc_discarding = true;
                            clear_apc_header(kitty, apc_buf);
                        }
                    }
                    i += 1;
                } else if apc_buf.len() >= KITTY_APC_MAX_CONTROL_BYTES
                    || !kitty.reserve_header_byte()
                {
                    clear_apc_header(kitty, apc_buf);
                    *apc_discarding = true;
                    kitty.abort();
                    if let Some(occurrence) = reject_log.record() {
                        warn_rejection(
                            occurrence,
                            "bitty: rejecting kitty APC: control exceeds parser budget: stored nothing",
                        );
                    }
                    i += 1;
                } else {
                    apc_buf.push(b);
                    i += 1;
                }
            } else {
                let b = bytes[i];
                if b == 0x1B {
                    if i + 1 >= bytes.len() {
                        *held_esc = true;
                        *held_in_apc = false;
                        break;
                    }
                    if bytes[i + 1] == b'_' {
                        begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                        i += 2;
                    } else if bytes[i + 1] == b']' {
                        // Possible `OSC 1337 ; File=`: see the held-ESC arm.
                        close_vte_string(state_machine, &mut bridge, vte_string);
                        kitty.note_interleaved(1);
                        iterm2.begin();
                        i += 2;
                    } else {
                        kitty.note_interleaved(1);
                        feed_vte(state_machine, &mut bridge, vte_string, &[0x1B]);
                        i += 1;
                    }
                } else {
                    // Note: C1 APC (0x9F) is intentionally not intercepted:
                    // it overlaps UTF-8 continuation bytes (e.g. `🎉` contains
                    // 0x9F), and kitty/chafa always use `ESC _`.
                    // Text interleaved with an open chunked kitty stream
                    // counts toward its stall bound (no-op when idle).
                    kitty.note_interleaved(1);
                    feed_vte(state_machine, &mut bridge, vte_string, &[b]);
                    i += 1;
                }
            }
        }
    }
}

fn begin_apc(
    kitty: &mut KittyApcAssembler,
    apc_buf: &mut Vec<u8>,
    apc_payload: &mut bool,
    in_apc: &mut bool,
    apc_discarding: &mut bool,
) {
    clear_apc_header(kitty, apc_buf);
    *apc_payload = false;
    *in_apc = true;
    *apc_discarding = false;
}

/// Feeds bytes to `vte` and keeps [`VteString`] in step with them.
///
/// Every byte the parser hands to `vte` must go through here (or the state
/// tracker diverges and a later diversion stops closing a string `vte` still
/// owns). Feeding the same bytes one call at a time is equivalent to feeding
/// them in one call: `vte` carries its state across calls.
fn feed_vte<F: FnMut(TerminalAction)>(
    state_machine: &mut vte::Parser,
    bridge: &mut Bridge<'_, F>,
    vte_string: &mut VteString,
    bytes: &[u8],
) {
    for &byte in bytes {
        *vte_string = vte_string.after(byte);
    }
    state_machine.advance(bridge, bytes);
}

/// Closes whatever `vte` still holds before an `ESC ]` is diverted away from
/// it (CTX-1590 review MF-1).
///
/// The diversion consumes the `ESC` that `vte` would have used to end an open
/// string, so without this an OSC it received through the replay path keeps
/// collecting the rest of the stream in its uncapped buffer. `ESC \` is
/// action-free in Ground and Escape, ends an `OSC`/`DCS` and dispatches the
/// pending OSC first: exactly the effect that `ESC` had before the diversion.
fn close_vte_string<F: FnMut(TerminalAction)>(
    state_machine: &mut vte::Parser,
    bridge: &mut Bridge<'_, F>,
    vte_string: &mut VteString,
) {
    if vte_string.needs_close() {
        feed_vte(state_machine, bridge, vte_string, b"\x1b\\");
    }
}

fn clear_apc_header(kitty: &mut KittyApcAssembler, apc_buf: &mut Vec<u8>) {
    kitty.release_header(apc_buf.len());
    apc_buf.clear();
}

fn terminate_apc<F: FnMut(TerminalAction)>(
    bridge: &mut Bridge<'_, F>,
    kitty: &mut KittyApcAssembler,
    apc_buf: &mut Vec<u8>,
    apc_payload: &mut bool,
    in_apc: &mut bool,
    apc_discarding: &mut bool,
) {
    if *apc_discarding {
        *in_apc = false;
        *apc_discarding = false;
        *apc_payload = false;
        clear_apc_header(kitty, apc_buf);
        return;
    }
    // Kitty font queries (`APC +q ... ST`, CTX-1370) are not graphics
    // transmissions: classify the bounded payload and emit one inert action.
    // Font values are configuration-owned, so the runtime synthesizes the
    // reply through the existing bounded `Reply` channel.
    if !*apc_payload {
        if let Some(query) = parse_kitty_font_query(apc_buf) {
            clear_apc_header(kitty, apc_buf);
            *in_apc = false;
            bridge.emit(TerminalAction::KittyFontQuery { query });
            return;
        }
    }
    let outcome = if *apc_payload {
        Some(kitty.finish())
    } else {
        match kitty.begin_control(apc_buf) {
            Ok(()) => Some(kitty.finish()),
            Err(_) => None,
        }
    };
    clear_apc_header(kitty, apc_buf);
    *in_apc = false;
    if let Some(KittyFeedOutcome::Completed(done)) = outcome {
        bridge.emit(TerminalAction::KittyGraphics {
            format_f: done.format_f,
            width_s: done.width_s,
            height_v: done.height_v,
            action_a: done.action_a,
            cols_c: done.cols_c,
            rows_r: done.rows_r,
            cursor_movement_c: done.cursor_movement_c,
            payload: done.payload,
            control: done.keys,
        });
    }
}

/// Emits the finished iTerm2 inline image or its counted refusal.
fn finish_iterm2<F: FnMut(TerminalAction)>(
    bridge: &mut Bridge<'_, F>,
    iterm2: &mut Iterm2OscScanner,
    reject_log: &mut RejectLog,
) {
    match iterm2.take_result() {
        Some(Ok(request)) => bridge.emit(crate::iterm2_osc::image_action(request)),
        Some(Err(reason)) => {
            if let Some(occurrence) = reject_log.record() {
                warn_rejection(
                    occurrence,
                    &format!("bitty: rejecting OSC 1337 inline image ({reason}): stored nothing"),
                );
            }
            bridge.emit(crate::iterm2_osc::rejected_action(reason));
        }
        None => {}
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

struct Bridge<'a, F> {
    emit: F,
    dcs: &'a mut PendingDcs,
}

impl<F: FnMut(TerminalAction)> Bridge<'_, F> {
    fn emit(&mut self, action: TerminalAction) {
        (self.emit)(action);
    }
}

fn sub_params(params: &Params, index: usize) -> Option<&[u16]> {
    params.iter().nth(index)
}
