//! `bitty-term-state`: the Terminal Truth core.
//!
//! This crate is the sole interpreter of the typed [`TerminalAction`]
//! stream produced by `bitty-vt` (see ADR-0003: this crate depends only on
//! `bitty-vt`). It owns grid, cursor, modes, scrollback, damage tracking,
//! replies, and semantic zones, and performs no I/O: device-status replies
//! are queued and returned to the caller. Reads occur through versioned
//! [`Snapshot`] values plus damage, never through mutable interior access,
//! preserving the presentation boundary of
//! `bitty-docs/docs/architecture/core-boundaries.md`.
//!
//! Contracts implemented here (accepted 2026-08-26):
//!
//! - `bitty-docs/docs/specifications/terminal-state-rfc.md`, sections
//!   "Grid and state invariants", "Damage tracking model", and
//!   "Deterministic replay guarantees".
//! - ADR-0003 "Core Workspace Topology": dependency row for
//!   `bitty-term-state` (workspace crates: `bitty-vt` only).
//!
//! # Config-free bounded constants
//!
//! The M1 slice takes no runtime configuration; every bound below is a
//! named constant so behavior stays deterministic and replayable. Each
//! cites its RFC clause:
//!
//! | Constant | Value | Contract |
//! |---|---|---|
//! | [`GRID_COLUMNS`] / [`GRID_ROWS`] | 80 x 24 | Initial geometry; resize awaits the singular reflow algorithm deferred under RFC "Open items remaining under OQ-007" |
//! | [`DEFAULT_TAB_INTERVAL`] | 8 | RFC invariant 6: default tab lattice; `FullReset` restores it |
//! | [`SCROLLBACK_MAX_LINES`](scrollback::SCROLLBACK_MAX_LINES) | 10 000 | RFC invariant 4: bounded pruning, oldest first |
//! | [`REPLY_CAP_BYTES`](replies::REPLY_CAP_BYTES) | 4096 | RFC invariant 7: reply bounds, drop-and-flag |
//! | [`DAMAGE_HISTORY_BATCHES`](damage::DAMAGE_HISTORY_BATCHES) | 64 | Bounded `damage_since` window |
//! | [`DAMAGE_MAX_REGIONS_PER_BATCH`](damage::DAMAGE_MAX_REGIONS_PER_BATCH) | 256 | Coarse fallback beyond the cap |
//! | [`HYPERLINK_TABLE_MAX`](state::HYPERLINK_TABLE_MAX) | 1024 | Bounded link table (threat T-01) |
//! | [`ZONE_RECORDS_MAX`](state::ZONE_RECORDS_MAX) | 1024 | Bounded `OSC 133` zone log |
//! | [`CANONICAL_HASH_VERSION`](canonical_public::CANONICAL_HASH_VERSION) | 1 | RFC replay guarantee 2 serialization version (evolution policy per RFC open item) |
//!
//! # Determinism
//!
//! Transitions are a pure function of `(initial state, action sequence)`:
//! no wall-clock time, randomness, thread scheduling, or platform variance
//! participates. [`State::state_hash`] hashes a canonical little-endian
//! serialization with FNV-1a, so identical inputs produce identical
//! digests on all platforms (RFC replay guarantee 2). Resize events,
//! user resets, and policy outcomes remain environment inputs to be
//! recorded by the future recording layer; none are hidden inside this
//! state machine (RFC replay guarantees 3-6).
//!
//! # Image store/placement status
//!
//! Image protocol placement semantics are out of scope pending OQ-008;
//! see [`crate::image`] for the typed stub and its contract references.

#![forbid(unsafe_code)]

mod canonical;
mod cell;
mod charsets;
mod cursor;
pub mod damage;
mod grid;
pub mod image;
pub mod modes;
pub mod replies;
pub mod scrollback;
mod state;
mod tabs;

/// Re-exported canonical-hash serialization version; see the crate-level
/// constant register.
pub mod canonical_public {
    pub use crate::canonical::CANONICAL_HASH_VERSION;
}

/// Default tab-stop interval restored by `FullReset` (RFC invariant 6).
pub const DEFAULT_TAB_INTERVAL: usize = 8;

pub use bitty_vt::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Color,
    Hyperlink, Mode, MouseCoordinateEncoding, MouseTrackingMode, Rgb, StatusKind, TerminalAction,
    UnderlineStyle, ZoneKind,
};
pub use cell::{Attributes, Cell, HyperlinkId, Style, char_cell_width};
pub use cursor::{Cursor, CursorPosition};
pub use damage::{Damage, DamageRect, DamagedRegion};
pub use image::{ImageId, ImageStore};
pub use modes::Modes;
pub use replies::{REPLY_CAP_BYTES, Replies};
pub use scrollback::{SCROLLBACK_MAX_LINES, ScrollbackLine};
pub use state::{
    GRID_COLUMNS, GRID_ROWS, HYPERLINK_TABLE_MAX, InvariantViolation, SNAPSHOT_VERSION, Snapshot,
    State, TelemetryCounters, ZONE_RECORDS_MAX,
};
pub use tabs::TabStops;
