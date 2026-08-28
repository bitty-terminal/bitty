//! Selection primitives with wide-character awareness.
//!
//! Selections anchor to grid coordinates `(row, col)`. Wide characters occupy
//! two cells: a leading cell with `width == 2` and a trailing spacer with
//! `spacer == true`. To preserve RFC invariant 2 ("no orphan spacers") the
//! selection algebra snaps any position that lands on a spacer to its leading
//! half and never splits a wide pair. All operations are deterministic,
//! headless, and total over all inputs.

#![forbid(unsafe_code)]

use bitty_term_state::Snapshot;

/// Grid position in cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct CellPos {
    /// Row.
    pub row: u16,
    /// Column (leader column for wide chars).
    pub col: u16,
}

impl CellPos {
    /// Creates a position.
    #[must_use]
    pub const fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }
}

/// Normalized inclusive range: `start <= end` lexicographically by `(row, col)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectionRange {
    /// Inclusive start.
    pub start: CellPos,
    /// Inclusive end.
    pub end: CellPos,
}

impl SelectionRange {
    /// Creates a normalized range (sorts endpoints).
    #[must_use]
    pub fn new(a: CellPos, b: CellPos) -> Self {
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    /// True when start == end (single cell).
    #[must_use]
    pub fn is_single_cell(self) -> bool {
        self.start == self.end
    }

    /// Returns true when `pos` lies within the range lex order inclusive.
    ///
    /// Semantics: row-major inclusive range (multi-line). For block selection
    /// use [`Selection::contains_block`].
    #[must_use]
    pub fn contains(self, pos: CellPos) -> bool {
        if pos.row < self.start.row || pos.row > self.end.row {
            return false;
        }
        if pos.row == self.start.row && pos.row == self.end.row {
            return pos.col >= self.start.col && pos.col <= self.end.col;
        }
        if pos.row == self.start.row {
            return pos.col >= self.start.col;
        }
        if pos.row == self.end.row {
            return pos.col <= self.end.col;
        }
        true
    }

    /// Number of rows spanned inclusive.
    #[must_use]
    pub fn row_span(self) -> usize {
        (self.end.row as usize) - (self.start.row as usize) + 1
    }
}

/// Kind of selection gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SelectionKind {
    /// Drag range.
    #[default]
    Simple,
    /// Word expansion on double-click.
    Word,
    /// Whole-line expansion on triple-click.
    Line,
    /// Rectangular block (columnar).
    Block,
}

/// Drag selection with anchor (press) and focus (release/cursor) points.
///
/// `anchor <= focus` is not required; [`Selection::normalized`] sorts them.
/// Wide-char snapping is applied on construction via [`Selection::snap`] or
/// on demand via [`Selection::snapped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    /// Point where the drag started.
    pub anchor: CellPos,
    /// Point where the drag ends (cursor).
    pub focus: CellPos,
    /// Kind influences expansion.
    pub kind: SelectionKind,
    /// Whether the selection is active (mouse down vs idle).
    pub active: bool,
}

impl Selection {
    /// Creates a simple (range) selection.
    #[must_use]
    pub fn simple(anchor: CellPos, focus: CellPos) -> Self {
        Self {
            anchor,
            focus,
            kind: SelectionKind::Simple,
            active: true,
        }
    }

    /// Creates an empty (collapsed) selection at `pos`.
    #[must_use]
    pub fn collapsed(pos: CellPos) -> Self {
        Self {
            anchor: pos,
            focus: pos,
            kind: SelectionKind::Simple,
            active: false,
        }
    }

    /// Normalizes to an ordered inclusive range sorted by `(row, col)`.
    #[must_use]
    pub fn normalized(self) -> SelectionRange {
        SelectionRange::new(self.anchor, self.focus)
    }

