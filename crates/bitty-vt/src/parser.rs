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
//! count (extra parameters are dropped and flagged via `ignore`) and
//! parameter magnitude (`u16` saturation) but not string payload size in its
//! `std` build; this module additionally bounds every materialized string or
//! byte payload through the [`crate::bounded`] types and enforces one
//! parser-wide control-string byte cap ([`CONTROL_STRING_MAX_BYTES`]) over
//! every string `vte` collects and over the diverted iTerm2 candidate.
//! Exceeding any limit therefore yields a well-defined truncated action
//! rather than unbounded growth (threat T-01).
//!
//! The control-string cap is the bound `vte` itself does not provide: its
//! `std` OSC collector is a growable `Vec` (`MAX_OSC_RAW` exists only in the
//! `no_std` build), so an unterminated `OSC`/`DCS`/`SOS`/`PM` grows memory
//! 1:1 with input. An over-cap string is aborted with an injected `CAN`,
//! its tail is dropped up to the real terminator, and the refusal is emitted
//! as [`TerminalAction::ControlStringRejected`] for telemetry. The bytes
//! forwarded before the abort are *dispatched first* — `vte` runs its OSC
//! dispatch (or `unhook`) on the injected `CAN` — so a bounded, truncated
//! prefix can still apply one mapped action (a truncated title, hyperlink,
//! zone mark, cwd, palette entry, or a clipboard write still subject to the
//! clipboard gate); only the tail past the cap is discarded. Every
//! terminated sequence that stays under the cap is dispatched exactly as
//! before (CTX-1592, bitty#2704).
//!
//! The crate holds no terminal state, but it retains bounded parser buffers
//! across input chunks: the `vte` machine (whose string collector keeps its
//! capacity after a refusal), the pending device-control-string marker, the
//! kitty APC assembler with at most one open chunked stream (IMG-1), the
//! iTerm2 OSC scanner with up to its ~4.19 MiB body buffer, and the APC
//! header buffer.

use crate::action::{ControlStringKind, ControlStringReject, TerminalAction};
use crate::diag::{RejectLog, warn_rejection};
use crate::iterm2_osc::{
    ITERM2_OSC_MAX_BODY_BYTES, ITERM2_OSC_PREFIX, Iterm2OscScanner, OscOutcome,
};
use crate::kitty_apc::{KITTY_APC_MAX_CONTROL_BYTES, KittyApcAssembler, KittyFeedOutcome};
use crate::kitty_font_query::{KITTY_FONT_QUERY_WIRE_MAX_BYTES, parse_kitty_font_query};
use vte::Params;

mod dispatch;
mod sgr;

#[cfg(test)]
mod tests;

/// Parser-wide byte cap for one control string (CTX-1592, bitty#2704).
///
/// A control string is an `OSC`, `DCS`, `SOS`, `PM`, or `APC` sequence, or
/// the diverted iTerm2 `OSC 1337;File=` candidate. The cap counts the bytes
/// `vte` collects for the string *body*; the `ESC ]`/`ESC P`/... introducer
/// is not charged. It is sized so the largest legitimate control string the
/// parser accepts fits exactly: the iTerm2 body cap
/// ([`ITERM2_OSC_MAX_BODY_BYTES`], the 4 MiB wire payload plus its attribute
/// header) plus the `ESC ]` introducer and `1337;File=` prefix, i.e.
/// 4 MiB + 4 KiB + 13 = 4,198,413 bytes. The diverted candidate is the one
/// path that charges an introducer (its `ESC ]` never reaches `vte`), so it
/// lands on the same value for the same body.
///
/// This is the bound `vte` does not provide: its `std` OSC collector is a
/// growable `Vec` (`MAX_OSC_RAW` exists only in the `no_std` build), so an
/// unterminated string grows memory 1:1 with input. On overflow the parser
/// injects `CAN` (`0x18`), which makes `vte` dispatch/abort the string and
/// clear the collected body, then swallows the rest of the string up to its
/// real terminator and emits [`TerminalAction::ControlStringRejected`].
/// `vte` keeps the collector's capacity after the body is cleared, so one
/// parser retains at most one doubling above the cap (8 MiB) once it has
/// refused a string; that is the steady-state memory bound.
///
/// The kitty graphics payload is deliberately outside this budget: it is
/// streamed into [`crate::kitty_apc`] and bounded by the IMG-1
/// ([`crate::kitty_apc::KITTY_APC_LEDGER_CAP`]) and IMG-3
/// ([`crate::kitty_apc::KITTY_APC_DECODE_MAX_BYTES`]) caps instead of being
/// buffered by the parser. Only the parser-handled `APC` bytes that are *not*
/// accepted kitty payload (header, discarded text, rejected payload) count
/// toward this cap.
///
/// Documented beside the other parser bounds in the `bitty-vt` crate-level
/// documentation; the memory and action bounds it implies are pinned by
/// `tests/control_string_cap.rs`.
pub const CONTROL_STRING_MAX_BYTES: usize = ITERM2_OSC_MAX_BODY_BYTES + ITERM2_OSC_PREFIX.len() + 2;

