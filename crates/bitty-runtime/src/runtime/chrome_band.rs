//! Window chrome band geometry (CTX-0873, issue #1431).
//!
//! Generic edge-band reservation carved out of the window grid *before*
//! layout: the layout container is the window minus the band, so every leaf,
//! the primary grid, and every PTY winsize are sized by the normal reflow
//! path and no terminal cell is ever painted under a band.
//!
//! The solver was first built for the Core workspace bar (W-104/CTX-0956
//! retired it: the `bar` plugin owns workspace/status UX and Core reserves
//! zero rows). It stays as the generic mechanism plugin bands offset
//! against, which is why the `present` predicate and the bar-band term
//! remain even though Core always solves them absent.
//!
//! The geometry is four-sided ([`ChromeInsets`]) so left/right bands can be
//! added later without reshaping callers; only [`BarEdge::Top`] and
//! [`BarEdge::Bottom`] exist today. Everything here is pure, total, and
//! headless: hostile or tiny windows hide the band instead of producing a
//! zero or negative content extent.

use crate::config::BarEdge;
use bitty_ui::Rect as UiRect;

/// Minimum terminal content rows a leaf must keep once a band is reserved,
/// before outer gaps and decoration (see [`min_container_rows`]). A window
/// shorter than the band plus the effective minimum hides the band
/// (fail-open for content: the terminal always keeps at least this much).
pub const MIN_CONTENT_ROWS: u16 = 1;

/// Minimum terminal content columns required before any band is reserved.
pub const MIN_CONTENT_COLS: u16 = 1;

/// Cells reserved on each window edge by Core chrome (window cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromeInsets {
    /// Rows reserved at the top edge.
    pub top: u16,
    /// Rows reserved at the bottom edge.
    pub bottom: u16,
    /// Columns reserved at the left edge (always `0` today).
    pub left: u16,
    /// Columns reserved at the right edge (always `0` today).
    pub right: u16,
}

impl ChromeInsets {
    /// No reserved chrome.
    pub const NONE: Self = Self {
        top: 0,
        bottom: 0,
        left: 0,
        right: 0,
    };

    /// Insets reserving `thickness` cells on `edge`.
    #[must_use]
    pub const fn for_edge(edge: BarEdge, thickness: u16) -> Self {
        match edge {
            BarEdge::Top => Self {
                top: thickness,
                ..Self::NONE
            },
            BarEdge::Bottom => Self {
                bottom: thickness,
                ..Self::NONE
            },
        }
    }

    /// Content rectangle left after removing the insets from `window`, or
    /// `None` when the remainder would fall below [`MIN_CONTENT_COLS`]
    /// columns or `min_rows` rows (the caller then hides the band;
    /// `min_rows` is floored at [`MIN_CONTENT_ROWS`]). Saturating: never
    /// wraps, never yields a zero extent.
    #[must_use]
    pub fn content_of(self, window: UiRect, min_rows: u16) -> Option<UiRect> {
        let vertical = u32::from(self.top) + u32::from(self.bottom);
        let horizontal = u32::from(self.left) + u32::from(self.right);
        let rows = u32::from(window.height).checked_sub(vertical)?;
        let cols = u32::from(window.width).checked_sub(horizontal)?;
        let min_rows = u32::from(min_rows.max(MIN_CONTENT_ROWS));
        if rows < min_rows || cols < u32::from(MIN_CONTENT_COLS) {
            return None;
        }
        // Bounded by the window extent above, so the narrowing is lossless;
        // the origin shift saturates at the u16 edge for hostile origins.
        Some(UiRect::new(
            window.x.saturating_add(self.left),
            window.y.saturating_add(self.top),
            saturate_u16(cols),
            saturate_u16(rows),
        ))
    }
}

fn saturate_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Effective minimum container rows before a band may be reserved: the
/// [`MIN_CONTENT_ROWS`] content floor plus the outer cell gap on both
/// sides (`layout.gaps_out`) plus the vertical pixel decoration
/// (`decoration_px`, both sides summed, at the live DPI scale) rounded up
/// to whole rows at the live cell height `cell_h`.
///
/// Without this, a window that keeps [`MIN_CONTENT_ROWS`] container rows
/// could still lose every content row to gaps and the decoration ring.
/// Total and saturating; a zero `cell_h` counts the decoration as no rows.
#[must_use]
pub fn min_container_rows(gaps_out_cells: u16, decoration_px: f64, cell_h: u32) -> u16 {
    let gaps = u32::from(gaps_out_cells).saturating_mul(2);
    let deco_rows = if cell_h == 0 || !decoration_px.is_finite() || decoration_px <= 0.0 {
        0
    } else {
        // Bounded by the u16 clamp below; the float is finite and positive.
        let rows = (decoration_px / f64::from(cell_h)).ceil();
        if rows >= f64::from(u16::MAX) {
            u32::from(u16::MAX)
        } else {
            rows as u32
        }
    };
    saturate_u16(
        u32::from(MIN_CONTENT_ROWS)
            .saturating_add(gaps)
            .saturating_add(deco_rows),
    )
}

