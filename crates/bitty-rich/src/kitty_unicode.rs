//! Kitty Unicode placeholder sizing (CTX-0821, issue #1400).
//!
//! [`bitty_term_state::kitty_unicode`] decodes one placeholder cell into
//! `(image id, placement id, tile row, tile column)`. This module is the
//! sizing half: given a grid run of such cells plus the virtual placement
//! the client registered (`U=1` with `c=`/`r=` cell spans), it resolves
//! the cell rectangle the run covers and the pixel rectangle the present
//! layer composites.
//!
//! # Model
//!
//! A virtual placement is a prototype, not a painted image: it names the
//! image and the `cols x rows` cell span the client's run will cover
//! (explicit `c=`/`r=` when non-zero, otherwise derived from decoded
//! pixels exactly like [`crate::kitty_place`] placements). The run's tile
//! coordinates select a sub-rectangle of that span: run width in cells by
//! one grid row. Runs that overrun the span clip (fail closed: only the
//! in-span part paints); runs naming an unknown virtual placement resolve
//! to `None` (the cells stay text).
//!
//! # Bounds
//!
//! All arithmetic saturates through `u64` into `i32`/`u32` (CTX-0253 F4
//! style); no allocation occurs here (the caller owns the run slice).

use crate::geometry::{CellMetrics, RectPx};
use bitty_term_state::{KittyUnicodeCell, KittyUnicodeRun};

/// Maximum tiles per axis a virtual placement may claim (matches the
/// `u8` tile coordinate range of [`bitty_term_state::kitty_unicode`]).
pub const KITTY_UNICODE_MAX_SPAN: u16 = 256;

/// A registered virtual placement prototype (`U=1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyUnicodeVirtual {
    /// 32-bit image id (24 low bits from the APC `i=`, plus high byte).
    pub image_id: u32,
    /// Placement id (`p=`); `None` when the client omitted it.
    pub placement_id: Option<u32>,
    /// Span columns (`c=`; `0` derives from pixels).
    pub cols: u16,
    /// Span rows (`r=`; `0` derives from pixels).
    pub rows: u16,
    /// Decoded image width in pixels (for `0`-span derivation).
    pub image_width: u32,
    /// Decoded image height in pixels (for `0`-span derivation).
    pub image_height: u32,
}

impl KittyUnicodeVirtual {
    /// Cell extent of the span: explicit `c=`/`r=` clamped to
    /// [`KITTY_UNICODE_MAX_SPAN`], else `ceil(pixels / cell)` (at least 1
    /// per axis, at most the max span).
    #[must_use]
    pub fn extent_cells(&self, metrics: CellMetrics) -> (u16, u16) {
        (
            virtual_extent_cells(self.cols, self.image_width, metrics.width),
            virtual_extent_cells(self.rows, self.image_height, metrics.height),
        )
    }
}

/// One axis of [`KittyUnicodeVirtual::extent_cells`].
#[must_use]
pub fn virtual_extent_cells(explicit: u16, pixels: u32, cell_px: u32) -> u16 {
    if explicit != 0 {
        return explicit.clamp(1, KITTY_UNICODE_MAX_SPAN);
    }
    let cell_px = cell_px.max(1);
    pixels
        .div_ceil(cell_px)
        .clamp(1, u32::from(KITTY_UNICODE_MAX_SPAN)) as u16
}

/// Resolved geometry of one grid run against its virtual placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyUnicodeRect {
    /// Grid row of the run.
    pub grid_row: usize,
    /// Grid column of the run's first cell.
    pub grid_col: usize,
    /// Covered columns (clipped to the span).
    pub cols: usize,
    /// Covered rows (always 1: runs are single-row).
    pub rows: usize,
    /// Pixel rectangle in content space.
    pub rect: RectPx,
}

/// Resolves `run` (with its decoded `cells`) against `virtual_placement`.
///
/// Returns `None` when the run names a different image/placement, when
/// the tile row/column starts outside the span, or when clipping leaves
/// nothing painted. The covered width is `min(run width, span cols -
/// tile col)`; the pixel rect is the covered cell rect at `metrics`.
/// `scrollback_now`/`scrollback_base` adjust the row exactly like
/// [`crate::kitty_place`] placements (images scroll with content);
/// `None` when the run scrolled fully off the top.
#[must_use]
pub fn unicode_run_rect(
    run: &KittyUnicodeRun,
    cells: &[KittyUnicodeCell],
    virtual_placement: &KittyUnicodeVirtual,
    metrics: CellMetrics,
    scrollback_now: usize,
    scrollback_base: usize,
) -> Option<KittyUnicodeRect> {
    if cells.is_empty() {
        return None;
    }
    if run.image_id != virtual_placement.image_id {
        return None;
    }
    if !placement_matches(run.placement_id, virtual_placement.placement_id) {
        return None;
    }
    let (span_cols, span_rows) = virtual_placement.extent_cells(metrics);
    let tile_col = usize::from(run.col);
    let tile_row = usize::from(run.row);
    if tile_col >= usize::from(span_cols) || tile_row >= usize::from(span_rows) {
        return None;
    }
    let room = usize::from(span_cols).saturating_sub(tile_col);
    let cols = cells.len().min(room);
    if cols == 0 {
        return None;
    }
    let scrolled = scrollback_now.saturating_sub(scrollback_base);
    let row = (run.grid_row as u64).checked_sub(scrolled as u64)?;
    let x = (run.grid_col as u64).saturating_mul(u64::from(metrics.width));
    let y = row.saturating_mul(u64::from(metrics.height));
    let w = (cols as u64).saturating_mul(u64::from(metrics.width));
    let h = u64::from(metrics.height);
    Some(KittyUnicodeRect {
        grid_row: run.grid_row,
        grid_col: run.grid_col,
        cols,
        rows: 1,
        rect: RectPx::new(
            saturating_i32(x),
            saturating_i32(y),
            saturating_u32(w),
            saturating_u32(h),
        ),
    })
}

