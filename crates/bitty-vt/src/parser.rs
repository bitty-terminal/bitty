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
use crate::kitty_apc::{KITTY_APC_MAX_CONTROL_BYTES, KittyApcAssembler, KittyFeedOutcome};
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
pub struct Parser {
    state_machine: vte::Parser,
    dcs: PendingDcs,
    kitty: KittyApcAssembler,
    apc_buf: Vec<u8>,
    apc_payload: bool,
    in_apc: bool,
    apc_discarding: bool,
    held_esc: bool,
    held_in_apc: bool,
    reject_log: RejectLog,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("dcs", &self.dcs)
            .field("in_apc", &self.in_apc)
            .field("kitty_pending", &self.kitty.has_pending())
            .finish_non_exhaustive()
    }
}

/// Marker for a device-control string opened by `hook` and closed by
/// `unhook`, so unmapped strings can be reported once, deterministically.
#[derive(Debug, Default)]
struct PendingDcs {
    active: bool,
    final_byte: u8,
    intermediates: [u8; 2],
}

impl Parser {
    /// Creates a fresh parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
            kitty: KittyApcAssembler::new(),
            apc_buf: Vec::new(),
            apc_payload: false,
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
            reject_log: RejectLog::default(),
        }
    }

    /// Creates a parser with a custom IMG-1 payload cap.
    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
            kitty: KittyApcAssembler::with_ledger_cap(ledger_cap),
            apc_buf: Vec::new(),
            apc_payload: false,
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
            reject_log: RejectLog::default(),
        }
    }

    /// Kitty payload cap in effect.
    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.kitty.ledger_cap()
    }

    /// Whether a kitty `m=1` stream is open awaiting more chunks.
    #[must_use]
    pub fn has_pending_kitty(&self) -> bool {
        self.kitty.has_pending()
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
            apc_buf,
            apc_payload,
            in_apc,
            apc_discarding,
            held_esc,
            held_in_apc,
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
                    state_machine.advance(&mut bridge, &[next]);
                    i = 1;
                } else if *apc_discarding {
                    i = 1;
                } else if next == b'_' {
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                    i = 1;
                } else {
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    state_machine.advance(&mut bridge, &[0x1B]);
                    i = 0;
                }
            } else {
                *held_esc = false;
                if next == b'_' {
                    begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                    i = 1;
                } else {
                    state_machine.advance(&mut bridge, &[0x1B]);
                    i = 0;
                }
            }
        }

        while i < bytes.len() {
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
                        i += 1;
                    } else {
                        clear_apc_header(kitty, apc_buf);
                        *in_apc = false;
                        *apc_payload = false;
                        kitty.abort();
                        if nxt == b'_' {
                            begin_apc(kitty, apc_buf, apc_payload, in_apc, apc_discarding);
                            i += 2;
                        } else {
                            state_machine.advance(&mut bridge, &[0x1B]);
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
                    state_machine.advance(&mut bridge, &[b]);
                } else if *apc_discarding {
                    i += 1;
                } else if *apc_payload {
                    if kitty.push_payload(std::slice::from_ref(&b)).is_err() {
                        *apc_payload = false;
                        *apc_discarding = true;
                    }
                    i += 1;
                } else if b == b';' {
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
                    } else {
                        state_machine.advance(&mut bridge, &[0x1B]);
                        i += 1;
                    }
                } else {
                    // Note: C1 APC (0x9F) is intentionally not intercepted:
                    // it overlaps UTF-8 continuation bytes (e.g. `🎉` contains
                    // 0x9F), and kitty/chafa always use `ESC _`.
                    state_machine.advance(&mut bridge, &[b]);
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
    *apc_payload = false;
    if let Some(KittyFeedOutcome::Completed(done)) = outcome {
        bridge.emit(TerminalAction::KittyGraphics {
            format_f: done.format_f,
            width_s: done.width_s,
            height_v: done.height_v,
            action_a: done.action_a,
            cols_c: done.cols_c,
            rows_r: done.rows_r,
            cursor_movement_C: done.cursor_movement_C,
            payload: done.payload,
        });
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
