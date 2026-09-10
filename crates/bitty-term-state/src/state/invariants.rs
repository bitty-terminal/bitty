//! Invariant contract checks for [`State`].

use crate::modes::AltScreen;

use super::State;

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
    /// Invariant 2: a cell's combining buffer exceeds its hard cap.
    ZerowidthOverCapacity {
        /// Cell row.
        row: u16,
        /// Cell column.
        col: u16,
        /// Retained combining scalars.
        len: usize,
        /// The cap.
        cap: usize,
    },
    /// Invariant 2: a wide-character spacer carries combining marks, which
    /// belong to the leading half only.
    SpacerWithZerowidth {
        /// Spacer row.
        row: u16,
        /// Spacer column.
        col: u16,
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

impl State {
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
            debug_assert_eq!(
                grid.wraps_slice().len(),
                rows,
                "wrap flags must track row count"
            );
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
                    if cell.zerowidth.len() > crate::cell::MAX_ZEROWIDTH_CHARS {
                        return Err(InvariantViolation::ZerowidthOverCapacity {
                            row: r as u16,
                            col: c as u16,
                            len: cell.zerowidth.len(),
                            cap: crate::cell::MAX_ZEROWIDTH_CHARS,
                        });
                    }
                    if cell.spacer && !cell.zerowidth.is_empty() {
                        return Err(InvariantViolation::SpacerWithZerowidth {
                            row: r as u16,
                            col: c as u16,
                        });
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
        let scrollback_cap = self.scrollback.max_lines();
        if self.scrollback.len() > scrollback_cap {
            return Err(InvariantViolation::ScrollbackOverCapacity {
                len: self.scrollback.len(),
                cap: scrollback_cap,
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
}
