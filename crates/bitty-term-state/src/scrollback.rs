//! Bounded, append-only scrollback (RFC invariant 4).
//!
//! Lines enter scrollback only via scroll-under-region operations; pruning
//! removes oldest first; contents are immutable once written. The single
//! exception is wholesale removal by `ED 3` / `FullReset`, which truncates
//! the buffer without rewriting any line. Capacity is the config-free
//! constant [`SCROLLBACK_MAX_LINES`]; see the crate documentation for the
//! constant register and RFC references.

use std::collections::VecDeque;

use crate::cell::Cell;

/// Hard cap on retained scrollback lines (RFC invariant 4 "pruning removes
/// oldest first"; bounded memory per threat T-01).
pub const SCROLLBACK_MAX_LINES: usize = 10_000;

/// One immutable scrollback line with its monotonically assigned id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackLine {
    /// Monotonic id assigned when the line entered scrollback; ids never
    /// repeat within a state's lifetime.
    pub id: u64,
    /// Cell content, exactly `width` cells wide.
    pub cells: Box<[Cell]>,
}

/// The bounded scrollback buffer.
#[derive(Debug, Clone)]
pub struct Scrollback {
    lines: VecDeque<ScrollbackLine>,
    next_id: u64,
    total_written: u64,
}

/// Result of a buffer-clearing operation: the removed id range
/// `[first_removed, first_removed + removed_count)` may be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClearedRange {
    pub first_line_id: u64,
    pub removed_count: u64,
}

impl Scrollback {
    /// An empty buffer.
    pub fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            next_id: 0,
            total_written: 0,
        }
    }

    /// Number of retained lines (never above [`SCROLLBACK_MAX_LINES`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether no lines are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Total lines ever written (retained plus pruned).
    #[must_use]
    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    /// The id that the next pushed line will receive.
    #[must_use]
    pub fn next_line_id(&self) -> u64 {
        self.next_id
    }

    /// The retained line at `index` from oldest to newest, if in range.
    #[must_use]
    pub fn line(&self, index: usize) -> Option<&ScrollbackLine> {
        self.lines.get(index)
    }

    /// Iterates retained lines oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &ScrollbackLine> {
        self.lines.iter()
    }

    /// Appends one line of exactly `width` cells; prunes oldest-first when
    /// over capacity and reports every evicted id range for damage.
    ///
    /// # Panics (debug builds only)
    /// Panics if `cells.len()` differs from the grid width; production
    /// callers derive lengths from the grid itself.
    pub fn push(&mut self, cells: Vec<Cell>) -> (u64, ClearedRange) {
        debug_assert!(!cells.is_empty());
        let id = self.next_id;
        self.next_id += 1;
        self.total_written += 1;
        self.lines.push_back(ScrollbackLine {
            id,
            cells: cells.into_boxed_slice(),
        });
        let evicted = if self.lines.len() > SCROLLBACK_MAX_LINES {
            let overflow = self.lines.len() - SCROLLBACK_MAX_LINES;
            let first_evicted = self.lines.front().map_or(0, |l| l.id);
            for _ in 0..overflow {
                self.lines.pop_front();
            }
            ClearedRange {
                first_line_id: first_evicted,
                removed_count: overflow as u64,
            }
        } else {
            ClearedRange::default()
        };
        (id, evicted)
    }

    /// Removes every retained line (the `ED 3` / `FullReset` exception to
    /// immutability) and reports the removed range.
    pub fn clear(&mut self) -> ClearedRange {
        let removed = self.lines.len() as u64;
        let first = self.lines.front().map_or(0, |l| l.id);
        self.lines.clear();
        ClearedRange {
            first_line_id: first,
            removed_count: removed,
        }
    }
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Style;

    fn blank_row(width: usize) -> Vec<Cell> {
        vec![Cell::erased(Style::default()); width]
    }

    #[test]
    fn push_assigns_monotonic_ids() {
        let mut sb = Scrollback::new();
        assert_eq!(sb.push(blank_row(4)).0, 0);
        assert_eq!(sb.push(blank_row(4)).0, 1);
        assert_eq!(sb.next_line_id(), 2);
        assert_eq!(sb.total_written(), 2);
    }

    #[test]
    fn prune_removes_oldest_first_and_reports_range() {
        let mut sb = Scrollback::new();
        for _ in 0..SCROLLBACK_MAX_LINES {
            sb.push(blank_row(1));
        }
        assert_eq!(sb.len(), SCROLLBACK_MAX_LINES);
        let (_, evicted) = sb.push(blank_row(1));
        assert_eq!(evicted.removed_count, 1);
        assert_eq!(evicted.first_line_id, 0);
        assert_eq!(sb.len(), SCROLLBACK_MAX_LINES);
        assert_eq!(sb.line(0).unwrap().id, 1);
    }

    #[test]
    fn retained_lines_are_immutable_snapshots() {
        let mut sb = Scrollback::new();
        let mut row = blank_row(2);
        row[0].glyph = 'x';
        sb.push(row.clone());
        row[0].glyph = 'y';
        assert_eq!(sb.line(0).unwrap().cells[0].glyph, 'x');
    }

    #[test]
    fn clear_reports_removed_span() {
        let mut sb = Scrollback::new();
        sb.push(blank_row(1));
        sb.push(blank_row(1));
        let cleared = sb.clear();
        assert_eq!(cleared.first_line_id, 0);
        assert_eq!(cleared.removed_count, 2);
        assert!(sb.is_empty());
        // Ids keep increasing after a clear.
        assert_eq!(sb.push(blank_row(1)).0, 2);
    }
}
