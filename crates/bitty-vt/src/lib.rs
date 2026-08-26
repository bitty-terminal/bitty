//! `bitty-vt`: byte-stream VT parser producing semantic
//! [`TerminalAction`] values.
//!
//! The crate implements the parser side of the Terminal State RFC's typed
//! action interface (see
//! `bitty-docs/docs/specifications/terminal-state-rfc.md`, section "Typed
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
//!   length, and OSC payload size are bounded; exceeding a limit yields a
//!   well-defined truncated or inert action, never unbounded growth.
//! - **Deterministic UTF-8 policy.** Invalid bytes decode to U+FFFD (one
//!   cell), identically offline and live, delegated to `vte`'s collector.
//! - **Zero unsafe.** The workspace denies `unsafe_code`; this crate adds no
//!   exception to that rule.
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
mod parser;

pub use action::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Col, Color,
    ControlChar, Count, CursorStyle, Direction, EraseDisplayMode, EraseLineMode, GraphemeCell,
    Hyperlink, Mode, MouseCoordinateEncoding, MouseTrackingMode, Rgb, Row, SequenceKind,
    StatusKind, TabTargets, TerminalAction, UnderlineStyle, UnrecognizedSequence, ZoneKind,
};
pub use bounded::{BoundedBytes, BoundedString};
pub use parser::Parser;
