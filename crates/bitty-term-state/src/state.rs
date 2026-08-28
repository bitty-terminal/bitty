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

use crate::canonical::{CANONICAL_HASH_VERSION, CanonicalHasher, write_cell, write_style};
use crate::cell::{AttributeChangeKind, Attributes, Cell, HyperlinkId, Style, char_cell_width};
use crate::charsets::Charsets;
use crate::cursor::{Cursor, CursorPosition, SavedCursor};
use crate::damage::{DAMAGE_HISTORY_BATCHES, Damage, DamageRect, DamagedRegion, coalesce};
use crate::grid::{Grid, ScreenPair};
use crate::image::ImageStore;
use crate::modes::{AltScreen, Modes};
use crate::replies::Replies;
use crate::scrollback::{ClearedRange, SCROLLBACK_MAX_LINES, Scrollback, ScrollbackLine};
use crate::tabs::TabStops;

/// Initial grid width in columns; resize awaits the singular reflow
/// algorithm the Terminal State RFC defers under "Open items remaining
/// under OQ-007".
pub const GRID_COLUMNS: usize = 80;

/// Initial grid height in rows; see [`GRID_COLUMNS`].
pub const GRID_ROWS: usize = 24;

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

/// Why [`State::check_invariants`] rejected the current state.
///
/// Every variant names the violated RFC invariant clause; production code
/// cannot construct these states because debug builds assert after every
/// action and every mutating helper preserves totality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantViolation {
    /// Invariant 1: a grid's cell count differs from `width * height`.
    GridDimensionsMismatch {
        /// Expected cell count.
        expected: usize,
        /// Actual cell count.
        actual: usize,
    },
    /// Invariant 1: scroll region violates `top <= bottom < height`.
    ScrollRegionInvalid {
        /// Region top row.
        top: u16,
        /// Region bottom row.
        bottom: u16,
        /// Grid height.
        height: u16,
    },
    /// Invariant 1: cursor outside screen bounds.
    CursorOutOfBounds {
        /// Cursor row.
        row: u16,
        /// Cursor column.
        col: u16,
        /// Grid height.
        height: u16,
        /// Grid width.
        width: u16,
    },
    /// Invariant 1: origin-mode cursor outside the scroll region.
    CursorOutsideRegion {
        /// Cursor row.
        row: u16,
        /// Region top row.
        region_top: u16,
        /// Region bottom row.
        region_bottom: u16,
    },
    /// Invariant 3: cursor rests on a wide-character spacer half.
    CursorOnSpacer {
        /// Cursor row.
        row: u16,
        /// Cursor column.
        col: u16,
    },
    /// Invariant 2: trailing spacer without a wide leading half before it.
    OrphanSpacer {
        /// Spacer row.
        row: u16,
        /// Spacer column.
        col: u16,
    },
    /// Invariant 2: wide leading half whose trailing half is missing.
    UnpairedWideLeading {
        /// Leading-cell row.
        row: u16,
        /// Leading-cell column.
        col: u16,
    },
    /// Invariant 2: a cell claims a width other than one or two.
    InvalidCellWidth {
        /// Cell row.
        row: u16,
        /// Cell column.
        col: u16,
        /// The invalid width.
        width: u8,
    },
    /// Invariant 4: scrollback exceeds its hard cap.
    ScrollbackOverCapacity {
        /// Retained lines.
        len: usize,
        /// The cap.
        cap: usize,
    },
    /// Invariant 4: a scrollback line's width differs from the grid width.
    ScrollbackWidthMismatch {
        /// Offending line id.
        line_id: u64,
        /// Stored cell count.
        cells: usize,
        /// Expected grid width.
        width: usize,
    },
    /// Invariant 4: scrollback ids decreased or repeated.
    ScrollbackIdsNotMonotonic {
        /// Previous line id.
        previous: u64,
        /// Later line id.
        current: u64,
    },
    /// Invariant 6: tab lattice covers a different column count.
    TabLatticeWidthMismatch {
        /// Stop-vector length.
        stops: usize,
        /// Grid columns.
        columns: usize,
    },
    /// Invariant 7: queued reply bytes exceed the cap.
    ReplyBudgetExceeded {
        /// Queued bytes.
        total: usize,
        /// The cap.
        cap: usize,
    },
    /// Invariant 5: alternate screen active without a saved primary set.
    AltScreenWithoutSavedPrimary,
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
    /// A freshly initialized terminal at [`GRID_ROWS`] x [`GRID_COLUMNS`].
    #[must_use]
    pub fn new() -> Self {
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
            scrollback: Scrollback::new(),
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

    /// Resizes the terminal grid to `new_cols x new_rows` using the singular
    /// deterministic reflow: truncate/pad with wide-pair orphan repair, bounded
    /// to `[1, 1000]` per dimension (same bound as `RuntimeConfig` to keep
    /// memory bounded under T-01). Scrollback lines are resized to the new
    /// width with ids preserved; scroll region is reset to the full screen
    /// and the cursor is clamped off spacers (RFC invariants 1-6). This is
    /// the environment-declared resize path (RFC "Environment declaration")
    /// and the only mutation of retained scrollback outside `push`/`clear`.
    /// Returns the damage for the resize batch (full grid plus scrollback
    /// reflow range when scrollback non-empty) tagged with the new
    /// generation. Headless: pure in-memory, no I/O, deterministic.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) -> Damage {
        let cols = new_cols.clamp(1, 1000);
        let rows = new_rows.clamp(1, 1000);
        if cols == self.width && rows == self.height {
            return Damage {
                generation: self.generation,
                regions: Vec::new().into_boxed_slice(),
            };
        }
        let erase = self.bce_style();
        // Resize stored scrollback lines to the new column width before the
        // grid changes, so `check_invariants` sees consistent widths throughout.
        self.scrollback.resize(cols, &erase);
        // Resize both screen grids with the same erase style.
        self.screens.main.resize(rows, cols, &erase);
        self.screens.alt.resize(rows, cols, &erase);
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
        // clamp cursor and saved cursors into the new bounds.
        self.scroll_region_top = 0;
        self.scroll_region_bottom = (rows - 1) as u16;
        self.cursor.position.row = self.cursor.position.row.min((rows - 1) as u16);
        self.cursor.position.col = self.cursor.position.col.min((cols - 1) as u16);
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

    /// Recomputes every RFC invariant against the live state.
    pub fn check_invariants(&self) -> Result<(), InvariantViolation> {
        let expected = self.width * self.height;
        for grid in [&self.screens.main, &self.screens.alt] {
            let (rows, cols) = grid.dims();
            if rows * cols != expected {
                return Err(InvariantViolation::GridDimensionsMismatch {
                    expected,
                    actual: rows * cols,
                });
            }
            for (r, row_cells) in grid.rows_iter().enumerate() {
                for (c, cell) in row_cells.iter().enumerate() {
                    if cell.width != 1 && cell.width != 2 {
                        return Err(InvariantViolation::InvalidCellWidth {
                            row: r as u16,
                            col: c as u16,
                            width: cell.width,
                        });
                    }
                    if cell.spacer {
                        let paired = c > 0 && {
                            let lead = &row_cells[c - 1];
                            lead.width == 2 && !lead.spacer
                        };
                        if !paired {
                            return Err(InvariantViolation::OrphanSpacer {
                                row: r as u16,
                                col: c as u16,
                            });
                        }
                    } else if cell.width == 2 {
                        let paired_trailer = c + 1 < cols && row_cells[c + 1].spacer;
                        if !paired_trailer {
                            return Err(InvariantViolation::UnpairedWideLeading {
                                row: r as u16,
                                col: c as u16,
                            });
                        }
                    }
                }
            }
        }
        if !(self.scroll_region_top <= self.scroll_region_bottom
            && (self.scroll_region_bottom as usize) < self.height)
        {
            return Err(InvariantViolation::ScrollRegionInvalid {
                top: self.scroll_region_top,
                bottom: self.scroll_region_bottom,
                height: self.height as u16,
            });
        }
        let (crow, ccol) = (self.cursor.position.row, self.cursor.position.col);
        if (crow as usize) >= self.height || (ccol as usize) >= self.width {
            return Err(InvariantViolation::CursorOutOfBounds {
                row: crow,
                col: ccol,
                height: self.height as u16,
                width: self.width as u16,
            });
        }
        if self.modes.origin && (crow < self.scroll_region_top || crow > self.scroll_region_bottom)
        {
            return Err(InvariantViolation::CursorOutsideRegion {
                row: crow,
                region_top: self.scroll_region_top,
                region_bottom: self.scroll_region_bottom,
            });
        }
        if self
            .screens_active()
            .get(crow as usize, ccol as usize)
            .spacer
        {
            return Err(InvariantViolation::CursorOnSpacer {
                row: crow,
                col: ccol,
            });
        }
        if self.scrollback.len() > SCROLLBACK_MAX_LINES {
            return Err(InvariantViolation::ScrollbackOverCapacity {
                len: self.scrollback.len(),
                cap: SCROLLBACK_MAX_LINES,
            });
        }
        let mut previous_id: Option<u64> = None;
        for line in self.scrollback.iter() {
            if line.cells.len() != self.width {
                return Err(InvariantViolation::ScrollbackWidthMismatch {
                    line_id: line.id,
                    cells: line.cells.len(),
                    width: self.width,
                });
            }
            if let Some(prev) = previous_id {
                if line.id <= prev {
                    return Err(InvariantViolation::ScrollbackIdsNotMonotonic {
                        previous: prev,
                        current: line.id,
                    });
                }
            }
            previous_id = Some(line.id);
        }
        if self.tabs.len() != self.width {
            return Err(InvariantViolation::TabLatticeWidthMismatch {
                stops: self.tabs.len(),
                columns: self.width,
            });
        }
        if self.replies.total_bytes() > crate::replies::REPLY_CAP_BYTES {
            return Err(InvariantViolation::ReplyBudgetExceeded {
                total: self.replies.total_bytes(),
                cap: crate::replies::REPLY_CAP_BYTES,
            });
        }
        if self.alt_screen != AltScreen::Off && self.primary_save.is_none() {
            return Err(InvariantViolation::AltScreenWithoutSavedPrimary);
        }
        Ok(())
    }

    /// Platform-stable state hash (RFC replay guarantee 2).
    ///
    /// FNV-1a over the canonical serialization: fixed field ordering,
    /// little-endian integers, UTF-32 scalars, length-prefixed strings.
    /// Same input conditions produce the identical digest everywhere.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = CanonicalHasher::new();
        h.u32(CANONICAL_HASH_VERSION);
        // Deliberately excluded: `generation` (damage bookkeeping, not
        // truth; RFC replay guarantee 2 enumerates grid, scrollback,
        // cursor, modes, tab stops, charset slots, and pending replies).
        h.u16(self.width as u16);
        h.u16(self.height as u16);

        h.u16(self.cursor.position.row);
        h.u16(self.cursor.position.col);
        h.boolean(self.cursor.pending_wrap);
        h.boolean(self.cursor.visible);
        h.u8(cursor_style_discriminant(self.cursor.cursor_style));
        write_style(&mut h, &self.cursor.style);

        h.boolean(self.modes.insert);
        h.boolean(self.modes.line_feed_new_line);
        h.boolean(self.modes.application_keypad);
        h.boolean(self.modes.application_cursor_keys);
        h.boolean(self.modes.column_132_requested);
        h.boolean(self.modes.reverse_video);
        h.boolean(self.modes.origin);
        h.boolean(self.modes.auto_wrap);
        h.boolean(self.modes.cursor_blinking);
        h.boolean(self.modes.bracketed_paste);
        h.boolean(self.modes.focus_events);
        h.option_tag(self.modes.mouse_tracking.is_some());
        if let Some(mode) = self.modes.mouse_tracking {
            h.u8(mouse_tracking_discriminant(mode));
        }
        h.option_tag(self.modes.mouse_coordinate_encoding.is_some());
        if let Some(encoding) = self.modes.mouse_coordinate_encoding {
            h.u8(mouse_encoding_discriminant(encoding));
        }

        h.u8(alt_screen_discriminant(self.alt_screen));
        h.option_tag(self.primary_save.is_some());
        if let Some(save) = &self.primary_save {
            out_save_cursor_fields(&mut h, save);
            write_modes(&mut h, &save.modes);
        }
        for saved in &self.saved_cursors {
            h.option_tag(saved.is_some());
            if let Some(saved) = saved {
                write_saved_cursor(&mut h, saved);
            }
        }

        h.u16(self.scroll_region_top);
        h.u16(self.scroll_region_bottom);

        h.u16(self.tabs.len() as u16);
        for col in 0..self.tabs.len() {
            h.boolean(self.tabs.contains(col));
        }

        write_charsets(&mut h, &self.charsets);

        h.str(self.title.as_str());
        h.option_tag(self.cwd_report.is_some());
        if let Some(cwd) = &self.cwd_report {
            h.str(cwd.as_str());
        }

        h.u32(self.hyperlink_table.len() as u32);
        for (id, uri) in &self.hyperlink_table {
            h.option_tag(id.is_some());
            if let Some(id) = id {
                h.str(id.as_str());
            }
            h.str(uri.as_str());
        }
        h.option_tag(self.current_hyperlink.is_some());
        if let Some(link) = self.current_hyperlink {
            h.u32(link.as_u32());
        }

        h.u64(self.zone_counter);
        h.u32(self.zones.len() as u32);
        for record in &self.zones {
            h.u64(record.ordinal);
            h.u8(zone_discriminant(record.kind));
        }

        h.u64(self.scrollback.next_line_id());
        h.u64(self.scrollback.total_written());
        h.u32(self.scrollback.len() as u32);
        for line in self.scrollback.iter() {
            h.u64(line.id);
            for cell in &line.cells {
                write_cell(&mut h, cell);
            }
        }

        h.u32(self.replies.total_bytes() as u32);
        h.boolean(self.replies.overflowed());
        let pending = self.peek_replies();
        h.u32(pending.len() as u32);
        for reply in pending {
            h.u32(reply.len() as u32);
            h.u8_slice(reply);
        }

        for cell in self.screens.main.all_cells() {
            write_cell(&mut h, cell);
        }
        for cell in self.screens.alt.all_cells() {
            write_cell(&mut h, cell);
        }

        h.finish()
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
                let mut col = self.cursor.position.col as usize;
                for _ in 0..effective_count(*n) {
                    col = self.tabs.next_after(col).unwrap_or(self.width - 1);
                }
                self.cursor.position.col = col as u16;
                self.cursor.pending_wrap = false;
            }
            TerminalAction::TabBackward { n } => {
                let mut col = self.cursor.position.col as usize;
                for _ in 0..effective_count(*n) {
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
            TerminalAction::OscPromptMark { kind } => self.record_zone(*kind),
            TerminalAction::OscUnknown { .. } => self.telemetry.unknown_osc += 1,

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
        let glyph_width = char_cell_width(ch);
        if glyph_width == 0 || ch == '\0' {
            // Zero-width scalars await grapheme composition from the text
            // RFC (ADR-0004 open item); dropping them keeps cells total.
            return;
        }
        let cols = self.width as u16;
        // Consume a pending wrap before placing the glyph (DECAWM).
        if self.modes.auto_wrap && self.cursor.pending_wrap {
            self.index_linefeed();
            self.cursor.position.col = 0;
        }
        self.cursor.pending_wrap = false;
        if glyph_width == 2 && self.cursor.position.col + 1 >= cols {
            if self.modes.auto_wrap {
                // Single documented rule for a wide character at the final
                // column: wrap, then place on the next line.
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
                let mut c = col as usize;
                for _ in 0..n {
                    if c < last_col as usize {
                        c += 1;
                        // Hop across a wide pair's trailing half so the
                        // cursor never rests on a spacer (invariant 3);
                        // a pair ending at the last column returns the
                        // cursor to its leading half.
                        if self
                            .screens_active()
                            .get(self.cursor.position.row as usize, c)
                            .spacer
                        {
                            c = if c < last_col as usize { c + 1 } else { c - 1 };
                        }
                    }
                }
                self.cursor.position.col = c as u16;
            }
            Direction::Left => {
                let mut c = col as usize;
                for _ in 0..n {
                    if c > 0 {
                        c -= 1;
                        if self
                            .screens_active()
                            .get(self.cursor.position.row as usize, c)
                            .spacer
                        {
                            c = c.saturating_sub(1);
                        }
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
        let mut col = self.cursor.position.col as usize;
        for _ in 0..steps {
            col = self.tabs.next_after(col).unwrap_or(self.width - 1);
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
        // bottom is the screen bottom (invariant 4).
        if self.scroll_region_bottom as usize == self.height - 1 {
            for line in removed {
                let (id, evicted) = self.scrollback.push(line);
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
            let kind = match change {
                AttributeChange::Reset => AttributeChangeKind::Reset,
                AttributeChange::Enable(attr) => AttributeChangeKind::Set(*attr, true),
                AttributeChange::Disable(attr) => AttributeChangeKind::Set(*attr, false),
                AttributeChange::Foreground(color) => {
                    self.cursor.style.foreground = color_option(*color);
                    continue;
                }
                AttributeChange::Background(color) => {
                    self.cursor.style.background = color_option(*color);
                    continue;
                }
                AttributeChange::UnderlineColor(color) => {
                    self.cursor.style.underline_color = color_option(*color);
                    continue;
                }
            };
            self.cursor.style.attributes.apply_change(&kind);
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

    fn record_zone(&mut self, kind: ZoneKind) {
        self.zone_counter += 1;
        self.zones.push_back(ZoneRecord {
            ordinal: self.zone_counter,
            kind,
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

// ----------------------------------------------------------------------
// Discriminant helpers (fixed values are part of the hash contract)
// ----------------------------------------------------------------------

fn cursor_style_discriminant(style: CursorStyle) -> u8 {
    match style {
        CursorStyle::Default => 0,
        CursorStyle::BlinkingBlock => 1,
        CursorStyle::SteadyBlock => 2,
        CursorStyle::BlinkingUnderline => 3,
        CursorStyle::SteadyUnderline => 4,
        CursorStyle::BlinkingBar => 5,
        CursorStyle::SteadyBar => 6,
    }
}

fn mouse_tracking_discriminant(mode: bitty_vt::MouseTrackingMode) -> u8 {
    match mode {
        bitty_vt::MouseTrackingMode::X10 => 1,
        bitty_vt::MouseTrackingMode::Normal => 2,
        bitty_vt::MouseTrackingMode::Button => 3,
        bitty_vt::MouseTrackingMode::Any => 4,
    }
}

fn mouse_encoding_discriminant(encoding: bitty_vt::MouseCoordinateEncoding) -> u8 {
    match encoding {
        bitty_vt::MouseCoordinateEncoding::Utf8 => 1,
        bitty_vt::MouseCoordinateEncoding::Sgr => 2,
        bitty_vt::MouseCoordinateEncoding::Urxvt => 3,
    }
}

fn alt_screen_discriminant(screen: AltScreen) -> u8 {
    match screen {
        AltScreen::Off => 0,
        AltScreen::Via47 => 1,
        AltScreen::Via1049 => 2,
    }
}

fn charset_discriminant(slot: bitty_vt::CharsetSlot) -> u8 {
    match slot {
        bitty_vt::CharsetSlot::G0 => 0,
        bitty_vt::CharsetSlot::G1 => 1,
        bitty_vt::CharsetSlot::G2 => 2,
        bitty_vt::CharsetSlot::G3 => 3,
    }
}

fn table_discriminant(table: bitty_vt::CharsetTable) -> u8 {
    match table {
        bitty_vt::CharsetTable::Ascii => 0,
        bitty_vt::CharsetTable::UnitedKingdom => 1,
        bitty_vt::CharsetTable::DecSpecialGraphics => 2,
    }
}

fn zone_discriminant(kind: ZoneKind) -> u8 {
    match kind {
        ZoneKind::PromptStart => 0,
        ZoneKind::InputStart => 1,
        ZoneKind::OutputStart => 2,
        ZoneKind::OutputEnd => 3,
    }
}

fn write_saved_cursor(out: &mut CanonicalHasher, saved: &SavedCursor) {
    out.u16(saved.position.row);
    out.u16(saved.position.col);
    out.boolean(saved.pending_wrap);
    write_style(out, &saved.style);
    out.boolean(saved.origin_mode);
    out.boolean(saved.auto_wrap);
    write_charsets(out, &saved.charsets);
}

fn out_save_cursor_fields(out: &mut CanonicalHasher, save: &ScreenSave) {
    out.u16(save.cursor_position.row);
    out.u16(save.cursor_position.col);
    out.boolean(save.pending_wrap);
    write_style(out, &save.style);
    out.boolean(save.origin_mode);
    out.boolean(save.auto_wrap);
    write_charsets(out, &save.charsets);
}

fn write_charsets(out: &mut CanonicalHasher, charsets: &Charsets) {
    out.u8(charset_discriminant(charsets.locking));
    for table in &charsets.slots {
        out.u8(table_discriminant(*table));
    }
    out.option_tag(charsets.single.is_some());
    if let Some(slot) = charsets.single {
        out.u8(charset_discriminant(slot));
    }
}

fn write_modes(out: &mut CanonicalHasher, modes: &Modes) {
    out.boolean(modes.insert);
    out.boolean(modes.line_feed_new_line);
    out.boolean(modes.application_keypad);
    out.boolean(modes.application_cursor_keys);
    out.boolean(modes.column_132_requested);
    out.boolean(modes.reverse_video);
    out.boolean(modes.origin);
    out.boolean(modes.auto_wrap);
    out.boolean(modes.cursor_blinking);
    out.boolean(modes.bracketed_paste);
    out.boolean(modes.focus_events);
}

fn color_option(color: bitty_vt::Color) -> Option<bitty_vt::Color> {
    match color {
        bitty_vt::Color::Default => None,
        other => Some(other),
    }
}

/// Parser-resolved counts are guaranteed positive; defensive floor at one.
fn effective_count(n: Count) -> u16 {
    n.0.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::SCROLLBACK_MAX_LINES;
    use bitty_vt::{AttributeChange, AttributeDiff, Color, ControlChar, GraphemeCell};

    fn prints(state: &mut State, text: &str) {
        for c in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
        }
    }

    #[test]
    fn print_places_glyphs_and_advances_cursor() {
        let mut s = State::new();
        prints(&mut s, "ab\u{4E2D}");
        assert_eq!(s.cursor().position.col, 4);
        let snap = s.snapshot();
        assert_eq!(snap.cells[0].glyph, 'a');
        assert_eq!(snap.cells[2].glyph, '\u{4E2D}');
        assert_eq!(snap.cells[2].width, 2);
        assert!(snap.cells[3].spacer);
        assert!(s.check_invariants().is_ok());
    }

    #[test]
    fn deferred_wrap_latches_at_last_column() {
        let mut s = State::new();
        s.cursor.position.col = GRID_COLUMNS as u16 - 1;
        prints(&mut s, "x");
        assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 1);
        assert!(s.cursor().pending_wrap);
        // Next print consumes the latch onto the next line.
        prints(&mut s, "y");
        assert_eq!(s.cursor().position.col, 1);
        assert_eq!(s.cursor().position.row, 1);
        assert!(!s.cursor().pending_wrap);
    }

    #[test]
    fn origin_mode_addresses_relative_to_region() {
        let mut s = State::new();
        s.apply(&TerminalAction::SetScrollRegion {
            top: Row(5),
            bottom: Row(10),
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::Origin,
            enabled: true,
        });
        // DECOM homes into the region.
        assert_eq!(s.cursor().position.row, 4);
        s.apply(&TerminalAction::CursorPosition {
            row: Row(1),
            col: Col(3),
        });
        assert_eq!(s.cursor().position.row, 4);
        assert_eq!(s.cursor().position.col, 2);
    }

    #[test]
    fn invalid_scroll_region_is_ignored() {
        let mut s = State::new();
        s.apply(&TerminalAction::SetScrollRegion {
            top: Row(10),
            bottom: Row(5),
        });
        assert_eq!(s.scroll_region_top, 0);
        assert_eq!(s.scroll_region_bottom, GRID_ROWS as u16 - 1);
    }

    #[test]
    fn alt_screen_roundtrip_restores_primary_set() {
        let mut s = State::new();
        prints(&mut s, "primary");
        s.apply(&TerminalAction::SetAttributes {
            attrs: AttributeDiff {
                changes: vec![
                    AttributeChange::Enable(bitty_vt::Attribute::Bold),
                    AttributeChange::Foreground(Color::Indexed(1)),
                ]
                .into_boxed_slice(),
            },
        });
        s.apply(&TerminalAction::CursorMove {
            dir: Direction::Down,
            n: Count(3),
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::BracketedPaste,
            enabled: true,
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::AlternateScreenClearAndRestore,
            enabled: true,
        });
        assert!(s.alt_screen_active());
        // Mutate the alternate context aggressively: modes flipped on alt
        // must NOT leak into the restored primary set (invariant 5), while
        // the pre-entry bracketed-paste state must come back.
        prints(&mut s, "alt junk");
        s.apply(&TerminalAction::SetMode {
            mode: Mode::BracketedPaste,
            enabled: false,
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::Origin,
            enabled: true,
        });
        s.apply(&TerminalAction::EraseInDisplay {
            mode: EraseDisplayMode::All,
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::AlternateScreenClearAndRestore,
            enabled: false,
        });
        // Full primary-screen cursor/style/mode set restored (invariant 5).
        assert!(!s.alt_screen_active());
        assert!(
            s.modes.bracketed_paste,
            "pre-entry mode state must survive the roundtrip"
        );
        assert!(!s.modes.origin);
        assert!(s.cursor().style.attributes.bold);
        assert_eq!(
            s.cursor().style.foreground,
            Some(Color::Indexed(1)),
            "pen style must survive the roundtrip"
        );
        assert_eq!(s.cursor().position.col, 7);
        let snap = s.snapshot();
        assert_eq!(
            &snap.cells[..7].iter().map(|c| c.glyph).collect::<String>(),
            "primary"
        );
        assert!(s.check_invariants().is_ok());
    }

    #[test]
    fn scroll_under_screen_bottom_captures_scrollback() {
        let mut s = State::new();
        prints(&mut s, "line one");
        for _ in 0..(GRID_ROWS + 3) {
            s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
        }
        assert!(
            s.scrollback_len() > 0 && s.scrollback_len() <= SCROLLBACK_MAX_LINES,
            "indexing at the screen bottom must feed scrollback"
        );
        assert_eq!(
            s.scrollback_line(0).unwrap().cells[0].glyph,
            'l',
            "oldest captured line first"
        );
        // Partial regions never capture (invariant 4).
        let before = s.scrollback_len();
        s.apply(&TerminalAction::SetScrollRegion {
            top: Row(2),
            bottom: Row(10),
        });
        s.apply(&TerminalAction::ScrollUp { n: Count(3) });
        assert_eq!(s.scrollback_len(), before);
    }

    #[test]
    fn reply_synthesis_is_origin_aware_and_bounded() {
        let mut s = State::new();
        s.apply(&TerminalAction::RequestDeviceStatus {
            kind: StatusKind::OperatingStatus,
        });
        s.apply(&TerminalAction::RequestDeviceStatus {
            kind: StatusKind::DeviceAttributes,
        });
        let replies = s.take_replies();
        assert_eq!(replies.len(), 2);
        assert_eq!(&replies[0][..], b"\x1b[0n");
        assert_eq!(&replies[1][..], b"\x1b[?6c");
        // CPR reflects origin-relative rows.
        s.apply(&TerminalAction::SetScrollRegion {
            top: Row(5),
            bottom: Row::SENTINEL,
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::Origin,
            enabled: true,
        });
        s.apply(&TerminalAction::RequestDeviceStatus {
            kind: StatusKind::CursorPosition,
        });
        let replies = s.take_replies();
        assert_eq!(&replies[0][..], b"\x1b[1;1R");
    }

    #[test]
    fn decstr_resets_defined_subset_only() {
        let mut s = State::new();
        s.apply(&TerminalAction::SetAttributes {
            attrs: AttributeDiff {
                changes: vec![AttributeChange::Enable(bitty_vt::Attribute::Bold)]
                    .into_boxed_slice(),
            },
        });
        s.apply(&TerminalAction::SetMode {
            mode: Mode::Origin,
            enabled: true,
        });
        s.apply(&TerminalAction::SoftReset);
        assert!(s.cursor().visible);
        assert!(!s.modes.origin);
        assert!(!s.modes.auto_wrap, "DECSTR resets DECAWM per VT510");
        assert_eq!(s.cursor().style, Style::default());
        assert_eq!(s.scroll_region_bottom, GRID_ROWS as u16 - 1);
    }

    #[test]
    fn full_reset_restores_initial_truth() {
        let mut s = State::new();
        prints(&mut s, "junk \u{4E2D} more");
        s.apply(&TerminalAction::OscTitle {
            text: BoundedString::new("t"),
        });
        s.apply(&TerminalAction::FullReset);
        assert!(s.check_invariants().is_ok());
        assert_eq!(s.state_hash(), State::new().state_hash());
        assert!(s.title().is_empty());
        assert_eq!(s.scrollback_len(), 0);
        let snap = s.snapshot();
        assert!(snap.cells.iter().all(Cell::is_blank));
    }
}
