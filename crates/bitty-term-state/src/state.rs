//! The Terminal Truth state machine.
//!
//! [`State`] is the sole interpreter of the parser's typed action stream
//! (RFC "Pipeline overview": the only write path into terminal state is the
//! action stream). After every applied action all eight "Grid and state
//! invariants" hold; [`State::check_invariants`] recomputes them and debug
//! builds assert them automatically behind every [`State::apply`] call.

use std::collections::VecDeque;

use bitty_vt::{
    AttributeChange, AttributeDiff, BoundedString, Col, Count, CursorStyle, Direction,
    EraseDisplayMode, EraseLineMode, KittyControlKeys, Mode, Row, SequenceKind, StatusKind,
    TabTargets, TerminalAction, ZoneKind,
};

use crate::cell::{
    AttributeChangeKind, Attributes, Cell, HyperlinkId, Style, Zerowidth, char_cell_width,
};
use crate::charsets::Charsets;
use crate::cursor::{Cursor, CursorPosition, SavedCursor};
use crate::damage::{
    DAMAGE_HISTORY_BATCHES, DAMAGE_MAX_REGIONS_PER_BATCH, Damage, DamageRect, DamagedRegion,
    coalesce,
};
use crate::grid::{Grid, ScreenPair};
use crate::image::ImageStore;
use crate::kitty_unicode::{KittyUnicodeCell, KittyUnicodeRunCells};
use crate::modes::{AltScreen, EnhancedKeyboardState, Modes};
use crate::placement::{KittyAnimState, KittyDeleteSelector, KittyPlacement, PlacementStore};
use crate::replies::Replies;
use crate::scrollback::{ClearedRange, SCROLLBACK_DEFAULT_LINES, Scrollback, ScrollbackLine};
use crate::tabs::TabStops;

mod hash;
mod invariants;
#[cfg(test)]
mod tests;

pub use invariants::InvariantViolation;

/// Initial grid width in columns; width resizes reflow primary logical lines
/// via soft-wrap flags (CTX-0266), alternate screen truncates (xterm).
pub const GRID_COLUMNS: usize = 80;

/// Initial grid height in rows; see [`GRID_COLUMNS`].
pub const GRID_ROWS: usize = 24;

/// Maximum grid dimension (columns or rows) accepted by [`State::resize`].
///
/// The single source of truth for the 1000x1000 grid bound: callers that
/// cap a derived dimension (runtime config validation and pixel-to-grid
/// derivation) reference this constant instead of restating the literal.
pub const MAX_GRID_DIM: usize = 1000;

/// Cap on distinct hyperlink identities retained (bounded memory per
/// threat T-01). Past the cap the oldest entry is evicted first (the
/// [`ImageStore`] precedent); evicted ids fail closed to no link while new
/// links keep working (CTX-0469).
pub const HYPERLINK_TABLE_MAX: usize = 1024;

/// Cap on retained semantic-zone records (`OSC 133`), oldest dropped
/// first; bounded memory per threat T-01.
pub const ZONE_RECORDS_MAX: usize = 1024;

/// One retained hyperlink identity: a stable monotonic id plus the OSC 8
/// `id=` parameter and target URI.
///
/// While an entry stays resident its id is never reused: eviction drops the
/// oldest entry and live cells holding its id fail closed
/// ([`State::hyperlink_entry`] returns `None`) instead of resolving to a
/// different URI (CTX-0469).
///
/// Honest bound (CTX-0490): the id space is a `u32`. When the counter reaches
/// its wrap point the table is cleared and issuance restarts at zero, so a
/// cell that still carries a pre-wrap id can resolve against a post-wrap
/// entry. The window needs 2^32 distinct OSC 8 identities in one session
/// lifetime and drops every resident entry first, but reuse is not
/// impossible. [`State::full_reset`] clears the screens before the table, so
/// no stale cell ids survive it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HyperlinkEntry {
    id: HyperlinkId,
    id_param: Option<BoundedString>,
    uri: BoundedString,
}

/// Version embedded in snapshots (RFC: reads occur through versioned
/// snapshots only).
pub const SNAPSHOT_VERSION: u32 = 1;

/// One recorded semantic prompt/command zone boundary.
///
/// In addition to the ordinal log position, each record carries a stable
/// buffer anchor (M1-18, CTX-0665): the combined scrollback + live buffer
/// row of the cursor line when the marker arrived, plus the prune/epoch
/// generation needed to resolve it later. [`State::zone_buffer_row`]
/// validates the anchor against the current buffer and returns `None`
/// when the marked line was pruned, cleared, reset, or reflowed.
///
/// CTX-0996 (issue #1688): `OutputEnd` (`D`) records carry per-row output
/// tracking (`output_on_mark_row`): whether command output landed on the
/// mark row itself after the last `OutputStart`. A `D` on a fresh row after
/// a trailing CR+LF carries `false`; a `D` on the same row as a final
/// partial line (no trailing newline) carries `true`. Resolution uses the
/// flag instead of inferring from column zero, since a bare LF advances the
/// row without resetting the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneRecord {
    /// Monotonic sequence number assigned when the marker arrived.
    pub ordinal: u64,
    /// Which zone boundary was marked.
    pub kind: ZoneKind,
    /// Exit status for `OutputEnd` (`D`), `None` otherwise or on parse failure.
    /// Bounded: validated as signed 32-bit integer; malformed yields `None`.
    pub exit_code: Option<i32>,
    /// Combined buffer row of the cursor line at mark time (`0` = oldest
    /// retained scrollback, `scrollback_len + cursor_row` for live rows).
    pub buffer_row: usize,
    /// Scrollback eviction total (`total_written - retained`) at mark time.
    /// Resolution subtracts the evictions since the mark (oldest-first
    /// pruning shifts every retained row down by exactly the evicted count).
    pub evicted_at_mark: u64,
    /// Buffer epoch at mark time (bumped on scrollback clear, full reset,
    /// and resize reflow). A mismatch fails resolution: the marked content
    /// is gone even when the arithmetic still lands in range.
    pub epoch_at_mark: u64,
    /// Whether the alternate screen was active at mark time. Resolution
    /// requires the same screen: alt-screen marks never resolve against
    /// the primary buffer and vice versa.
    pub on_alt_screen: bool,
    /// CTX-0996: for `OutputEnd` only, whether command output after the
    /// last `OutputStart` landed on this mark row. Always `false` for other
    /// kinds. Decided at mark time from per-row print tracking, never from
    /// column zero.
    pub output_on_mark_row: bool,
}

/// Anchor of the last printable output since the last `OutputStart`
/// (CTX-0996, issue #1688).
///
/// Per-row output tracking: every placed glyph records its combined buffer
/// row plus the prune/epoch/screen generation needed to resolve it later,
/// mirroring [`ZoneRecord`] anchors. [`State::output_print_on_row`] resolves
/// the anchor against the current buffer and reports whether the given row
/// holds command output. A bare LF advances the row without resetting the
/// column, so column zero alone cannot answer this; the anchor can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputPrintMark {
    /// Combined buffer row where the glyph landed.
    buffer_row: usize,
    /// Eviction total at print time.
    evicted_at: u64,
    /// Buffer epoch at print time.
    epoch_at: u64,
    /// Whether the alternate screen was active at print time.
    on_alt_screen: bool,
}

/// Versioned read-only view of terminal state for renderers and plugins.
///
/// Snapshot types live here by mandate (ADR-0003 dependency rule 3):
/// downstream crates consume damage plus snapshots and never touch grid
/// internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Structural version of this view ([`SNAPSHOT_VERSION`]).
    pub version: u32,
    /// Damage generation the snapshot corresponds to.
    pub generation: u64,
    /// Grid width in columns.
    pub width: usize,
    /// Grid height in rows.
    pub height: usize,
    /// Active screen cells, row-major, length `width * height`.
    pub cells: Box<[Cell]>,
    /// Live cursor (position, pen, visibility).
    pub cursor: Cursor,
    /// Current mode register.
    pub modes: Modes,
    /// Window/icon title (`OSC 0`/`OSC 2`).
    pub title: BoundedString,
}

/// Counters for semantically inert unmapped sequences (RFC coverage rule:
/// catch-all variants are inert and counted in telemetry). Telemetry lives
/// outside the state hash: counting must never mutate Terminal Truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TelemetryCounters {
    /// Unmapped CSI dispatches.
    pub unknown_csi: u64,
    /// Unmapped ESC dispatches.
    pub unknown_esc: u64,
    /// Unmapped DCS strings.
    pub unknown_dcs: u64,
    /// Unmapped OSC codes.
    pub unknown_osc: u64,
    /// Kitty graphics commands refused after parsing (unknown delete
    /// selector, missing relative parent, virtual/relative mix,
    /// unresolvable animation target, over-cap frames). The wire bytes
    /// were well-formed; the request itself could not be honored.
    pub kitty_refused: u64,
}

#[derive(Debug, Clone)]
struct ScreenSave {
    cursor_position: CursorPosition,
    pending_wrap: bool,
    style: Style,
    cursor_style: CursorStyle,
    cursor_visible: bool,
    origin_mode: bool,
    auto_wrap: bool,
    charsets: Charsets,
    modes: Modes,
}

/// The terminal state machine: grid, cursor, modes, scrollback, damage,
/// replies, and the typed action transition function.
#[derive(Debug, Clone)]
pub struct State {
    width: usize,
    height: usize,
    screens: ScreenPair,
    alt_screen: AltScreen,
    primary_save: Option<ScreenSave>,
    /// Kitty keyboard flag register of the currently **inactive** screen.
    ///
    /// The kitty protocol keeps separate flag stacks for the main and
    /// alternate screens (`main_key_encoding_flags` / `alt_key_encoding_flags`
    /// in the reference), so a `CSI = u` inside an alt screen that never
    /// negotiated must report zero, and main's register must survive
    /// untouched. [`Self::modes`]`.enhanced_keyboard` is the live (active screen)
    /// register; this field holds the other screen's register and is swapped
    /// on each alt-screen transition (`CTX-0575` F3).
    enhanced_keyboard_stash: EnhancedKeyboardState,
    saved_cursors: [Option<SavedCursor>; 2],
    cursor: Cursor,
    /// Configured default cursor shape (CTX-0756, issue #1359
    /// `terminal.cursor_style`). Seeds new panes and resolves an app
    /// `DECSCUSR 0` (`CursorStyle::Default`) reset; `Default` itself means
    /// the renderer's block fallback.
    default_cursor_style: CursorStyle,
    modes: Modes,
    scroll_region_top: u16,
    scroll_region_bottom: u16,
    tabs: TabStops,
    charsets: Charsets,
    scrollback: Scrollback,
    replies: Replies,
    title: BoundedString,
    cwd_report: Option<BoundedString>,
    hyperlink_table: VecDeque<HyperlinkEntry>,
    next_hyperlink_id: u32,
    current_hyperlink: Option<HyperlinkId>,
    zones: VecDeque<ZoneRecord>,
    zone_counter: u64,
    /// Last printable output since the last `OutputStart` (CTX-0996).
    /// Reset to `None` at each `OutputStart` so pre-command prompt text on
    /// the same row never counts as output; set by every placed glyph.
    /// `None` means no command output has landed since the last start.
    last_output_print: Option<OutputPrintMark>,
    /// Buffer epoch for zone-anchor validation (M1-18, CTX-0665). Bumped
    /// on every operation that wholesale invalidates buffer-row identity:
    /// scrollback clear (`ED 3`) and resize reflow (which reassigns
    /// scrollback ids). Full reset returns it to zero alongside dropping
    /// the zone log, so a reset state matches a fresh one. Push-prune
    /// needs no bump: oldest-first eviction shifts rows arithmetically
    /// (see `evicted_at_mark`).
    buffer_epoch: u64,
    generation: u64,
    damage_history: VecDeque<Damage>,
    batch_rects: Vec<DamageRect>,
    batch_scroll_events: Vec<(u64, u64)>,
    telemetry: TelemetryCounters,
    images: ImageStore,
    /// Kitty graphics placements + animation descriptors (CTX-0950).
    ///
    /// Grid-anchored display records for `a=p`/`a=T` and virtual
    /// (`U=1`) prototypes for `U+10EEEE` runs; pixels live downstream.
    kitty_placements: PlacementStore,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// A freshly initialized terminal at [`GRID_ROWS`] x [`GRID_COLUMNS`]
    /// retaining at most [`SCROLLBACK_DEFAULT_LINES`] scrollback lines.
    #[must_use]
    pub fn new() -> Self {
        Self::with_scrollback_lines(SCROLLBACK_DEFAULT_LINES)
    }

    /// A freshly initialized terminal at [`GRID_ROWS`] x [`GRID_COLUMNS`]
    /// retaining at most `max_lines` scrollback lines (the effective
    /// `terminal.scrollback` value). The capacity is clamped to
    /// [`SCROLLBACK_MAX_LINES`](crate::scrollback::SCROLLBACK_MAX_LINES);
    /// `0` disables scrollback retention.
    #[must_use]
    pub fn with_scrollback_lines(max_lines: usize) -> Self {
        Self {
            width: GRID_COLUMNS,
            height: GRID_ROWS,
            screens: ScreenPair::new(GRID_ROWS, GRID_COLUMNS),
            alt_screen: AltScreen::Off,
            primary_save: None,
            enhanced_keyboard_stash: EnhancedKeyboardState::default(),
            saved_cursors: [None, None],
            cursor: Cursor::default(),
            default_cursor_style: CursorStyle::Default,
            modes: Modes::default(),
            scroll_region_top: 0,
            scroll_region_bottom: (GRID_ROWS - 1) as u16,
            tabs: TabStops::default_lattice(GRID_COLUMNS),
            charsets: Charsets::default(),
            scrollback: Scrollback::with_max_lines(max_lines),
            replies: Replies::new(),
            title: BoundedString::new(""),
            cwd_report: None,
            hyperlink_table: VecDeque::new(),
            next_hyperlink_id: 0,
            current_hyperlink: None,
            zones: VecDeque::new(),
            zone_counter: 0,
            last_output_print: None,
            buffer_epoch: 0,
            generation: 0,
            damage_history: VecDeque::new(),
            batch_rects: Vec::new(),
            batch_scroll_events: Vec::new(),
            telemetry: TelemetryCounters::default(),
            images: ImageStore::new(),
            kitty_placements: PlacementStore::new(),
        }
    }

    /// Configured default cursor shape (CTX-0756, issue #1359).
    #[must_use]
    pub fn default_cursor_style(&self) -> CursorStyle {
        self.default_cursor_style
    }

    /// Sets the configured default cursor shape (the effective
    /// `terminal.cursor_style` value, applied by the runtime at terminal
    /// creation).
    ///
    /// The stored default resolves every later app `DECSCUSR 0` reset. When
    /// the live cursor still shows `Default` (a fresh pane, or one the app
    /// never reshaped) it is seeded to `style` as well, so one call at
    /// creation establishes both; an app-reshaped live cursor is never
    /// clobbered.
    pub fn set_default_cursor_style(&mut self, style: CursorStyle) {
        self.default_cursor_style = style;
        if self.cursor.cursor_style == CursorStyle::Default {
            self.cursor.cursor_style = style;
        }
    }

    /// Resolves an incoming `DECSCUSR` style against the configured
    /// default: `Default` (app reset, `CSI 0 SP q`) maps back to
    /// [`Self::default_cursor_style`]; every explicit shape applies as-is.
    fn resolve_cursor_style(&self, style: CursorStyle) -> CursorStyle {
        match style {
            CursorStyle::Default => self.default_cursor_style,
            other => other,
        }
    }

    // ------------------------------------------------------------------
    // Read-only accessors (snapshot-oriented public API)
    // ------------------------------------------------------------------