/// `vte` ends a control string on the byte the caller injected to abort an
/// over-cap string.
const CAN: u8 = 0x18;

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
///
/// The same tracker carries the parser-wide control-string budget: every
/// byte of a string `vte` owns, of the diverted iTerm2 candidate, and of a
/// parser-handled `APC` that is not accepted kitty payload is charged
/// against [`CONTROL_STRING_MAX_BYTES`] (CTX-1592). An over-cap `vte` string
/// is aborted with an injected `CAN`; an over-cap `APC` is aborted locally
/// because it never reached `vte`.
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
    control_string: ControlString,
    reject_log: RejectLog,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("dcs", &self.dcs)
            .field("in_apc", &self.in_apc)
            .field("kitty_pending", &self.kitty.has_pending())
            .field("iterm2_pending", &self.iterm2.is_active())
            .field("control_string", &self.control_string)
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
/// `vte` ends an `OSC` on `BEL`, `CAN`, `SUB`, or `ESC`, and a
/// `DCS`/`SOS`/`PM` on `CAN`, `SUB`, or `ESC` (plus C1 `ST` for a hooked
/// `DCS`); the kind is kept so a forwarded `BEL` never clears a string that
/// is still open. The tracked state only has to answer "would the next
/// forwarded byte land inside a string?", so non-string sequence states
/// collapse into [`Self::Ground`] (nothing to close) and every string state
/// into [`Self::Other`] (only terminators can leave it).
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
            // `advance_esc`: `ESC` stays, `CAN`/`SUB` execute and return to
            // Ground, `]` opens an OSC, `P`/`X`/`^`/`_` open a string, and
            // every other dispatchable or collectable byte lands outside a
            // string. `0x7F`/`0x80..=0xFF` are *not* dispatchable: `vte`'s
            // `_ => ()` arm keeps them in `Escape`, so the tracker must too,
            // or a following raw `]` opens an unobserved OSC string.
            Self::Escape => match byte {
                0x1B => Self::Escape,
                0x18 | 0x1A => Self::Ground,
                b']' => Self::Osc,
                b'P' | b'X' | b'^' | b'_' => Self::Other,
                0x00..=0x17 | 0x19 | 0x1C..=0x2F | 0x7F..=0xFF => Self::Escape,
                _ => Self::Ground,
            },
            // `advance_osc_string`: collected until `BEL`/`CAN`/`SUB`/`ESC`.
            Self::Osc => match byte {
                0x07 | 0x18 | 0x1A => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Osc,
            },
            // `advance_dcs_passthrough` / `anywhere`: `CAN`/`SUB`/`ESC` end
            // it. C1 `ST` (0x9C) unhooks only a *hooked* DCS: a DCS header
            // (`DcsEntry`/`DcsParam`/`DcsIntermediate`/`DcsIgnore`) and a
            // SOS/PM string ignore it, so the collapsed state stays `Other`.
            // `feed_vte` consults [`ControlString::dcs_hooked`] for the one
            // family-specific case (CTX-1592).
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

    /// Whether this state is inside a control string (the only states that
    /// grow a `vte` collector).
    const fn is_string(self) -> bool {
        matches!(self, Self::Osc | Self::Other)
    }

    /// Whether `byte` ends a string of this family while its overflow tail
    /// is being discarded (CTX-1592).
    ///
    /// `Some(true)` means "the string ends and `byte` must still be
    /// forwarded" (`ESC` opens whatever sequence follows); `Some(false)`
    /// means the byte is the terminator and is swallowed with the tail.
    /// `Other` needs the DCS substate: C1 `ST` (`0x9C`) unhooks a DCS only
    /// when `vte` had hooked it, and is ignored in a DCS header and in a
    /// SOS/PM string.
    const fn discard_ends_at(self, dcs_hooked: bool, byte: u8) -> Option<bool> {
        match self {
            Self::Osc => match byte {
                0x07 | CAN | 0x1A => Some(false),
                0x1B => Some(true),
                _ => None,
            },
            Self::Other => match byte {
                CAN | 0x1A => Some(false),
                0x1B => Some(true),
                0x9C if dcs_hooked => Some(false),
                _ => None,
            },
            Self::Ground | Self::Escape => None,
        }
    }
}

