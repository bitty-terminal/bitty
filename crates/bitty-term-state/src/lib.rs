//! `bitty-term-state`: the Terminal Truth core.
//!
//! This crate is the sole interpreter of the typed [`TerminalAction`]
//! stream produced by `bitty-vt` (see ADR-0003: this crate depends only on
//! `bitty-vt`).
//!
//! CTX-1612 adds exactly one more edge: `unicode-bidi` 0.3.18
//! (MIT OR Apache-2.0, MSRV 1.47, UCD 16.0.0 bidi classes) for UAX #9
//! resolution in [`bidi`], the only module that names the crate. The
//! dependency is a pure algorithm/data implementation behind the
//! [`BidiGrid`] facade; it never participates in Terminal Truth, the
//! canonical hash, or the snapshot codec, and it is inert while
//! `terminal.bidi = off` (the default). It owns grid, cursor, modes, scrollback, damage tracking,
//! replies, and semantic zones, and performs no I/O: device-status replies
//! are queued and returned to the caller. Reads occur through versioned
//! [`Snapshot`] values plus damage, never through mutable interior access,
//! preserving the presentation boundary of
//! `docs/architecture/core-boundaries.md`.
//!
//! Contracts implemented here (accepted 2026-08-26):
//!
//! - `docs/specifications/terminal-state-rfc.md`, sections
//!   "Grid and state invariants", "Damage tracking model", and
//!   "Deterministic replay guarantees".
//! - ADR-0003 "Core Workspace Topology": dependency row for
//!   `bitty-term-state` (workspace crates: `bitty-vt`; external:
//!   `unicode-bidi` as the CTX-1612 bidi-class source).
//!
//! # Bounded constants
//!
//! The M1 slice takes no runtime configuration; every bound below is a
//! named constant so behavior stays deterministic and replayable, except
//! scrollback retention, whose per-buffer capacity is chosen at construction
//! (`State::with_scrollback_lines`) within the hard
//! [`SCROLLBACK_MAX_LINES`] bound and
//! defaulting to [`SCROLLBACK_DEFAULT_LINES`].
//! Each cites its RFC clause:
//!
//! | Constant | Value | Contract |
//! |---|---|---|
//! | [`GRID_COLUMNS`] / [`GRID_ROWS`] | 80 x 24 | Initial geometry; width resizes reflow primary logical lines via soft-wrap flags (CTX-0266), alt screen truncates (xterm) |
//! | [`DEFAULT_TAB_INTERVAL`] | 8 | RFC invariant 6: default tab lattice; `FullReset` restores it |
//! | [`SCROLLBACK_DEFAULT_LINES`] | 10 000 | Default scrollback retention (`terminal.scrollback` default) |
//! | [`SCROLLBACK_MAX_LINES`] | 100 000 | Hard retention bound (RFC invariant 4: bounded pruning, oldest first; mirrors the accepted `terminal.scrollback` maximum) |
//! | [`REPLY_CAP_BYTES`] | 4096 | RFC invariant 7: reply bounds, drop-and-flag |
//! | [`DAMAGE_HISTORY_BATCHES`](damage::DAMAGE_HISTORY_BATCHES) | 64 | Bounded `damage_since` window |
//! | [`DAMAGE_MAX_REGIONS_PER_BATCH`](damage::DAMAGE_MAX_REGIONS_PER_BATCH) | 256 | Coarse fallback beyond the cap |
//! | [`HYPERLINK_TABLE_MAX`] | 1024 | Bounded link table (threat T-01) |
//! | [`MAX_CLUSTER_SCALARS`] | 6 | Per-cell cluster-tail cap (threat T-01; CTX-1609; the historical `MAX_ZEROWIDTH_CHARS` name aliases the same bound) |
//! | [`MAX_BIDI_PARAGRAPH_CELLS`] | 4096 | BiDi paragraph bound: longer logical lines split at cell boundaries (CTX-1612; the S0 caller resolves one logical line, `OQ-091` paragraph extent stays open) |
//! | [`ZONE_RECORDS_MAX`] | 1024 | Bounded `OSC 133` zone log |
//! | [`CANONICAL_HASH_VERSION`](canonical_public::CANONICAL_HASH_VERSION) | 13 | RFC replay guarantee 2 serialization version (evolution policy per RFC open item; v2 adds saved cursor style/visibility, v3 combining buffers, v4 soft-wrap flags, v5 synchronized-update mode, v6 hyperlink id-space position, v7 alternate-scroll mode, v8 Kitty keyboard stack, v9 inactive-screen Kitty register, v10 zone anchors, v11 OSC 133 output tracking, v12 cell layout-padding marker, v13 East Asian ambiguous-width policy) |
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
//! Image protocol placement semantics follow the accepted Rich Presentation RFC
//! that closed OQ-008 on 2026-08-28 (frontmatter `status: accepted`; bitty-docs
//! open-questions register); see [`crate::image`] for the typed placeholder and
//! its contract references.