/// Placement-id match: an explicit run placement requires the same
/// explicit virtual placement; an unspecified run placement (`None`)
/// matches any virtual placement of the image (kitty any-placement
/// fallback).
fn placement_matches(run: Option<u32>, virtual_placement: Option<u32>) -> bool {
    match (run, virtual_placement) {
        (None, _) => true,
        (Some(a), Some(b)) => a == b,
        (Some(_), None) => false,
    }
}

const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

const fn saturating_u32(value: u64) -> u32 {
    if value > u32::MAX as u64 {
        u32::MAX
    } else {
        value as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{Color, KittyUnicodeId};

    const METRICS: CellMetrics = CellMetrics {
        width: 8,
        height: 16,
    };

    fn cell(grid_row: usize, grid_col: usize, row: u8, col: u8) -> KittyUnicodeCell {
        KittyUnicodeCell {
            grid_row,
            grid_col,
            id: KittyUnicodeId {
                image_id: 42,
                placement_id: None,
                row,
                col,
            },
        }
    }

    fn make_run(
        grid_col: usize,
        width: usize,
        col: u8,
    ) -> (KittyUnicodeRun, Vec<KittyUnicodeCell>) {
        let cells: Vec<KittyUnicodeCell> = (0..width)
            .map(|i| cell(5, grid_col + i, 0, col + i as u8))
            .collect();
        let run = KittyUnicodeRun {
            image_id: 42,
            placement_id: None,
            row: 0,
            col,
            grid_row: 5,
            grid_col,
            width,
        };
        (run, cells)
    }

    fn virtual_rect() -> KittyUnicodeVirtual {
        KittyUnicodeVirtual {
            image_id: 42,
            placement_id: None,
            cols: 4,
            rows: 2,
            image_width: 32,
            image_height: 32,
        }
    }

    #[test]
    fn run_resolves_to_cell_rect() {
        let (run, cells) = make_run(3, 2, 1);
        let rect = unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).unwrap();
        assert_eq!((rect.grid_row, rect.grid_col), (5, 3));
        assert_eq!((rect.cols, rect.rows), (2, 1));
        assert_eq!(rect.rect, RectPx::new(3 * 8, 5 * 16, 2 * 8, 16));
    }

    #[test]
    fn run_clips_at_span_edge() {
        // Tile col 3 of a 4-wide span with 2 cells: only 1 paints.
        let (run, cells) = make_run(0, 2, 3);
        let rect = unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).unwrap();
        assert_eq!(rect.cols, 1);
        assert_eq!(rect.rect, RectPx::new(0, 5 * 16, 8, 16));
    }

    #[test]
    fn run_outside_span_paints_nothing() {
        let (run, cells) = make_run(0, 2, 4);
        assert!(unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).is_none());
        let (run, cells) = make_run(0, 1, 0);
        let mut run = run;
        run.row = 2;
        let mut cells = cells;
        cells[0].id.row = 2;
        assert!(unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).is_none());
    }

    #[test]
    fn image_mismatch_paints_nothing() {
        let (mut run, cells) = make_run(0, 1, 0);
        run.image_id = 43;
        assert!(unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).is_none());
        assert!(unicode_run_rect(&run, &[], &virtual_rect(), METRICS, 0, 0).is_none());
    }

    #[test]
    fn placement_matching_follows_any_fallback() {
        let (run, cells) = make_run(0, 1, 0);
        // Unspecified run matches any virtual placement.
        let named = KittyUnicodeVirtual {
            placement_id: Some(9),
            ..virtual_rect()
        };
        assert!(unicode_run_rect(&run, &cells, &named, METRICS, 0, 0).is_some());
        // Explicit run needs the same explicit virtual placement.
        let (mut run, mut cells) = make_run(0, 1, 0);
        run.placement_id = Some(9);
        cells[0].id.placement_id = Some(9);
        assert!(unicode_run_rect(&run, &cells, &named, METRICS, 0, 0).is_some());
        assert!(unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 0, 0).is_none());
        let other = KittyUnicodeVirtual {
            placement_id: Some(10),
            ..virtual_rect()
        };
        assert!(unicode_run_rect(&run, &cells, &other, METRICS, 0, 0).is_none());
    }

    #[test]
    fn scroll_adjusts_row_and_off_top_is_none() {
        let (run, cells) = make_run(0, 1, 0);
        let rect =
            unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 3, 0).expect("scrolled");
        assert_eq!(rect.rect.y, (5 - 3) * 16);
        assert!(unicode_run_rect(&run, &cells, &virtual_rect(), METRICS, 6, 0).is_none());
    }

    #[test]
    fn span_derivation_matches_place_layer() {
        let metrics = METRICS;
        // Explicit spans clamp to the max span.
        assert_eq!(virtual_extent_cells(3, 0, 8), 3);
        assert_eq!(virtual_extent_cells(9999, 0, 8), KITTY_UNICODE_MAX_SPAN);
        // Derived spans ceil(pixels / cell), at least 1.
        assert_eq!(virtual_extent_cells(0, 20, 8), 3);
        assert_eq!(virtual_extent_cells(0, 0, 8), 1);
        let v = KittyUnicodeVirtual {
            image_id: 1,
            placement_id: None,
            cols: 0,
            rows: 0,
            image_width: 20,
            image_height: 40,
        };
        assert_eq!(v.extent_cells(metrics), (3, 3));
        let _ = Color::Indexed(0);
    }
}