/// Substate of a `DCS` that `vte` has not hooked yet.
///
/// `vte` 0.15 advances `DcsEntry` → `DcsParam`/`DcsIntermediate` and calls
/// `hook` (moving to `DcsPassthrough`) on a `0x40..=0x7E` byte. `DcsIgnore`
/// is entered from `DcsParam` on `0x3C..=0x3F` and from `DcsIntermediate` on
/// `0x30..=0x3F`, and reaches no hook at all. The distinction matters for
/// the tracker because C1 `ST` (`0x9C`) unhooks only a hooked DCS: in every
/// header substate `vte` ignores `0x9C` (`anywhere`) and stays in the
/// header, so treating it as a terminator would desync the tracker from the
/// machine (CTX-1592 review MF-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DcsHeader {
    /// `DcsEntry`.
    Entry,
    /// `DcsParam`.
    Param,
    /// `DcsIntermediate`.
    Intermediate,
    /// `DcsIgnore`: never hooks, so every byte up to a terminator is inert.
    Ignore,
}

/// Control-string state and the parser-wide byte budget (CTX-1592).
///
/// Wraps the [`VteString`] tracker so the "would the next forwarded byte
/// land inside a string?" answer and the per-string byte count move
/// together: the count can only advance on bytes [`VteString::after`] maps
/// into a string, and it resets exactly when that string ends. It also
/// shadows the `DCS` header substate, which the collapsed [`VteString`]
/// cannot express (CTX-1592 review MF-1).
#[derive(Debug)]
struct ControlString {
    /// Reachable string state of the wrapped `vte` machine.
    state: VteString,
    /// Body bytes forwarded to `vte` for the string [`Self::state`] names.
    bytes: usize,
    /// Family of the open `vte` string, captured at its introducer.
    kind: ControlStringKind,
    /// `DCS` header substate while `vte` has not hooked the string; `None`
    /// for another family, once hooked, or after the string ended.
    dcs_header: Option<DcsHeader>,
    /// `vte` called `hook` for the open `DCS`, so it is in
    /// `DcsPassthrough` and C1 `ST` (`0x9C`) unhooks it. Never set for a
    /// `DCS` header or for another family.
    dcs_hooked: bool,
    /// Set after an overflow: the rest of the string is swallowed until its
    /// real terminator resynchronizes the wire.
    discard: Option<VteString>,
    /// Bytes of the parser-handled `APC` currently open (header and
    /// discarded text; accepted kitty payload uses the IMG ledger instead).
    apc_bytes: usize,
    /// The open `APC` was already refused by the cap; stop charging it.
    apc_refused: bool,
}

impl Default for ControlString {
    fn default() -> Self {
        Self {
            state: VteString::default(),
            bytes: 0,
            kind: ControlStringKind::Osc,
            dcs_header: None,
            dcs_hooked: false,
            discard: None,
            apc_bytes: 0,
            apc_refused: false,
        }
    }
}

