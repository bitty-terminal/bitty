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
use crate::kitty_apc::{KittyApcAssembler, KittyFeedOutcome};
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
/// `SOS/PM/APC` strings inert with no callback. Complete `APC` buffers are
/// fed to [`KittyApcAssembler`] (base64 unwrap, `m=` reassembly under the
/// ledger cap); completed transmissions emit
/// [`TerminalAction::KittyGraphics`]. All other `APC` (and `PM`/`SOS`,
/// which stay with `vte`) remain inert.
pub struct Parser {
    state_machine: vte::Parser,
    dcs: PendingDcs,
    kitty: KittyApcAssembler,
    apc_buf: Vec<u8>,
    in_apc: bool,
    apc_discarding: bool,
    held_esc: bool,
    held_in_apc: bool,
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
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
        }
    }

    /// Creates a parser with a custom kitty ledger cap (tests exercise cap
    /// behavior without allocating hundreds of megabytes).
    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
            kitty: KittyApcAssembler::with_ledger_cap(ledger_cap),
            apc_buf: Vec::new(),
            in_apc: false,
            apc_discarding: false,
            held_esc: false,
            held_in_apc: false,
        }
    }

    /// Kitty ledger cap in effect (max raw `APC` / encoded bytes in flight).
    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.kitty.ledger_cap()
    }

    /// Whether a kitty `m=1` stream is open awaiting more chunks.
    #[must_use]
    pub fn has_pending_kitty(&self) -> bool {
        self.kitty.has_pending()
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
            in_apc,
            apc_discarding,
            held_esc,
            held_in_apc,
        } = self;
        let mut bridge = Bridge { emit, dcs };
        let mut i = 0;

        // Resolve a trailing ESC held from the previous call.
        if *held_esc {
            if bytes.is_empty() {
                return;
            }
            let next = bytes[0];
            if *held_in_apc {
                *held_esc = false;
                if next == b'\\' {
                    i = 1;
                    terminate_apc(&mut bridge, kitty, apc_buf, in_apc, apc_discarding);
                } else if *apc_discarding {
                    // Over-cap discard continues: the held ESC was payload.
                    i = 1;
                } else {
                    // Abort the raw APC (malformed: ESC without ST).
                    apc_buf.clear();
                    *in_apc = false;
                    if next == b'_' {
                        *in_apc = true;
                        *apc_discarding = false;
                        i = 1;
                    } else {
                        state_machine.advance(&mut bridge, &[0x1B]);
                        i = 0;
                    }
                }
            } else {
                *held_esc = false;
                if next == b'_' {
                    *in_apc = true;
                    *apc_discarding = false;
                    apc_buf.clear();
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
                        terminate_apc(&mut bridge, kitty, apc_buf, in_apc, apc_discarding);
                    } else if *apc_discarding {
                        // Stay discarding; the ESC was over-cap payload.
                        i += 1;
                    } else {
                        apc_buf.clear();
                        *in_apc = false;
                        if nxt == b'_' {
                            *in_apc = true;
                            *apc_discarding = false;
                            i += 2;
                        } else {
                            state_machine.advance(&mut bridge, &[0x1B]);
                            i += 1;
                        }
                    }
                } else if b == 0x07 || b == 0x9C {
                    i += 1;
                    terminate_apc(&mut bridge, kitty, apc_buf, in_apc, apc_discarding);
                } else if b == 0x18 || b == 0x1A {
                    i += 1;
                    apc_buf.clear();
                    *in_apc = false;
                    *apc_discarding = false;
                    state_machine.advance(&mut bridge, &[b]);
                } else if *apc_discarding {
                    i += 1;
                } else if apc_buf.len() >= kitty.ledger_cap() {
                    apc_buf.clear();
                    *apc_discarding = true;
                    // The current chunk is lost, so any in-flight `m=` stream
                    // it belonged to is corrupted: drop it fail-closed too.
                    kitty.abort();
                    eprintln!("bitty: rejecting kitty APC: raw exceeds ledger cap: stored nothing");
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
                        *in_apc = true;
                        *apc_discarding = false;
                        apc_buf.clear();
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

/// Completes one `APC` buffer: routes `G` through the kitty assembler and
/// emits [`TerminalAction::KittyGraphics`] on success. Over-cap discards
/// clear silently here (already warned at overflow); assembler rejections
/// already warned inside [`KittyApcAssembler`].
fn terminate_apc<F: FnMut(TerminalAction)>(
    bridge: &mut Bridge<'_, F>,
    kitty: &mut KittyApcAssembler,
    apc_buf: &mut Vec<u8>,
    in_apc: &mut bool,
    apc_discarding: &mut bool,
) {
    if *apc_discarding {
        *in_apc = false;
        *apc_discarding = false;
        apc_buf.clear();
        return;
    }
    let raw = std::mem::take(apc_buf);
    *in_apc = false;
    match kitty.feed(&raw) {
        KittyFeedOutcome::NeedMore { .. } | KittyFeedOutcome::Rejected(_) => {}
        KittyFeedOutcome::Completed(done) => {
            bridge.emit(TerminalAction::KittyGraphics {
                format_f: done.format_f,
                width_s: done.width_s,
                height_v: done.height_v,
                action_a: done.action_a,
                cols_c: done.cols_c,
                rows_r: done.rows_r,
                payload: done.payload,
            });
        }
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