    /// True when anchor == focus (no span).
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.anchor == self.focus
    }

    /// Snap both endpoints so they never address a spacer half.
    ///
    /// If `snapshot` is `None`, returns self unchanged (total over all inputs).
    /// Otherwise, any position landing on a spacer is moved left by one column
    /// to its leading char. This preserves wide-char totality deterministically.
    #[must_use]
    pub fn snapped(self, snapshot: Option<&Snapshot>) -> Self {
        let Some(snap) = snapshot else {
            return self;
        };
        let anchor = snap_to_leading(snap, self.anchor);
        let focus = snap_to_leading(snap, self.focus);
        Self {
            anchor,
            focus,
            ..self
        }
    }

    /// Snap in place.
    pub fn snap(&mut self, snapshot: &Snapshot) {
        *self = self.snapped(Some(snapshot));
    }

    /// True when `pos` (snapped to leading) lies in the normalized range.
    #[must_use]
    pub fn contains(self, pos: CellPos, snapshot: Option<&Snapshot>) -> bool {
        let norm = self.snapped(snapshot).normalized();
        let sp = snap_to_leading_opt(snapshot, pos);
        norm.contains(sp)
    }

    /// Block-mode containment: `pos.col` must lie within block column range,
    /// independent of row-major start/end column asymmetry.
    #[must_use]
    pub fn contains_block(self, pos: CellPos, snapshot: Option<&Snapshot>) -> bool {
        let a = snap_to_leading_opt(snapshot, self.anchor);
        let b = snap_to_leading_opt(snapshot, self.focus);
        let (rmin, rmax) = if a.row <= b.row {
            (a.row, b.row)
        } else {
            (b.row, a.row)
        };
        if pos.row < rmin || pos.row > rmax {
            return false;
        }
        let (cmin, cmax) = if a.col <= b.col {
            (a.col, b.col)
        } else {
            (b.col, a.col)
        };
        // Snap query to leading as well.
        let qp = snap_to_leading_opt(snapshot, pos);
        qp.col >= cmin && qp.col <= cmax
    }

    /// Clamps both endpoints to `snapshot` bounds and snaps wide positions.
    #[must_use]
    pub fn clamped(self, snapshot: &Snapshot) -> Self {
        let clamp = |p: CellPos| {
            let row = (p.row as usize).min(snapshot.height.saturating_sub(1)) as u16;
            let col = (p.col as usize).min(snapshot.width.saturating_sub(1)) as u16;
            snap_to_leading(snapshot, CellPos::new(row, col))
        };
        Self {
            anchor: clamp(self.anchor),
            focus: clamp(self.focus),
            ..self
        }
    }

    /// Expands to the word containing `pos` using `is_word_char` to classify
    /// cells. Snaps `pos` to leading and then scans left/right across the same
    /// row until a delimiter (blank, non-word, or row edge) is hit.
    ///
    /// Returns a normalized `SelectionRange` covering the word. If the cell at
    /// `pos` is a blank or word delimiter, returns a single-cell range.
    #[must_use]
    pub fn word_at(snapshot: &Snapshot, pos: CellPos) -> SelectionRange {
        let origin = snap_to_leading(snapshot, clamp_pos(snapshot, pos));
        let row = origin.row as usize;
        if row >= snapshot.height {
            return SelectionRange::new(origin, origin);
        }
        let width = snapshot.width;
        let Some(cell) = cell_at(snapshot, origin) else {
            return SelectionRange::new(origin, origin);
        };
        if cell.is_blank() || !is_word_char(cell.glyph) {
            return SelectionRange::new(origin, origin);
        }
        let (word_left, word_right) = {
            // Left scan.
            let mut l = origin.col as usize;
            loop {
                if l == 0 {
                    break;
                }
                let prev = l - 1;
                let prev_pos = CellPos::new(origin.row, prev as u16);
                let snapped_prev = snap_to_leading(snapshot, prev_pos);
                let col_to_check = snapped_prev.col as usize;
                // If prev was spacer, we snapped to its leader; check leader cell.
                let Some(c) = cell_at(snapshot, CellPos::new(origin.row, col_to_check as u16))
                else {
                    break;
                };
                if c.is_blank() || !is_word_char(c.glyph) || c.spacer {
                    break;
                }
                // Also need to ensure the next col (col_to_check+1 spacer) not double counted?
                // For wide leading, that char occupies col_to_check.
                l = col_to_check;
                if l == 0 {
                    break;
                }
                // If col_to_check was a spacer snap, we already moved to leader, but
                // there may be gap? Ensure we moved at least one; loop will then try l-1 next.
            }
            // Right scan.
            let mut r = origin.col as usize;
            loop {
                let next_col = r + 1;
                // If current `r` is wide leading, its spacer sits at r+1; that column
                // is not a separable character — skip it.
                if let Some(cur) = cell_at(snapshot, CellPos::new(origin.row, r as u16)) {
                    if cur.width == 2 && !cur.spacer && next_col < width {
                        // Jump over spacer.
                        let try_after = next_col + 1;
                        if try_after >= width {
                            break;
                        }
                        let Some(after) =
                            cell_at(snapshot, CellPos::new(origin.row, try_after as u16))
                        else {
                            break;
                        };
                        if after.is_blank() || !is_word_char(after.glyph) || after.spacer {
                            break;
                        }
                        r = try_after;
                        continue;
                    }
                }
                if next_col >= width {
                    break;
                }
                let try_pos = CellPos::new(origin.row, next_col as u16);
                let snapped = snap_to_leading(snapshot, try_pos);
                if snapped.col as usize != next_col {
                    // next_col was a spacer; snapped to its leader which is r (wide's leader)
                    // already counted. So next distinct char is at next_col+1 handled above.
                    break;
                }
                let Some(c) = cell_at(snapshot, try_pos) else {
                    break;
                };
                if c.is_blank() || !is_word_char(c.glyph) || c.spacer {
                    break;
                }
                r = next_col;
            }
            (l, r)
        };
        SelectionRange::new(
            CellPos::new(origin.row, word_left as u16),
            CellPos::new(origin.row, word_right as u16),
        )
    }

    /// Expands to the whole line at `row`, clamped to snapshot width.
    #[must_use]
    pub fn line_at(snapshot: &Snapshot, row: u16) -> SelectionRange {
        let r = (row as usize).min(snapshot.height.saturating_sub(1)) as u16;
        let w = snapshot.width as u16;
        if w == 0 {
            return SelectionRange::new(CellPos::new(r, 0), CellPos::new(r, 0));
        }
        let start = CellPos::new(r, 0);
        let mut end = CellPos::new(r, w.saturating_sub(1));
        // Snap end if it's a spacer to its leader (line still covers whole row).
        end = snap_to_leading(snapshot, end);
        // For a line covering wide trailing half at last column, consider that
        // wide char's spacer was at w-1? But then leader at w-2. Our end snapping
        // moves it to w-2 visually, but logically line covers the entire row.
        // To keep line-cover semantics, ensure start=0,w-1 normalized accounts for spacer?
        // Return range 0..w-1 (snapped) which still covers all cells inclusive.
        SelectionRange::new(start, end)
    }

    /// Produces selected text for this selection by concatenating glyphs of
    /// covered cells row-major, respecting snapped columns and skipping spacer
    /// halves. Lines are joined with `\n` when the selection spans multiple rows.
    ///
    /// Wide-char leaders emit their single glyph; spacers are never emitted.
    #[must_use]
    pub fn text(self, snapshot: &Snapshot) -> String {
        let clamped = self.clamped(snapshot);
        let snapped = clamped.snapped(Some(snapshot));
        let range = snapped.normalized();
        selected_text_for_range(snapshot, range)
    }
}

