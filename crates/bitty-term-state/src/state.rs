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
    EraseDisplayMode, EraseLineMode, Mode, Row, SequenceKind, StatusKind, TabTargets,
    TerminalAction, ZoneKind,
};

use crate::cell::{AttributeChangeKind, Attributes, Cell, HyperlinkId, Style, char_cell_width};
use crate::charsets::Charsets;
use crate::cursor::{Cursor, CursorPosition, SavedCursor};
use crate::damage::{DAMAGE_HISTORY_BATCHES, Damage, DamageRect, DamagedRegion, coalesce};
use crate::grid::{Grid, ScreenPair};
use crate::image::ImageStore;
use crate::modes::{AltScreen, Modes};
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
/// threat T-01). Beyond the cap, new distinct links degrade to no link.
pub const HYPERLINK_TABLE_MAX: usize = 1024;

/// Cap on retained semantic-zone records (`OSC 133`), oldest dropped
/// first; bounded memory per threat T-01.
pub const ZONE_RECORDS_MAX: usize = 1024;

/// Version embedded in snapshots (RFC: reads occur through versioned
/// snapshots only).
pub const SNAPSHOT_VERSION: u32 = 1;

/// One recorded semantic prompt/command zone boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneRecord {
    /// Monotonic sequence number assigned when the marker arrived.
    pub ordinal: u64,
    /// Which zone boundary was marked.
    pub kind: ZoneKind,
    /// Exit status for `OutputEnd` (`D`), `None` otherwise or on parse failure.
    /// Bounded: validated as signed 32-bit integer; malformed yields `None`.
    pub exit_code: Option<i32>,
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
    saved_cursors: [Option<SavedCursor>; 2],
    cursor: Cursor,
    modes: Modes,
    scroll_region_top: u16,
    scroll_region_bottom: u16,
    tabs: TabStops,
    charsets: Charsets,
    scrollback: Scrollback,
    replies: Replies,
    title: BoundedString,
    cwd_report: Option<BoundedString>,
    hyperlink_table: Vec<(Option<BoundedString>, BoundedString)>,
    current_hyperlink: Option<HyperlinkId>,
    zones: VecDeque<ZoneRecord>,
    zone_counter: u64,
    generation: u64,
    damage_history: VecDeque<Damage>,
    batch_rects: Vec<DamageRect>,
    batch_scroll_events: Vec<(u64, u64)>,
    telemetry: TelemetryCounters,
    images: ImageStore,
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
            saved_cursors: [None, None],
            cursor: Cursor::default(),
            modes: Modes::default(),
            scroll_region_top: 0,
            scroll_region_bottom: (GRID_ROWS - 1) as u16,
            tabs: TabStops::default_lattice(GRID_COLUMNS),
            charsets: Charsets::default(),
            scrollback: Scrollback::with_max_lines(max_lines),
            replies: Replies::new(),
            title: BoundedString::new(""),
            cwd_report: None,
            hyperlink_table: Vec::new(),
            current_hyperlink: None,
            zones: VecDeque::new(),
            zone_counter: 0,
            generation: 0,
            damage_history: VecDeque::new(),
            batch_rects: Vec::new(),
            batch_scroll_events: Vec::new(),
            telemetry: TelemetryCounters::default(),
            images: ImageStore::new(),
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
    /// blanks.
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
        let old_cols = self.width;
        let old_rows = self.height;
        let alt_active = self.alt_screen != AltScreen::Off;
        // Snapshot old logical lines + primary cursor offset before mutating,
        // so a width reflow can keep the cursor on the same content. Only
        // when the primary screen is active; alt-screen cursors clamp.
        let (old_logicals, cursor_logical): (Vec<Vec<Cell>>, Option<(usize, usize)>) =
            if !alt_active && cols != old_cols {
                let crow = self.cursor.position.row as usize;
                let ccol = self.cursor.position.col as usize;
                let sb_len = self.scrollback.len();
                let total = sb_len + old_rows;
                let mut logicals: Vec<Vec<Cell>> = Vec::new();
                let mut cur: Vec<Cell> = Vec::new();
                // (logical_idx, seg_start_in_logical) per combined row.
                let mut row_map: Vec<(usize, usize)> = Vec::with_capacity(total);
                for combined in 0..total {
                    let (segment, wrapped) = if combined < sb_len {
                        let line = self.scrollback.line(combined).expect("scrollback index");
                        (trim_row_to_leads(&line.cells), line.wrapped)
                    } else {
                        let gr = combined - sb_len;
                        let row_cells = self.screens.main.row(gr).to_vec();
                        let w = if gr + 1 < old_rows {
                            self.screens.main.wrapped(gr)
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
                let seg_len = {
                    // Re-derive this row's trimmed segment length.
                    let rc = self.screens.main.row(crow).to_vec();
                    trim_row_to_leads(&rc).len()
                };
                let offset = seg_start + units_before.min(seg_len);
                (logicals, Some((cli, offset)))
            } else {
                (Vec::new(), None)
            };
        if cols == old_cols {
            // Height-only: no logical rewrapping. Truncate/pad both grids;
            // scrollback widths already match.
            self.screens.main.resize(rows, cols, &erase);
            self.screens.alt.resize(rows, cols, &erase);
        } else {
            // Width change: reflow primary (main + scrollback), truncate alt.
            Self::reflow_primary(
                &mut self.screens.main,
                &mut self.scrollback,
                cols,
                rows,
                &erase,
            );
            self.screens.alt.resize(rows, cols, &erase);
        }
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
                // Locate this logical's start in the new total order by
                // rewrapping all old logicals the same way reflow did.
                // (Bounded: same work as reflow, resize-rare.)
                let mut total_idx = 0usize;
                for (li, ll) in old_logicals.iter().enumerate() {
                    let nrows = rewrap_one_logical(ll, cols, &erase).len();
                    if li == line_idx {
                        break;
                    }
                    total_idx += nrows;
                }
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

    /// Reflows the primary screen (`grid` + `scrollback`) to a new width,
    /// bottom-aligning the last `new_rows` physical rows into the grid.
    ///
    /// Unwraps via soft-wrap flags, rewraps with wide-pair atomicity (see
    /// `rewrap_one_logical`), pads underflow at the grid bottom, caps
    /// scrollback at the buffer's configured capacity oldest-first, and
    /// reassigns fresh monotonic scrollback ids. Deterministic, headless,
    /// bounded.
    fn reflow_primary(
        grid: &mut crate::grid::Grid,
        scrollback: &mut crate::scrollback::Scrollback,
        new_cols: usize,
        new_rows: usize,
        erase: &Style,
    ) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        let (old_rows, old_cols) = grid.dims();
        // Collect combined physical rows oldest-first.
        let sb_len = scrollback.len();
        let total = sb_len + old_rows;
        let mut logicals: Vec<Vec<Cell>> = Vec::new();
        let mut cur: Vec<Cell> = Vec::new();
        for combined in 0..total {
            let (segment, wrapped) = if combined < sb_len {
                let line = scrollback.line(combined).expect("scrollback index");
                (trim_row_to_leads(&line.cells), line.wrapped)
            } else {
                let gr = combined - sb_len;
                let row_cells = grid.row(gr).to_vec();
                // Last grid row has no next grid row: hard break.
                let w = if gr + 1 < old_rows {
                    grid.wrapped(gr)
                } else {
                    false
                };
                // Old width is irrelevant here: `trim_row_to_leads` drops
                // spacers/padding, keeping only content leads.
                let _ = old_cols;
                (trim_row_to_leads(&row_cells), w)
            };
            cur.extend(segment);
            if !wrapped {
                logicals.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() || logicals.is_empty() {
            logicals.push(std::mem::take(&mut cur));
        }
        // Rewrap every logical line.
        let mut physical: Vec<(Vec<Cell>, bool)> = Vec::new();
        for ll in &logicals {
            physical.extend(rewrap_one_logical(ll, new_cols, erase));
        }
        // Split bottom-aligned: last `new_rows` to grid, rest to scrollback.
        if physical.len() < new_rows {
            let need = new_rows - physical.len();
            for _ in 0..need {
                physical.push((vec![Cell::erased(erase.clone()); new_cols], false));
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
    #[must_use]
    pub fn hyperlink_entry(&self, id: HyperlinkId) -> Option<(Option<&str>, &str)> {
        let index = id.as_u32() as usize;
        self.hyperlink_table
            .get(index)
            .map(|(opt_id, uri)| (opt_id.as_ref().map(BoundedString::as_str), uri.as_str()))
    }

    /// Iterates the hyperlink table oldest first; `(HyperlinkId, Option<id>,
    /// uri)`.
    pub fn hyperlink_table(&self) -> impl Iterator<Item = (HyperlinkId, Option<&str>, &str)> + '_ {
        self.hyperlink_table
            .iter()
            .enumerate()
            .map(|(idx, (opt_id, uri))| {
                (
                    HyperlinkId::new(idx as u32),
                    opt_id.as_ref().map(BoundedString::as_str),
                    uri.as_str(),
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

    /// The image store; see `crate::image` for the OQ-008 status.
    ///
    /// Bounded placeholder stub (64 entries, 4096 bytes each) until the
    /// image RFC lands; no decoded pixels are held here.
    #[must_use]
    pub fn image_store(&self) -> &ImageStore {
        &self.images
    }

    /// Mutable image store (bounded placeholder). Restricted to tests and
    /// the future image protocol; not used by the parser path in this
    /// milestone.
    pub fn image_store_mut(&mut self) -> &mut ImageStore {
        &mut self.images
    }

    /// Inert-sequence telemetry counters (outside the state hash).
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
    #[must_use]
    pub fn damage_since(&self, generation: u64) -> Vec<DamagedRegion> {
        let mut regions = Vec::new();
        for batch in &self.damage_history {
            if batch.generation > generation {
                regions.extend_from_slice(&batch.regions);
            }
        }
        regions
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
            TerminalAction::CursorStyle { style } => self.cursor.cursor_style = *style,
            TerminalAction::CursorVisibility { visible } => self.cursor.visible = *visible,

            TerminalAction::EraseInDisplay { mode } => self.erase_in_display(*mode),
            TerminalAction::EraseInLine { mode } => self.erase_in_line(*mode),
            TerminalAction::EraseChars { n } => {
                let (row, col) = self.cursor_xy();
                let end = col
                    .saturating_add(effective_count(*n) as usize)
                    .saturating_sub(1);
                let erase = self.bce_style();
                self.screens_active_mut()
                    .erase_range_in_row(row, col, end, &erase);
                if self.screens_active_mut().repair_row(row, &erase) {
                    let last_col = self.width as u16 - 1;
                    self.damage_grid_rect(row as u16, col as u16, row as u16, last_col);
                }
                self.damage_grid_rect(row as u16, col as u16, row as u16, end as u16);
            }

            TerminalAction::InsertLines { n } => self.insert_lines(effective_count(*n)),
            TerminalAction::DeleteLines { n } => self.delete_lines(effective_count(*n)),
            TerminalAction::InsertChars { n } => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                self.screens_active_mut().insert_blanks_in_row(
                    row,
                    col,
                    effective_count(*n) as usize,
                    &erase,
                );
                self.screens_active_mut().repair_row(row, &erase);
                self.damage_row_tail(row as u16, col as u16);
            }
            TerminalAction::DeleteChars { n } => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                self.screens_active_mut().delete_chars_in_row(
                    row,
                    col,
                    effective_count(*n) as usize,
                    &erase,
                );
                self.screens_active_mut().repair_row(row, &erase);
                self.damage_row_tail(row as u16, col as u16);
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
            TerminalAction::InvokeCharset { slot } => self.charsets.invoke(*slot),

            TerminalAction::RequestDeviceStatus { kind } => self.request_device_status(*kind),
            TerminalAction::Reply { bytes } => self.replies.queue(bytes.clone()),

            TerminalAction::OscTitle { text } => self.title = text.clone(),
            TerminalAction::OscClipboard { .. } => {
                // Semantically inert here by contract (RFC replay guarantee
                // 6): clipboard effects enter state only through recorded
                // policy outcomes delivered as environment inputs once the
                // policy channel exists; the P0 consent gates in
                // bitty-docs/docs/security/ remain authoritative.
            }
            TerminalAction::OscCwd { url } => self.cwd_report = Some(url.clone()),
            TerminalAction::OscHyperlink { link } => self.osc_hyperlink(link.as_ref()),
            TerminalAction::OscPromptMark { kind, exit_code } => {
                self.record_zone(*kind, *exit_code);
            }
            TerminalAction::OscUnknown { .. } => self.telemetry.unknown_osc += 1,
            // Kitty graphics (CTX-0256): grid truth is untouched. Images live
            // in the runtime `KittyImageLayer`, which routes this action to
            // `kitty_display_image`; state stays inert by design.
            TerminalAction::KittyGraphics { .. } => {}

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
        let old_at_col = self.screens_active().get(row, col).clone();
        let old_ahead =
            (col < last_col_idx).then(|| self.screens_active().get(row, col + 1).clone());
        if old_at_col.spacer && col > 0 {
            let cleared = Cell::erased(erase.clone());
            self.screens_active_mut().set(row, col - 1, cleared);
            let c = (col - 1) as u16;
            self.damage_grid_rect(c, c, c, c);
        }
        match glyph_width {
            1 => {
                if old_at_col.width == 2 && !old_at_col.spacer && col < last_col_idx {
                    let cleared = Cell::erased(erase.clone());
                    self.screens_active_mut().set(row, col + 1, cleared);
                    let c = (col + 1) as u16;
                    self.damage_grid_rect(c, c, c, c);
                }
            }
            _ => {
                // Our trailing half replaces `col + 1`; if it previously
                // held a DIFFERENT pair's leading half, that pair's spacer
                // at `col + 2` survives unpaired.
                if let Some(ahead) = old_ahead {
                    if ahead.width == 2 && !ahead.spacer && col + 2 <= last_col_idx {
                        let cleared = Cell::erased(erase.clone());
                        self.screens_active_mut().set(row, col + 2, cleared);
                        let c = (col + 2) as u16;
                        self.damage_grid_rect(c, c, c, c);
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
        let style = self.cursor.style.clone();
        let link = self.current_hyperlink;
        self.screens_active_mut().set(
            row,
            col,
            Cell {
                glyph: ch,
                style: style.clone(),
                width: glyph_width,
                spacer: false,
                hyperlink: link,
                zerowidth: Vec::new(),
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
        let advanced = col + glyph_width as usize;
        if advanced >= self.width {
            self.cursor.position.col = cols - 1;
            self.cursor.pending_wrap = self.modes.auto_wrap;
        } else {
            self.cursor.position.col = advanced as u16;
        }
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
        let mut cell = self.screens_active().get(target_row, target_col).clone();
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
            0x0E => self.charsets.invoke(bitty_vt::CharsetSlot::G1),
            0x0F => self.charsets.invoke(bitty_vt::CharsetSlot::G0),
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
            style: self.cursor.style.clone(),
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
                self.screens_active_mut()
                    .erase_range_in_row(row, col, last_col as usize, &erase);
                self.screens_active_mut()
                    .fill_rect(row as u16 + 1, 0, last_row, last_col, &erase);
                let (row_u, col_u) = (row as u16, col as u16);
                self.damage_grid_rect(row_u, col_u, last_row, last_col);
            }
            EraseDisplayMode::Above => {
                let (row, col) = self.cursor_xy();
                let erase = self.bce_style();
                let last_col_u = self.width as u16 - 1;
                if row > 0 {
                    self.screens_active_mut()
                        .fill_rect(0, 0, row as u16 - 1, last_col_u, &erase);
                }
                self.screens_active_mut()
                    .erase_range_in_row(row, 0, col, &erase);
                let (row_u, col_u) = (row as u16, col as u16);
                self.damage_grid_rect(0, 0, row_u, col_u);
            }
            EraseDisplayMode::All => {
                let erase = self.bce_style();
                let (last_row_u, last_col_u) = (self.height as u16 - 1, self.width as u16 - 1);
                self.screens_active_mut()
                    .fill_rect(0, 0, last_row_u, last_col_u, &erase);
                self.damage_grid_rect(0, 0, last_row_u, last_col_u);
            }
            EraseDisplayMode::Scrollback => {
                let cleared = self.scrollback.clear();
                self.push_scroll_damage(cleared);
            }
        }
    }

    fn erase_in_line(&mut self, mode: EraseLineMode) {
        let (row, col) = self.cursor_xy();
        let last_col = self.width as u16 - 1;
        let erase = self.bce_style();
        let (row_u, col_u) = (row as u16, col as u16);
        match mode {
            EraseLineMode::Right => {
                self.screens_active_mut()
                    .erase_range_in_row(row, col, last_col as usize, &erase);
                self.damage_grid_rect(row_u, col_u, row_u, last_col);
            }
            EraseLineMode::Left => {
                self.screens_active_mut()
                    .erase_range_in_row(row, 0, col, &erase);
                self.damage_grid_rect(row_u, 0, row_u, col_u);
            }
            EraseLineMode::All => {
                self.screens_active_mut()
                    .erase_range_in_row(row, 0, last_col as usize, &erase);
                self.damage_grid_rect(row_u, 0, row_u, last_col);
            }
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
            Mode::KittyKeyboard(flags) => {
                if enabled {
                    // Progressive flags: OR in bounded bits (candidate spec: u32, unknown bits ignored)
                    self.modes.kitty_keyboard |= flags & 0x1F;
                } else if flags == 0 {
                    // CSI ? 7727 l without flags disables all
                    self.modes.kitty_keyboard = 0;
                } else {
                    self.modes.kitty_keyboard &= !(flags & 0x1F);
                }
            }
            Mode::MouseTracking(tracking) => {
                self.modes.mouse_tracking = enabled.then_some(tracking);
            }
            Mode::MouseCoordinateEncoding(encoding) => {
                self.modes.mouse_coordinate_encoding = enabled.then_some(encoding);
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
                style: self.cursor.style.clone(),
                cursor_style: self.cursor.cursor_style,
                cursor_visible: self.cursor.visible,
                origin_mode: self.modes.origin,
                auto_wrap: self.modes.auto_wrap,
                charsets: self.charsets.clone(),
                modes: self.modes.clone(),
            });
            self.alt_screen = variant;
            if variant == AltScreen::Via1049 {
                // ?1049 clears the alternate screen on entry; ?47 keeps
                // whatever the alt grid last held.
                let erase = self.bce_style();
                self.screens.alt.fill_all(&erase);
            }
            self.damage_grid_rect(0, 0, self.height as u16 - 1, self.width as u16 - 1);
        } else {
            if self.alt_screen == AltScreen::Off {
                return;
            }
            if let Some(save) = self.primary_save.take() {
                self.cursor.position = save.cursor_position;
                self.cursor.pending_wrap = save.pending_wrap;
                self.cursor.style = save.style;
                self.cursor.cursor_style = save.cursor_style;
                self.cursor.visible = save.cursor_visible;
                self.charsets = save.charsets;
                self.modes = save.modes;
            }
            self.alt_screen = AltScreen::Off;
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
                    .position(|(id, uri)| *id == key_id && *uri == key_uri);
                let resolved = match existing {
                    Some(index) => Some(HyperlinkId::new(index as u32)),
                    None => {
                        if self.hyperlink_table.len() < HYPERLINK_TABLE_MAX {
                            self.hyperlink_table.push((key_id, key_uri));
                            Some(HyperlinkId::new((self.hyperlink_table.len() - 1) as u32))
                        } else {
                            // Table at capacity: degrade to no link rather
                            // than grow without bound (threat T-01).
                            None
                        }
                    }
                };
                self.current_hyperlink = resolved;
            }
        }
    }

    fn record_zone(&mut self, kind: ZoneKind, exit_code: Option<i32>) {
        self.zone_counter += 1;
        let code = if kind == ZoneKind::OutputEnd {
            exit_code
        } else {
            None
        };
        self.zones.push_back(ZoneRecord {
            ordinal: self.zone_counter,
            kind,
            exit_code: code,
        });
        while self.zones.len() > ZONE_RECORDS_MAX {
            self.zones.pop_front();
        }
    }

    // ------------------------------------------------------------------
    // Replies (queued, never written anywhere)
    // ------------------------------------------------------------------

    fn request_device_status(&mut self, kind: StatusKind) {
        let payload: Box<[u8]> = match kind {
            StatusKind::OperatingStatus => b"\x1b[0n".to_vec().into_boxed_slice(),
            StatusKind::CursorPosition => {
                let row = if self.modes.origin {
                    self.cursor.position.row - self.scroll_region_top + 1
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
        self.saved_cursors = [None, None];
        self.cursor = Cursor::default();
        self.modes = Modes::default();
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.height as u16 - 1;
        self.tabs = TabStops::default_lattice(self.width);
        self.charsets = Charsets::default();
        let cleared = self.scrollback.clear();
        self.push_scroll_damage(cleared);
        self.replies.clear();
        self.title = BoundedString::new("");
        self.cwd_report = None;
        self.hyperlink_table.clear();
        self.current_hyperlink = None;
        self.zones.clear();
        self.zone_counter = 0;
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
        leads.push(cell.clone());
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
        return vec![(vec![Cell::erased(erase.clone()); new_cols], false)];
    }
    let mut out: Vec<(Vec<Cell>, bool)> = Vec::new();
    let mut cur: Vec<Cell> = Vec::with_capacity(new_cols);
    let mut used = 0usize;
    let mut flush_row = |cur: &mut Vec<Cell>, used: &mut usize, wrapped: bool| {
        while *used < new_cols {
            cur.push(Cell::erased(erase.clone()));
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
                cur.push(Cell::erased(erase.clone()));
                used += 1;
                flush_row(&mut cur, &mut used, true);
            } else {
                flush_row(&mut cur, &mut used, true);
            }
        }
        cur.push(lead.clone());
        used += 1;
        if w == 2 {
            cur.push(Cell::wide_spacer(lead.style.clone()));
            used += 1;
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