impl ControlString {
    /// Whether `vte` holds state that `ESC \` closes without an action.
    const fn needs_close(&self) -> bool {
        self.state.needs_close()
    }

    /// Opens the `vte` string whose introducer is `introducer`.
    ///
    /// A `DCS` starts in `vte`'s `DcsEntry` header, so the substate is
    /// recorded and [`Self::dcs_hooked`] stays clear until the final byte.
    fn begin_vte_string(&mut self, introducer: u8) {
        self.kind = kind_of_introducer(introducer);
        self.bytes = 0;
        self.dcs_header = match self.kind {
            ControlStringKind::Dcs => Some(DcsHeader::Entry),
            _ => None,
        };
        self.dcs_hooked = false;
    }

    /// Ends the open `vte` string at its terminator.
    fn end_vte_string(&mut self) {
        self.bytes = 0;
        self.dcs_header = None;
        self.dcs_hooked = false;
    }

    /// Advances the `DCS` header shadow by one forwarded body byte.
    ///
    /// Mirrors `vte` 0.15's `DcsEntry`/`DcsParam`/`DcsIntermediate`/
    /// `DcsIgnore` transitions closely enough to know when the machine calls
    /// `hook`; see [`DcsHeader`]. Terminators never reach this method.
    fn track_dcs_header(&mut self, byte: u8) {
        let Some(header) = self.dcs_header else {
            return;
        };
        // `DcsIgnore` routes through `anywhere`: no hook, no substate change.
        if header == DcsHeader::Ignore {
            return;
        }
        if (0x40..=0x7E).contains(&byte) {
            // `vte` hooks on the final byte and moves to `DcsPassthrough`.
            self.dcs_header = None;
            self.dcs_hooked = true;
            return;
        }
        self.dcs_header = Some(match (header, byte) {
            (DcsHeader::Entry, 0x20..=0x2F)
            | (DcsHeader::Param, 0x20..=0x2F)
            | (DcsHeader::Intermediate, 0x20..=0x2F) => DcsHeader::Intermediate,
            // `0x30..=0x39` continue the numeric parameter, `0x3A`/`0x3B`
            // store it, and `0x3C..=0x3F` collect a private marker; in
            // `DcsEntry` all of them land in `DcsParam`.
            (DcsHeader::Entry, 0x30..=0x3F) => DcsHeader::Param,
            (DcsHeader::Param, 0x30..=0x3B) => DcsHeader::Param,
            // A private marker in `DcsParam`, or a digit after an
            // intermediate, is an unparseable header: `vte` never hooks it.
            (DcsHeader::Param, 0x3C..=0x3F) | (DcsHeader::Intermediate, 0x30..=0x3F) => {
                DcsHeader::Ignore
            }
            // `anywhere` (`0x00..=0x17 | 0x19 | 0x1C..=0x1F`, `0x80..=0xFF`)
            // and `0x7F` keep the current substate.
            _ => header,
        });
    }

    /// Opens the diverted iTerm2 candidate (`ESC ]` already consumed).
    ///
    /// The diversion replaces any open string (`vte` was closed first), so
    /// the shared budget restarts at zero: the introducer and prefix are
    /// charged when the scanner finishes matching the prefix
    /// ([`Self::charge_diversion`]).
    fn begin_diversion(&mut self) {
        self.state = VteString::Ground;
        self.bytes = 0;
        self.kind = ControlStringKind::Osc;
        self.dcs_header = None;
        self.dcs_hooked = false;
        self.discard = None;
    }