/// Returns whether `ch` is a word-character for double-click selection.
///
/// Word characters are `alphanumeric` or `_`. All other printable chars and
/// blanks are delimiters. This matches a common terminal word-class rule and
/// is deterministic without locale.
#[must_use]
pub fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Snap `pos` so it never addresses a spacer half.
///
/// If the cell at `pos` is a spacer (`spacer == true`), returns the leader
/// at `col - 1`; otherwise returns `pos` unchanged. Out-of-bounds positions
/// clamp first, then snap (so the far-right spacer snaps inward rather than
/// panicking).
#[must_use]
pub fn snap_to_leading(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return pos;
    }
    let row = (pos.row as usize).min(snapshot.height.saturating_sub(1));
    let col = (pos.col as usize).min(snapshot.width.saturating_sub(1));
    let idx = row * snapshot.width + col;
    if let Some(cell) = snapshot.cells.get(idx) {
        if cell.spacer && col > 0 {
            return CellPos::new(pos.row, (col - 1) as u16);
        }
    }
    CellPos::new(pos.row, pos.col)
}

fn snap_to_leading_opt(snapshot: Option<&Snapshot>, pos: CellPos) -> CellPos {
    if let Some(s) = snapshot {
        snap_to_leading(s, pos)
    } else {
        pos
    }
}

fn clamp_pos(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    let row = (pos.row as usize).min(snapshot.height.saturating_sub(1)) as u16;
    let col = (pos.col as usize).min(snapshot.width.saturating_sub(1)) as u16;
    CellPos::new(row, col)
}