/// Solved chrome geometry for one window grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeLayout {
    /// Layout container (window minus every reserved band).
    pub container: UiRect,
    /// The Core bar band in window cells, when reserved (always `None`
    /// since the Core bar retired in W-104/CTX-0956; the field stays so
    /// the generic solve shape is unchanged).
    pub bar: Option<UiRect>,
}

/// Solves the layout container and the bar band for `window`.
///
/// `present` is the bar presence predicate (visible and more than one
/// workspace). An absent bar, a zero `thickness`, or a window too small to
/// keep `min_rows` container rows (see [`min_container_rows`]) reserves
/// nothing: the container is the full window. Pure and deterministic.
#[must_use]
pub fn solve(
    window: UiRect,
    edge: BarEdge,
    thickness: u16,
    present: bool,
    min_rows: u16,
) -> ChromeLayout {
    let full = ChromeLayout {
        container: window,
        bar: None,
    };
    if !present || thickness == 0 {
        return full;
    }
    let Some(container) = ChromeInsets::for_edge(edge, thickness).content_of(window, min_rows)
    else {
        return full;
    };
    let bar_y = match edge {
        BarEdge::Top => window.y,
        BarEdge::Bottom => container.y.saturating_add(container.height),
    };
    ChromeLayout {
        container,
        bar: Some(UiRect::new(window.x, bar_y, window.width, thickness)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: UiRect = UiRect::new(0, 0, 80, 24);

    #[test]
    fn absent_bar_keeps_the_full_window() {
        let solved = solve(WINDOW, BarEdge::Bottom, 1, false, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, WINDOW);
        assert_eq!(solved.bar, None);
        let zero = solve(WINDOW, BarEdge::Top, 0, true, MIN_CONTENT_ROWS);
        assert_eq!(zero.container, WINDOW);
        assert_eq!(zero.bar, None);
    }

    #[test]
    fn bottom_band_takes_the_last_row() {
        let solved = solve(WINDOW, BarEdge::Bottom, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, UiRect::new(0, 0, 80, 23));
        assert_eq!(solved.bar, Some(UiRect::new(0, 23, 80, 1)));
    }

    #[test]
    fn top_band_takes_row_zero_and_shifts_content() {
        let solved = solve(WINDOW, BarEdge::Top, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container, UiRect::new(0, 1, 80, 23));
        assert_eq!(solved.bar, Some(UiRect::new(0, 0, 80, 1)));
    }

    #[test]
    fn tiny_window_hides_the_band_instead_of_zero_content() {
        for edge in [BarEdge::Top, BarEdge::Bottom] {
            let one_row = UiRect::new(0, 0, 80, 1);
            let solved = solve(one_row, edge, 1, true, MIN_CONTENT_ROWS);
            assert_eq!(solved.container, one_row, "{edge:?}");
            assert_eq!(solved.bar, None, "{edge:?}");
            let no_cols = UiRect::new(0, 0, 0, 24);
            assert_eq!(
                solve(no_cols, edge, 1, true, MIN_CONTENT_ROWS).bar,
                None,
                "{edge:?}"
            );
        }
        // Exactly band + MIN_CONTENT_ROWS still reserves.
        let two = UiRect::new(0, 0, 10, 1 + MIN_CONTENT_ROWS);
        let solved = solve(two, BarEdge::Bottom, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container.height, MIN_CONTENT_ROWS);
        assert!(solved.bar.is_some());
    }

    #[test]
    fn insets_saturate_on_hostile_extents() {
        let huge = ChromeInsets {
            top: u16::MAX,
            bottom: u16::MAX,
            ..ChromeInsets::NONE
        };
        assert_eq!(huge.content_of(WINDOW, MIN_CONTENT_ROWS), None);
        let edge = UiRect::new(u16::MAX, u16::MAX, 4, 4);
        let solved = solve(edge, BarEdge::Top, 1, true, MIN_CONTENT_ROWS);
        assert_eq!(solved.container.y, u16::MAX, "origin saturates");
        assert_eq!(solved.container.height, 3);
    }

    #[test]
    fn min_container_rows_counts_gaps_and_decoration() {
        assert_eq!(min_container_rows(0, 0.0, 22), MIN_CONTENT_ROWS);
        // 2 * (6 + 2 + 6) px = 28 px at a 22 px cell rounds up to 2 rows.
        assert_eq!(min_container_rows(0, 28.0, 22), MIN_CONTENT_ROWS + 2);
        assert_eq!(min_container_rows(1, 0.0, 22), MIN_CONTENT_ROWS + 2);
        assert_eq!(min_container_rows(0, 28.0, 0), MIN_CONTENT_ROWS);
        assert_eq!(min_container_rows(0, f64::NAN, 22), MIN_CONTENT_ROWS);
        assert_eq!(min_container_rows(u16::MAX, 1e30, 1), u16::MAX);
    }

    #[test]
    fn effective_floor_hides_the_band_when_decoration_eats_content() {
        let floor = min_container_rows(0, 28.0, 22);
        let short = UiRect::new(0, 0, 80, floor);
        assert_eq!(solve(short, BarEdge::Bottom, 1, true, floor).bar, None);
        let fits = UiRect::new(0, 0, 80, floor + 1);
        let solved = solve(fits, BarEdge::Bottom, 1, true, floor);
        assert_eq!(solved.container.height, floor);
        assert!(solved.bar.is_some());
    }
}