    /// Grid width in columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Grid height in rows.
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Resizes the terminal grid to `new_cols x new_rows` with xterm-class
    /// reflow, bounded to `[1, 1000]` per dimension (same bound as
    /// `RuntimeConfig` to keep memory bounded under T-01).
    ///
    /// Primary screen (`main` grid + scrollback) rewraps logical lines to
    /// the new width so narrowing never drops tails: scrollback and grid
    /// rows are unwrapped via their soft-wrap continuation flags, then
    /// rewrapped to `cols` with wide-pair atomicity (a `width == 2` lead
    /// plus its spacer never splits; a wide that would straddle the right
    /// margin wraps whole, leaving one blank). Combining marks ride on
    /// their base cell (`Cell::zerowidth`) and are never torn. Overflow
    /// past `rows` feeds scrollback oldest-first (capped by the scrollback
    /// buffer's configured capacity); underflow pads the grid bottom with
    /// blanks. When the height shrinks in the same call, trailing blank
    /// viewport rows absorb up to the height reduction first, so real content
    /// is not bottom-aligned behind them into scrollback (CTX-0312).
    /// Scrollback ids are reassigned fresh (still monotonic) because the
    /// physical row count changes; `total_written` advances with them.
    /// The cursor follows its logical line/offset when the primary screen
    /// is active, else it is clamped.
    ///
    /// Alternate screen (`alt` grid) never reflows (xterm behavior: the
    /// fullscreen application owns its layout): it truncates/pads with
    /// wide-pair orphan repair and clears all wrap flags.
    ///
    /// Height-only resizes keep the old truncate/pad path for both grids
    /// (no width change, no logical rewrapping). Scroll region resets to
    /// full screen and the cursor is stepped off spacers (RFC invariants
    /// 1-6). This is the environment-declared resize path (RFC "Environment
    /// declaration") and the only mutation of retained scrollback outside
    /// `push`/`clear`. Returns full-grid plus scrollback-reflow damage
    /// tagged with the new generation. Headless: pure in-memory, no I/O,
    /// deterministic.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) -> Damage {
        let cols = new_cols.clamp(1, MAX_GRID_DIM);
        let rows = new_rows.clamp(1, MAX_GRID_DIM);
        if cols == self.width && rows == self.height {
            return Damage {
                generation: self.generation,
                regions: Vec::new().into_boxed_slice(),
            };
        }
        let erase = self.bce_style();
        // M1-18: any geometry change reassigns scrollback ids (width
        // reflow) or drops grid rows (height shrink), so every zone
        // buffer anchor from before this call is invalid.
        self.buffer_epoch += 1;
        let old_cols = self.width;
        let old_rows = self.height;
        let alt_active = self.alt_screen != AltScreen::Off;
        // Snapshot old logical lines + primary cursor offset before mutating,
        // so a width reflow can keep the cursor on the same content. Only
        // when the primary screen is active; alt-screen cursors clamp.
        // Collected once and reused by the reflow below (single pass,
        // CTX-0469); the cursor mapping additionally reuses the reflow's
        // per-logical row counts instead of rewrapping every logical again.
        let width_changed = cols != old_cols;
        let (old_logicals, row_map): (Vec<Vec<Cell>>, Vec<(usize, usize)>) = if width_changed {
            Self::collect_logical_lines(&self.screens.main, &self.scrollback, old_rows)
        } else {
            (Vec::new(), Vec::new())
        };
        let cursor_logical: Option<(usize, usize)> = if !alt_active && width_changed {
            let crow = self.cursor.position.row as usize;
            let ccol = self.cursor.position.col as usize;
            let sb_len = self.scrollback.len();
            // Cursor offset: leads before `ccol` within its row, clamped
            // to the trimmed segment (blank tail -> end-of-content).
            let cursor_combined = sb_len + crow;
            let (cli, seg_start) = row_map[cursor_combined];
            let row_cells = self.screens.main.row(crow).to_vec();
            let mut units_before = 0usize;
            for (idx, cell) in row_cells.iter().enumerate() {
                if idx >= ccol {
                    break;
                }
                if !cell.spacer {
                    units_before += 1;
                }
            }
            let seg_len = trim_row_to_leads(&row_cells).len();
            let offset = seg_start + units_before.min(seg_len);
            Some((cli, offset))
        } else {
            None
        };
        let reflow_counts: Vec<usize> = if cols == old_cols {
            // Height-only: no logical rewrapping. Truncate/pad both grids;
            // scrollback widths already match.
            self.screens.main.resize(rows, cols, &erase);
            self.screens.alt.resize(rows, cols, &erase);
            Vec::new()
        } else {
            // Width change: reflow primary (main + scrollback), truncate alt.
            let counts = Self::reflow_primary(
                &mut self.screens.main,
                &mut self.scrollback,
                &old_logicals,
                cols,
                rows,
                &erase,
            );
            self.screens.alt.resize(rows, cols, &erase);
            counts
        };
        // Resize tab lattice: preserve stops that still fit, default for new columns.
        let old_len = self.tabs.len();
        let mut new_tabs = crate::tabs::TabStops::default_lattice(cols);
        for c in 0..old_len.min(cols) {
            if self.tabs.contains(c) {
                new_tabs.set(c);
            } else {
                new_tabs.clear_at(c);
            }
        }
        self.tabs = new_tabs;
        self.width = cols;
        self.height = rows;
        // Reset scroll region to the full screen (clamps invariants 1) and
        // place the cursor: logical mapping after a width reflow, else clamp.
        self.scroll_region_top = 0;
        self.scroll_region_bottom = (rows - 1) as u16;
        if let Some((line_idx, unit_offset)) = cursor_logical {
            // Map the saved logical position through the new width. Find
            // where logical `line_idx` starts in the new combined order and
            // offset within it.
            if line_idx < old_logicals.len() {
                let logical = &old_logicals[line_idx];
                // Rewrap just this logical to locate the target row/col.
                let (row_off, col) = map_unit_to_rewrapped(logical, unit_offset, cols, &erase);
                // This logical's start in the new total order is the prefix
                // sum of the reflow's per-logical row counts (no second
                // rewrapping pass, CTX-0469).
                let total_idx: usize = reflow_counts.iter().take(line_idx).sum();
                let target_total = total_idx + row_off;
                let new_sb_len = self.scrollback.len();
                if target_total < new_sb_len {
                    // Content scrolled into history: pin to grid top, mapped col.
                    self.cursor.position.row = 0;
                    self.cursor.position.col = (col.min(cols - 1)) as u16;
                } else {
                    let grid_row = (target_total - new_sb_len).min(rows - 1);
                    self.cursor.position.row = grid_row as u16;
                    self.cursor.position.col = (col.min(cols - 1)) as u16;
                }
            } else {
                self.cursor.position.row = self.cursor.position.row.min((rows - 1) as u16);
                self.cursor.position.col = self.cursor.position.col.min((cols - 1) as u16);
            }
        } else {
            self.cursor.position.row = self.cursor.position.row.min((rows - 1) as u16);
            self.cursor.position.col = self.cursor.position.col.min((cols - 1) as u16);
        }
        self.cursor.pending_wrap = false;
        self.enforce_cursor_invariants();
        for (slot, saved) in self.saved_cursors.iter_mut().enumerate() {
            if let Some(s) = saved {
                s.position.row = s.position.row.min((rows - 1) as u16);
                s.position.col = s.position.col.min((cols - 1) as u16);
                let grid = if slot == 0 {
                    &self.screens.main
                } else {
                    &self.screens.alt
                };
                if grid
                    .get(s.position.row as usize, s.position.col as usize)
                    .spacer
                    && s.position.col > 0
                {
                    s.position.col -= 1;
                }
            }
        }
        if let Some(save) = &mut self.primary_save {
            save.cursor_position.row = save.cursor_position.row.min((rows - 1) as u16);
            save.cursor_position.col = save.cursor_position.col.min((cols - 1) as u16);
            if self
                .screens
                .main
                .get(
                    save.cursor_position.row as usize,
                    save.cursor_position.col as usize,
                )
                .spacer
                && save.cursor_position.col > 0
            {
                save.cursor_position.col -= 1;
            }
        }
        // Damage for resize: full grid plus scrollback reflow range when
        // scrollback non-empty (RFC damage model). Coalesce ordering is grid
        // rectangles first, then scrollback ranges.
        self.generation += 1;
        let mut regions: Vec<DamagedRegion> = Vec::new();
        regions.push(DamagedRegion::Grid(DamageRect::full(
            rows as u16,
            cols as u16,
        )));
        if !self.scrollback.is_empty() {
            let first = self.scrollback.line(0).map(|l| l.id).unwrap_or(0);
            let count = self.scrollback.len() as u64;
            regions.push(DamagedRegion::Scrollback {
                first_line_id: first,
                count,
            });
        }
        let damage = Damage {
            generation: self.generation,
            regions: regions.into_boxed_slice(),
        };
        if self.damage_history.len() == DAMAGE_HISTORY_BATCHES {
            self.damage_history.pop_front();
        }
        self.damage_history.push_back(damage.clone());
        self.batch_rects.clear();
        self.batch_scroll_events.clear();
        debug_assert!(
            self.check_invariants().is_ok(),
            "RFC invariants violated after resize: {:?}",
            self.check_invariants()
        );
        damage
    }

    /// Collects combined (scrollback + grid) physical rows into logical
    /// lines by unwrapping via soft-wrap flags, oldest first. Called once by
    /// [`State::resize`] and consumed by [`State::reflow_primary`], so a
    /// width resize collects exactly once (CTX-0469).
    ///
    /// Returns the logical lines plus a per-combined-row map of
    /// `(logical_idx, seg_start_in_logical)` for cursor bookkeeping. The
    /// last grid row is always a hard break (no next grid row).
    /// Deterministic, headless, bounded.
    fn collect_logical_lines(
        main: &Grid,
        scrollback: &Scrollback,
        old_rows: usize,
    ) -> (Vec<Vec<Cell>>, Vec<(usize, usize)>) {
        let sb_len = scrollback.len();
        let total = sb_len + old_rows;
        let mut logicals: Vec<Vec<Cell>> = Vec::new();
        let mut cur: Vec<Cell> = Vec::new();
        // (logical_idx, seg_start_in_logical) per combined row.
        let mut row_map: Vec<(usize, usize)> = Vec::with_capacity(total);
        for combined in 0..total {
            let (segment, wrapped) = if combined < sb_len {
                let line = scrollback.line(combined).expect("scrollback index");
                (trim_row_to_leads(&line.cells), line.wrapped)
            } else {
                let gr = combined - sb_len;
                let row_cells = main.row(gr).to_vec();
                // Last grid row has no next grid row: hard break.
                let w = if gr + 1 < old_rows {
                    main.wrapped(gr)
                } else {
                    false
                };
                (trim_row_to_leads(&row_cells), w)
            };
            row_map.push((logicals.len(), cur.len()));
            cur.extend(segment);
            if !wrapped {
                logicals.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() || logicals.is_empty() {
            logicals.push(std::mem::take(&mut cur));
        }
        (logicals, row_map)
    }

    /// Reflows the primary screen (`grid` + `scrollback`) to a new width,
    /// bottom-aligning the last `new_rows` physical rows into the grid.
    ///
    /// Unwraps via soft-wrap flags, rewraps with wide-pair atomicity (see
    /// `rewrap_one_logical`), pads underflow at the grid bottom, caps
    /// scrollback at the buffer's configured capacity oldest-first, and
    /// reassigns fresh monotonic scrollback ids. Trailing blank viewport rows
    /// absorb up to the height reduction before the bottom-align split
    /// (CTX-0312), so a width+height shrink keeps real content visible.
    /// Deterministic, headless, bounded.
    ///
    /// Returns the physical row count per input logical line so callers can
    /// map positions without rewrapping again (CTX-0469).
    fn reflow_primary(
        grid: &mut crate::grid::Grid,
        scrollback: &mut crate::scrollback::Scrollback,
        logicals: &[Vec<Cell>],
        new_cols: usize,
        new_rows: usize,
        erase: &Style,
    ) -> Vec<usize> {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        let (old_rows, _) = grid.dims();
        // Rewrap every logical line, recording row counts for the caller.
        let mut physical: Vec<(Vec<Cell>, bool)> = Vec::new();
        let mut row_counts: Vec<usize> = Vec::with_capacity(logicals.len());
        for ll in logicals {
            let rewrapped = rewrap_one_logical(ll, new_cols, erase);
            row_counts.push(rewrapped.len());
            physical.extend(rewrapped);
        }
        // CTX-0312: when the height shrinks in the same call as a width
        // change, trailing blank viewport rows absorb the reduction first.
        // Without this the bottom-align split keeps the blank tail and moves
        // real content rows above it into scrollback (80x24 "hello world" ->
        // resize(38, 23) used to leave sb=1 and a blank grid). Only up to the
        // height delta is trimmed, so a width-only reflow never consumes blank
        // rows and never pulls existing history back into the grid.
        let shrink = old_rows.saturating_sub(new_rows);
        if shrink > 0 {
            let mut trimmed = 0;
            while trimmed < shrink
                && physical.len() > trimmed
                && physical[physical.len() - 1 - trimmed]
                    .0
                    .iter()
                    .all(Cell::is_blank)
            {
                trimmed += 1;
            }
            if trimmed > 0 {
                physical.truncate(physical.len() - trimmed);
            }
        }
        // Split bottom-aligned: last `new_rows` to grid, rest to scrollback.
        if physical.len() < new_rows {
            let need = new_rows - physical.len();
            for _ in 0..need {
                physical.push((vec![Cell::erased(*erase); new_cols], false));
            }
        }
        let split = physical.len() - new_rows;
        let (sb_part, grid_part) = physical.split_at(split);
        // Rebuild scrollback with fresh ids (monotonic), oldest-first, capped
        // at this buffer's configured capacity.
        let max_lines = scrollback.max_lines();
        scrollback.clear();
        let start = sb_part.len().saturating_sub(max_lines);
        for (cells, wrapped) in &sb_part[start..] {
            scrollback.push_with_wrap(cells.clone(), *wrapped);
        }
        // Rebuild grid.
        let mut new_cells: Vec<Cell> = Vec::with_capacity(new_rows * new_cols);
        let mut new_wraps: Vec<bool> = Vec::with_capacity(new_rows);
        for (cells, wrapped) in grid_part {
            debug_assert_eq!(cells.len(), new_cols);
            new_cells.extend(cells.iter().cloned());
            new_wraps.push(*wrapped);
        }
        grid.replace_grid(new_rows, new_cols, new_cells, new_wraps);
        row_counts
    }

    /// The live cursor.
    #[must_use]
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    /// The current mode register.
    #[must_use]
    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    /// Whether the alternate screen is active.
    #[must_use]
    pub fn alt_screen_active(&self) -> bool {
        self.alt_screen != AltScreen::Off
    }

    /// The window/icon title.
    #[must_use]
    pub fn title(&self) -> &str {
        self.title.as_str()
    }

    /// The most recent working-directory report (`OSC 7`), if any.
    #[must_use]
    pub fn cwd_report(&self) -> Option<&str> {
        self.cwd_report.as_ref().map(BoundedString::as_str)
    }

    /// Retained scrollback line count.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// The retained scrollback line at `index` (oldest first).
    #[must_use]
    pub fn scrollback_line(&self, index: usize) -> Option<&ScrollbackLine> {
        self.scrollback.line(index)
    }

    /// Iterates retained scrollback lines oldest first.
    pub fn scrollback(&self) -> impl Iterator<Item = &ScrollbackLine> {
        self.scrollback.iter()
    }

    /// Owned heap retained by this terminal (CTX-1026).
    ///
    /// Capacity-accurate sum over every owned container: both screens
    /// (cells + wrap flags by capacity, not `width * height`), the
    /// scrollback ring plus every boxed line, the hyperlink/zone/damage
    /// tables, the reply/image/placement stores, tab stops, and the
    /// title/cwd strings. A wider [`Cell`], an over-allocating grid, or a
    /// new unbounded container trips the memory gate instead of hiding
    /// behind constants.
    #[must_use]
    pub fn retained_heap_bytes(&self) -> usize {
        let screens = self.screens.heap_bytes();
        let scrollback = self.scrollback.heap_bytes();
        let tabs = self.tabs.heap_bytes();
        let replies = self.replies.heap_bytes();
        let images = self.images.heap_bytes();
        let placements = self.kitty_placements.heap_bytes();
        let title = self.title.len();
        let cwd = self.cwd_report.as_ref().map_or(0, |s| s.len());
        let hyperlink_ring =
            self.hyperlink_table.capacity() * std::mem::size_of::<HyperlinkEntry>();
        let hyperlink_strings: usize = self
            .hyperlink_table
            .iter()
            .map(|entry| entry.id_param.as_ref().map_or(0, |s| s.len()) + entry.uri.len())
            .sum();
        let zones = self.zones.capacity() * std::mem::size_of::<ZoneRecord>();
        let damage_ring = self.damage_history.capacity() * std::mem::size_of::<Damage>();
        let damage_regions: usize = self
            .damage_history
            .iter()
            .map(|damage| damage.regions.len() * std::mem::size_of::<DamagedRegion>())
            .sum();
        let batch_rects = self.batch_rects.capacity() * std::mem::size_of::<DamageRect>();
        let batch_scrolls = self.batch_scroll_events.capacity() * std::mem::size_of::<(u64, u64)>();
        screens
            + scrollback
            + tabs
            + replies
            + images
            + placements
            + title
            + cwd
            + hyperlink_ring
            + hyperlink_strings
            + zones
            + damage_ring
            + damage_regions
            + batch_rects
            + batch_scrolls
    }

    /// Rehydrates scrollback history from plain-text lines (CTX-0393 session
    /// restore).
    ///
    /// Each line becomes one immutable scrollback entry at the current grid
    /// width: wide scalars keep lead-plus-spacer atomicity (a wide scalar
    /// without room wraps no tail — the line truncates there, mirroring
    /// resize reflow), combining marks ride on their base cell, control
    /// scalars degrade to `U+FFFD`, and short lines pad with erased cells.
    /// Lines append oldest-first; over-capacity input prunes oldest-first
    /// through the buffer's configured bound, so memory stays bounded no
    /// matter the input length. Callers bound the input (the session layer
    /// caps lines per pane); this method stays total over any input and
    /// returns the pushed line count. It records no damage: restore callers
    /// force a full present after rehydration.
    pub fn restore_scrollback_text(&mut self, lines: &[&str]) -> usize {
        let width = self.width.max(1);
        let blank = Style::default();
        let mut pushed = 0usize;
        for text in lines {
            let mut cells: Vec<Cell> = Vec::with_capacity(width);
            for scalar in text.chars() {
                if cells.len() >= width {
                    break;
                }
                let scalar = if scalar.is_control() {
                    '\u{FFFD}'
                } else {
                    scalar
                };
                match char_cell_width(scalar) {
                    0 => {
                        if let Some(prev) = cells.last_mut() {
                            let _ = prev.push_zerowidth(scalar);
                        }
                    }
                    2 => {
                        if cells.len() + 2 <= width {
                            cells.push(Cell {
                                glyph: scalar,
                                style: blank,
                                width: 2,
                                spacer: false,
                                hyperlink: None,
                                zerowidth: Zerowidth::new(),
                            });
                            cells.push(Cell::wide_spacer(blank));
                        } else {
                            break;
                        }
                    }
                    _ => cells.push(Cell {
                        glyph: scalar,
                        style: blank,
                        width: 1,
                        spacer: false,
                        hyperlink: None,
                        zerowidth: Zerowidth::new(),
                    }),
                }
            }
            while cells.len() < width {
                cells.push(Cell::erased(blank));
            }
            self.scrollback.push_with_wrap(cells, false);
            pushed += 1;
        }
        pushed
    }

    /// Retained semantic-zone records oldest first.
    pub fn zones(&self) -> impl Iterator<Item = &ZoneRecord> {
        self.zones.iter()
    }

    /// Number of distinct hyperlink identities retained (bounded, see
    /// [`HYPERLINK_TABLE_MAX`]).
    #[must_use]
    pub fn hyperlink_count(&self) -> usize {
        self.hyperlink_table.len()
    }

    /// Resolves a [`HyperlinkId`] to its `(id, uri)` pair when present.
    ///
    /// `id` is the optional OSC 8 `id=` parameter; `uri` is the target.
    /// Evicted or unknown ids fail closed (`None`), and no resident id ever
    /// resolves to a different URI than the one it was issued for while it
    /// stays resident (CTX-0469).
    ///
    /// Issuance is monotonic and the table is a FIFO window, so resident ids
    /// are contiguous and ascending ([`Self::hyperlink_table`] exposes the
    /// window); lookup is front-id arithmetic over the deque — O(1) per call
    /// instead of a scan of up to [`HYPERLINK_TABLE_MAX`] entries (CTX-0490).
    ///
    /// Honest bound (CTX-0490): once per 2^32 distinct links the id space
    /// restarts at zero after clearing the table, so a cell that kept a
    /// pre-wrap id can resolve to a post-wrap entry (see [`HyperlinkEntry`]).
    #[must_use]
    pub fn hyperlink_entry(&self, id: HyperlinkId) -> Option<(Option<&str>, &str)> {
        let front_id = self.hyperlink_table.front()?.id.as_u32();
        let offset = usize::try_from(id.as_u32().checked_sub(front_id)?).ok()?;
        let entry = self.hyperlink_table.get(offset)?;
        if entry.id != id {
            // Defensive: verify the window invariant instead of assuming it.
            return None;
        }
        Some((
            entry.id_param.as_ref().map(BoundedString::as_str),
            entry.uri.as_str(),
        ))
    }

    /// Iterates the hyperlink table oldest first; `(HyperlinkId, Option<id>,
    /// uri)`.
    pub fn hyperlink_table(&self) -> impl Iterator<Item = (HyperlinkId, Option<&str>, &str)> + '_ {
        self.hyperlink_table.iter().map(|entry| {
            (
                entry.id,
                entry.id_param.as_ref().map(BoundedString::as_str),
                entry.uri.as_str(),
            )
        })
    }

    /// The hyperlink currently applied to newly printed cells, if any.
    #[must_use]
    pub fn current_hyperlink(&self) -> Option<HyperlinkId> {
        self.current_hyperlink
    }

    /// Number of retained semantic-zone records.
    #[must_use]
    pub fn zone_len(&self) -> usize {
        self.zones.len()
    }

    /// Current buffer epoch for zone-anchor validation (M1-18, CTX-0665).
    #[must_use]
    pub fn buffer_epoch(&self) -> u64 {
        self.buffer_epoch
    }

    /// Total scrollback lines evicted so far (pushed minus retained).
    ///
    /// `Scrollback::clear` keeps `total_written` while dropping retained
    /// lines, so the count jumps there too — but every clear also bumps
    /// [`Self::buffer_epoch`], and resolution checks the epoch first, so
    /// a cleared anchor fails closed instead of landing on new content.
    #[must_use]
    pub fn scrollback_evicted_total(&self) -> u64 {
        self.scrollback
            .total_written()
            .saturating_sub(self.scrollback.len() as u64)
    }

    /// Resolves a zone record's buffer anchor to its current combined
    /// buffer row (`0` = oldest retained scrollback).
    ///
    /// Headless, deterministic, total: returns `None` when the marked
    /// line is gone — pruned past the retained window (eviction drift
    /// exceeds the mark row), cleared/reset/reflowed (epoch mismatch),
    /// recorded on the other screen, or out of the current buffer — and
    /// `Some(row)` otherwise. Live content that scrolled into history
    /// keeps resolving: full-screen scrolls preserve `buffer_row`
    /// exactly, and prune shifts are subtracted arithmetically.
    #[must_use]
    pub fn zone_buffer_row(&self, record: &ZoneRecord) -> Option<usize> {
        if record.epoch_at_mark != self.buffer_epoch {
            return None;
        }
        if record.on_alt_screen != self.alt_screen_active() {
            return None;
        }
        let drift = self
            .scrollback_evicted_total()
            .checked_sub(record.evicted_at_mark)? as usize;
        let row = record.buffer_row.checked_sub(drift)?;
        if row < self.scrollback.len() + self.height {
            Some(row)
        } else {
            None
        }
    }

    /// Buffer row of the nearest `PromptStart` (`OSC 133;A`) marker
    /// strictly before `from`, if it still resolves.
    ///
    /// Prompt-jump primitive (M1-18): view layers scroll to the returned
    /// row to move to the previous command block. Unresolvable (pruned,
    /// cleared, other screen) prompts are skipped, never returned.
    #[must_use]
    pub fn prev_prompt_buffer_row(&self, from: usize) -> Option<usize> {
        self.zones
            .iter()
            .filter(|r| r.kind == ZoneKind::PromptStart)
            .filter_map(|r| self.zone_buffer_row(r))
            .filter(|row| *row < from)
            .max()
    }

    /// Buffer row of the nearest `PromptStart` (`OSC 133;A`) marker
    /// strictly after `from`, if it still resolves.
    ///
    /// Mirror of [`Self::prev_prompt_buffer_row`] for forward prompt-jump.
    #[must_use]
    pub fn next_prompt_buffer_row(&self, from: usize) -> Option<usize> {
        self.zones
            .iter()
            .filter(|r| r.kind == ZoneKind::PromptStart)
            .filter_map(|r| self.zone_buffer_row(r))
            .filter(|row| *row > from)
            .min()
    }

    /// Combined buffer rows `(start, end)` (inclusive) of the last command's
    /// output, when a complete non-empty range still resolves.
    ///
    /// Select-output primitive (CTX-0952, issue #1670; per-row tracking
    /// CTX-0996, issue #1688): the last `OutputStart` (`OSC 133;C`) by
    /// arrival order opens the range; the first `OutputEnd` (`OSC 133;D`)
    /// after it closes the range. When the end mark row itself holds command
    /// output (final partial line without a trailing newline) the range
    /// includes that row; otherwise — the trailing-CR+LF shape where `D`
    /// arrives on the fresh row that becomes the next prompt — the range
    /// closes at the row before the end mark, so the next prompt line is
    /// never swallowed. With no end mark yet the command is still running
    /// and the range closes at the cursor row (see [`Self::live_output_end`]).
    ///
    /// The `D`-row decision uses per-row print tracking
    /// (`output_on_mark_row`, decided at mark time), never column zero: a
    /// bare LF advances the row without resetting the column, so a fresh
    /// row can carry a nonzero column while holding no output.
    ///
    /// Fail-closed (`None`) when no output start resolves (no marks, pruned,
    /// cleared, resized, or another screen), when the resolved range is
    /// empty (end before start: zero-byte output, a same-row `C`/`D` pair
    /// with no output on it, or the running-command cursor sitting above
    /// the start), or when the end mark lands on row zero. Unresolvable
    /// marks are skipped, never returned — like [`Self::prev_prompt_buffer_row`].
    #[must_use]
    pub fn last_command_output_rows(&self) -> Option<(usize, usize)> {
        let (start_ordinal, start) = self
            .zones
            .iter()
            .filter(|r| r.kind == ZoneKind::OutputStart)
            .filter_map(|r| self.zone_buffer_row(r).map(|row| (r.ordinal, row)))
            .max_by_key(|(ordinal, _)| *ordinal)?;
        let end = match self
            .zones
            .iter()
            .filter(|r| r.kind == ZoneKind::OutputEnd && r.ordinal > start_ordinal)
            .filter_map(|r| {
                self.zone_buffer_row(r)
                    .map(|row| (r.ordinal, row, r.output_on_mark_row))
            })
            .min_by_key(|(ordinal, _, _)| *ordinal)
        {
            Some((_, end_mark, has_output)) => {
                if has_output {
                    end_mark
                } else {
                    end_mark.checked_sub(1)?
                }
            }
            None => self.live_output_end(start)?,
        };
        if end < start {
            return None;
        }
        Some((start, end))
    }

    /// Whether command output since the last `OutputStart` landed on combined
    /// buffer `row` (CTX-0996).
    ///
    /// Resolves [`Self::last_output_print`] against the current buffer with
    /// the same prune/epoch/screen rules as [`Self::zone_buffer_row`]: pruned,
    /// cleared, reflowed, or other-screen prints never match, and a row that
    /// never received command output reports `false` even when the cursor
    /// column is nonzero there (bare LF shape).
    fn output_print_on_row(&self, row: usize) -> bool {
        let Some(mark) = self.last_output_print else {
            return false;
        };
        if mark.epoch_at != self.buffer_epoch {
            return false;
        }
        if mark.on_alt_screen != self.alt_screen_active() {
            return false;
        }
        let Some(drift) = self
            .scrollback_evicted_total()
            .checked_sub(mark.evicted_at)
            .map(|d| d as usize)
        else {
            return false;
        };
        let Some(resolved) = mark.buffer_row.checked_sub(drift) else {
            return false;
        };
        if resolved >= self.scrollback.len() + self.height {
            return false;
        }
        resolved == row
    }

    /// Closing row for a still-running command whose output starts at
    /// `start` (combined buffer row): the cursor's combined row when that
    /// row holds command output, otherwise the row before it (a fresh line
    /// with no output on it yet). `None` when the cursor is above `start`
    /// or on `start` with no output yet (zero-byte running output).
    ///
    /// CTX-0996: the fresh-vs-partial decision uses per-row print tracking
    /// ([`Self::output_print_on_row`]), never column zero, so a fresh row
    /// entered by a bare LF (nonzero column, no output) still closes before
    /// it.
    fn live_output_end(&self, start: usize) -> Option<usize> {
        let cursor_row = (self.cursor.position.row as usize).min(self.height.saturating_sub(1));
        let cursor_buf = self.scrollback.len() + cursor_row;
        let end = if self.output_print_on_row(cursor_buf) {
            cursor_buf
        } else if cursor_buf > start {
            cursor_buf - 1
        } else {
            return None;
        };
        if end < start { None } else { Some(end) }
    }

    /// The image store; see `crate::image` for the OQ-008 status.
    ///
    /// Bounded placeholder stub (64 entries, 4096 bytes each) until the
    /// image RFC lands; no decoded pixels are held here.
    #[must_use]
    pub fn image_store(&self) -> &ImageStore {
        &self.images
    }

    /// The kitty placement store (CTX-0950, issue #1668).
    ///
    /// Grid-anchored display records and virtual prototypes plus bounded
    /// animation descriptors. The renderer composites them once decoded
    /// pixels arrive downstream; text-erase commands other than full
    /// clear leave them alone per the specification.
    #[must_use]
    pub fn kitty_placements(&self) -> &PlacementStore {
        &self.kitty_placements
    }

    #[must_use]
    pub fn telemetry(&self) -> TelemetryCounters {
        self.telemetry
    }

    /// Current damage generation; increments once per applied batch.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Coalesced damaged regions from batches newer than `generation`.
    ///
    /// History is bounded by [`DAMAGE_HISTORY_BATCHES`]; generations older
    /// than the retained window behave as a full-grid redraw request.
    ///
    /// The accumulated result is bounded by
    /// [`DAMAGE_MAX_REGIONS_PER_BATCH`], the same cap the render-side frame
    /// planner (`MAX_FRAME_REGIONS`) enforces, so both sides of the
    /// presentation boundary stay consistent: either the exact incremental
    /// union (when it fits) or one coarse full-grid rectangle. Over-damage
    /// is safe; under-damage is impossible because every fallback covers
    /// everything.
    ///
    /// The common path stays `O(window)` with an early empty return and no
    /// merging: the fallback only triggers on overflow.
    #[must_use]
    pub fn damage_since(&self, generation: u64) -> Vec<DamagedRegion> {
        if generation >= self.generation {
            return Vec::new();
        }
        let full_fallback = || {
            vec![DamagedRegion::Grid(DamageRect::full(
                self.height as u16,
                self.width as u16,
            ))]
        };
        let Some(oldest) = self.damage_history.front() else {
            // No retained batches but the caller is behind: without history
            // we cannot prove what changed, so request a full redraw
            // (conservative over-damage).
            return full_fallback();
        };
        if generation.saturating_add(1) < oldest.generation {
            return full_fallback();
        }
        let mut regions = Vec::new();
        for batch in &self.damage_history {
            if batch.generation > generation {
                if regions.len().saturating_add(batch.regions.len()) > DAMAGE_MAX_REGIONS_PER_BATCH
                {
                    return full_fallback();
                }
                regions.extend_from_slice(&batch.regions);
            }
        }
        // Defensive: a single batch already respects the per-batch cap, but
        // scrollback ranges ride along, so re-check before returning.
        if regions.len() > DAMAGE_MAX_REGIONS_PER_BATCH {
            return full_fallback();
        }
        regions
    }

    /// Borrows one active-screen row without cloning (CTX-0469).
    ///
    /// [`State::search`] uses this instead of [`State::snapshot`]: a full
    /// snapshot clones up to `MAX_GRID_DIM` squared (1M) cells per call.
    /// Returns `None` when `row` is out of bounds.
    #[must_use]
    pub fn live_grid_row(&self, row: usize) -> Option<&[Cell]> {
        let (rows, _) = self.screens_active().dims();
        if row < rows {
            Some(self.screens_active().row(row))
        } else {
            None
        }
    }

    /// Builds a versioned snapshot of the active screen.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let (rows, cols) = self.screens_active().dims();
        Snapshot {
            version: SNAPSHOT_VERSION,
            generation: self.generation,
            width: cols,
            height: rows,
            cells: self.screens_active().flatten_cells(),
            cursor: self.cursor.clone(),
            modes: self.modes.clone(),
            title: self.title.clone(),
        }
    }

    /// Drains queued device-status replies (RFC: replies are returned to
    /// the caller; terminal state performs no I/O).
    pub fn take_replies(&mut self) -> Vec<Box<[u8]>> {
        self.replies.drain()
    }

    /// Whether any reply was dropped due to the reply cap since the last
    /// drain (RFC invariant 7).
    #[must_use]
    pub fn replies_overflowed(&self) -> bool {
        self.replies.overflowed()
    }

    fn peek_replies(&self) -> Vec<&[u8]> {
        // Replies exposes drain-only access; the hash needs a non-consuming
        // read, provided through this internal projection.
        self.replies.pending_slices()
    }

    // ------------------------------------------------------------------
    // Transition API
    // ------------------------------------------------------------------

    /// Applies one action as one processed batch: state transitions first,
    /// then the batch's damage is coalesced and returned tagged with the
    /// new generation (RFC damage model). Debug builds assert every RFC
    /// invariant afterwards.
    pub fn apply(&mut self, action: &TerminalAction) -> Damage {
        self.dispatch(action);
        self.finalize_batch()
    }

    fn dispatch(&mut self, action: &TerminalAction) {
        match action {
            TerminalAction::Print(cell) => self.print((*cell).clone().scalar()),
            TerminalAction::PrintControl(control) => self.print_control(control.0),

            TerminalAction::CursorMove { dir, n } => self.cursor_move(*dir, effective_count(*n)),
            TerminalAction::CursorPosition { row, col } => self.cursor_position(*row, *col),
            TerminalAction::CursorSave => self.cursor_save(),
            TerminalAction::CursorRestore => self.cursor_restore(),
            TerminalAction::CursorStyle { style } => {
                self.cursor.cursor_style = self.resolve_cursor_style(*style);
            }
            TerminalAction::CursorVisibility { visible } => self.cursor.visible = *visible,

            TerminalAction::EraseInDisplay { mode } => self.erase_in_display(*mode),
            TerminalAction::EraseInLine { mode } => self.erase_in_line(*mode),
            TerminalAction::EraseChars { n } => {
                let (row, col) = self.cursor_xy();
                let end = col
                    .saturating_add(effective_count(*n) as usize)
                    .saturating_sub(1);
                let erase = self.bce_style();
                let actual_range = self
                    .screens_active_mut()
                    .erase_range_in_row(row, col, end, &erase);
                if self.screens_active_mut().repair_row(row, &erase) {
                    let last_col = self.width as u16 - 1;
                    self.damage_grid_rect(row as u16, 0, row as u16, last_col);
                }
                if let Some((start, end)) = actual_range {
                    self.damage_grid_rect(row as u16, start as u16, row as u16, end as u16);
                }
            }

            TerminalAction::InsertLines { n } => self.insert_lines(effective_count(*n)),
            TerminalAction::DeleteLines { n } => self.delete_lines(effective_count(*n)),
            TerminalAction::InsertChars { n } => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                let damaged_col = if col > 0 && self.screens_active().get(row, col).spacer {
                    col - 1
                } else {
                    col
                };
                self.screens_active_mut().insert_blanks_in_row(
                    row,
                    col,
                    effective_count(*n) as usize,
                    &erase,
                );
                if self.screens_active_mut().repair_row(row, &erase) {
                    self.damage_row_tail(row as u16, 0);
                } else {
                    self.damage_row_tail(row as u16, damaged_col as u16);
                }
            }
            TerminalAction::DeleteChars { n } => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                let damaged_col = if col > 0 && self.screens_active().get(row, col).spacer {
                    col - 1
                } else {
                    col
                };
                self.screens_active_mut().delete_chars_in_row(
                    row,
                    col,
                    effective_count(*n) as usize,
                    &erase,
                );
                if self.screens_active_mut().repair_row(row, &erase) {
                    self.damage_row_tail(row as u16, 0);
                } else {
                    self.damage_row_tail(row as u16, damaged_col as u16);
                }
            }

            TerminalAction::ScrollUp { n } => self.scroll_up_region(effective_count(*n)),
            TerminalAction::ScrollDown { n } => self.scroll_down_region(effective_count(*n)),
            TerminalAction::SetScrollRegion { top, bottom } => {
                self.set_scroll_region(*top, *bottom)
            }

            TerminalAction::SetAttributes { attrs } => self.apply_attribute_diff(attrs),

            TerminalAction::SetMode { mode, enabled } => self.set_mode(*mode, *enabled),

            TerminalAction::TabSet => {
                let (_, col) = self.cursor_xy();
                self.tabs.set(col);
            }
            TerminalAction::TabClear { targets } => {
                let (_, col) = self.cursor_xy();
                match targets {
                    TabTargets::Current => self.tabs.clear_at(col),
                }
            }
            TerminalAction::TabClearAll => self.tabs.clear_all(),
            TerminalAction::TabForward { n } => {
                // CR-TERM-02: break at the right margin so a 65535-count
                // tab run from untrusted input cannot burn 65k lattice
                // scans; iterations past the margin are no-ops.
                let last = self.width - 1;
                let mut col = self.cursor.position.col as usize;
                for _ in 0..effective_count(*n) {
                    if col >= last {
                        break;
                    }
                    col = self.tabs.next_after(col).unwrap_or(last);
                }
                self.cursor.position.col = col as u16;
                self.cursor.pending_wrap = false;
            }
            TerminalAction::TabBackward { n } => {
                // CR-TERM-02: symmetric left-margin break for the same
                // amplification class (iterations at column 0 are no-ops).
                let mut col = self.cursor.position.col as usize;
                for _ in 0..effective_count(*n) {
                    if col == 0 {
                        break;
                    }
                    col = self.tabs.prev_before(col).unwrap_or(0);
                }
                self.cursor.position.col = col as u16;
                self.cursor.pending_wrap = false;
            }

            TerminalAction::SelectCharset { slot, table } => self.charsets.designate(*slot, *table),
            TerminalAction::InvokeCharset { slot } => self.charsets.lock(*slot),
            TerminalAction::SingleShiftCharset { slot } => self.charsets.arm_single(*slot),

            TerminalAction::RequestDeviceStatus { kind } => self.request_device_status(*kind),
            TerminalAction::Reply { bytes } => self.replies.queue(bytes.clone()),

            TerminalAction::OscTitle { text } => self.title = text.clone(),
            // Dynamic default colors (CTX-0381): grid truth is untouched.
            // The runtime owns the active palette, query replies, and the
            // gated set path; terminal state stays inert by contract.
            TerminalAction::OscDynamicColor { .. } => {}
            // Palette operations (CTX-0392 OSC 4): grid truth is untouched.
            // The runtime owns the active 256-entry palette, query replies,
            // and the gated set path; terminal state stays inert by contract
            // (malformed sequences never reach this arm).
            TerminalAction::OscPalette { .. } => {}
            TerminalAction::OscClipboard { .. } => {
                // Semantically inert here by contract (RFC replay guarantee
                // 6): clipboard effects enter state only through recorded
                // policy outcomes delivered as environment inputs once the
                // policy channel exists; the P0 consent gates in the
                // bitty-docs security corpus remain authoritative.
            }
            TerminalAction::OscCwd { url } => self.cwd_report = Some(url.clone()),
            // OSC 9 / OSC 777 / Kitty OSC 99 notifications (CTX-0577,
            // CTX-1008): grid truth is untouched. The runtime owns the
            // bell/notification policy (default deny) and the
            // capability/consent and rate gates; terminal state stays inert
            // by contract (Kitty chunks assemble in the runtime, so the
            // replay/canonical hash is unchanged).
            TerminalAction::OscNotification { .. } => {}
            TerminalAction::KittyNotificationChunk { .. } => {}
            TerminalAction::OscHyperlink { link } => self.osc_hyperlink(link.as_ref()),
            TerminalAction::OscPromptMark { kind, exit_code } => {
                self.record_zone(*kind, *exit_code);
            }
            // OSC 22 pointer shapes (issue #1762): presentation-only cursor
            // icon, never terminal truth. The runtime owns the per-pane shape
            // stack and the focused-pane platform dispatch; terminal state
            // stays inert by contract (like OSC 10/11/4 and notifications).
            TerminalAction::OscPointerShape { .. } => {}
            TerminalAction::OscUnknown { .. } => self.telemetry.unknown_osc += 1,
            // Kitty graphics (CTX-0256 base, CTX-0950 advanced): pixel
            // decode and paint live downstream (`KittyImageLayer` routes
            // to `kitty_display_image`), but placement, deletion,
            // animation, and lifetime are Terminal Truth: grid anchors
            // must scroll with text and die on clear/reset, so they are
            // recorded here.
            TerminalAction::KittyGraphics {
                action_a,
                width_s,
                height_v,
                cols_c,
                rows_r,
                cursor_movement_c,
                control,
                ..
            } => self.kitty_graphics(KittyCommand {
                action: *action_a,
                width_s: *width_s,
                height_v: *height_v,
                cols: *cols_c,
                rows: *rows_r,
                cursor_move: *cursor_movement_c,
                keys: *control,
            }),

            // Kitty keyboard progressive-enhancement negotiation (CTX-0575).
            // The bounded register and push/pop stack live in the mode
            // register; a `Query` synthesizes the spec reply `CSI ? flags u`.
            TerminalAction::EnhancedKeyboard { op } => self.apply_enhanced_keyboard(*op),

            TerminalAction::Unknown(report) => match report.kind {
                SequenceKind::Csi => self.telemetry.unknown_csi += 1,
                SequenceKind::Esc => self.telemetry.unknown_esc += 1,
                SequenceKind::Dcs => self.telemetry.unknown_dcs += 1,
            },

            TerminalAction::SoftReset => self.soft_reset(),
            TerminalAction::FullReset => self.full_reset(),
        }
        self.enforce_cursor_invariants();
    }

    fn finalize_batch(&mut self) -> Damage {
        let rects = std::mem::take(&mut self.batch_rects);
        let scroll_events = std::mem::take(&mut self.batch_scroll_events);
        let (rows, cols) = (self.height as u16, self.width as u16);
        let regions = coalesce(rects, scroll_events, rows, cols);
        self.generation += 1;
        let damage = Damage {
            generation: self.generation,
            regions: regions.into_boxed_slice(),
        };
        if self.damage_history.len() == DAMAGE_HISTORY_BATCHES {
            self.damage_history.pop_front();
        }
        self.damage_history.push_back(damage.clone());
        debug_assert!(
            self.check_invariants().is_ok(),
            "RFC invariants violated after action batch: {:?}",
            self.check_invariants()
        );
        damage
    }

    // ------------------------------------------------------------------
    // Printing
    // ------------------------------------------------------------------

    fn print(&mut self, scalar: char) {
        let table = self.charsets.consume_translation_table();
        let ch = Charsets::translate(table, scalar);
        if ch == '\0' {
            return;
        }
        // CTX-0821: Kitty Unicode placeholder U+10EEEE (issue #1400).
        // The base scalar is an ordinary width-1 grid cell carrying the
        // cursor pen (foreground names the image id, underline color the
        // placement id; row/column/high-byte diacritics ride in the
        // combining buffer, decoded headlessly via kitty_unicode).
        // Recognition, sizing, and resize/delete semantics live in
        // `kitty_unicode` + `State::kitty_unicode_*` below; printing stays
        // total and grid-shaped here.
        if crate::kitty_unicode::is_kitty_placeholder(ch) {
            self.print_kitty_placeholder(ch);
            return;
        }
        let glyph_width = char_cell_width(ch);
        if glyph_width == 0 {
            // CR-TERM-01: zero-width scalars (combining marks, ZWJ,
            // variation selectors) attach to the preceding cell's bounded
            // combining buffer instead of being silently dropped. The
            // cursor does not advance and no wrap is consumed.
            self.attach_zerowidth(ch);
            return;
        }
        let cols = self.width as u16;
        // Consume a pending wrap before placing the glyph (DECAWM).
        // This is a soft wrap: the row we leave continues onto the next.
        // Mark it BEFORE `index_linefeed` so a scroll carries the flag with
        // the row (the scrolled-off top keeps its own flag; the wrapping
        // row shifts but retains `true`).
        if self.modes.auto_wrap && self.cursor.pending_wrap {
            let old_row = self.cursor.position.row as usize;
            self.screens_active_mut().set_wrapped(old_row, true);
            self.index_linefeed();
            self.cursor.position.col = 0;
        }
        self.cursor.pending_wrap = false;
        // CTX-0910: A wide character (width=2) requires two consecutive
        // columns. If the terminal width is only 1 column, a wide character
        // cannot be printed anywhere, even after wrapping. Drop it.
        if glyph_width == 2 && self.width < 2 {
            return;
        }
        if glyph_width == 2 && self.cursor.position.col + 1 >= cols {
            if self.modes.auto_wrap {
                // Single documented rule for a wide character at the final
                // column: wrap, then place on the next line (also soft).
                let old_row = self.cursor.position.row as usize;
                self.screens_active_mut().set_wrapped(old_row, true);
                self.index_linefeed();
                self.cursor.position.col = 0;
            } else {
                return;
            }
        }
        let row = self.cursor.position.row as usize;
        let col = self.cursor.position.col as usize;
        // Break every wide pair the write range `[col, col + width)` only
        // partially overlaps: any surviving outer half would otherwise be
        // orphaned (RFC invariant 2). All probes read the ORIGINAL cells.
        let last_col_idx = self.width - 1;
        let erase = self.bce_style();
        let old_at_col = *self.screens_active().get(row, col);
        let old_ahead = (col < last_col_idx).then(|| *self.screens_active().get(row, col + 1));
        if old_at_col.spacer && col > 0 {
            let cleared = Cell::erased(erase);
            self.screens_active_mut().set(row, col - 1, cleared);
            let c = (col - 1) as u16;
            self.damage_grid_rect(row as u16, c, row as u16, c);
        }
        match glyph_width {
            1 => {
                if old_at_col.width == 2 && !old_at_col.spacer && col < last_col_idx {
                    let cleared = Cell::erased(erase);
                    self.screens_active_mut().set(row, col + 1, cleared);
                    let c = (col + 1) as u16;
                    self.damage_grid_rect(row as u16, c, row as u16, c);
                }
            }
            _ => {
                // Our trailing half replaces `col + 1`; if it previously
                // held a DIFFERENT pair's leading half, that pair's spacer
                // at `col + 2` survives unpaired.
                if let Some(ahead) = old_ahead {
                    if ahead.width == 2 && !ahead.spacer && col + 2 <= last_col_idx {
                        let cleared = Cell::erased(erase);
                        self.screens_active_mut().set(row, col + 2, cleared);
                        let c = (col + 2) as u16;
                        self.damage_grid_rect(row as u16, c, row as u16, c);
                    }
                }
            }
        }
        if self.modes.insert {
            let insert_erase = self.bce_style();
            self.screens_active_mut().insert_blanks_in_row(
                row,
                col,
                glyph_width as usize,
                &insert_erase,
            );
        }
        let style = self.cursor.style;
        let link = self.current_hyperlink;
        self.screens_active_mut().set(
            row,
            col,
            Cell {
                glyph: ch,
                style,
                width: glyph_width,
                spacer: false,
                hyperlink: link,
                zerowidth: Zerowidth::new(),
            },
        );
        if glyph_width == 2 {
            self.screens_active_mut()
                .set(row, col + 1, Cell::wide_spacer(style));
        }
        let write_erase = self.bce_style();
        if self.screens_active_mut().repair_row(row, &write_erase) {
            let last_col = self.width as u16 - 1;
            self.damage_grid_rect(row as u16, col as u16, row as u16, last_col);
        }
        self.damage_grid_rect(
            row as u16,
            col as u16,
            row as u16,
            (col + glyph_width as usize - 1) as u16,
        );
        // CTX-0996: every placed glyph marks its row as holding command
        // output (resolved later against the last `OutputStart`). Dropped
        // wide chars return early above and never reach here.
        self.note_output_print();
        let advanced = col + glyph_width as usize;
        if advanced >= self.width {
            self.cursor.position.col = cols - 1;
            self.cursor.pending_wrap = self.modes.auto_wrap;
        } else {
            self.cursor.position.col = advanced as u16;
        }
    }

    /// Prints one Kitty Unicode placeholder cell (CTX-0821, issue #1400).
    ///
    /// The `U+10EEEE` base is an ordinary width-1 cell carrying the cursor
    /// pen: the foreground names the image id and the underline color the
    /// placement id (kitty wire rules; see [`crate::kitty_unicode`]).
    /// Row/column/high-byte diacritics arrive as following zero-width
    /// marks and attach through the normal combining path, so the run
    /// decoders read them from the cell's combining buffer. Wrap,
    /// insert-mode, wide-pair repair, and damage behave exactly like any
    /// other width-1 print (grid invariants unchanged).
    fn print_kitty_placeholder(&mut self, ch: char) {
        debug_assert!(crate::kitty_unicode::is_kitty_placeholder(ch));
        let cols = self.width as u16;
        if self.modes.auto_wrap && self.cursor.pending_wrap {
            let old_row = self.cursor.position.row as usize;
            self.screens_active_mut().set_wrapped(old_row, true);
            self.index_linefeed();
            self.cursor.position.col = 0;
        }
        self.cursor.pending_wrap = false;
        let row = self.cursor.position.row as usize;
        let col = self.cursor.position.col as usize;
        let last_col_idx = self.width - 1;
        let erase = self.bce_style();
        let old_at_col = *self.screens_active().get(row, col);
        if old_at_col.spacer && col > 0 {
            let cleared = Cell::erased(erase);
            self.screens_active_mut().set(row, col - 1, cleared);
            let c = (col - 1) as u16;
            self.damage_grid_rect(row as u16, c, row as u16, c);
        }
        if old_at_col.width == 2 && !old_at_col.spacer && col < last_col_idx {
            let cleared = Cell::erased(erase);
            self.screens_active_mut().set(row, col + 1, cleared);
            let c = (col + 1) as u16;
            self.damage_grid_rect(row as u16, c, row as u16, c);
        }
        if self.modes.insert {
            let insert_erase = self.bce_style();
            self.screens_active_mut()
                .insert_blanks_in_row(row, col, 1, &insert_erase);
        }
        let style = self.cursor.style;
        let link = self.current_hyperlink;
        self.screens_active_mut().set(
            row,
            col,
            Cell {
                glyph: ch,
                style,
                width: 1,
                spacer: false,
                hyperlink: link,
                zerowidth: Zerowidth::new(),
            },
        );
        let write_erase = self.bce_style();
        if self.screens_active_mut().repair_row(row, &write_erase) {
            let last_col = self.width as u16 - 1;
            self.damage_grid_rect(row as u16, col as u16, row as u16, last_col);
        }
        self.damage_grid_rect(row as u16, col as u16, row as u16, col as u16);
        // CTX-0996: placeholder cells are ordinary width-1 output.
        self.note_output_print();
        let advanced = col + 1;
        if advanced >= self.width {
            self.cursor.position.col = cols - 1;
            self.cursor.pending_wrap = self.modes.auto_wrap;
        } else {
            self.cursor.position.col = advanced as u16;
        }
    }

    /// Applies one completed Kitty graphics command (CTX-0950).
    ///
    /// Pixel decode, file/shm reads, and paint stay downstream; what is
    /// recorded here is display intent that the grid owns: placement
    /// anchors (which scroll with text and die on clear), deletion, and
    /// animation descriptors. Well-formed wire that names nothing
    /// actionable (unknown action, unresolvable animation target,
    /// over-cap frame, missing relative parent) is refused with a
    /// telemetry tick, never a panic and never grid damage.
    fn kitty_graphics(&mut self, cmd: KittyCommand) {
        let KittyCommand {
            action,
            width_s,
            height_v,
            cols,
            rows,
            cursor_move,
            keys,
        } = cmd;
        match action {
            None | Some('T') | Some('p') => {
                self.kitty_place(cols, rows, cursor_move, keys);
                if keys.image_number != 0 && keys.image_id != 0 {
                    self.kitty_placements
                        .register_number(keys.image_number, keys.image_id);
                }
            }
            // Transmit-only stores without painting: no placement, no
            // cursor motion. The number mapping still lets later `a=p`
            // and `a=a` commands address the image by number.
            Some('t') => {
                if keys.image_number != 0 && keys.image_id != 0 {
                    self.kitty_placements
                        .register_number(keys.image_number, keys.image_id);
                }
            }
            Some('d') => self.kitty_delete(keys),
            Some('f') => self.kitty_frame_data(rows, keys),
            Some('a') => self.kitty_anim_control(width_s, height_v, cols, rows, keys),
            // `a=c` composes pixel rectangles between frames: a pure
            // pixel op resolved downstream with the decode, with no
            // grid truth of its own. `a=q` queries are answered by the
            // runtime (which owns the PTY write path). Unknown actions
            // stay stored-not-painted downstream.
            Some(_) => {}
        }
    }

    /// Records an `a=p` (or sized `a=T`) placement at the cursor.
    ///
    /// Virtual prototypes (`U=1`) need explicit `c=`/`r=` spans and take
    /// no anchor; relative placements resolve the parent anchor plus the
    /// `H=`/`V=` offset (a missing parent, or a virtual child, refuses
    /// the placement, mirroring `ENOPARENT`/`EINVAL`). The cursor moves
    /// past the span unless `C=1`, the placement is virtual, or it is
    /// relative (the specification forbids cursor motion for relatives
    /// regardless of `C=`).
    fn kitty_place(&mut self, cols: u16, rows: u16, cursor_move: u8, keys: KittyControlKeys) {
        let on_alt = self.alt_screen_active();
        let virtual_proto = keys.is_virtual_placement();
        if virtual_proto && keys.has_parent() {
            // A virtual prototype cannot hang off a parent (kitty
            // `EINVAL`): refuse, count, done.
            self.telemetry.kitty_refused += 1;
            return;
        }
        if cols == 0 && rows == 0 {
            // Headless sizing needs explicit spans: pixels are unknown
            // here, so a spanless place names nothing to anchor and no
            // cursor advance to apply. (The decoder still stores the
            // pixels downstream; only the grid anchor is skipped.)
            self.telemetry.kitty_refused += 1;
            return;
        }
        let (anchor_row, anchor_col) = self.cursor_xy();
        let (anchor_row, anchor_col, relative) = if keys.has_parent() {
            let Some(parent) = self
                .kitty_placements
                .resolve_parent(keys.parent_id, keys.parent_placement_id)
            else {
                self.telemetry.kitty_refused += 1;
                return;
            };
            if parent.virtual_proto {
                // Offsets from a prototype's nowhere-anchor are
                // meaningless; the renderer derives virtual children
                // from `U+10EEEE` runs instead.
                self.telemetry.kitty_refused += 1;
                return;
            }
            let row = parent.anchor_row.saturating_add_signed(keys.parent_dy);
            let col = parent.anchor_col.saturating_add_signed(keys.parent_dx);
            (row as usize, col as usize, true)
        } else {
            (anchor_row, anchor_col, false)
        };
        let placement = KittyPlacement {
            image_id: keys.image_id,
            placement_id: keys.placement_id,
            anchor_row: anchor_row as u32,
            anchor_col: anchor_col as u32,
            rows,
            cols,
            z_index: keys.z_index,
            virtual_proto,
            parent: keys
                .has_parent()
                .then_some((keys.parent_id, keys.parent_placement_id)),
            parent_offset: (keys.parent_dx, keys.parent_dy),
            on_alt_screen: on_alt,
        };
        self.kitty_placements.upsert(placement);
        if !virtual_proto {
            let last_row = self.height as u16 - 1;
            let last_col = self.width as u16 - 1;
            let bottom = (anchor_row as u16)
                .saturating_add(rows)
                .saturating_sub(1)
                .min(last_row);
            let right = (anchor_col as u16)
                .saturating_add(cols)
                .saturating_sub(1)
                .min(last_col);
            self.damage_grid_rect(
                anchor_row as u16,
                anchor_col as u16,
                bottom.max(anchor_row as u16),
                right.max(anchor_col as u16),
            );
        }
        if cursor_move != 1 && !virtual_proto && !relative {
            // The specification leaves post-image cursor placement past
            // the screen or scroll area undefined; clamp into the grid
            // and let the invariant pass snap wide pairs.
            let last_col = self.width.saturating_sub(1) as u16;
            self.cursor.position.col = (anchor_col as u16).saturating_add(cols).min(last_col);
            self.cursor.position.row = (anchor_row as u16)
                .saturating_add(rows)
                .min(self.height as u16 - 1);
        }
    }

    /// Applies an `a=d` deletion command.
    fn kitty_delete(&mut self, keys: KittyControlKeys) {
        let Some(selector) = kitty_delete_selector(&keys) else {
            self.telemetry.kitty_refused += 1;
            return;
        };
        let free_data = keys.delete.is_some_and(|d| d.is_ascii_uppercase());
        let (cursor_row, cursor_col) = self.cursor_xy();
        let removed = self.kitty_placements.apply_delete(
            selector,
            (cursor_row as u32, cursor_col as u32),
            self.alt_screen_active(),
            free_data,
        );
        if removed > 0 {
            // Removed placements may have covered anywhere; damage is
            // coarse but total (placements are rare control-plane events,
            // never hot-path text).
            self.damage_grid_rect(0, 0, self.height as u16 - 1, self.width as u16 - 1);
        }
    }

    /// Records `a=f` frame data arrival for an animation.
    ///
    /// A new frame (the overloaded `r=` span field is `0`) appends a gap
    /// entry (`z=0`/absent resolves to the `40ms` default, negative is
    /// gapless); an edit (`r>0`) adjusts that frame's gap when `z` is
    /// given. Past [`crate::placement::KITTY_ANIM_MAX_FRAMES`] the frame
    /// is refused instead of growing memory. Pixel composition itself is
    /// downstream.
    fn kitty_frame_data(&mut self, rows_r: u16, keys: KittyControlKeys) {
        let Some(id) = self.kitty_anim_target(&keys) else {
            self.telemetry.kitty_refused += 1;
            return;
        };
        let frame_r = u32::from(rows_r);
        if frame_r == 0 {
            let gap = if keys.z_index == 0 {
                crate::placement::KITTY_ANIM_DEFAULT_GAP_MS
            } else {
                keys.z_index.max(0) as u32
            };
            if self
                .kitty_placements
                .animation_or_insert(id)
                .push_frame(gap)
                .is_err()
            {
                self.telemetry.kitty_refused += 1;
            }
        } else if keys.z_index != 0 {
            let gap = keys.z_index.max(0) as u32;
            if self
                .kitty_placements
                .animation_or_insert(id)
                .set_gap(frame_r, gap)
                .is_err()
            {
                self.telemetry.kitty_refused += 1;
            }
        } else {
            // Edit with no new data and no gap change: ensure the
            // descriptor exists so later controls resolve.
            self.kitty_placements.animation_or_insert(id);
        }
        self.damage_image_placements(id);
    }

    /// Applies an `a=a` animation control command.
    ///
    /// The overloaded span fields carry the frames (`c=` current,
    /// `r=` affected), `s=` stops/runs, `v=` sets the loop budget, and
    /// `z=` retargets the affected frame's gap. Partial failures
    /// (unknown frame) apply the rest and count one refusal.
    fn kitty_anim_control(
        &mut self,
        width_s: Option<u32>,
        height_v: Option<u32>,
        cols_c: u16,
        rows_r: u16,
        keys: KittyControlKeys,
    ) {
        let Some(id) = self.kitty_anim_target(&keys) else {
            self.telemetry.kitty_refused += 1;
            return;
        };
        let current = u32::from(cols_c);
        let affected = u32::from(rows_r);
        let anim = self.kitty_placements.animation_or_insert(id);
        let mut refused = false;
        if current != 0 && anim.set_current(current).is_err() {
            refused = true;
        }
        if affected != 0 && keys.z_index != 0 {
            let gap = keys.z_index.max(0) as u32;
            if anim.set_gap(affected, gap).is_err() {
                refused = true;
            }
        }
        // The overloaded `s=` key is the animation state (`1` stop,
        // `2` run-loading, `3` run); anything else is ignored.
        if let Some(state) = width_s
            .and_then(|s| u8::try_from(s).ok())
            .and_then(|s| match s {
                1 => Some(KittyAnimState::Stopped),
                2 => Some(KittyAnimState::Loading),
                3 => Some(KittyAnimState::Running),
                _ => None,
            })
        {
            anim.set_state(state);
        }
        // The overloaded `v=` key is the loop budget (`0` ignored,
        // `1` infinite, `n > 1` plays `n - 1` loops).
        anim.set_loops(height_v.unwrap_or(0));
        if refused {
            self.telemetry.kitty_refused += 1;
        }
        self.damage_image_placements(id);
    }

    /// Resolves the animation target image: explicit `i=`, else the
    /// newest image under `I=`. `None` when neither names an image.
    fn kitty_anim_target(&self, keys: &KittyControlKeys) -> Option<u32> {
        if keys.image_id != 0 {
            return Some(keys.image_id);
        }
        if keys.image_number != 0 {
            return self.kitty_placements.newest_with_number(keys.image_number);
        }
        None
    }

    /// Damages every span of one image's placements (animation steps and
    /// frame arrivals change painted pixels without moving anchors).
    fn damage_image_placements(&mut self, image_id: u32) {
        let last_row = self.height as u16 - 1;
        let last_col = self.width as u16 - 1;
        let on_alt = self.alt_screen_active();
        let spans: Vec<(u32, u32, u16, u16)> = self
            .kitty_placements
            .iter()
            .filter(|entry| entry.image_id == image_id && entry.on_alt_screen == on_alt)
            .map(|entry| (entry.anchor_row, entry.anchor_col, entry.rows, entry.cols))
            .collect();
        for (row, col, rows, cols) in spans {
            let bottom = (row as u16)
                .saturating_add(rows)
                .saturating_sub(1)
                .min(last_row);
            let right = (col as u16)
                .saturating_add(cols)
                .saturating_sub(1)
                .min(last_col);
            self.damage_grid_rect(
                row as u16,
                col as u16,
                bottom.max(row as u16),
                right.max(col as u16),
            );
        }
    }

    /// Decodes the placeholder run covering `(row, col)` on the active
    /// screen, if that cell is a Kitty Unicode placeholder (CTX-0821).
    ///
    /// The run extends left and right across adjacent placeholder cells
    /// whose decoded image/placement identity and tile row match with
    /// consecutive tile columns (the kitty left-to-right inheritance
    /// rule, applied here at query time so decode never depends on print
    /// order). Returns the decoded cells left-to-right plus the run key
    /// `(image_id, placement_id)`, or `None` when the cell is not a
    /// placeholder. Headless, deterministic, total: neighboring
    /// reservation-window scalars (`U+10EEEF..=U+10EEFF`) are ordinary
    /// text and never start or extend a run.
    #[must_use]
    pub fn kitty_unicode_run_at(&self, row: usize, col: usize) -> Option<KittyUnicodeRunCells> {
        use crate::kitty_unicode::{KittyRunBuilder, is_kitty_placeholder};
        let (rows, cols) = self.screens_active().dims();
        if row >= rows || col >= cols {
            return None;
        }
        if !is_kitty_placeholder(self.screens_active().get(row, col).glyph) {
            return None;
        }
        // Find the run start: scan left while cells decode as a run.
        let mut start_col = col;
        while start_col > 0 {
            let candidate = start_col - 1;
            if !self.kitty_run_covers(row, candidate, col) {
                break;
            }
            start_col = candidate;
        }
        let mut builder = KittyRunBuilder::new();
        let mut cells = Vec::new();
        let mut c = start_col;
        while c < cols {
            let cell = self.screens_active().get(row, c);
            let accepted = builder.push(
                row,
                c,
                is_kitty_placeholder(cell.glyph),
                cell.style.foreground,
                cell.style.underline_color,
                cell.zerowidth.as_slice(),
            );
            match accepted {
                Some(decoded) => {
                    cells.push(decoded);
                    c += 1;
                }
                None => break,
            }
        }
        if cells.is_empty() {
            return None;
        }
        // The query cell must lie inside the decoded run.
        if col < start_col || col >= start_col + cells.len() {
            return None;
        }
        let key = crate::kitty_unicode::run_key(&cells[0]);
        Some((cells, key))
    }

    /// Whether the placeholder cell at `(row, candidate)` belongs to the
    /// same run as the anchor column: decodes the `candidate..=anchor`
    /// segment and checks the anchor joins one run.
    fn kitty_run_covers(&self, row: usize, candidate: usize, anchor: usize) -> bool {
        use crate::kitty_unicode::{KittyRunBuilder, is_kitty_placeholder};
        let (_, cols) = self.screens_active().dims();
        if candidate > anchor || anchor >= cols {
            return false;
        }
        let mut builder = KittyRunBuilder::new();
        for c in candidate..=anchor {
            let cell = self.screens_active().get(row, c);
            let accepted = builder.push(
                row,
                c,
                is_kitty_placeholder(cell.glyph),
                cell.style.foreground,
                cell.style.underline_color,
                cell.zerowidth.as_slice(),
            );
            if accepted.is_none() {
                return false;
            }
        }
        true
    }

    /// All placeholder runs on the active screen's `row`, left-to-right
    /// (CTX-0821).
    ///
    /// Each run is `(cells, key)` like [`Self::kitty_unicode_run_at`].
    /// Non-placeholder cells split runs; every cell belongs to at most
    /// one run. Headless and deterministic.
    #[must_use]
    pub fn kitty_unicode_runs_on_row(&self, row: usize) -> Vec<KittyUnicodeRunCells> {
        use crate::kitty_unicode::{KittyRunBuilder, is_kitty_placeholder};
        let (rows, cols) = self.screens_active().dims();
        if row >= rows {
            return Vec::new();
        }
        let mut runs: Vec<KittyUnicodeRunCells> = Vec::new();
        let mut builder = KittyRunBuilder::new();
        let mut current: Vec<KittyUnicodeCell> = Vec::new();
        let flush = |current: &mut Vec<KittyUnicodeCell>, runs: &mut Vec<KittyUnicodeRunCells>| {
            if !current.is_empty() {
                let key = crate::kitty_unicode::run_key(&current[0]);
                runs.push((std::mem::take(current), key));
            }
        };
        for c in 0..cols {
            let cell = self.screens_active().get(row, c);
            match builder.push(
                row,
                c,
                is_kitty_placeholder(cell.glyph),
                cell.style.foreground,
                cell.style.underline_color,
                cell.zerowidth.as_slice(),
            ) {
                Some(decoded) => current.push(decoded),
                None => {
                    flush(&mut current, &mut runs);
                    if is_kitty_placeholder(cell.glyph) {
                        // A placeholder that breaks the run starts the next
                        // run (new image, new row, or column jump).
                        builder = KittyRunBuilder::new();
                        if let Some(decoded) = builder.push(
                            row,
                            c,
                            true,
                            cell.style.foreground,
                            cell.style.underline_color,
                            cell.zerowidth.as_slice(),
                        ) {
                            current.push(decoded);
                        } else {
                            builder = KittyRunBuilder::new();
                        }
                    } else {
                        builder = KittyRunBuilder::new();
                    }
                }
            }
        }
        flush(&mut current, &mut runs);
        runs
    }

    /// Clears every grid cell whose placeholder run names `(image_id,
    /// placement_id)` (CTX-0821 delete semantics).
    ///
    /// `placement_id == None` clears every run naming `image_id`
    /// (placement-unspecified delete, kitty `d=i`); `Some(p)` clears only
    /// runs naming `(image_id, Some(p))` (kitty `d=i,p=`). Matching runs
    /// on both screens' live grids are erased with the BCE style; scrollback
    /// lines are immutable and keep their (now-dangling, fail-closed)
    /// placeholder bytes. Returns the cleared cell count. Deterministic.
    pub fn kitty_unicode_clear(&mut self, image_id: u32, placement_id: Option<u32>) -> usize {
        let mut cleared = 0usize;
        let erase = self.bce_style();
        for screen in 0..2 {
            let (rows, cols) = if screen == 0 {
                self.screens.main.dims()
            } else {
                self.screens.alt.dims()
            };
            // Collect first (immutable scan), then erase: the run decode
            // borrows the grid.
            let mut targets: Vec<(usize, usize)> = Vec::new();
            for row in 0..rows {
                for run in self.kitty_runs_on_screen_row(screen, row, cols) {
                    let (cells, _) = run;
                    if cells.is_empty() {
                        continue;
                    }
                    let first = &cells[0];
                    let matches = first.id.image_id == image_id
                        && (placement_id.is_none() || first.id.placement_id == placement_id);
                    if matches {
                        targets.extend(cells.iter().map(|c| (c.grid_row, c.grid_col)));
                    }
                }
            }
            let mut damaged: Vec<(u16, u16)> = Vec::with_capacity(targets.len());
            if screen == 0 {
                for (row, col) in targets {
                    self.screens.main.set(row, col, Cell::erased(erase));
                    self.screens.main.set_wrapped(row, false);
                    damaged.push((row as u16, col as u16));
                    cleared += 1;
                }
            } else {
                for (row, col) in targets {
                    self.screens.alt.set(row, col, Cell::erased(erase));
                    self.screens.alt.set_wrapped(row, false);
                    damaged.push((row as u16, col as u16));
                    cleared += 1;
                }
            }
            for (row, col) in damaged {
                self.damage_grid_rect(row, col, row, col);
            }
        }
        cleared
    }

    /// Placeholder runs on one screen's row (helper for
    /// [`Self::kitty_unicode_clear`]; `screen` 0 = main, 1 = alt).
    fn kitty_runs_on_screen_row(
        &self,
        screen: usize,
        row: usize,
        cols: usize,
    ) -> Vec<KittyUnicodeRunCells> {
        use crate::kitty_unicode::{KittyRunBuilder, is_kitty_placeholder};
        let grid = if screen == 0 {
            &self.screens.main
        } else {
            &self.screens.alt
        };
        let mut runs: Vec<KittyUnicodeRunCells> = Vec::new();
        let mut builder = KittyRunBuilder::new();
        let mut current: Vec<KittyUnicodeCell> = Vec::new();
        for c in 0..cols {
            let cell = grid.get(row, c);
            match builder.push(
                row,
                c,
                is_kitty_placeholder(cell.glyph),
                cell.style.foreground,
                cell.style.underline_color,
                cell.zerowidth.as_slice(),
            ) {
                Some(decoded) => current.push(decoded),
                None => {
                    if !current.is_empty() {
                        let key = crate::kitty_unicode::run_key(&current[0]);
                        runs.push((std::mem::take(&mut current), key));
                    }
                    if is_kitty_placeholder(cell.glyph) {
                        builder = KittyRunBuilder::new();
                        if let Some(decoded) = builder.push(
                            row,
                            c,
                            true,
                            cell.style.foreground,
                            cell.style.underline_color,
                            cell.zerowidth.as_slice(),
                        ) {
                            current.push(decoded);
                        } else {
                            builder = KittyRunBuilder::new();
                        }
                    } else {
                        builder = KittyRunBuilder::new();
                    }
                }
            }
        }
        if !current.is_empty() {
            let key = crate::kitty_unicode::run_key(&current[0]);
            runs.push((current, key));
        }
        runs
    }

    /// Attaches a zero-width scalar to the preceding cell (CR-TERM-01).
    ///
    /// The target is the last written cell: the cursor cell itself while a
    /// deferred wrap is latched (the latch is preserved for the next
    /// full-width print), else the cell left of the cursor, else the last
    /// column of the previous row. A spacer target steps back to its
    /// leading half. With no preceding cell (top-left corner) or a full
    /// combining buffer, the mark is dropped: the buffer stays bounded
    /// (threat T-01) and the cursor never moves.
    fn attach_zerowidth(&mut self, mark: char) {
        let (row, col) = self.cursor_xy();
        let (target_row, target_col) = if self.cursor.pending_wrap {
            (row, col)
        } else if col > 0 {
            (row, col - 1)
        } else if row > 0 {
            (row - 1, self.width - 1)
        } else {
            return;
        };
        let mut target_col = target_col;
        if self.screens_active().get(target_row, target_col).spacer {
            if target_col == 0 {
                return;
            }
            target_col -= 1;
        }
        let mut cell = *self.screens_active().get(target_row, target_col);
        if !cell.push_zerowidth(mark) {
            return;
        }
        self.screens_active_mut().set(target_row, target_col, cell);
        self.damage_grid_rect(
            target_row as u16,
            target_col as u16,
            target_row as u16,
            target_col as u16,
        );
    }

    fn print_control(&mut self, byte: u8) {
        match byte {
            0x08 => {
                // Backspace clamps at column 0 (reverse-wrap is not part of
                // the M1 slice and is unnecessary for determinism).
                self.cursor.position.col = self.cursor.position.col.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            0x09 => self.tab_forward_steps(1),
            0x0A..=0x0C => {
                if self.modes.line_feed_new_line {
                    self.cursor.position.col = 0;
                }
                self.index_linefeed();
            }
            0x0D => {
                self.cursor.position.col = 0;
                self.cursor.pending_wrap = false;
            }
            0x0E => self.charsets.lock(bitty_vt::CharsetSlot::G1),
            0x0F => self.charsets.lock(bitty_vt::CharsetSlot::G0),
            0x84 => self.index_linefeed(),
            0x85 => {
                self.cursor.position.col = 0;
                self.index_linefeed();
            }
            0x8D => self.reverse_index(),
            // BEL and every other C0/C1 byte are inert: no hidden state
            // channels exist outside the declared invariant domains.
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Cursor motion primitives
    // ------------------------------------------------------------------

    fn cursor_move(&mut self, dir: Direction, n: u16) {
        let last_row = self.height as u16 - 1;
        let last_col = self.width as u16 - 1;
        let (row, col) = (self.cursor.position.row, self.cursor.position.col);
        match dir {
            Direction::Up => {
                let floor = if row >= self.scroll_region_top {
                    self.scroll_region_top
                } else {
                    0
                };
                self.cursor.position.row = row.saturating_sub(n).max(floor);
            }
            Direction::Down => {
                let ceiling = if row <= self.scroll_region_bottom {
                    self.scroll_region_bottom
                } else {
                    last_row
                };
                self.cursor.position.row = row.saturating_add(n).min(ceiling).min(last_row);
            }
            Direction::Right => {
                // CR-TERM-02: break at the right margin so a 65535-count
                // CUF from untrusted input cannot amplify into 65k grid
                // probes; iterations past the margin are no-ops.
                let last = last_col as usize;
                let mut c = col as usize;
                for _ in 0..n {
                    if c >= last {
                        break;
                    }
                    c += 1;
                    // Hop across a wide pair's trailing half so the
                    // cursor never rests on a spacer (invariant 3).
                    if self
                        .screens_active()
                        .get(self.cursor.position.row as usize, c)
                        .spacer
                    {
                        if c < last {
                            c += 1;
                        } else {
                            // A pair ending at the last column returns the
                            // cursor to its leading half: a fixed point, so
                            // every remaining step is a no-op.
                            c -= 1;
                            break;
                        }
                    }
                }
                self.cursor.position.col = c as u16;
            }
            Direction::Left => {
                // CR-TERM-02: symmetric left-margin break for CUB
                // (iterations at column 0 are no-ops).
                let mut c = col as usize;
                for _ in 0..n {
                    if c == 0 {
                        break;
                    }
                    c -= 1;
                    if self
                        .screens_active()
                        .get(self.cursor.position.row as usize, c)
                        .spacer
                    {
                        c = c.saturating_sub(1);
                    }
                }
                self.cursor.position.col = c as u16;
            }
        }
        self.cursor.pending_wrap = false;
    }

    fn cursor_position(&mut self, row: Row, col: Col) {
        let region_rows = self.scroll_region_bottom - self.scroll_region_top + 1;
        if row != Row::SENTINEL {
            let raw = row.0.max(1) - 1;
            self.cursor.position.row = if self.modes.origin {
                self.scroll_region_top + raw.min(region_rows.saturating_sub(1))
            } else {
                raw.min(self.height as u16 - 1)
            };
        }
        if col != Col::SENTINEL {
            let raw = col.0.max(1) - 1;
            self.cursor.position.col = raw.min(self.width as u16 - 1);
        }
        self.cursor.pending_wrap = false;
    }

    fn cursor_save(&mut self) {
        let slot = usize::from(self.alt_screen != AltScreen::Off);
        self.saved_cursors[slot] = Some(SavedCursor {
            position: self.cursor.position,
            pending_wrap: self.cursor.pending_wrap,
            style: self.cursor.style,
            origin_mode: self.modes.origin,
            auto_wrap: self.modes.auto_wrap,
            charsets: self.charsets.clone(),
        });
    }

    fn cursor_restore(&mut self) {
        let slot = usize::from(self.alt_screen != AltScreen::Off);
        match self.saved_cursors[slot].clone() {
            Some(saved) => {
                self.cursor.position = saved.position;
                self.cursor.pending_wrap = saved.pending_wrap;
                self.cursor.style = saved.style;
                self.modes.origin = saved.origin_mode;
                self.modes.auto_wrap = saved.auto_wrap;
                self.charsets = saved.charsets;
            }
            None => {
                self.cursor.position = CursorPosition::default();
                self.cursor.pending_wrap = false;
                self.cursor.style = Style::default();
                self.modes.origin = false;
                self.modes.auto_wrap = true;
                self.charsets = Charsets::default();
            }
        }
    }

    fn tab_forward_steps(&mut self, steps: u16) {
        // CR-TERM-02: break at the right margin (same rationale as the
        // TabForward dispatch arm above).
        let last = self.width - 1;
        let mut col = self.cursor.position.col as usize;
        for _ in 0..steps {
            if col >= last {
                break;
            }
            col = self.tabs.next_after(col).unwrap_or(last);
        }
        self.cursor.position.col = col as u16;
        self.cursor.pending_wrap = false;
    }

    /// Linefeed/index: scrolls the region at its bottom margin, otherwise
    /// moves down until the screen bottom.
    fn index_linefeed(&mut self) {
        let row = self.cursor.position.row;
        if row == self.scroll_region_bottom {
            self.scroll_up_region(1);
        } else if (row as usize) < self.height - 1 {
            self.cursor.position.row = row + 1;
        }
        self.cursor.pending_wrap = false;
    }

    /// Reverse index: reverse-scrolls the region at its top margin.
    fn reverse_index(&mut self) {
        let row = self.cursor.position.row;
        if row == self.scroll_region_top {
            self.scroll_down_region(1);
        } else {
            self.cursor.position.row = row.saturating_sub(1);
        }
        self.cursor.pending_wrap = false;
    }

    /// Mechanical post-action normalization guaranteeing invariants 1 and
    /// 3 regardless of which handler ran: clamp to the screen, honor
    /// origin-mode region bounds, and step off any spacer half.
    fn enforce_cursor_invariants(&mut self) {
        let last_row = self.height as u16 - 1;
        let last_col = self.width as u16 - 1;
        let mut row = self.cursor.position.row.min(last_row);
        let mut col = self.cursor.position.col.min(last_col);
        if self.modes.origin {
            row = row.clamp(self.scroll_region_top, self.scroll_region_bottom);
        }
        if self.screens_active().get(row as usize, col as usize).spacer && col > 0 {
            col -= 1;
        }
        self.cursor.position = CursorPosition { row, col };
    }

    // ------------------------------------------------------------------
    // Erase / insert / delete / scroll
    // ------------------------------------------------------------------

    fn erase_in_display(&mut self, mode: EraseDisplayMode) {
        match mode {
            EraseDisplayMode::Below => {
                let (row, col) = self.cursor_xy();
                let last_row = self.height as u16 - 1;
                let last_col = self.width as u16 - 1;
                let erase = self.bce_style();
                let actual_range = self.screens_active_mut().erase_range_in_row(
                    row,
                    col,
                    last_col as usize,
                    &erase,
                );
                if let Some((start, end)) = actual_range {
                    self.damage_grid_rect(row as u16, start as u16, row as u16, end as u16);
                }
                if (row as u16) < last_row {
                    self.screens_active_mut().fill_rect(
                        row as u16 + 1,
                        0,
                        last_row,
                        last_col,
                        &erase,
                    );
                    self.damage_grid_rect(row as u16 + 1, 0, last_row, last_col);
                }
            }
            EraseDisplayMode::Above => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                let last_col_u = self.width as u16 - 1;
                if row > 0 {
                    self.screens_active_mut()
                        .fill_rect(0, 0, row as u16 - 1, last_col_u, &erase);
                    self.damage_grid_rect(0, 0, row as u16 - 1, last_col_u);
                }
                let actual_range = self
                    .screens_active_mut()
                    .erase_range_in_row(row, 0, col, &erase);
                if let Some((start, end)) = actual_range {
                    self.damage_grid_rect(row as u16, start as u16, row as u16, end as u16);
                }
            }
            EraseDisplayMode::All => {
                let erase = self.bce_style();
                let (last_row_u, last_col_u) = (self.height as u16 - 1, self.width as u16 - 1);
                self.screens_active_mut()
                    .fill_rect(0, 0, last_row_u, last_col_u, &erase);
                self.damage_grid_rect(0, 0, last_row_u, last_col_u);
                // The specification clears images on full clear (so the
                // `clear` command works); other text erases leave them.
                self.kitty_placements.clear_screen(self.alt_screen_active());
            }
            EraseDisplayMode::Scrollback => {
                let cleared = self.scrollback.clear();
                self.push_scroll_damage(cleared);
                // M1-18: wholesale buffer-identity invalidation for anchors.
                self.buffer_epoch += 1;
            }
            EraseDisplayMode::ScrollAndClear => {
                // `ED 22` (kitty scroll-and-clear, adopted by ghostty): the
                // visible screen scrolls into the scrollback and the screen
                // is then cleared. Retained scrollback content is preserved
                // (never `ED 3` semantics), and the capture is bounded like
                // every other scroll-into-scrollback path: each row goes
                // through `Scrollback::push_with_wrap`, so capacity pruning
                // and eviction damage reuse the existing mechanism.
                //
                // The alternate screen owns no scrollback of its own, so
                // `ED 22` there only clears: capturing alt-screen UI into the
                // primary history would leak it. The cursor, scroll region,
                // and pen are left unchanged, matching `ED 2`.
                if !self.alt_screen_active() {
                    for row in 0..self.height {
                        let cells = self.screens_active().snapshot_row(row);
                        let wrapped = self.screens_active().wrapped(row);
                        let (_, evicted) = self.scrollback.push_with_wrap(cells, wrapped);
                        self.push_scroll_damage(evicted);
                    }
                }
                let erase = self.bce_style();
                let (last_row_u, last_col_u) = (self.height as u16 - 1, self.width as u16 - 1);
                self.screens_active_mut()
                    .fill_rect(0, 0, last_row_u, last_col_u, &erase);
                self.damage_grid_rect(0, 0, last_row_u, last_col_u);
                // The scrolled-away screen takes its placements with it:
                // real placements are screen-anchored (virtual prototypes
                // survive as text in the scrollback cells).
                self.kitty_placements.clear_screen(self.alt_screen_active());
            }
        }
    }

    fn erase_in_line(&mut self, mode: EraseLineMode) {
        let (row, col) = self.cursor_xy();
        let last_col = self.width as u16 - 1;
        let erase = self.bce_style();
        let row_u = row as u16;
        let actual_range = match mode {
            EraseLineMode::Right => {
                self.screens_active_mut()
                    .erase_range_in_row(row, col, last_col as usize, &erase)
            }
            EraseLineMode::Left => self
                .screens_active_mut()
                .erase_range_in_row(row, 0, col, &erase),
            EraseLineMode::All => {
                self.screens_active_mut()
                    .erase_range_in_row(row, 0, last_col as usize, &erase)
            }
        };
        if let Some((start, end)) = actual_range {
            self.damage_grid_rect(row_u, start as u16, row_u, end as u16);
        }
    }

    fn insert_lines(&mut self, n: u16) {
        let row = self.cursor.position.row;
        if row < self.scroll_region_top || row > self.scroll_region_bottom {
            return;
        }
        let erase = self.bce_style();
        let bottom_usize = self.scroll_region_bottom as usize;
        self.screens_active_mut().insert_blank_lines_down(
            row as usize,
            bottom_usize,
            n as usize,
            &erase,
        );
        self.damage_grid_rect(row, 0, self.scroll_region_bottom, self.width as u16 - 1);
    }

    fn delete_lines(&mut self, n: u16) {
        let row = self.cursor.position.row;
        if row < self.scroll_region_top || row > self.scroll_region_bottom {
            return;
        }
        let erase = self.bce_style();
        // Deleted lines are discarded, never captured into scrollback
        // (invariant 4 reserves capture for scroll-under-region).
        let bottom_usize = self.scroll_region_bottom as usize;
        let _removed = self.screens_active_mut().remove_lines_up(
            row as usize,
            bottom_usize,
            n as usize,
            &erase,
        );
        self.damage_grid_rect(row, 0, self.scroll_region_bottom, self.width as u16 - 1);
    }

    fn scroll_up_region(&mut self, n: u16) {
        let top = self.scroll_region_top as usize;
        let bottom = self.scroll_region_bottom as usize;
        let erase = self.bce_style();
        let removed = self
            .screens_active_mut()
            .remove_lines_up(top, bottom, n as usize, &erase);
        // Lines enter scrollback only when scrolling under a region whose
        // bottom is the screen bottom (invariant 4). Wrap flags travel with
        // their rows so reflow can unwrap logical lines later.
        if self.scroll_region_bottom as usize == self.height - 1 {
            for (line, wrapped) in removed {
                let (id, evicted) = self.scrollback.push_with_wrap(line, wrapped);
                self.batch_scroll_events.push((id, 1));
                self.push_scroll_damage(evicted);
            }
        }
        // Kitty placements scroll with their rows (CTX-0950): only rows
        // entirely inside the region move; rows pushed out clip away.
        // The region damage below covers moved and removed spans.
        self.kitty_placements.scroll_up(
            u32::from(self.scroll_region_top),
            u32::from(self.scroll_region_bottom),
            u32::from(n),
            self.alt_screen_active(),
        );
        self.damage_grid_rect(
            self.scroll_region_top,
            0,
            self.scroll_region_bottom,
            self.width as u16 - 1,
        );
    }

    fn scroll_down_region(&mut self, n: u16) {
        // Scroll-down displaces region rows downward with blanks entering
        // at the top; displaced rows are discarded (never captured into
        // scrollback: invariant 4 reserves capture for scroll-up under a
        // screen-bottom region).
        let erase = self.bce_style();
        let (top, bottom) = (
            self.scroll_region_top as usize,
            self.scroll_region_bottom as usize,
        );
        self.screens_active_mut()
            .insert_blank_lines_down(top, bottom, n as usize, &erase);
        // Placements ride the displaced rows downward like text, gated
        // to the active screen like scroll-up.
        self.kitty_placements.scroll_down(
            top as u32,
            bottom as u32,
            u32::from(n),
            self.alt_screen_active(),
        );
        let last_col = self.width as u16 - 1;
        self.damage_grid_rect(
            self.scroll_region_top,
            0,
            self.scroll_region_bottom,
            last_col,
        );
    }

    fn set_scroll_region(&mut self, top: Row, bottom: Row) {
        let last = self.height as u16 - 1;
        let t = top.0.saturating_sub(1);
        let b = if bottom == Row::SENTINEL {
            last
        } else {
            bottom.0.saturating_sub(1)
        }
        .min(last);
        // Invalid requests are ignored wholesale (xterm-compatible).
        if t > b {
            return;
        }
        self.scroll_region_top = t;
        self.scroll_region_bottom = b;
        self.home_cursor();
    }

    fn home_cursor(&mut self) {
        self.cursor.position.row = if self.modes.origin {
            self.scroll_region_top
        } else {
            0
        };
        self.cursor.position.col = 0;
        self.cursor.pending_wrap = false;
    }

    // ------------------------------------------------------------------
    // Attributes, modes, OSC
    // ------------------------------------------------------------------

    fn apply_attribute_diff(&mut self, diff: &AttributeDiff) {
        for change in &diff.changes {
            match change {
                AttributeChange::Reset => {
                    self.cursor.style.foreground = None;
                    self.cursor.style.background = None;
                    self.cursor.style.underline_color = None;
                    self.cursor
                        .style
                        .attributes
                        .apply_change(&AttributeChangeKind::Reset);
                }
                AttributeChange::Enable(attr) => {
                    self.cursor
                        .style
                        .attributes
                        .apply_change(&AttributeChangeKind::Set(*attr, true));
                }
                AttributeChange::Disable(attr) => {
                    self.cursor
                        .style
                        .attributes
                        .apply_change(&AttributeChangeKind::Set(*attr, false));
                }
                AttributeChange::Foreground(color) => {
                    self.cursor.style.foreground = color_option(*color);
                }
                AttributeChange::Background(color) => {
                    self.cursor.style.background = color_option(*color);
                }
                AttributeChange::UnderlineColor(color) => {
                    self.cursor.style.underline_color = color_option(*color);
                }
            }
        }
    }

    fn set_mode(&mut self, mode: Mode, enabled: bool) {
        match mode {
            Mode::Insert => self.modes.insert = enabled,
            Mode::LineFeedNewLine => self.modes.line_feed_new_line = enabled,
            Mode::ApplicationKeypad => self.modes.application_keypad = enabled,
            Mode::ApplicationCursorKeys => self.modes.application_cursor_keys = enabled,
            Mode::Column132 => {
                // Side effects per spec; the column-dimension change itself
                // awaits resize environment support and the singular reflow
                // algorithm (RFC open item under OQ-007).
                self.modes.column_132_requested = enabled;
                let erase = self.bce_style();
                let last_row = self.height as u16 - 1;
                let last_col = self.width as u16 - 1;
                self.screens_active_mut()
                    .fill_rect(0, 0, last_row, last_col, &erase);
                self.damage_grid_rect(0, 0, last_row, last_col);
                self.scroll_region_top = 0;
                self.scroll_region_bottom = self.height as u16 - 1;
                self.home_cursor();
            }
            Mode::ReverseVideo => self.modes.reverse_video = enabled,
            Mode::Origin => {
                self.modes.origin = enabled;
                self.home_cursor();
            }
            Mode::AutoWrap => {
                self.modes.auto_wrap = enabled;
                self.cursor.pending_wrap = false;
            }
            Mode::CursorBlinking => self.modes.cursor_blinking = enabled,
            Mode::AlternateScreen => self.switch_alt_screen(AltScreen::Via47, enabled),
            Mode::AlternateScreenClearAndRestore => {
                self.switch_alt_screen(AltScreen::Via1049, enabled);
            }
            Mode::BracketedPaste => self.modes.bracketed_paste = enabled,
            Mode::FocusEvents => self.modes.focus_events = enabled,
            Mode::AlternateScroll => self.modes.alternate_scroll = enabled,
            Mode::SynchronizedUpdate => self.modes.synchronized_update = enabled,
            Mode::KittyKeyboard(flags) => {
                if enabled {
                    // Legacy `?7727 h/l` alias: OR in bounded bits.
                    self.modes
                        .enhanced_keyboard
                        .set(flags, bitty_vt::EnhancedKeyboardSetMode::Set);
                } else if flags == 0 {
                    // `CSI ? 7727 l` without flags disables all.
                    self.modes.enhanced_keyboard.pop(u32::MAX);
                } else {
                    self.modes
                        .enhanced_keyboard
                        .set(flags, bitty_vt::EnhancedKeyboardSetMode::Reset);
                }
            }
            Mode::MouseTracking(tracking) => {
                self.modes.mouse_tracking = enabled.then_some(tracking);
            }
            Mode::MouseCoordinateEncoding(encoding) => {
                // CTX-0566 (#1127): the coordinate encodings are mutually
                // exclusive (xterm `charproc.c`: "they are mutually
                // exclusive. For consistency, a reset is only effective
                // against the matching mode."). A reset of a mode that is
                // not active must not clobber a different active encoding,
                // so an app that resets 1015 cannot break another app's
                // active 1006.
                if enabled {
                    self.modes.mouse_coordinate_encoding = Some(encoding);
                } else if self.modes.mouse_coordinate_encoding == Some(encoding) {
                    self.modes.mouse_coordinate_encoding = None;
                }
            }
        }
    }

    fn switch_alt_screen(&mut self, variant: AltScreen, enabled: bool) {
        if enabled {
            if self.alt_screen != AltScreen::Off {
                return;
            }
            self.primary_save = Some(ScreenSave {
                cursor_position: self.cursor.position,
                pending_wrap: self.cursor.pending_wrap,
                style: self.cursor.style,
                cursor_style: self.cursor.cursor_style,
                cursor_visible: self.cursor.visible,
                origin_mode: self.modes.origin,
                auto_wrap: self.modes.auto_wrap,
                charsets: self.charsets.clone(),
                modes: self.modes.clone(),
            });
            // Kitty keeps a separate flag register per screen: swap the live
            // (main) register with the inactive-screen stash so the alt screen
            // starts from its own negotiated flags, not main's (F3).
            std::mem::swap(
                &mut self.modes.enhanced_keyboard,
                &mut self.enhanced_keyboard_stash,
            );
            self.alt_screen = variant;
            if variant == AltScreen::Via1049 {
                // ?1049 clears the alternate screen on entry; ?47 keeps
                // whatever the alt grid last held.
                let erase = self.bce_style();
                self.screens.alt.fill_all(&erase);
                // The specification clears alt-screen images on the 1049
                // switch (like its text); a ?47 entry keeps the alt
                // placements its grid still shows.
                self.kitty_placements.clear_screen(true);
            }
            self.damage_grid_rect(0, 0, self.height as u16 - 1, self.width as u16 - 1);
        } else {
            if self.alt_screen == AltScreen::Off {
                return;
            }
            // The cursor is saved/restored only by the `?1049` pair. xterm's
            // `srm_ALTBUF` (the `?47` DECSET/DECRST arm) performs neither
            // `CursorSave` nor `CursorRestore`, and ghostty's `.@"47"` "only
            // copies the cursor" (the screen is not saved); only
            // `srm_OPT_ALTBUF_CURSOR` (`?1049`) saves on entry and restores on
            // exit (issue #1173 / CTX-0582). Because Bitty keeps one shared
            // cursor rather than per-screen cursors, "copies the cursor" is a
            // no-op and the live cursor simply stays where the alt screen left
            // it. Restore the saved cursor only when the alt screen was both
            // entered and exited through `?1049`, so an entry/exit through
            // `?47` never restores a cursor it did not save; this also covers
            // a mixed `?47h`/`?1049l` pair without resurrecting a pre-entry
            // position the reference would not have saved. The primary
            // `modes`/`charsets` snapshot is still taken and restored for
            // every variant: it backs the single-register model required by
            // terminal-state invariant 5 (alternate-screen entry saves and
            // exit restores the primary mode/charset set), and `?1049` keeps
            // its full restore unchanged.
            let restore_cursor =
                self.alt_screen == AltScreen::Via1049 && variant == AltScreen::Via1049;
            // Preserve the alt screen's own register for the next alt session
            // while main's is restored from the entry snapshot.
            let alt_enhanced = std::mem::take(&mut self.modes.enhanced_keyboard);
            if let Some(save) = self.primary_save.take() {
                if restore_cursor {
                    self.cursor.position = save.cursor_position;
                    self.cursor.pending_wrap = save.pending_wrap;
                    self.cursor.style = save.style;
                    self.cursor.cursor_style = save.cursor_style;
                    self.cursor.visible = save.cursor_visible;
                }
                self.charsets = save.charsets;
                self.modes = save.modes;
            }
            self.enhanced_keyboard_stash = alt_enhanced;
            self.alt_screen = AltScreen::Off;
            // Alt-screen placements die with the session (restoring main
            // must never resurrect them); main-screen placements kept
            // their anchors untouched underneath.
            self.kitty_placements.clear_screen(true);
            self.damage_grid_rect(0, 0, self.height as u16 - 1, self.width as u16 - 1);
        }
    }

    fn osc_hyperlink(&mut self, link: Option<&bitty_vt::Hyperlink>) {
        match link {
            None => self.current_hyperlink = None,
            Some(link) => {
                let key_id = link.id.clone();
                let key_uri = link.uri.clone();
                let existing = self
                    .hyperlink_table
                    .iter()
                    .find(|entry| entry.id_param == key_id && entry.uri == key_uri)
                    .map(|entry| entry.id);
                let resolved = match existing {
                    Some(id) => Some(id),
                    None => {
                        if self.next_hyperlink_id == u32::MAX {
                            // Once per 2^32 distinct links: clear the table
                            // and restart the id space. Cells keep their
                            // pre-wrap ids, so a stale id can resolve to a
                            // post-wrap entry — bounded numeric reuse, stated
                            // honestly in `HyperlinkEntry` (CTX-0490).
                            self.hyperlink_table.clear();
                            self.next_hyperlink_id = 0;
                        }
                        if self.hyperlink_table.len() >= HYPERLINK_TABLE_MAX {
                            // Bounded memory (threat T-01): evict the oldest
                            // entry. Its live cells fail closed to no link
                            // instead of degrading every future link (CTX-0469).
                            self.hyperlink_table.pop_front();
                        }
                        let id = HyperlinkId::new(self.next_hyperlink_id);
                        self.next_hyperlink_id += 1;
                        self.hyperlink_table.push_back(HyperlinkEntry {
                            id,
                            id_param: key_id,
                            uri: key_uri,
                        });
                        Some(id)
                    }
                };
                self.current_hyperlink = resolved;
            }
        }
    }

    /// Records the current live row as holding command output (CTX-0996).
    ///
    /// Called by every placed glyph. Zero-width marks are excluded: they
    /// attach to the preceding cell without advancing, and the base glyph
    /// already marked the row (a leading mark with no base is dropped and
    /// marks nothing).
    fn note_output_print(&mut self) {
        let cursor_row = (self.cursor.position.row as usize).min(self.height.saturating_sub(1));
        self.last_output_print = Some(OutputPrintMark {
            buffer_row: self.scrollback.len() + cursor_row,
            evicted_at: self.scrollback_evicted_total(),
            epoch_at: self.buffer_epoch,
            on_alt_screen: self.alt_screen_active(),
        });
    }

    fn record_zone(&mut self, kind: ZoneKind, exit_code: Option<i32>) {
        self.zone_counter += 1;
        let code = if kind == ZoneKind::OutputEnd {
            exit_code
        } else {
            None
        };
        // M1-18 anchor: the marked content sits at the cursor's live row,
        // whose combined buffer row is `scrollback_len + cursor_row`. The
        // cursor is clamped defensively (this runs before
        // `enforce_cursor_invariants`).
        let cursor_row = (self.cursor.position.row as usize).min(self.height.saturating_sub(1));
        let buffer_row = self.scrollback.len() + cursor_row;
        // CTX-0996: a new output start opens a fresh command, so prompt and
        // input text already on this row never counts as output. An output
        // end captures whether output landed on its own row (final partial
        // line) via live tracking, never via column zero.
        let output_on_mark_row = if kind == ZoneKind::OutputStart {
            self.last_output_print = None;
            false
        } else if kind == ZoneKind::OutputEnd {
            self.output_print_on_row(buffer_row)
        } else {
            false
        };
        self.zones.push_back(ZoneRecord {
            ordinal: self.zone_counter,
            kind,
            exit_code: code,
            buffer_row,
            evicted_at_mark: self.scrollback_evicted_total(),
            epoch_at_mark: self.buffer_epoch,
            on_alt_screen: self.alt_screen_active(),
            output_on_mark_row,
        });
        while self.zones.len() > ZONE_RECORDS_MAX {
            self.zones.pop_front();
        }
    }

    // ------------------------------------------------------------------
    // Replies (queued, never written anywhere)
    // ------------------------------------------------------------------

    /// Applies one Kitty keyboard-protocol operation (CTX-0575).
    ///
    /// `Query` queues the spec reply `CSI ? flags u`; the register never
    /// emits bytes on its own (RFC: replies are queued, not written).
    fn apply_enhanced_keyboard(&mut self, op: bitty_vt::EnhancedKeyboardOp) {
        use bitty_vt::EnhancedKeyboardOp;
        match op {
            EnhancedKeyboardOp::Set { flags, mode } => {
                self.modes.enhanced_keyboard.set(flags, mode);
            }
            EnhancedKeyboardOp::Push { flags } => self.modes.enhanced_keyboard.push(flags),
            EnhancedKeyboardOp::Pop { n } => self.modes.enhanced_keyboard.pop(u32::from(n)),
            EnhancedKeyboardOp::Query => {
                let payload = format!("\x1b[?{}u", self.modes.enhanced_keyboard.flags())
                    .into_bytes()
                    .into_boxed_slice();
                self.replies.queue(payload);
            }
        }
    }

    fn request_device_status(&mut self, kind: StatusKind) {
        let payload: Box<[u8]> = match kind {
            StatusKind::OperatingStatus => b"\x1b[0n".to_vec().into_boxed_slice(),
            StatusKind::CursorPosition => {
                // Saturating: a cursor restored above the region with origin
                // mode on (DECSC/DECSTBM/DECRC) must report row 1, never
                // panic on a bare u16 subtraction (CTX-0469).
                let row = if self.modes.origin {
                    self.cursor
                        .position
                        .row
                        .saturating_sub(self.scroll_region_top)
                        + 1
                } else {
                    self.cursor.position.row + 1
                };
                let col = self.cursor.position.col + 1;
                format!("\x1b[{};{}R", row, col)
                    .into_bytes()
                    .into_boxed_slice()
            }
            StatusKind::DeviceAttributes => b"\x1b[?6c".to_vec().into_boxed_slice(),
        };
        self.replies.queue(payload);
    }

    // ------------------------------------------------------------------
    // Resets
    // ------------------------------------------------------------------

    fn soft_reset(&mut self) {
        // DECSTR subset (VT510 manual): cursor shown, replace mode,
        // absolute origin, autowrap reset, margins cleared, SGR and
        // charsets defaulted. Alternate-screen state is untouched.
        self.cursor.visible = true;
        self.cursor.pending_wrap = false;
        self.cursor.style = Style::default();
        self.modes.insert = false;
        self.modes.origin = false;
        self.modes.auto_wrap = false;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.height as u16 - 1;
        self.charsets = Charsets::default();
    }

    fn full_reset(&mut self) {
        let blank = Style::default();
        self.screens.main.fill_all(&blank);
        self.screens.alt.fill_all(&blank);
        self.damage_grid_rect(0, 0, self.height as u16 - 1, self.width as u16 - 1);
        self.alt_screen = AltScreen::Off;
        self.primary_save = None;
        self.enhanced_keyboard_stash = EnhancedKeyboardState::default();
        self.saved_cursors = [None, None];
        self.cursor = Cursor::default();
        self.cursor.cursor_style = self.default_cursor_style;
        self.modes = Modes::default();
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.height as u16 - 1;
        self.tabs = TabStops::default_lattice(self.width);
        self.charsets = Charsets::default();
        let cleared = self.scrollback.clear();
        self.push_scroll_damage(cleared);
        // M1-18: zones are dropped below, so no anchor outlives the reset;
        // the epoch returns to its initial value so a reset state hashes
        // (and resolves) exactly like a fresh one.
        self.buffer_epoch = 0;
        self.replies.clear();
        self.title = BoundedString::new("");
        self.cwd_report = None;
        self.hyperlink_table.clear();
        self.next_hyperlink_id = 0;
        self.current_hyperlink = None;
        self.zones.clear();
        self.zone_counter = 0;
        self.last_output_print = None;
        // Reset clears every visible image (specification): placements,
        // prototypes, animations, and number mappings all go.
        self.kitty_placements.clear_all();
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn screens_active(&self) -> &Grid {
        match self.alt_screen {
            AltScreen::Off => &self.screens.main,
            AltScreen::Via47 | AltScreen::Via1049 => &self.screens.alt,
        }
    }

    fn screens_active_mut(&mut self) -> &mut Grid {
        match self.alt_screen {
            AltScreen::Off => &mut self.screens.main,
            AltScreen::Via47 | AltScreen::Via1049 => &mut self.screens.alt,
        }
    }

    fn cursor_xy(&self) -> (usize, usize) {
        (
            self.cursor.position.row as usize,
            self.cursor.position.col as usize,
        )
    }

    /// Background-color-erase style (BCE): erased cells adopt the current
    /// background color with all attributes cleared.
    fn bce_style(&self) -> Style {
        Style {
            foreground: None,
            background: self.cursor.style.background,
            underline_color: None,
            attributes: Attributes::default(),
        }
    }

    fn damage_grid_rect(&mut self, top: u16, left: u16, bottom: u16, right: u16) {
        self.batch_rects.push(DamageRect {
            top,
            left,
            bottom,
            right,
        });
    }

    fn damage_row_tail(&mut self, row: u16, from_col: u16) {
        self.damage_grid_rect(row, from_col, row, self.width as u16 - 1);
    }

    fn push_scroll_damage(&mut self, cleared: ClearedRange) {
        if cleared.removed_count > 0 {
            self.batch_scroll_events
                .push((cleared.first_line_id, cleared.removed_count));
        }
    }
}

fn color_option(color: bitty_vt::Color) -> Option<bitty_vt::Color> {
    match color {
        bitty_vt::Color::Default => None,
        other => Some(other),
    }
}

/// Destructured `KittyGraphics` fields for [`State::kitty_graphics`].
///
/// Bundles the seven action fields so the handler stays under clippy's
/// argument limit; construction sites copy them straight from the action.
#[derive(Debug, Clone, Copy)]
struct KittyCommand {
    action: Option<char>,
    width_s: Option<u32>,
    height_v: Option<u32>,
    cols: u16,
    rows: u16,
    cursor_move: u8,
    keys: KittyControlKeys,
}

/// Maps wire `d=` + keys to a [`KittyDeleteSelector`].
/// `None` (absent `d=`) means "all visible" per the specification.
/// An unknown selector refuses the whole delete (`None` return):
/// deleting anything on an unrecognized request would be guessing.
/// `p=` pins one placement only when non-zero (anonymous placements
/// are never singly addressable).
fn kitty_delete_selector(keys: &KittyControlKeys) -> Option<KittyDeleteSelector> {
    let selector = keys.delete.unwrap_or('a').to_ascii_lowercase();
    let pin = (keys.placement_id != 0).then_some(keys.placement_id);
    Some(match selector {
        'a' => KittyDeleteSelector::AllVisible,
        'i' => KittyDeleteSelector::ImageId {
            id: keys.image_id,
            placement: pin,
        },
        'n' => KittyDeleteSelector::NewestNumber {
            number: keys.image_number,
            placement: pin,
        },
        'c' => KittyDeleteSelector::AtCursor,
        'f' => KittyDeleteSelector::FramesOf { id: keys.image_id },
        'p' => KittyDeleteSelector::AtCell {
            x: keys.src_x,
            y: keys.src_y,
        },
        'q' => KittyDeleteSelector::AtCellZ {
            x: keys.src_x,
            y: keys.src_y,
            z: keys.z_index,
        },
        'r' => KittyDeleteSelector::IdRange {
            lo: keys.src_x,
            hi: keys.src_y,
        },
        'x' => KittyDeleteSelector::Column { x: keys.src_x },
        'y' => KittyDeleteSelector::Row { y: keys.src_y },
        'z' => KittyDeleteSelector::ZIndex { z: keys.z_index },
        _ => return None,
    })
}

/// Collects a physical row's content as grapheme leads, trimming trailing
/// blanks. Spacers (wide trailing halves) are skipped: the lead carries the
/// glyph, style, hyperlink, and combining buffer as one atomic unit, so
/// reflow never splits mid-grapheme. Uses stored `width`, reusing the
/// existing width logic.
fn trim_row_to_leads(cells: &[Cell]) -> Vec<Cell> {
    let mut leads = Vec::new();
    for cell in cells {
        if cell.spacer {
            continue;
        }
        leads.push(*cell);
    }
    while leads.last().is_some_and(Cell::is_blank) {
        leads.pop();
    }
    leads
}

/// Rewraps one logical line (leads, no spacers, trimmed) into physical rows
/// of `new_cols` columns, padded with `erase`. Wide leads (`width == 2`)
/// are atomic: when a single column remains, the row is padded with one
/// blank and the wide starts the next row (mirrors `print`'s margin rule).
/// Returns `(padded_row_cells, wrapped)` per physical row: all but the last
/// wrap (`true`), the last is a hard break (`false`). Empty input yields one
/// blank row.
fn rewrap_one_logical(logical: &[Cell], new_cols: usize, erase: &Style) -> Vec<(Vec<Cell>, bool)> {
    let new_cols = new_cols.max(1);
    if logical.is_empty() {
        return vec![(vec![Cell::erased(*erase); new_cols], false)];
    }
    let mut out: Vec<(Vec<Cell>, bool)> = Vec::new();
    let mut cur: Vec<Cell> = Vec::with_capacity(new_cols);
    let mut used = 0usize;
    let mut flush_row = |cur: &mut Vec<Cell>, used: &mut usize, wrapped: bool| {
        while *used < new_cols {
            cur.push(Cell::erased(*erase));
            *used += 1;
        }
        let row = std::mem::replace(cur, Vec::with_capacity(new_cols));
        *used = 0;
        out.push((row, wrapped));
    };
    for lead in logical {
        let w = usize::from(lead.width.clamp(1, 2));
        if used + w > new_cols {
            if w == 2 && used + 1 == new_cols {
                // One column left: pad it blank, wide starts next row.
                cur.push(Cell::erased(*erase));
                used += 1;
                flush_row(&mut cur, &mut used, true);
            } else {
                flush_row(&mut cur, &mut used, true);
            }
        }
        // Width-one bounded representation (CTX-0829): when new_cols == 1,
        // a width=2 char cannot fit with its spacer. Emit the lead as width=1.
        if w == 2 && new_cols == 1 {
            let mut narrow = *lead;
            narrow.width = 1;
            cur.push(narrow);
            used += 1;
        } else {
            cur.push(*lead);
            used += 1;
            if w == 2 {
                cur.push(Cell::wide_spacer(lead.style));
                used += 1;
            }
        }
    }
    flush_row(&mut cur, &mut used, false);
    out
}

/// Maps a cursor unit offset within one logical line to its rewrapped
/// `(row_offset, col)`. `unit_offset` counts leads (0-based index of the
/// lead under the cursor, or `logical.len()` for end-of-line past content).
/// Returns the row index within this logical's rewrapped rows plus the cell
/// column of that lead (or the content-end column for end-of-line).
fn map_unit_to_rewrapped(
    logical: &[Cell],
    unit_offset: usize,
    new_cols: usize,
    _erase: &Style,
) -> (usize, usize) {
    let new_cols = new_cols.max(1);
    if logical.is_empty() {
        return (0, 0);
    }
    let clamped = unit_offset.min(logical.len());
    // Simulate the same packing as `rewrap_one_logical`, tracking positions.
    let mut row_idx = 0usize;
    let mut used = 0usize;
    for (idx, lead) in logical.iter().enumerate() {
        let w = usize::from(lead.width.clamp(1, 2));
        if used + w > new_cols {
            if w == 2 && used + 1 == new_cols {
                // Padding blank fills the last column.
                row_idx += 1;
                used = 0;
            } else {
                row_idx += 1;
                used = 0;
            }
        }
        if idx == clamped {
            return (row_idx, used);
        }
        used += w;
    }
    // End-of-line (clamped == len): content ends at current (row, used).
    (row_idx, used.min(new_cols))
}

/// Parser-resolved counts are guaranteed positive; defensive floor at one.
fn effective_count(n: Count) -> u16 {
    n.0.max(1)
}
