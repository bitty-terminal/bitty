//! The visible character grid and its line-oriented mutations.
//!
//! The grid is the primary Terminal Truth surface (RFC "Pipeline overview"):
//! a row-major, always-total matrix of [`Cell`] values. All mutating helpers
//! preserve cell totality: wide-character pairs are written, moved, and
//! erased atomically so no orphan spacer can exist after any operation
//! (RFC invariant 2). Scroll-region line motion lives here; lines leaving
//! the grid are returned to the caller so scrollback capture stays a
//! terminal-state policy decision (RFC invariant 4).

use crate::cell::{Cell, Style};

/// A dense `rows x cols` matrix of cells.
#[derive(Debug, Clone)]
pub(crate) struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Cell>,
}

impl Grid {
    /// A fully erased grid with the given dimensions.
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            cells: vec![Cell::erased(crate::cell::Style::default()); rows * cols],
        }
    }

    #[inline]
    pub fn dims(&self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn set(&mut self, row: usize, col: usize, cell: Cell) {
        self.cells[row * self.cols + col] = cell;
    }

    #[inline]
    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    /// Iterates rows top to bottom as slices.
    pub fn rows_iter(&self) -> impl Iterator<Item = &[Cell]> {
        self.cells.chunks_exact(self.cols)
    }

    /// Iterates every cell in row-major order.
    pub fn all_cells(&self) -> impl Iterator<Item = &Cell> {
        self.cells.iter()
    }

    /// A row-major copy of the full grid (snapshot payload).
    pub fn flatten_cells(&self) -> Box<[Cell]> {
        self.cells.clone().into_boxed_slice()
    }

    /// Replaces an entire row; the caller supplies exactly `cols` cells.
    pub fn replace_row(&mut self, row: usize, cells: &mut [Cell]) {
        debug_assert_eq!(cells.len(), self.cols);
        let start = row * self.cols;
        self.cells[start..start + self.cols].swap_with_slice(&mut cells[..]);
    }

    /// Takes a copy of the row's cells (used to push lines into scrollback).
    pub fn snapshot_row(&self, row: usize) -> Vec<Cell> {
        self.row(row).to_vec()
    }

    /// Fills every cell of the grid with `fill`.
    pub fn fill_all(&mut self, erase_style: &Style) {
        for slot in &mut self.cells {
            *slot = Cell::erased(erase_style.clone());
        }
    }

    /// Fills the inclusive rectangle with `fill`. Bounds are caller-clamped.
    pub fn fill_rect(&mut self, top: u16, left: u16, bottom: u16, right: u16, erase_style: &Style) {
        let (rows, cols) = (self.rows as u16, self.cols as u16);
        let top = top.min(rows.saturating_sub(1));
        let bottom = bottom.min(rows.saturating_sub(1));
        let left = left.min(cols.saturating_sub(1));
        let right = right.min(cols.saturating_sub(1));
        for r in top..=bottom {
            for c in left..=right {
                self.set(r as usize, c as usize, Cell::erased(erase_style.clone()));
            }
        }
    }

    /// Erases columns `start..=end` of one row, expanding the range
    /// outward when its edges would split a wide-character pair. The pair
    /// straddling either edge is removed whole so no orphan spacer is
    /// created (RFC invariant 2).
    pub fn erase_range_in_row(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        erase_style: &Style,
    ) {
        let last = self.cols - 1;
        let mut start = start.min(last);
        let mut end = end.min(last);
        if start > end {
            return;
        }
        if start > 0 && self.get(row, start).spacer {
            start -= 1;
        } else if self.get(row, end).width == 2 && end < last {
            // The range ends inside a wide pair only if it stops on a
            // leading half whose spacer sits at `end + 1`.
            if self.get(row, end + 1).spacer {
                end += 1;
            }
        }
        for c in start..=end {
            self.set(row, c, Cell::erased(erase_style.clone()));
        }
    }

    /// Shifts cells in one row right by `n` starting at `col`, blanking the
    /// vacated positions (`ICH`, insert-mode printing).
    ///
    /// Wide pairs are pre-cleared wherever the shift would tear them: the
    /// stay/move cut at `col` and both cuts of the moved span.
    pub fn insert_blanks_in_row(&mut self, row: usize, col: usize, n: usize, erase_style: &Style) {
        let last = self.cols - 1;
        let col = col.min(last);
        let n = n.max(1);
        // Cut between staying cells and movers: a spacer at `col` moves
        // while its lead at `col - 1` stays; blank both.
        if col > 0 && self.get(row, col).spacer {
            self.set(row, col - 1, Cell::erased(erase_style.clone()));
            self.set(row, col, Cell::erased(erase_style.clone()));
        }
        // Movers occupy sources `[col..=last - n]`. A lead at `last - n`
        // would move away from its overwritten spacer; a spacer at
        // `col + n` would move away from its moving lead. Blank both pairs
        // before the shift.
        if n <= last - col {
            let move_end = last - n;
            if self.get(row, move_end).width == 2 && !self.get(row, move_end).spacer {
                self.set(row, move_end, Cell::erased(erase_style.clone()));
                self.set(row, move_end + 1, Cell::erased(erase_style.clone()));
            }
            let move_start = col + n;
            if move_start <= last && self.get(row, move_start).spacer {
                self.set(row, move_start - 1, Cell::erased(erase_style.clone()));
            }
        }
        for target in (col + n..=last).rev() {
            let source = target - n;
            let moved = self.get(row, source).clone();
            self.set(row, target, moved);
        }
        for c in col..(col + n).min(last + 1) {
            self.set(row, c, Cell::erased(erase_style.clone()));
        }
    }

    /// Deletes `n` cells at `col` shifting the tail left (`DCH`), blanking
    /// the vacated tail.
    ///
    /// Boundary pairs are pre-cleared: the stay/destruction cut at `col`
    /// and the destruction/move cut at `col + n`.
    pub fn delete_chars_in_row(&mut self, row: usize, col: usize, n: usize, erase_style: &Style) {
        let last = self.cols - 1;
        let col = col.min(last);
        let n = n.max(1);
        // Cut between staying cells and destroyed cells: a spacer at `col`
        // is destroyed while its lead at `col - 1` stays; blank both.
        if col > 0 && self.get(row, col).spacer {
            self.set(row, col - 1, Cell::erased(erase_style.clone()));
            self.set(row, col, Cell::erased(erase_style.clone()));
        }
        // Cut between destroyed cells and the first mover at `col + n`: a
        // spacer there moves without its lead (which is overwritten by the
        // shift); blank both.
        let source_start = col + n;
        if source_start <= last && self.get(row, source_start).spacer {
            self.set(row, source_start, Cell::erased(erase_style.clone()));
            if source_start > 0 {
                self.set(row, source_start - 1, Cell::erased(erase_style.clone()));
            }
        }
        for target in col..=last {
            let source = target + n;
            let cell = if source <= last {
                self.get(row, source).clone()
            } else {
                Cell::erased(erase_style.clone())
            };
            self.set(row, target, cell);
        }
    }

    /// Deterministic orphan-repair pass over one row — the mechanical
    /// backstop behind RFC invariant 2 ("no orphan spacers exist").
    ///
    /// One left-to-right scan demotes any wide half whose partner is
    /// missing to an erased cell. Repairs only ever narrow cells, so a
    /// single pass reaches a fixed point. Returns whether anything changed,
    /// letting callers widen their damage marks.
    pub fn repair_row(&mut self, row: usize, erase_style: &Style) -> bool {
        let mut changed = false;
        let mut i = 0;
        while i < self.cols {
            let cell = self.get(row, i).clone();
            if cell.spacer {
                let paired = i > 0 && {
                    let lead = self.get(row, i - 1);
                    lead.width == 2 && !lead.spacer
                };
                if !paired {
                    self.set(row, i, Cell::erased(erase_style.clone()));
                    changed = true;
                }
            } else if cell.width == 2 {
                let paired_trailer = i + 1 < self.cols && self.get(row, i + 1).spacer;
                if !paired_trailer {
                    self.set(row, i, Cell::erased(erase_style.clone()));
                    changed = true;
                } else {
                    i += 1;
                }
            }
            i += 1;
        }
        changed
    }

    /// Removes and returns rows `top..=top + count - 1`, shifting the rows
    /// above `bottom` down into their place and blanking the freed bottom
    /// rows. This is "scroll region up"; returned rows may be captured into
    /// scrollback by the caller.
    pub fn remove_lines_up(
        &mut self,
        top: usize,
        bottom: usize,
        count: usize,
        erase_style: &Style,
    ) -> Vec<Vec<Cell>> {
        let span = bottom - top + 1;
        let count = count.min(span);
        let mut removed = Vec::with_capacity(count);
        for _ in 0..count {
            removed.push(self.snapshot_row(top));
            for r in top..bottom {
                let mut carried: Vec<Cell> = self.row(r + 1).to_vec();
                self.replace_row(r, &mut carried);
            }
            let mut blank = vec![Cell::erased(erase_style.clone()); self.cols];
            self.replace_row(bottom, &mut blank);
        }
        removed
    }

    /// Inserts `count` blank lines at `top` pushing existing rows within
    /// `top..=bottom` down; displaced rows are discarded (they never enter
    /// scrollback: RFC invariant 4 restricts capture to scroll-under-region).
    pub fn insert_blank_lines_down(
        &mut self,
        top: usize,
        bottom: usize,
        count: usize,
        erase_style: &Style,
    ) {
        let span = bottom - top + 1;
        let count = count.min(span);
        for _ in 0..count {
            for r in (top..bottom).rev() {
                let mut carried: Vec<Cell> = self.row(r).to_vec();
                self.replace_row(r + 1, &mut carried);
            }
            let mut blank = vec![Cell::erased(erase_style.clone()); self.cols];
            self.replace_row(top, &mut blank);
        }
    }

    /// Resizes the grid to `new_rows x new_cols`, preserving overlapping cells
    /// and repairing wide-character pairs that would otherwise straddle the new
    /// boundary (RFC invariant 2). New area is filled with `erase_style`.
    /// This is the singular reflow primitive for resize: truncate/pad with
    /// orphan repair, deterministically identical on all platforms.
    pub(crate) fn resize(&mut self, new_rows: usize, new_cols: usize, erase_style: &Style) {
        let new_rows = new_rows.max(1);
        let new_cols = new_cols.max(1);
        if new_rows == self.rows && new_cols == self.cols {
            return;
        }
        let mut new_cells = vec![Cell::erased(erase_style.clone()); new_rows * new_cols];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                let src = self.get(r, c).clone();
                new_cells[r * new_cols + c] = src;
            }
        }
        // Repair every copied row for possible orphaned wide halves at the
        // truncation boundary (new_cols may cut a pair).
        for r in 0..copy_rows {
            let start = r * new_cols;
            let row_slice = &mut new_cells[start..start + new_cols];
            let mut i = 0;
            while i < new_cols {
                let cell = row_slice[i].clone();
                if cell.spacer {
                    let paired = i > 0 && {
                        let lead = &row_slice[i - 1];
                        lead.width == 2 && !lead.spacer
                    };
                    if !paired {
                        row_slice[i] = Cell::erased(erase_style.clone());
                    }
                } else if cell.width == 2 {
                    let paired_trailer = i + 1 < new_cols && row_slice[i + 1].spacer;
                    if !paired_trailer {
                        row_slice[i] = Cell::erased(erase_style.clone());
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
        }
        self.rows = new_rows;
        self.cols = new_cols;
        self.cells = new_cells;
    }
}

/// Primary and alternate screen grids; see [`crate::state::State`] for the
/// switching and save/restore policy (RFC invariant 5).
#[derive(Debug, Clone)]
pub(crate) struct ScreenPair {
    pub main: Grid,
    pub alt: Grid,
}

impl ScreenPair {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            main: Grid::new(rows, cols),
            alt: Grid::new(rows, cols),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Cell, Style};

    fn glyph(ch: char) -> Cell {
        Cell {
            glyph: ch,
            style: Style::default(),
            width: 1,
            spacer: false,
            hyperlink: None,
        }
    }

    #[test]
    fn new_grid_is_total_and_erased() {
        let g = Grid::new(3, 5);
        assert_eq!(g.dims(), (3, 5));
        assert!(g.row(2).iter().all(Cell::is_blank));
    }

    #[test]
    fn erase_range_expands_over_wide_pairs() {
        let mut g = Grid::new(1, 6);
        // Row: A 中 B (wide occupies cols 1-2).
        g.set(0, 0, glyph('A'));
        g.set(0, 1, glyph('中'));
        g.set(0, 2, Cell::wide_spacer(Style::default()));
        g.set(0, 3, glyph('B'));
        g.erase_range_in_row(0, 2, 2, &Style::default());
        // The range hit the spacer at col 2; expansion removes the leading
        // half at col 1 too.
        assert!(g.get(0, 1).is_blank());
        assert!(g.get(0, 2).is_blank());
        assert!(!g.get(0, 3).is_blank());
    }

    #[test]
    fn erase_range_extends_into_trailing_half() {
        let mut g = Grid::new(1, 6);
        let style = Style::default();
        g.set(
            0,
            1,
            Cell {
                glyph: '中',
                style: style.clone(),
                width: 2,
                spacer: false,
                hyperlink: None,
            },
        );
        g.set(0, 2, Cell::wide_spacer(style));
        g.erase_range_in_row(0, 1, 1, &Style::default());
        assert!(g.get(0, 1).is_blank());
        assert!(g.get(0, 2).is_blank());
    }

    #[test]
    fn insert_and_delete_keep_row_length() {
        let mut g = Grid::new(1, 4);
        g.set(0, 0, glyph('a'));
        g.set(0, 1, glyph('b'));
        g.set(0, 2, glyph('c'));
        g.insert_blanks_in_row(0, 1, 1, &Style::default());
        assert_eq!(g.row(0).len(), 4);
        assert_eq!(g.get(0, 0).glyph, 'a');
        assert!(g.get(0, 1).is_blank());
        assert_eq!(g.get(0, 2).glyph, 'b');

        g.delete_chars_in_row(0, 0, 1, &Style::default());
        assert!(g.get(0, 0).is_blank());
        assert_eq!(g.get(0, 1).glyph, 'b');
        assert_eq!(g.get(0, 2).glyph, 'c');
        assert!(g.get(0, 3).is_blank());
    }

    #[test]
    fn remove_lines_up_returns_removed_rows() {
        let mut g = Grid::new(3, 2);
        g.set(0, 0, glyph('x'));
        let removed = g.remove_lines_up(0, 2, 1, &Style::default());
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0][0].glyph, 'x');
        assert!(g.get(0, 0).is_blank());
        assert!(g.row(2).iter().all(Cell::is_blank));
    }

    #[test]
    fn insert_blank_lines_down_discards_displaced_rows() {
        let mut g = Grid::new(3, 2);
        g.set(2, 0, glyph('y'));
        g.insert_blank_lines_down(0, 2, 1, &Style::default());
        assert!(g.row(0).iter().all(Cell::is_blank));
        assert!(g.row(1).iter().all(Cell::is_blank));
        // Bottom row was displaced and discarded, not preserved.
        assert!(g.row(2).iter().all(Cell::is_blank));
    }
}
