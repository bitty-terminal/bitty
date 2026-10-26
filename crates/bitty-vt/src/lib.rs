//! `bitty-vt`: byte-stream VT parser producing semantic
//! [`TerminalAction`] values.
//!
//! The crate implements the parser side of the Terminal State RFC's typed
//! action interface (see
//! `docs/specifications/terminal-state-rfc.md`, section "Typed
//! Action interface" and "Parser obligations") under the topology rules of
//! ADR-0003 and the upstream-dependency decision of ADR-0004:
//!
//! - **No terminal state, no I/O.** The parser is a pure function from byte
//!   stream to action stream; grid, cursor, modes, scrollback, damage and
//!   replies live in the downstream terminal-state crate.
//! - **`vte` stays behind this API.** The adopted `vte` (~0.15) state machine
//!   is an implementation detail; no `vte` type appears in the public
//!   surface.
//! - **Bounded, panic-free parsing.** Parameter count/magnitude, sequence
//!   length, and string payload size are bounded; exceeding a limit yields a
//!   well-defined truncated or inert action, never unbounded growth.
//! - **Deterministic UTF-8 policy.** Invalid bytes decode to U+FFFD (one
//!   cell), identically offline and live, delegated to `vte`'s collector.
//! - **Zero unsafe.** The workspace denies `unsafe_code`; this crate adds no
//!   exception to that rule.
//!
//! # Parser bounds
//!
//! | Bound | Value | Enforcement |
//! | --- | --- | --- |
//! | CSI parameter count | `vte`'s 32-parameter cap | extra parameters dropped and flagged `ignore` |
//! | CSI numeric magnitude | `u16` saturation | saturating parse in the dispatcher |
//! | Control string (OSC/DCS/SOS/PM/APC) | [`CONTROL_STRING_MAX_BYTES`] (4 MiB iTerm2 body cap + its introducer; the introducer is not charged for a `vte`-owned string) | `CAN` injection for `vte` strings, local abort for a parser-handled APC |
//! | Materialized string/bytes | [`BoundedString`]/[`BoundedBytes`] 4096 bytes | truncation at action construction |
//! | Kitty graphics | IMG-1 [`KITTY_APC_LEDGER_CAP`], IMG-3 [`KITTY_APC_DECODE_MAX_BYTES`] | streamed decode before allocation |
//! | iTerm2 inline image | [`ITERM2_OSC_MAX_WIRE_BYTES`], [`ITERM2_OSC_MAX_ATTRIBUTE_BYTES`] | pre-scan body cap before dispatch |
//!
//! The control-string cap is the one bound `vte` does not provide (its `std`
//! OSC collector is an uncapped growable `Vec`), so it is enforced here:
//! `vte`-owned strings are aborted with an injected `CAN` and the diverted
//! iTerm2 candidate is charged against the same budget (for the diversion the
//! counter only accounts; the scanner's own body cap refuses first). Over-cap
//! strings emit [`TerminalAction::ControlStringRejected`] and are counted in
//! terminal-state telemetry (`TelemetryCounters::control_string_refused`).
//! The action is inert, but the refusal is not: the bounded prefix forwarded
//! before the abort is dispatched first, so one truncated mapped action may
//! already have applied (see [`ControlStringReject::Overflow`]).
//!
//! Kitty `APC G` wiring (CTX-0256) pre-scans `APC` (which `vte` 0.15 leaves
//! inert), base64-decodes with a fail-closed alphabet check, and reassembles
//! `m=` chunks under the ledger cap. Rejections warn via `stderr`
//! (diagnostic only, no state change) and emit no action.
//!
//! iTerm2 inline images (`OSC 1337 ; File=`, CTX-1590) are pre-scanned the
//! same way: `vte`'s `std` OSC collector grows without a cap, so the
//! bounded intake ([`iterm2_osc`]) buffers the body itself, enforces the
//! wire/decoded/pixel caps, and diverts only the `1337;File=` prefix.
//! Admitted images emit [`TerminalAction::Iterm2Image`]; refusals emit the
//! inert [`TerminalAction::Iterm2ImageRejected`] telemetry action.
//!
//! # Example
//!
//! ```
//! use bitty_vt::{Parser, TerminalAction};
//!
//! let mut parser = Parser::new();
//! let mut actions = Vec::new();
//! parser.advance(b"\x1b[1;31mhi\x1b[0m", |action| actions.push(action));
//!
//! assert_eq!(
//!     actions.first(),
//!     Some(&TerminalAction::SetAttributes {
//!         attrs: bitty_vt::AttributeDiff {
//!             changes: vec![
//!                 bitty_vt::AttributeChange::Enable(bitty_vt::Attribute::Bold),
//!                 bitty_vt::AttributeChange::Foreground(bitty_vt::Color::Indexed(1)),
//!             ]
//!             .into_boxed_slice(),
//!         },
//!     })
//! );
//! ```

#![forbid(unsafe_code)]

mod action;
mod bounded;
mod diag;
pub mod iterm2_osc;
pub mod kitty_apc;
pub mod kitty_font_query;
mod parser;

pub use action::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Col, Color,
    ControlChar, ControlStringKind, ControlStringReject, Count, CursorStyle, Direction,
    DynamicColorOp, DynamicColorTarget, EnhancedKeyboardOp, EnhancedKeyboardSetMode,
    EraseDisplayMode, EraseLineMode, GraphemeCell, Hyperlink, KittyNotificationChunk,
    KittyPayloadType, MAX_OSC4_OPS, MAX_OSC22_SHAPES, Mode, MouseCoordinateEncoding,
    MouseTrackingMode, Notification, NotificationSource, PaletteColorOp, PaletteOp, PointerShape,
    PointerShapeOp, Rgb, Row, SequenceKind, StatusKind, TabTargets, TerminalAction, UnderlineStyle,
    UnrecognizedSequence, ZoneKind,
};
pub use bounded::{BoundedBytes, BoundedString};
pub use iterm2_osc::{
    ITERM2_DECODE_MAX_DIMENSION, ITERM2_DECODE_MAX_PIXELS, ITERM2_OSC_MAX_ATTRIBUTE_BYTES,
    ITERM2_OSC_MAX_BODY_BYTES, ITERM2_OSC_MAX_CELL_SPAN, ITERM2_OSC_MAX_DECODED_BYTES,
    ITERM2_OSC_MAX_NAME_BYTES, ITERM2_OSC_MAX_WIRE_BYTES, ITERM2_OSC_PREFIX, Iterm2Reject,
    Iterm2Request,
};
pub use kitty_apc::{
    KITTY_APC_DECODE_MAX_BYTES, KITTY_APC_DECODE_MAX_DIMENSION, KITTY_APC_DECODE_MAX_PIXELS,
    KITTY_APC_LEDGER_CAP, KITTY_APC_PATH_MAX_BYTES, KITTY_APC_SHM_NAME_MAX,
    KITTY_APC_STALL_MAX_BYTES, KITTY_APC_TMP_NAME_MARKER, KittyApcAssembler, KittyApcParams,
    KittyApcReject, KittyCompleted, KittyControlKeys, KittyFeedOutcome, KittyMedium,
};
pub use kitty_font_query::{KittyFontQuery, parse_kitty_font_query};
pub use parser::{CONTROL_STRING_MAX_BYTES, Parser};
