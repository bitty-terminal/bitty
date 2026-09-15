//! `Runtime` — Bounded multi-click tracker (CTX-0385, issue #641).
//!
//! Distinguishes single/double/triple left presses for word/line selection.
//! `O(1)` state (last press time, cell, button, count); no history grows.
//! Headless and deterministic with an explicit wall clock (`Instant` passed
//! in, virtual-clock seam like `handle_cursor_moved_at`).

use std::time::{Duration, Instant};

use bitty_platform::MouseButton;
use bitty_ui::CellPos;

/// Maximum interval between presses to chain into a multi-click.
pub const MULTI_CLICK_TIMEOUT_MS: u64 = 500;

/// Maximum cell distance between presses to chain (Chebyshev).
pub const MULTI_CLICK_MAX_CELL_DISTANCE: u16 = 1;

/// Minimum click count (single click).
pub const CLICK_COUNT_MIN: u8 = 1;

/// Maximum click count (triple-click); the next quick press wraps to single.
pub const CLICK_COUNT_MAX: u8 = 3;

/// Timeout as a `Duration` (derived from [`MULTI_CLICK_TIMEOUT_MS`]).
#[must_use]
pub const fn multi_click_timeout() -> Duration {
    Duration::from_millis(MULTI_CLICK_TIMEOUT_MS)
}

/// Bounded click state machine: `O(1)`, no unbounded history.
#[derive(Debug, Clone, Copy)]
pub struct ClickTracker {
    last_time: Option<Instant>,
    last_cell: Option<CellPos>,
    last_button: Option<MouseButton>,
    count: u8,
}

impl ClickTracker {
    /// Creates an idle tracker (no press seen, count single).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_time: None,
            last_cell: None,
            last_button: None,
            count: CLICK_COUNT_MIN,
        }
    }

    /// Current chained count (`1..=3`).
    #[must_use]
    pub const fn count(self) -> u8 {
        self.count
    }

    /// Resets to idle (next press is single).
    pub fn reset(&mut self) {
        self.last_time = None;
        self.last_cell = None;
        self.last_button = None;
        self.count = CLICK_COUNT_MIN;
    }

    /// Records a press and returns its click count (`1..=3`).
    ///
    /// Only the left button chains; any other button resets and returns
    /// single (fail-closed: right/middle paste never arms word/line mode).
    /// A press chains when the button matches, the elapsed time since the
    /// last press fits [`MULTI_CLICK_TIMEOUT_MS`], and the cell distance
    /// fits [`MULTI_CLICK_MAX_CELL_DISTANCE`]; otherwise it restarts at
    /// single. A quick fourth press wraps to single (standard `1-2-3-1`
    /// cycle). A backwards clock (`now < last`) resets fail-closed.
    pub fn press(&mut self, button: MouseButton, cell: CellPos, now: Instant) -> u8 {
        if button != MouseButton::Left {
            self.reset();
            return CLICK_COUNT_MIN;
        }
        let chained = match (self.last_time, self.last_cell, self.last_button) {
            (Some(last), Some(last_cell), Some(last_button)) => {
                if last_button != MouseButton::Left {
                    false
                } else if let Some(elapsed) = now.checked_duration_since(last) {
                    elapsed <= multi_click_timeout()
                        && cell_distance(last_cell, cell) <= MULTI_CLICK_MAX_CELL_DISTANCE
                } else {
                    // Backwards clock: fail-closed restart.
                    false
                }
            }
            _ => false,
        };
        if chained {
            self.count = if self.count >= CLICK_COUNT_MAX {
                CLICK_COUNT_MIN
            } else {
                self.count + 1
            };
        } else {
            self.count = CLICK_COUNT_MIN;
        }
        self.last_time = Some(now);
        self.last_cell = Some(cell);
        self.last_button = Some(button);
        self.count
    }
}

impl Default for ClickTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Chebyshev cell distance between two grid positions.
#[must_use]
pub const fn cell_distance(a: CellPos, b: CellPos) -> u16 {
    let dr = a.row.abs_diff(b.row);
    let dc = a.col.abs_diff(b.col);
    if dr >= dc { dr } else { dc }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u16, col: u16) -> CellPos {
        CellPos::new(row, col)
    }

    #[test]
    fn first_press_is_single() {
        let mut t = ClickTracker::new();
        let now = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(0, 0), now), 1);
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn quick_same_cell_chains_to_triple_then_wraps() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(2, 2), base), 1);
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(2, 2),
                base + Duration::from_millis(100)
            ),
            2
        );
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(2, 2),
                base + Duration::from_millis(200)
            ),
            3
        );
        // Fourth quick press wraps to single (standard cycle).
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(2, 2),
                base + Duration::from_millis(300)
            ),
            1
        );
    }

    #[test]
    fn timeout_breaks_chain() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(0, 0), base), 1);
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(0, 0),
                base + Duration::from_millis(MULTI_CLICK_TIMEOUT_MS + 1)
            ),
            1
        );
    }

    #[test]
    fn far_cell_breaks_chain() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(0, 0), base), 1);
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(0, 5),
                base + Duration::from_millis(50)
            ),
            1
        );
    }

    #[test]
    fn nearby_cell_within_tolerance_chains() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(4, 4), base), 1);
        // One-cell jitter still chains (touchpad/rounding tolerance).
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(4, 5),
                base + Duration::from_millis(50)
            ),
            2
        );
    }

    #[test]
    fn other_button_resets_and_returns_single() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(0, 0), base), 1);
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(0, 0),
                base + Duration::from_millis(50)
            ),
            2
        );
        assert_eq!(
            t.press(
                MouseButton::Right,
                cell(0, 0),
                base + Duration::from_millis(100)
            ),
            1
        );
        // Chain restarts after the non-left press.
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(0, 0),
                base + Duration::from_millis(150)
            ),
            1
        );
    }

    #[test]
    fn backwards_clock_resets_fail_closed() {
        let mut t = ClickTracker::new();
        let base = Instant::now();
        assert_eq!(t.press(MouseButton::Left, cell(1, 1), base), 1);
        assert_eq!(
            t.press(
                MouseButton::Left,
                cell(1, 1),
                base + Duration::from_millis(50)
            ),
            2
        );
        // Earlier timestamp cannot chain.
        assert_eq!(t.press(MouseButton::Left, cell(1, 1), base), 1);
    }

    #[test]
    fn tracker_is_o1_bounded() {
        // State is a fixed-size struct: no heap, no history growth.
        // (Exact size is layout-dependent; the bound is what matters.)
        assert!(std::mem::size_of::<ClickTracker>() <= 64);
        let mut t = ClickTracker::new();
        let base = Instant::now();
        for i in 0..1000u64 {
            let _ = t.press(
                MouseButton::Left,
                cell(0, 0),
                base + Duration::from_millis(i * 10),
            );
        }
        assert!(t.count() >= CLICK_COUNT_MIN && t.count() <= CLICK_COUNT_MAX);
    }
}
