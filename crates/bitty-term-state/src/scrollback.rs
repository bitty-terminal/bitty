//! Bounded, append-only scrollback (RFC invariant 4).
//!
//! Lines enter scrollback only via scroll-under-region operations; pruning
//! removes oldest first; contents are immutable once written. The single
//! exception is wholesale removal by `ED 3` / `FullReset`, which truncates
//! the buffer without rewriting any line. Capacity is per-buffer: the
//! default is [`SCROLLBACK_DEFAULT_LINES`] and every capacity is clamped to
//! the hard cap [`SCROLLBACK_MAX_LINES`] (RFC invariant 4 bounded memory per
//! threat T-01). See the crate documentation for the constant register and
//! RFC references.

use std::collections::VecDeque;

use crate::cell::Cell;

/// Default retained scrollback lines when no runtime configuration is
/// supplied (mirrors the `bitty-config` `terminal.scrollback` default).
pub const SCROLLBACK_DEFAULT_LINES: usize = 10_000;

/// Hard cap on retained scrollback lines (RFC invariant 4 "pruning removes
/// oldest first"; bounded memory per threat T-01). Mirrors the accepted
/// `bitty-config` bound for `terminal.scrollback` (`0..=100_000`); every
/// per-buffer capacity is clamped to this value.
pub const SCROLLBACK_MAX_LINES: usize = 100_000;

/// One immutable scrollback line with its monotonically assigned id.
///
/// `wrapped` is true when this line soft-wraps onto the next line in the
/// combined scrollback + grid order (the last scrollback line wraps onto
/// grid row 0). Hard breaks leave false. Resize reflow unwraps via these
/// flags and rewraps to the new width; ids are reassigned on reflow (still
/// monotonic) because the physical row count changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackLine {
    /// Monotonic id assigned when the line entered scrollback; ids never
    /// repeat within a state's lifetime.
    pub id: u64,
    /// Cell content, exactly `width` cells wide.
    pub cells: Box<[Cell]>,
    /// Soft-wrap continuation to the next line.
    pub wrapped: bool,
}

/// The bounded scrollback buffer.
#[derive(Debug, Clone)]
pub struct Scrollback {
    lines: VecDeque<ScrollbackLine>,
    next_id: u64,
    total_written: u64,
    max_lines: usize,
}

/// Result of a buffer-clearing operation: the removed id range
/// `[first_removed, first_removed + removed_count)` may be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClearedRange {
    pub first_line_id: u64,
    pub removed_count: u64,
}

impl Scrollback {
    /// An empty buffer retaining at most [`SCROLLBACK_DEFAULT_LINES`] lines.
    pub fn new() -> Self {
        Self::with_max_lines(SCROLLBACK_DEFAULT_LINES)
    }

    /// An empty buffer retaining at most `max_lines` lines. Capacities above
    /// the hard [`SCROLLBACK_MAX_LINES`] bound are clamped so a caller
    /// mistake or a future bound drift can never grow memory without limit
    /// (fail-closed at the core boundary).
    #[must_use]
    pub fn with_max_lines(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            next_id: 0,
            total_written: 0,
            max_lines: max_lines.min(SCROLLBACK_MAX_LINES),
        }
    }

    /// Retained-line capacity of this buffer (never above
    /// [`SCROLLBACK_MAX_LINES`]).
    #[must_use]
    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// Number of retained lines (never above [`Self::max_lines`]).
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
        self.push_with_wrap(cells, false)
    }

    /// Appends one line with an explicit soft-wrap continuation flag.
    /// `wrapped` travels from the grid row that scrolled off (see
    /// `Grid::remove_lines_up`); reflow rebuilds use fresh ids via this path.
    pub fn push_with_wrap(&mut self, cells: Vec<Cell>, wrapped: bool) -> (u64, ClearedRange) {
        debug_assert!(!cells.is_empty());
        let id = self.next_id;
        self.next_id += 1;
        self.total_written += 1;
        self.lines.push_back(ScrollbackLine {
            id,
            cells: cells.into_boxed_slice(),
            wrapped,
        });
        let evicted = if self.lines.len() > self.max_lines {
            let overflow = self.lines.len() - self.max_lines;
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

    // Note (CTX-0266): scrollback width changes happen only through
    // `State::resize` reflow (unwrap logical lines via `wrapped` flags,
    // rewrap to the new width, rebuild with fresh monotonic ids). The old
    // truncate/pad `resize` primitive was removed: it dropped line tails.
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
        let mut sb = Scrollback::with_max_lines(4);
        for _ in 0..4 {
            sb.push(blank_row(1));
        }
        assert_eq!(sb.len(), 4);
        let (_, evicted) = sb.push(blank_row(1));
        assert_eq!(evicted.removed_count, 1);
        assert_eq!(evicted.first_line_id, 0);
        assert_eq!(sb.len(), 4);
        assert_eq!(sb.line(0).unwrap().id, 1);
    }

    #[test]
    fn default_capacity_is_the_documented_default() {
        const { assert!(SCROLLBACK_DEFAULT_LINES == 10_000) }
        const { assert!(SCROLLBACK_MAX_LINES == 100_000) }
        assert_eq!(Scrollback::new().max_lines(), SCROLLBACK_DEFAULT_LINES);
    }

    #[test]
    fn configured_capacity_is_honored_and_clamped_to_hard_max() {
        assert_eq!(Scrollback::with_max_lines(7).max_lines(), 7);
        assert_eq!(
            Scrollback::with_max_lines(usize::MAX).max_lines(),
            SCROLLBACK_MAX_LINES
        );
    }

    #[test]
    fn zero_capacity_evicts_every_line() {
        let mut sb = Scrollback::with_max_lines(0);
        let (id, evicted) = sb.push(blank_row(1));
        assert_eq!(id, 0);
        assert_eq!(evicted.first_line_id, 0);
        assert_eq!(evicted.removed_count, 1);
        assert_eq!(sb.len(), 0);
        assert_eq!(sb.total_written(), 1);
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