fn cell_at(snapshot: &Snapshot, pos: CellPos) -> Option<&bitty_term_state::Cell> {
    let row = pos.row as usize;
    let col = pos.col as usize;
    if row >= snapshot.height || col >= snapshot.width {
        return None;
    }
    let idx = row * snapshot.width + col;
    snapshot.cells.get(idx)
}

fn selected_text_for_range(snapshot: &Snapshot, range: SelectionRange) -> String {
    if snapshot.width == 0 || snapshot.height == 0 {
        return String::new();
    }
    let start = range.start;
    let end = range.end;
    let mut out = String::new();
    for row in start.row..=end.row {
        let row_usize = row as usize;
        if row_usize >= snapshot.height {
            break;
        }
        let col_start = if row == start.row {
            start.col as usize
        } else {
            0
        };
        let col_end = if row == end.row {
            end.col as usize
        } else {
            snapshot.width.saturating_sub(1)
        };
        let mut col = col_start;
        while col <= col_end && col < snapshot.width {
            let idx = row_usize * snapshot.width + col;
            if let Some(cell) = snapshot.cells.get(idx) {
                if cell.spacer {
                    col += 1;
                    continue;
                }
                if cell.is_blank() {
                    out.push(' ');
                } else {
                    out.push(cell.glyph);
                }
                if cell.width == 2 {
                    col += 2;
                    continue;
                }
            }
            col += 1;
        }
        if row != end.row {
            out.push('\n');
        }
    }
    out
}

/// Buffer-anchored position for persistent selection.
///
/// `buffer_row` is a zero-based index into the combined scrollback + live grid
/// buffer: `0` is the oldest retained scrollback line, `sb_len-1` the newest
/// scrollback line, `sb_len` the live grid's top row (row 0), and
/// `sb_len + height - 1` the live grid's bottom row. This anchoring survives
/// scroll (lines move from grid into scrollback with the same `buffer_row`),
/// and survives `View` scroll offset changes (viewport rows are a window into
/// the buffer). Headless and deterministic: no I/O, no wall-clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferPos {
    /// Combined buffer row index.
    pub buffer_row: usize,
    /// Cell column (lead column for wide chars).
    pub col: u16,
}

impl BufferPos {
    /// Creates a buffer position.
    #[must_use]
    pub const fn new(buffer_row: usize, col: u16) -> Self {
        Self { buffer_row, col }
    }
}

/// Buffer-anchored selection that persists across scroll, scrollback pruning
/// (when not pruned), and resize (clamped). Conversion to/from the live-grid
/// `Selection` is explicit via `State` (and optionally `View`).
///
/// This is the selection persistence primitive for CTX-0060: a live-grid
/// `Selection` can be lifted to `PersistentSelection` via
/// [`PersistentSelection::from_grid_selection`] (anchored to the combined
/// buffer), survive state mutations, and be resolved back to a live-grid
/// `Selection` when its buffer rows still map into the current live grid
/// window. When a buffer row has been pruned (scrollback capacity) or moved
/// entirely into history, `to_grid_selection` returns `None` and `is_valid`
/// is `false`; when it survives, `text` extracts the same glyphs deterministically.
///
/// Bounded and headless: the stored rows are `usize` and no heap grows beyond
/// the selection itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PersistentSelection {
    /// Anchor in buffer coordinates.
    pub anchor: BufferPos,
    /// Optional stable line id for the anchor when it was in scrollback
    /// (None for live-grid anchors). Used to detect pruning.
    pub anchor_line_id: Option<u64>,
    /// Focus in buffer coordinates.
    pub focus: BufferPos,
    /// Optional stable line id for the focus when it was in scrollback.
    pub focus_line_id: Option<u64>,
    /// Kind influences word/line expansion semantics when re-resolved.
    pub kind: SelectionKind,
    /// Whether the drag is still active.
    pub active: bool,
}

impl PersistentSelection {
    /// Lifts a live-grid `Selection` (snapshot coordinates) to a buffer-anchored
    /// persistent selection using the current `State`.
    ///
    /// `Selection` rows `0..height-1` map to buffer rows `sb_len + row`. Column
    /// is preserved (snapped later on resolution). `line_id` is always `None`
    /// for live-grid selections; scrollback-anchored selections created via
    /// `from_view_selection` may carry ids.
    #[must_use]
    pub fn from_grid_selection(sel: Selection, state: &bitty_term_state::State) -> Self {
        let sb_len = state.scrollback_len();
        let map = |pos: CellPos| {
            let buffer_row = sb_len + pos.row as usize;
            BufferPos::new(buffer_row, pos.col)
        };
        Self {
            anchor: map(sel.anchor),
            anchor_line_id: None,
            focus: map(sel.focus),
            focus_line_id: None,
            kind: sel.kind,
            active: sel.active,
        }
    }