    /// Charges one byte consumed by the diverted iTerm2 candidate.
    ///
    /// Nothing is charged while the scanner is still probing the prefix
    /// (CTX-1592 review F6): a non-matching candidate is replayed into
    /// `vte`, which charges exactly the same bytes once as an ordinary
    /// `OSC`. When the prefix completes, the two introducer bytes and the
    /// prefix are charged in one step, so a maximum-size body lands exactly
    /// on [`CONTROL_STRING_MAX_BYTES`].
    ///
    /// The counter saturates instead of refusing: the scanner's own body cap
    /// ([`crate::iterm2_osc::ITERM2_OSC_MAX_BODY_BYTES`]) binds first and
    /// reports `Iterm2Reject::OversizePayload`, so a diverted candidate never
    /// produces [`TerminalAction::ControlStringRejected`].
    fn charge_diversion(&mut self, collecting: bool) {
        if !collecting {
            return;
        }
        if self.bytes == 0 {
            // The introducer (`ESC ]`, two bytes) and every prefix byte but
            // the one this call charges; the running total after the add is
            // exactly introducer + prefix.
            self.bytes = 1 + ITERM2_OSC_PREFIX.len();
        }
        self.bytes = self.bytes.saturating_add(1).min(CONTROL_STRING_MAX_BYTES);
    }

    /// Leaves the diversion: `vte` owns no string and the budget restarts.
    fn end_diversion(&mut self) {
        self.state = VteString::Ground;
        self.bytes = 0;
        self.dcs_header = None;
        self.dcs_hooked = false;
    }

    /// Opens a parser-handled `APC` (`ESC _` already consumed).
    ///
    /// An `APC` introducer ends an overflowing `vte` string on the wire even
    /// though the parser consumes it itself, so the discard tail ends here.
    fn begin_apc(&mut self) {
        self.discard = None;
        self.apc_bytes = 0;
        self.apc_refused = false;
    }

    /// Closes the parser-handled `APC`.
    fn end_apc(&mut self) {
        self.apc_bytes = 0;
        self.apc_refused = false;
    }

