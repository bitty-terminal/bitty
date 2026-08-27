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