    /// Lifts a viewport `Selection` (where `sel` rows are viewport rows `0..rows-1`)
    /// to a buffer-anchored selection using `View` scroll offset and `State`.
    ///
    /// When the view is live (`scroll_offset == 0`) the viewport shows the bottom
    /// `rows` of the combined buffer; when scrolled, it shows an earlier window.
    /// This method preserves the logical buffer row so the selection persists
    /// across `View::scroll_by` and state growth.
    #[must_use]
    pub fn from_view_selection(
        sel: Selection,
        view: &crate::view::View,
        state: &bitty_term_state::State,
    ) -> Self {
        let sb_len = state.scrollback_len();
        let total = sb_len + state.height();
        let rows = view.rows() as usize;
        let offset = view.scroll_offset().min(sb_len);
        let start = total.saturating_sub(rows).saturating_sub(offset);
        let map = |pos: CellPos| {
            let viewport_row = pos.row as usize;
            let buffer_row = start + viewport_row;
            // Resolve line id if buffer row is in scrollback.
            let line_id = if buffer_row < sb_len {
                state.scrollback_line(buffer_row).map(|l| l.id)
            } else {
                None
            };
            (BufferPos::new(buffer_row, pos.col), line_id)
        };
        let (anchor, anchor_line_id) = map(sel.anchor);
        let (focus, focus_line_id) = map(sel.focus);
        Self {
            anchor,
            anchor_line_id,
            focus,
            focus_line_id,
            kind: sel.kind,
            active: sel.active,
        }
    }

    /// Attempts to resolve back to a live-grid `Selection`.
    ///
    /// Returns `Some` when both endpoints still map into the current live grid
    /// window (`sb_len .. sb_len+height-1`) and survive pruning checks;
    /// `None` when either endpoint has been pruned or now lives in scrollback
    /// history (use `text` to read buffer content even when not live).
    #[must_use]
    pub fn to_grid_selection(&self, state: &bitty_term_state::State) -> Option<Selection> {
        if !self.is_valid(state) {
            return None;
        }
        let sb_len = state.scrollback_len();
        let height = state.height();
        let total = sb_len + height;
        // Both endpoints must be within live window.
        let in_live = |bp: BufferPos| bp.buffer_row >= sb_len && bp.buffer_row < total;
        if !in_live(self.anchor) || !in_live(self.focus) {
            return None;
        }
        let to_cell = |bp: BufferPos| {
            let row = (bp.buffer_row - sb_len) as u16;
            let col = bp.col.min(state.width().saturating_sub(1) as u16);
            CellPos::new(row, col)
        };
        let mut sel = Selection {
            anchor: to_cell(self.anchor),
            focus: to_cell(self.focus),
            kind: self.kind,
            active: self.active,
        };
        // Snap wide positions and clamp to current snapshot bounds.
        let snap = state.snapshot();
        sel = sel.clamped(&snap).snapped(Some(&snap));
        Some(sel)
    }

    /// Attempts to resolve to a viewport `Selection` (rows `0..view.rows-1`).
    ///
    /// Returns `Some` when the buffer rows are currently visible in the view's
    /// viewport window; `None` when outside the window or pruned.
    #[must_use]
    pub fn to_view_selection(
        &self,
        view: &crate::view::View,
        state: &bitty_term_state::State,
    ) -> Option<Selection> {
        if !self.is_valid(state) {
            return None;
        }
        let sb_len = state.scrollback_len();
        let total = sb_len + state.height();
        let rows = view.rows() as usize;
        let offset = view.scroll_offset().min(sb_len);
        let start = total.saturating_sub(rows).saturating_sub(offset);
        let end = start + rows; // exclusive
        let in_view = |bp: BufferPos| bp.buffer_row >= start && bp.buffer_row < end;
        if !in_view(self.anchor) || !in_view(self.focus) {
            return None;
        }
        let to_cell = |bp: BufferPos| {
            let row = (bp.buffer_row - start) as u16;
            let col = bp.col;
            CellPos::new(row, col)
        };
        let anchor = snap_to_leading(&state.snapshot(), to_cell(self.anchor));
        let focus = snap_to_leading(&state.snapshot(), to_cell(self.focus));
        // Clamp viewport rows/cols to view size.
        let max_row = view.rows().saturating_sub(1);
        let max_col = view.cols().saturating_sub(1);
        let clamp_vp = |p: CellPos| CellPos::new(p.row.min(max_row), p.col.min(max_col));
        Some(Selection {
            anchor: clamp_vp(anchor),
            focus: clamp_vp(focus),
            kind: self.kind,
            active: self.active,
        })
    }