    /// Charges one parser-handled `APC` byte that is not accepted kitty
    /// payload; `true` means the byte crossed the cap and the caller must
    /// abort the `APC`.
    fn charge_apc(&mut self) -> bool {
        if self.apc_refused {
            return false;
        }
        self.apc_bytes += 1;
        if self.apc_bytes > CONTROL_STRING_MAX_BYTES {
            self.apc_refused = true;
            return true;
        }
        false
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
            control_string: ControlString::default(),
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
            control_string: ControlString::default(),
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
    ///
    /// One control-string body may hold at most
    /// [`CONTROL_STRING_MAX_BYTES`] bytes; the introducer is not charged.
    /// A longer string is refused: the prefix already forwarded is
    /// dispatched first, so a truncated but bounded mapped action can still
    /// apply (for `OSC 52` the existing clipboard gate still decides
    /// whether the write is honoured), and the tail past the cap is dropped
    /// up to the real terminator with one
    /// [`TerminalAction::ControlStringRejected`] emitted for telemetry.
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
            control_string,
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
                        control_string,
                    );
                } else if next == 0x18 || next == 0x1A {
                    control_string.end_apc();
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    *apc_discarding = false;
                    kitty.abort();
                    feed_vte(
                        state_machine,
                        &mut bridge,
                        control_string,
                        reject_log,
                        &[next],
                    );
                    i = 1;
                } else if *apc_discarding {
                    // Held ESC is discarded. A following ESC is re-examined
                    // by the main loop (it may start `ESC \`), matching the
                    // in-buffer path; any other byte is discarded with it.
                    if control_string.charge_apc() {
                        refuse_apc(
                            &mut bridge,
                            kitty,
                            apc_buf,
                            apc_payload,
                            apc_discarding,
                            reject_log,
                        );
                    }
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
                    begin_apc(
                        kitty,
                        apc_buf,
                        apc_payload,
                        in_apc,
                        apc_discarding,
                        control_string,
                    );
                    i = 1;
                } else if next == b']' {
                    // Aborted APC followed by `OSC 1337 ; File=` (CTX-1590
                    // review MF-2): the aborting `ESC` must not hand the body
                    // to `vte`'s uncapped OSC collector. Route into the same
                    // diversion the ground path uses.
                    control_string.end_apc();
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    close_vte_string(state_machine, &mut bridge, control_string, reject_log);
                    kitty.note_interleaved(1);
                    control_string.begin_diversion();
                    iterm2.begin();
                    i = 1;
                } else {
                    control_string.end_apc();
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    kitty.abort();
                    feed_vte(
                        state_machine,
                        &mut bridge,
                        control_string,
                        reject_log,
                        &[0x1B],
                    );
                    i = 0;
                }
            } else {
                *held_esc = false;
                if next == b'_' {
                    begin_apc(
                        kitty,
                        apc_buf,
                        apc_payload,
                        in_apc,
                        apc_discarding,
                        control_string,
                    );
                    i = 1;
                } else if next == b']' {
                    // Possible `OSC 1337 ; File=` (CTX-1590): the introducer
                    // is consumed here and the scanner decides whether the
                    // prefix matches or must be replayed into `vte`.
                    close_vte_string(state_machine, &mut bridge, control_string, reject_log);
                    kitty.note_interleaved(1);
                    control_string.begin_diversion();
                    iterm2.begin();
                    i = 1;
                } else {
                    feed_vte(
                        state_machine,
                        &mut bridge,
                        control_string,
                        reject_log,
                        &[0x1B],
                    );
                    i = 0;
                }
            }
        }

        while i < bytes.len() {
            if iterm2.is_active() {
                let byte = bytes[i];
                match iterm2.push(byte) {
                    OscOutcome::Consumed => {
                        control_string.charge_diversion(iterm2.is_collecting());
                        kitty.note_interleaved(1);
                        i += 1;
                        continue;
                    }
                    OscOutcome::PassThrough => {
                        control_string.end_diversion();
                        feed_vte(
                            state_machine,
                            &mut bridge,
                            control_string,
                            reject_log,
                            iterm2.replay(),
                        );
                        iterm2.clear_replay();
                        // Fall through: the current byte takes the normal
                        // path now that `vte` has the whole OSC prefix.
                    }
                    OscOutcome::Finished => {
                        control_string.end_diversion();
                        finish_iterm2(&mut bridge, iterm2, reject_log);
                        i += 1;
                        continue;
                    }
                    OscOutcome::FinishedThenPassThrough => {
                        control_string.end_diversion();
                        finish_iterm2(&mut bridge, iterm2, reject_log);
                        feed_vte(
                            state_machine,
                            &mut bridge,
                            control_string,
                            reject_log,
                            iterm2.replay(),
                        );
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
                            control_string,
                        );
                    } else if *apc_discarding {
                        // A discarded ESC still counts toward the stall
                        // bound and the control-string budget, so an ESC
                        // flood cannot evade either.
                        if control_string.charge_apc() {
                            refuse_apc(
                                &mut bridge,
                                kitty,
                                apc_buf,
                                apc_payload,
                                apc_discarding,
                                reject_log,
                            );
                        }
                        kitty.note_interleaved(1);
                        i += 1;
                    } else {
                        control_string.end_apc();
                        clear_apc_header(kitty, apc_buf);
                        *in_apc = false;
                        *apc_payload = false;
                        kitty.abort();
                        if nxt == b'_' {
                            begin_apc(
                                kitty,
                                apc_buf,
                                apc_payload,
                                in_apc,
                                apc_discarding,
                                control_string,
                            );
                            i += 2;
                        } else if nxt == b']' {
                            // Aborted APC followed by `OSC 1337 ; File=`
                            // (CTX-1590 review MF-2): divert the body before
                            // `vte` can collect it as an OSC.
                            close_vte_string(
                                state_machine,
                                &mut bridge,
                                control_string,
                                reject_log,
                            );
                            kitty.note_interleaved(1);
                            control_string.begin_diversion();
                            iterm2.begin();
                            i += 2;
                        } else {
                            feed_vte(
                                state_machine,
                                &mut bridge,
                                control_string,
                                reject_log,
                                &[0x1B],
                            );
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
                        control_string,
                    );
                } else if b == 0x18 || b == 0x1A {
                    i += 1;
                    control_string.end_apc();
                    clear_apc_header(kitty, apc_buf);
                    *in_apc = false;
                    *apc_payload = false;
                    *apc_discarding = false;
                    kitty.abort();
                    feed_vte(state_machine, &mut bridge, control_string, reject_log, &[b]);
                } else if *apc_payload {
                    // Accepted payload bytes are charged to the kitty ledger,
                    // not the control-string budget (see
                    // [`CONTROL_STRING_MAX_BYTES`]).
                    if kitty.push_payload(std::slice::from_ref(&b)).is_err() {
                        *apc_payload = false;
                        *apc_discarding = true;
                    }
                    i += 1;
                } else {
                    if control_string.charge_apc() {
                        refuse_apc(
                            &mut bridge,
                            kitty,
                            apc_buf,
                            apc_payload,
                            apc_discarding,
                            reject_log,
                        );
                        i += 1;
                        continue;
                    }
                    if *apc_discarding {
                        // Discarded APC bytes (non-`G` commands, rejected
                        // headers) count toward an open stream's stall bound.
                        kitty.note_interleaved(1);
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
                        begin_apc(
                            kitty,
                            apc_buf,
                            apc_payload,
                            in_apc,
                            apc_discarding,
                            control_string,
                        );
                        i += 2;
                    } else if bytes[i + 1] == b']' {
                        // Possible `OSC 1337 ; File=`: see the held-ESC arm.
                        close_vte_string(state_machine, &mut bridge, control_string, reject_log);
                        kitty.note_interleaved(1);
                        control_string.begin_diversion();
                        iterm2.begin();
                        i += 2;
                    } else {
                        kitty.note_interleaved(1);
                        feed_vte(
                            state_machine,
                            &mut bridge,
                            control_string,
                            reject_log,
                            &[0x1B],
                        );
                        i += 1;
                    }
                } else {
                    // Note: C1 APC (0x9F) is intentionally not intercepted:
                    // it overlaps UTF-8 continuation bytes (e.g. `🎉` contains
                    // 0x9F), and kitty/chafa always use `ESC _`.
                    // Text interleaved with an open chunked kitty stream
                    // counts toward its stall bound (no-op when idle).
                    kitty.note_interleaved(1);
                    feed_vte(state_machine, &mut bridge, control_string, reject_log, &[b]);
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
    control_string: &mut ControlString,
) {
    clear_apc_header(kitty, apc_buf);
    *apc_payload = false;
    *in_apc = true;
    *apc_discarding = false;
    control_string.begin_apc();
}

/// Feeds bytes to `vte`, keeps [`ControlString`] in step with them, and
/// enforces [`CONTROL_STRING_MAX_BYTES`] (CTX-1592).
///
/// Every byte the parser hands to `vte` must go through here (or the state
/// tracker diverges and a later diversion stops closing a string `vte` still
/// owns). Feeding the same bytes one call at a time is equivalent to feeding
/// them in one call: `vte` and the tracker carry state across calls.
///
/// The cap is a byte count, so overflow lands at the same offset however the
/// stream is chunked. On overflow the bytes forwarded so far are flushed, a
/// `CAN` is injected (which makes `vte` dispatch/abort the string and clear
/// its collector), and the rest of the string is swallowed until the byte
/// that would have ended it on the wire — except `ESC`, which is forwarded
/// so the sequence after the string parses normally.
fn feed_vte<F: FnMut(TerminalAction)>(
    state_machine: &mut vte::Parser,
    bridge: &mut Bridge<'_, F>,
    control_string: &mut ControlString,
    reject_log: &mut RejectLog,
    bytes: &[u8],
) {
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(family) = control_string.discard {
            match family.discard_ends_at(control_string.dcs_hooked, byte) {
                Some(forward) => {
                    control_string.discard = None;
                    control_string.end_vte_string();
                    if forward {
                        // `ESC` ends the string and opens what follows it.
                        control_string.state = control_string.state.after(byte);
                        start = i;
                        i += 1;
                    } else {
                        i += 1;
                        start = i;
                    }
                }
                None => {
                    i += 1;
                    start = i;
                }
            }
            continue;
        }
        let previous = control_string.state;
        // `advance_dcs_passthrough` unhooks a DCS on C1 `ST` (`0x9C`), but
        // the DCS header states and every SOS/PM string ignore it. The
        // collapsed `Other` state cannot tell them apart, so `dcs_hooked`
        // decides; the byte still reaches `vte` through the trailing flush so
        // the machine performs the unhook itself (CTX-1592 review MF-1).
        if previous == VteString::Other && byte == 0x9C && control_string.dcs_hooked {
            control_string.end_vte_string();
            i += 1;
            continue;
        }
        let next = previous.after(byte);
        if next.is_string() && !previous.is_string() {
            control_string.begin_vte_string(byte);
        } else if next.is_string() {
            if control_string.bytes == CONTROL_STRING_MAX_BYTES {
                // Flush what `vte` has not seen yet, then abort the string
                // with `CAN`: `vte` runs its dispatch/unhook/hook-end path
                // and clears the collected body. The refused byte and the
                // rest of the string never reach it.
                state_machine.advance(bridge, &bytes[start..i]);
                state_machine.advance(bridge, &[CAN]);
                let kind = control_string.kind;
                control_string.state = previous.after(CAN);
                control_string.bytes = 0;
                control_string.discard = Some(next);
                emit_control_string_refusal(bridge, reject_log, kind);
                start = i + 1;
                i += 1;
                continue;
            }
            control_string.bytes += 1;
            control_string.track_dcs_header(byte);
        } else if previous.is_string() {
            // The terminator itself is not retained; the next string starts
            // from zero.
            control_string.end_vte_string();
        }
        control_string.state = next;
        i += 1;
    }
    state_machine.advance(bridge, &bytes[start..]);
}

/// Family of a control string from the byte that opens it in `vte`'s
/// `advance_esc` arm.
const fn kind_of_introducer(byte: u8) -> ControlStringKind {
    match byte {
        b'P' => ControlStringKind::Dcs,
        b'X' => ControlStringKind::Sos,
        b'^' => ControlStringKind::Pm,
        b'_' => ControlStringKind::Apc,
        // `]` (the only remaining introducer) opens an OSC.
        _ => ControlStringKind::Osc,
    }
}

/// Emits one counted control-string refusal and its rate-limited diagnostic.
///
/// The diagnostic must not claim the sequence stored nothing: the prefix
/// forwarded before the abort was dispatched and a truncated mapped action
/// may already have applied (CTX-1592 review MF-2).
fn emit_control_string_refusal<F: FnMut(TerminalAction)>(
    bridge: &mut Bridge<'_, F>,
    reject_log: &mut RejectLog,
    kind: ControlStringKind,
) {
    if let Some(occurrence) = reject_log.record() {
        warn_rejection(
            occurrence,
            &format!(
                "bitty: rejecting {kind} control string ({}): tail discarded past \
                 {CONTROL_STRING_MAX_BYTES} bytes, bounded prefix already dispatched",
                ControlStringReject::Overflow.label()
            ),
        );
    }
    bridge.emit(TerminalAction::ControlStringRejected {
        kind,
        reason: ControlStringReject::Overflow,
    });
}

/// Aborts an open parser-handled `APC` that crossed the cap (CTX-1592).
///
/// The `APC` never reaches `vte`, so there is no string to abort there: the
/// accumulated header/payload is dropped, the tail is discarded until the
/// real terminator, and one refusal is counted. `in_apc` stays set so the
/// discard path owns the remaining bytes.
fn refuse_apc<F: FnMut(TerminalAction)>(
    bridge: &mut Bridge<'_, F>,
    kitty: &mut KittyApcAssembler,
    apc_buf: &mut Vec<u8>,
    apc_payload: &mut bool,
    apc_discarding: &mut bool,
    reject_log: &mut RejectLog,
) {
    clear_apc_header(kitty, apc_buf);
    kitty.abort();
    *apc_payload = false;
    *apc_discarding = true;
    emit_control_string_refusal(bridge, reject_log, ControlStringKind::Apc);
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
    control_string: &mut ControlString,
    reject_log: &mut RejectLog,
) {
    if control_string.needs_close() {
        feed_vte(state_machine, bridge, control_string, reject_log, b"\x1b\\");
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
    control_string: &mut ControlString,
) {
    control_string.end_apc();
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