#![forbid(unsafe_code)]

pub mod bidi;
mod canonical;
mod cell;
mod charsets;
mod cursor;
pub mod damage;
pub mod grapheme;
mod grid;
pub mod image;
pub mod kitty_unicode;
pub mod modes;
pub mod placement;
pub mod replies;
pub mod scrollback;
pub mod search;
mod state;
mod tabs;
mod unicode_tables;

/// Re-exported canonical-hash serialization version; see the crate-level
/// constant register.
pub mod canonical_public {
    pub use crate::canonical::CANONICAL_HASH_VERSION;
}

/// Default tab-stop interval restored by `FullReset` (RFC invariant 6).
pub const DEFAULT_TAB_INTERVAL: usize = 8;

pub use bidi::{
    BidiGrid, BidiLine, BidiText, MAX_BIDI_PARAGRAPH_CELLS, ParaDirection, UNICODE_BIDI_VERSION,
    is_bidi_format_scalar, logical_line_ranges, resolve_text,
};
pub use bitty_vt::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Color,
    CursorStyle, Hyperlink, KittyControlKeys, KittyMedium, Mode, MouseCoordinateEncoding,
    MouseTrackingMode, Rgb, StatusKind, TerminalAction, UnderlineStyle, ZoneKind,
};
pub use cell::{
    AmbiguousWidth, Attributes, Cell, HyperlinkId, MAX_CLUSTER_SCALARS, MAX_ZEROWIDTH_CHARS, Style,
    Zerowidth, char_cell_width, char_cell_width_in,
};
pub use cursor::{Cursor, CursorPosition};
pub use damage::{Damage, DamageRect, DamagedRegion};
pub use grapheme::{
    Graphemes, cluster_cell_width, is_extended_pictographic, is_grapheme_extend,
    is_regional_indicator, split_clusters,
};
pub use image::{
    IMAGE_STORE_MAX_ENTRIES, IMAGE_STORE_MAX_PAYLOAD_BYTES, ImageId, ImagePlaceholder, ImageStore,
};
pub use kitty_unicode::{
    KITTY_PLACEHOLDER, KITTY_PLACEHOLDER_RANGE_END, KITTY_PLACEHOLDER_RANGE_START, KittyRunBuilder,
    KittyUnicodeCell, KittyUnicodeId, KittyUnicodePrev, KittyUnicodeRun, KittyUnicodeRunCells,
    color_id_fragment, decode_cell, decode_cell_with_prev, decode_marks, diacritic_index,
    is_kitty_placeholder, kitty_image_id, kitty_placement_id, run_key,
};
pub use modes::Modes;
pub use placement::{
    KITTY_ANIM_DEFAULT_GAP_MS, KITTY_ANIM_MAX_FRAMES, KITTY_ANIM_MAX_IMAGES,
    KITTY_ANIM_MAX_STEPS_PER_TICK, KITTY_ANIM_TICK_QUANTUM_MS, KITTY_PLACE_MAX_PLACEMENTS,
    KITTY_Z_BELOW_BACKGROUND, KittyAnimState, KittyAnimation, KittyDeleteSelector, KittyFrameError,
    KittyPlacement, PlacementStore,
};
pub use replies::{REPLY_CAP_BYTES, Replies};
pub use scrollback::{SCROLLBACK_DEFAULT_LINES, SCROLLBACK_MAX_LINES, ScrollbackLine};
pub use state::{
    GRID_COLUMNS, GRID_ROWS, HYPERLINK_TABLE_MAX, InvariantViolation, MAX_GRID_DIM, ResizeReport,
    SNAPSHOT_VERSION, Snapshot, State, TelemetryCounters, ZONE_RECORDS_MAX, ZoneRecord,
};
pub use tabs::TabStops;