    /// Whether the anchored buffer rows still exist.
    ///
    /// For scrollback-anchored endpoints (`line_id.is_some()`) the method checks
    /// that the buffered row still holds the same logical line id. This detects
    /// prune drift where the buffer row now points to a different line after
    /// oldest-first eviction, and also detects pruned ids. For live endpoints
    /// (`line_id.is_none()`) it checks only combined buffer bounds; column is
    /// not validated here because it is clamped on resolution. A grid-anchored
    /// selection that has scrolled into history (`buffer_row < sb_len` without
    /// an id) remains valid for buffer text extraction (live-grid resolve will
    /// reject it as not live).
    #[must_use]
    pub fn is_valid(&self, state: &bitty_term_state::State) -> bool {
        let sb_len = state.scrollback_len();
        let total = sb_len + state.height();
        let check = |bp: BufferPos, line_id: Option<u64>| {
            if bp.buffer_row >= total {
                return false;
            }
            if let Some(id) = line_id {
                if bp.buffer_row >= sb_len {
                    return false;
                }
                match state.scrollback_line(bp.buffer_row) {
                    Some(line) if line.id == id => {}
                    _ => return false,
                }
            }
            true
        };
        check(self.anchor, self.anchor_line_id) && check(self.focus, self.focus_line_id)
    }

    /// Extracts the selected text from the combined buffer (scrollback + grid).
    ///
    /// This is the buffer-level counterpart to `Selection::text(&Snapshot)`: it
    /// concatenates glyphs between the two buffer positions row-major, skipping
    /// spacers and emitting `' '` for blanks, joining rows with `\n`. Returns
    /// `None` when the selection is not valid (pruned or drifted).
    ///
    /// For scrollback-anchored endpoints the buffer row is validated against the
    /// stored line id and located by id search; if the stored row no longer
    /// holds the same id (prune shift) the selection is considered pruned.
    #[must_use]
    pub fn text(&self, state: &bitty_term_state::State) -> Option<String> {
        if !self.is_valid(state) {
            return None;
        }
        // Defensive id-located resolve for scrollback endpoints: is_valid already
        // guarantees scrollback_line(buffer_row).id == stored id, but double-check
        // here and locate by id to guard against stale buffer_row after pruning.
        let resolve = |bp: BufferPos, line_id: Option<u64>| -> Option<BufferPos> {
            if let Some(id) = line_id {
                let line = state.scrollback_line(bp.buffer_row)?;
                if line.id != id {
                    return None;
                }
                // Also verify via id search that the line is still at the stored row
                // (handles prune shift where id moved to a different index).
                let mut found_idx = None;
                for (idx, l) in state.scrollback().enumerate() {
                    if l.id == id {
                        found_idx = Some(idx);
                        break;
                    }
                }
                match found_idx {
                    Some(idx) if idx == bp.buffer_row => Some(bp),
                    _ => None,
                }
            } else {
                Some(bp)
            }
        };
        let eff_anchor = resolve(self.anchor, self.anchor_line_id)?;
        let eff_focus = resolve(self.focus, self.focus_line_id)?;
        // Order endpoints
        let (start_bp, end_bp) = if eff_anchor.buffer_row < eff_focus.buffer_row
            || (eff_anchor.buffer_row == eff_focus.buffer_row && eff_anchor.col <= eff_focus.col)
        {
            (eff_anchor, eff_focus)
        } else {
            (eff_focus, eff_anchor)
        };
        let sb_len = state.scrollback_len();
        let mut out = String::new();
        for buffer_row in start_bp.buffer_row..=end_bp.buffer_row {
            let (cells, width) = if buffer_row < sb_len {
                if let Some(line) = state.scrollback_line(buffer_row) {
                    (line.cells.clone(), line.cells.len())
                } else {
                    continue;
                }
            } else {
                let snap = state.snapshot();
                let grid_row = buffer_row - sb_len;
                if grid_row >= snap.height {
                    continue;
                }
                let start = grid_row * snap.width;
                let end = start + snap.width;
                if end > snap.cells.len() {
                    continue;
                }
                let slice = snap.cells[start..end].to_vec();
                (slice.into_boxed_slice(), snap.width)
            };
            let col_start = if buffer_row == start_bp.buffer_row {
                start_bp.col as usize
            } else {
                0
            };
            let col_end = if buffer_row == end_bp.buffer_row {
                end_bp.col as usize
            } else {
                width.saturating_sub(1)
            };
            let mut col = col_start;
            while col <= col_end && col < width {
                if let Some(cell) = cells.get(col) {
                    if cell.spacer {
                        col += 1;
                        continue;
                    }
                    if cell.is_blank() {
                        out.push(' ');
                    } else {
                        out.push(cell.glyph);
                    }
                    if cell.width == 2 {
                        col += 2;
                        continue;
                    }
                }
                col += 1;
            }
            if buffer_row != end_bp.buffer_row {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// Clamps columns to the current state's width (preserving `buffer_row`).
    ///
    /// Used after `State::resize` to keep a persistent selection within the new
    /// geometry without changing its logical buffer row. Column is clamped to
    /// `width - 1`; wide-char snapping is applied on resolution
    /// (`to_grid_selection` / `to_view_selection` / `text`) rather than here,
    /// keeping this operation deterministic and independent of line content.
    #[must_use]
    pub fn clamped(&self, state: &bitty_term_state::State) -> Self {
        let width = state.width() as u16;
        let clamp_col = |c: u16| {
            if width == 0 {
                0
            } else {
                c.min(width.saturating_sub(1))
            }
        };
        Self {
            anchor: BufferPos::new(self.anchor.buffer_row, clamp_col(self.anchor.col)),
            anchor_line_id: self.anchor_line_id,
            focus: BufferPos::new(self.focus.buffer_row, clamp_col(self.focus.col)),
            focus_line_id: self.focus_line_id,
            kind: self.kind,
            active: self.active,
        }
    }

    /// Clears the selection after a full reset.
    ///
    /// FullReset clears scrollback and erases the grid. Any persistent selection
    /// is therefore no longer anchored to valid content. This helper always
    /// returns `None` to signal that the caller should drop the persistent
    /// selection. It is a headless, deterministic convenience for callers that
    /// observe `TerminalAction::FullReset`; the state parameter is retained
    /// for API symmetry and future use.
    #[must_use]
    pub fn after_full_reset(self, _state: &bitty_term_state::State) -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{Cell, Style};

    fn make_snapshot(cells_grid: Vec<Vec<char>>) -> Snapshot {
        // cells_grid is rows x cols with ' ' blank, '中' wide char via two entries: '中' at lead, '\0' spacer marker.
        let height = cells_grid.len();
        let width = cells_grid[0].len();
        let mut cells = Vec::with_capacity(width * height);
        for row in cells_grid {
            for ch in row {
                if ch == '\0' {
                    // spacer marker
                    cells.push(Cell::wide_spacer(Style::default()));
                } else {
                    let is_wide = ch == '中' || ch == '好';
                    if is_wide {
                        cells.push(Cell {
                            glyph: ch,
                            style: Style::default(),
                            width: 2,
                            spacer: false,
                            hyperlink: None,
                        });
                    } else {
                        cells.push(Cell {
                            glyph: ch,
                            style: Style::default(),
                            width: 1,
                            spacer: false,
                            hyperlink: None,
                        });
                    }
                }
            }
        }
        // Reuse a real snapshot as template to avoid constructing BoundedString directly.
        let mut template = bitty_term_state::State::new().snapshot();
        template.width = width;
        template.height = height;
        template.cells = cells.into_boxed_slice();
        template
    }

    #[test]
    fn snap_to_leading_handles_spacer() {
        // Row: 'A' '中' spacer 'B'
        let snap = make_snapshot(vec![vec!['A', '中', '\0', 'B']]);
        let pos_spacer = CellPos::new(0, 2);
        let snapped = snap_to_leading(&snap, pos_spacer);
        assert_eq!(snapped, CellPos::new(0, 1));
        let pos_leading = CellPos::new(0, 1);
        assert_eq!(snap_to_leading(&snap, pos_leading), pos_leading);
    }

    #[test]
    fn normalized_orders_endpoints() {
        let a = CellPos::new(2, 5);
        let b = CellPos::new(1, 10);
        let s = Selection::simple(a, b);
        let n = s.normalized();
        assert_eq!(n.start, b);
        assert_eq!(n.end, a);
    }

    #[test]
    fn contains_row_major() {
        let s = Selection::simple(CellPos::new(0, 2), CellPos::new(1, 3));
        // Range spans row 0 col2..end plus row1 0..3
        assert!(s.contains(CellPos::new(0, 2), None));
        assert!(s.contains(CellPos::new(0, 5), None));
        assert!(s.contains(CellPos::new(1, 0), None));
        assert!(s.contains(CellPos::new(1, 3), None));
        assert!(!s.contains(CellPos::new(1, 4), None));
        assert!(!s.contains(CellPos::new(0, 1), None));
    }

    #[test]
    fn block_contains_is_rectangular() {
        let s = Selection::simple(CellPos::new(0, 1), CellPos::new(2, 3));
        assert!(s.contains_block(CellPos::new(1, 2), None));
        assert!(!s.contains_block(CellPos::new(1, 0), None));
        assert!(!s.contains_block(CellPos::new(3, 2), None));
    }

    #[test]
    fn word_at_expands_to_word() {
        // Row: "hello world" -> cells 'h','e','l','l','o',' ','w','o','r','l','d'
        let snap = make_snapshot(vec![vec![
            'h', 'e', 'l', 'l', 'o', ' ', 'w', 'o', 'r', 'l', 'd',
        ]]);
        let pos = CellPos::new(0, 7); // inside "world"
        let r = Selection::word_at(&snap, pos);
        assert_eq!(r.start, CellPos::new(0, 6));
        assert_eq!(r.end, CellPos::new(0, 10));
    }

    #[test]
    fn word_at_on_blank_returns_single_cell() {
        let snap = make_snapshot(vec![vec!['a', ' ', 'b']]);
        let r = Selection::word_at(&snap, CellPos::new(0, 1));
        assert_eq!(r.start.col, 1);
        assert_eq!(r.end.col, 1);
    }

    #[test]
    fn word_at_with_wide_char() {
        // Row: "a中b c" with wide 中 occupying cols 1-2
        let snap = make_snapshot(vec![vec!['a', '中', '\0', 'b', ' ', 'c']]);
        // Click on spacer (col 2) should snap to 中 at col 1 and expand to word "a中b" ?
        // '中' is alphanumeric? char::is_alphanumeric for CJK is true, so considered word char.
        let r = Selection::word_at(&snap, CellPos::new(0, 2));
        // Word should expand across a, 中, b (cols 0-3) but skipping spacer: leaders at 0,1,3
        // So left 0, right 3
        assert_eq!(r.start, CellPos::new(0, 0));
        assert_eq!(r.end, CellPos::new(0, 3));
    }

    #[test]
    fn line_at_covers_whole_row() {
        let snap = make_snapshot(vec![vec!['a', 'b', 'c'], vec!['d', 'e', 'f']]);
        let r = Selection::line_at(&snap, 1);
        assert_eq!(r.start, CellPos::new(1, 0));
        assert_eq!(r.end, CellPos::new(1, 2));
    }

    #[test]
    fn selection_text_across_rows() {
        let snap = make_snapshot(vec![vec!['a', 'b', 'c'], vec!['d', 'e', 'f']]);
        let sel = Selection::simple(CellPos::new(0, 1), CellPos::new(1, 1));
        assert_eq!(sel.text(&snap), "bc\nde");
    }

    #[test]
    fn selection_text_with_wide_skip_spacer() {
        let snap = make_snapshot(vec![vec!['A', '中', '\0', 'B']]);
        let sel = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 3));
        assert_eq!(sel.text(&snap), "A中B");
        // Also selection starting at spacer should snap and not duplicate.
        let sel2 = Selection::simple(CellPos::new(0, 2), CellPos::new(0, 2));
        assert_eq!(sel2.text(&snap), "中");
    }

    #[test]
    fn clamped_snaps_and_clamps() {
        let snap = make_snapshot(vec![vec!['a', 'b'], vec!['c', 'd']]);
        let sel = Selection::simple(CellPos::new(5, 5), CellPos::new(0, 0));
        let c = sel.clamped(&snap);
        assert_eq!(c.anchor.row, 1);
        assert_eq!(c.anchor.col, 1);
    }
}
