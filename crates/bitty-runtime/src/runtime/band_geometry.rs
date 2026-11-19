//! One physical mapping from granted band cells to band pixels
//! (CTX-1906, bitty#3263).
//!
//! Band placement is solved in window **cells** ([`ChromeBands::band_row`]):
//! a bottom band owns the last window rows. Paint and hit-testing used to
//! derive the pixel origin from that row alone (`pad + row * cell_h`), which
//! is right for every row but the outermost bottom one: the window's physical
//! extent is almost never an exact multiple of the cell height, and the grid
//! is derived by *flooring* the padded content extent to whole rows
//!
//! ```text
//! remainder = (surface_height - 2 * padding) % cell_height   // 0 ..= cell_h-1
//! ```
//!
//! so the cell grid's bottom edge stops that many physical pixels short of
//! the bottom inset. For terminal content the leftover strip is invisible (it
//! is background under the last row), but a band is a painted strip: the
//! remainder showed up as a blank gap between the bottom plugin chrome and the
//! window's bottom padding (bitty#3263).
//!
//! This module is the *one* mapping from a granted band rect to physical
//! pixels. Band paint, pointer hit-testing, hover/click/scroll routing,
//! tooltip anchoring, and the band's damage all read it, so painting and
//! interaction cannot disagree about where a band is.
//!
//! # The rule
//!
//! A **bottom** row band is anchored to the physical bottom inset instead of
//! the cell grid. Band `index` (`0` = outermost, at the window edge) occupies
//!
//! ```text
//! y = surface_height - padding - (index + 1) * cell_height  ..  + cell_height
//! ```
//!
//! so the outermost band's bottom edge lands on the bottom inset exactly,
//! whatever the remainder is, and the configured window padding is the only
//! gap below it. The stack stays contiguous, and the innermost visible band
//! absorbs the sub-cell remainder at its **top** edge (height
//! `cell_height + remainder`), which keeps it flush against the layout
//! container — no new gap appears immediately above the band — while every
//! band keeps its one-cell logical budget. The cell rows themselves, and
//! therefore the container, the grid, and every PTY winsize, do not move.
//!
//! **Top** row bands and **column** bands keep the plain cell mapping
//! (`pad + row * cell_h`): nothing pushes a top band away from its cells, and
//! a column band lives *inside* the layout container, so its rows are cell
//! rows. Left/right band behavior is untouched by the bottom anchor.
//!
//! Everything here is total and bounded:
//!
//! - a missing surface extent keeps the cell mapping, i.e. exactly the
//!   pre-#3263 behavior (the remainder is `0` by construction);
//! - the physical anchor only applies to a genuine **sub-cell** strip
//!   (`0 <= strip < cell_h`). A larger strip means the extent does not describe
//!   this grid — a grid clamped to the row bound, or a caller that set the
//!   window cells without resizing the surface — and stretching chrome across
//!   it would drag the band far from its content, so the cell mapping stands;
//! - a clamped or degrading grid never paints a band above the layout
//!   container's bottom edge: the rect's top is clamped to
//!   `container_bottom` and its height saturates at `0` instead of inverting.
//!   Every physically anchored rect is inside the surface by construction
//!   (`container_bottom <= y < y + height <= bottom inset`); the cell-mapping
//!   fallback keeps the pre-#3263 rect, which the surface viewport clips
//!   exactly as it always did. Row bands are budgeted away long before that
//!   (see `Runtime::plugin_band_budget`); the clamp is the backstop.

use super::Runtime;
use super::band_slots::BandEdge;

/// Physical, window-relative pixel rectangle of one chrome band.
///
/// The single shape paint and hit-testing share: paint emits a fill at exactly
/// this rect (so the rect *is* the band's damage), and hit-testing asks whether
/// the pointer is inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BandPixels {
    /// Left edge, window-relative physical px.
    pub(super) x: i32,
    /// Top edge, window-relative physical px.
    pub(super) y: i32,
    /// Width in physical px.
    pub(super) width: u32,
    /// Height in physical px.
    pub(super) height: u32,
}

impl BandPixels {
    /// Whether physical point `(x, y)` falls inside this rect.
    ///
    /// Half-open on both axes, matching how the paint rects tile: the pixel on
    /// the right/bottom edge belongs to the next rect, never to two bands, so
    /// a stacked pair of bands has no double-claimed row.
    #[must_use]
    pub(super) fn contains(&self, x: f64, y: f64) -> bool {
        let left = f64::from(self.x);
        let top = f64::from(self.y);
        let right = left + f64::from(self.width);
        let bottom = top + f64::from(self.height);
        x >= left && x < right && y >= top && y < bottom
    }
}

/// The one physical mapping from granted band cells to band pixels.
///
/// Built once per frame (paint) and once per pointer query (routing) from the
/// same live inputs — padding, cell metrics, window grid, visible bottom band
/// count, surface extent — so both readers get identical numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BandPixelGeometry {
    /// Window padding inset, physical px (both sides).
    pad_px: i32,
    /// Live cell width, physical px.
    cell_w: u32,
    /// Live cell height, physical px.
    cell_h: u32,
    /// Window grid origin row (window-relative band rows are measured from
    /// here; `0` for every shipped window).
    window_y: u16,
    /// Window grid rows (the band's logical row space).
    window_rows: u16,
    /// Visible bottom bands the row budget granted (`0` = no bottom stack).
    bottom_bands: u16,
    /// Surface height in physical px, when a surface is configured.
    surface_h: Option<i32>,
}

impl BandPixelGeometry {
    /// Builds the mapping from live inputs.
    ///
    /// `pad_px` is the padding the caller paints with — the live physical
    /// inset in production — so a caller can never paint against one inset and
    /// hit-test against another.
    #[must_use]
    pub(super) fn new(
        pad_px: u32,
        cell: (u32, u32),
        window: (u16, u16),
        bottom_bands: u16,
        surface_h: Option<u32>,
    ) -> Self {
        Self {
            pad_px: i32::try_from(pad_px).unwrap_or(i32::MAX),
            cell_w: cell.0,
            cell_h: cell.1,
            window_y: window.0,
            window_rows: window.1,
            bottom_bands,
            surface_h: surface_h.map(|height| i32::try_from(height).unwrap_or(i32::MAX)),
        }
    }

    /// Physical rect of one row-band row on `edge`, `width_cells` cells wide
    /// from window column `column`.
    ///
    /// `row` is a window-relative grid row (what
    /// [`Runtime::visible_band_row`] reports). For `bottom` the rect is the
    /// physical-inset-anchored stack slot (see the module docs); for `top` —
    /// and for the `left`/`right` content rows a column band paints — it is
    /// the plain cell rect `pad + row * cell_h`.
    #[must_use]
    pub(super) fn row_band(
        &self,
        edge: BandEdge,
        row: u16,
        column: u16,
        width_cells: u16,
    ) -> BandPixels {
        let (y, height) = match edge {
            BandEdge::Bottom => self.bottom_row(row),
            // Only a bottom band is displaced by the sub-cell remainder.
            BandEdge::Top | BandEdge::Left | BandEdge::Right => (
                i64::from(self.pad_px) + i64::from(row) * i64::from(self.cell_h),
                i64::from(self.cell_h),
            ),
        };
        self.rect(y, height, column, width_cells)
    }

    /// Physical rect of a container-bound band area: `height_cells` container
    /// rows from `row`, `width_cells` columns from `column`.
    ///
    /// Column bands reserve columns but span the row-carved container, so their
    /// rows are plain cell rows — the bottom anchor never applies to them.
    #[must_use]
    pub(super) fn area_band(
        &self,
        row: u16,
        column: u16,
        width_cells: u16,
        height_cells: u16,
    ) -> BandPixels {
        let y = i64::from(self.pad_px) + i64::from(row) * i64::from(self.cell_h);
        let height = i64::from(height_cells) * i64::from(self.cell_h);
        self.rect(y, height, column, width_cells)
    }

    /// The physical strip between the cell grid's bottom edge and the bottom
    /// inset: `(surface_height - 2 * padding) - window_rows * cell_height`.
    ///
    /// This is bitty#3263's measured gap. It is `0` when no surface is
    /// configured, `0` when the grid exactly fills the padded content, and a
    /// sub-cell `0..cell_h` in the normal case; a grid clamped to the row bound
    /// leaves a larger strip, which the band mapping deliberately does *not*
    /// absorb (only a sub-cell remainder is absorbed — see the module docs).
    ///
    /// Test-only: the mapping absorbs the remainder itself, so no product path
    /// needs to branch on it. The regressions read it to state the precondition
    /// they exercise (a remainder that is *not* zero) and to derive the pixel
    /// the pre-#3263 rule would have stopped at.
    #[cfg(test)]
    #[must_use]
    pub(super) fn bottom_remainder(&self) -> i32 {
        if self.surface_h.is_none() {
            return 0;
        }
        let slack = self.bottom_inset() - self.grid_bottom();
        i32::try_from(slack.max(0)).unwrap_or(i32::MAX)
    }

    /// Assembles the pixel rect from a vertical span and a column grant.
    fn rect(&self, y: i64, height: i64, column: u16, width_cells: u16) -> BandPixels {
        let x = i64::from(self.pad_px) + i64::from(column) * i64::from(self.cell_w);
        BandPixels {
            x: clamp_i32(x),
            y: clamp_i32(y),
            width: saturate_u32(i64::from(width_cells) * i64::from(self.cell_w)),
            height: saturate_u32(height.max(0)),
        }
    }

    /// Physical y where the cell grid ends: `pad + rows * cell_h`.
    fn grid_bottom(&self) -> i64 {
        i64::from(self.pad_px) + self.absolute_rows() * i64::from(self.cell_h)
    }

    /// Physical y of the bottom window inset: the surface bottom minus the
    /// padding inset, or the cell grid's bottom edge when no surface is
    /// configured (then the remainder is `0` and the cell mapping stands).
    fn bottom_inset(&self) -> i64 {
        match self.surface_h {
            Some(surface_h) => i64::from(surface_h) - i64::from(self.pad_px),
            None => self.grid_bottom(),
        }
    }

    /// Physical y the bottom band stack is anchored to.
    ///
    /// The surface's bottom inset when the extent leaves a genuine **sub-cell**
    /// remainder under the cell grid (`0 <= strip < cell_h`) — the bitty#3263
    /// gap — and the cell grid's own bottom edge otherwise.
    ///
    /// That bound is what keeps the mapping honest. A strip of a whole cell or
    /// more is not a rounding remainder: it means the extent does not describe
    /// this grid (a grid clamped to the row bound, or a caller that set the
    /// window cells without resizing the surface). Stretching a band across
    /// such a strip would move chrome far from the content it labels, so the
    /// mapping falls back to the cell grid — the pre-#3263 behavior — instead.
    fn anchor_bottom(&self) -> i64 {
        let inset = self.bottom_inset();
        let strip = inset - self.grid_bottom();
        if self.surface_h.is_some() && (0..i64::from(self.cell_h)).contains(&strip) {
            inset
        } else {
            self.grid_bottom()
        }
    }

    /// Physical y of the layout container's bottom edge: the bottom band stack
    /// starts exactly here, which is why the innermost band may grow upward to
    /// it but never past it.
    fn container_bottom(&self) -> i64 {
        let content_rows =
            self.absolute_rows() - i64::from(self.bottom_bands).min(self.absolute_rows());
        i64::from(self.pad_px) + content_rows * i64::from(self.cell_h)
    }

    /// Rows between the window grid origin and the bottom of the grid.
    fn absolute_rows(&self) -> i64 {
        i64::from(self.window_y) + i64::from(self.window_rows)
    }

    /// Physical `(y, height)` of one bottom band row: the stack slot anchored
    /// to the bottom inset, with the innermost band absorbing the remainder.
    fn bottom_row(&self, row: u16) -> (i64, i64) {
        let cell_h = i64::from(self.cell_h);
        let count = i64::from(self.bottom_bands);
        let absolute = self.absolute_rows();
        let row = i64::from(row);
        let cell_y = i64::from(self.pad_px) + row * cell_h;
        if cell_h <= 0 || count <= 0 || row < absolute - count || row >= absolute {
            // No bottom stack, or a row outside it: the plain cell mapping.
            return (cell_y, cell_h.max(0));
        }
        // Distance from the window's bottom edge: band 0 is the outermost.
        let from_edge = absolute - 1 - row;
        let end = self.anchor_bottom() - from_edge * cell_h;
        let mut top = end - cell_h;
        // The innermost visible band absorbs the sub-cell remainder at its top
        // edge, so the stack stays flush with the layout container and the
        // remainder cannot resurface as a gap above the band.
        if from_edge == count - 1 {
            let container_bottom = self.container_bottom();
            let slack = top - container_bottom;
            if slack > 0 && slack < cell_h {
                top = container_bottom;
            }
        }
        // Bounded: never above the container's bottom edge (it would cover
        // content) and never a negative origin. The anchored rect is inside the
        // surface by construction (`end == bottom inset < surface bottom`); the
        // cell-mapping fallback keeps the pre-#3263 rect, which the surface
        // viewport clips exactly as it always did.
        let end = end.max(0);
        let top = top.max(self.container_bottom().min(end));
        let end = end.max(top);
        (top, end - top)
    }
}

/// Saturating `i64` -> `i32` for physical pixel geometry (compositors clip in
/// `i64`; hostile cell metrics must not wrap here).
fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// Saturating `i64` -> `u32` for a pixel span.
fn saturate_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(if value < 0 { 0 } else { u32::MAX })
}

impl Runtime {
    /// The live physical band mapping for `pad_px` (CTX-1906, bitty#3263).
    ///
    /// Every band consumer — paint, hit-test, hover/click/scroll routing,
    /// tooltip anchoring — resolves through this, so the origin painted and
    /// the origin routed are one number.
    #[must_use]
    pub(super) fn band_pixel_geometry(&self, pad_px: u32) -> BandPixelGeometry {
        let live = self.live_cell_metrics();
        let window = self.window_cells();
        BandPixelGeometry::new(
            pad_px,
            (live.width, live.height),
            (window.y, window.height),
            self.visible_band_count(BandEdge::Bottom),
            self.surface_extent().map(|extent| extent.height()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cell height used by the arithmetic below: an odd number, so a
    /// remainder is never accidentally a half cell.
    const CELL_H: u32 = 19;
    const PAD: u32 = 8;
    const ROWS: u16 = 24;

    fn geometry(surface_h: Option<u32>, bottom_bands: u16) -> BandPixelGeometry {
        BandPixelGeometry::new(PAD, (9, CELL_H), (0, ROWS), bottom_bands, surface_h)
    }

    /// Surface height for a padded content extent of `rows` rows plus
    /// `remainder` physical pixels.
    fn surface_for(remainder: u32) -> u32 {
        PAD * 2 + u32::from(ROWS) * CELL_H + remainder
    }

    #[test]
    fn exact_multiple_keeps_the_cell_mapping() {
        // The pre-#3263 behavior, pinned: a window whose padded content is a
        // whole number of rows places every band exactly on its cell row.
        let geo = geometry(Some(surface_for(0)), 1);
        assert_eq!(geo.bottom_remainder(), 0);
        let band = geo.row_band(BandEdge::Bottom, ROWS - 1, 0, 80);
        assert_eq!(
            band.y,
            i32::try_from(PAD + u32::from(ROWS - 1) * CELL_H).unwrap()
        );
        assert_eq!(band.height, CELL_H);
        assert_eq!(
            band.y + i32::try_from(band.height).unwrap(),
            i32::try_from(surface_for(0) - PAD).unwrap(),
            "an exact multiple is already flush with the bottom inset"
        );
    }

    #[test]
    fn remainder_is_absorbed_above_the_band_not_below_it() {
        // bitty#3263: with a 7 px remainder the cell row stops 7 px short of
        // the bottom inset. The band now grows *upward* by those 7 px, so the
        // painted strip reaches the inset and no gap opens above it.
        let remainder = 7;
        let surface = surface_for(remainder);
        let geo = geometry(Some(surface), 1);
        assert_eq!(geo.bottom_remainder(), i32::try_from(remainder).unwrap());
        let band = geo.row_band(BandEdge::Bottom, ROWS - 1, 0, 80);
        let expected_top = i32::try_from(PAD + u32::from(ROWS - 1) * CELL_H).unwrap();
        assert_eq!(band.y, expected_top, "the band top does not move");
        assert_eq!(
            band.height,
            CELL_H + remainder,
            "the remainder is absorbed at the top edge"
        );
        assert_eq!(
            band.y + i32::try_from(band.height).unwrap(),
            i32::try_from(surface - PAD).unwrap(),
            "the band bottom is the physical bottom inset"
        );
    }

    #[test]
    fn stacked_bottom_bands_tile_the_zone_without_a_gap() {
        // Two bands over a 7 px remainder: the outer band sits on the inset,
        // the inner band keeps the container's bottom edge, and the two tile
        // the zone with no seam between them.
        let remainder = 7;
        let surface = surface_for(remainder);
        let geo = geometry(Some(surface), 2);
        let outer = geo.row_band(BandEdge::Bottom, ROWS - 1, 0, 80);
        let inner = geo.row_band(BandEdge::Bottom, ROWS - 2, 0, 80);
        let container_bottom = i32::try_from(PAD + u32::from(ROWS - 2) * CELL_H).unwrap();
        assert_eq!(inner.y, container_bottom, "the stack meets the container");
        assert_eq!(
            inner.height,
            CELL_H + remainder,
            "the innermost band absorbs the remainder"
        );
        assert_eq!(
            inner.y + i32::try_from(inner.height).unwrap(),
            outer.y,
            "the stacked bands are contiguous"
        );
        assert_eq!(
            outer.y + i32::try_from(outer.height).unwrap(),
            i32::try_from(surface - PAD).unwrap(),
            "the outermost band is flush with the bottom inset"
        );
    }

    #[test]
    fn three_stacked_bands_keep_one_cell_budgets_and_a_flush_bottom() {
        let surface = surface_for(5);
        let geo = geometry(Some(surface), 3);
        let rects: Vec<_> = (0..3)
            .map(|index| geo.row_band(BandEdge::Bottom, ROWS - 1 - index, 0, 80))
            .collect();
        // Outermost two own exactly one cell; the innermost absorbs 5 px.
        assert_eq!(rects[0].height, CELL_H);
        assert_eq!(rects[1].height, CELL_H);
        assert_eq!(rects[2].height, CELL_H + 5);
        for pair in rects.windows(2) {
            assert_eq!(
                pair[0].y,
                pair[1].y + i32::try_from(pair[1].height).unwrap(),
                "bands stay contiguous"
            );
        }
        assert_eq!(
            rects[2].y,
            i32::try_from(PAD + u32::from(ROWS - 3) * CELL_H).unwrap(),
            "the innermost band still starts at the container's bottom edge"
        );
        assert_eq!(
            rects[0].y + i32::try_from(rects[0].height).unwrap(),
            i32::try_from(surface - PAD).unwrap()
        );
    }

    #[test]
    fn top_bands_and_column_areas_are_never_displaced() {
        // A remainder must not move any other band: only the bottom edge is
        // anchored physically.
        let geo = geometry(Some(surface_for(11)), 1);
        let top = geo.row_band(BandEdge::Top, 0, 0, 80);
        assert_eq!(top.y, i32::try_from(PAD).unwrap());
        assert_eq!(top.height, CELL_H);
        let second = geo.row_band(BandEdge::Top, 1, 0, 80);
        assert_eq!(second.y, i32::try_from(PAD + CELL_H).unwrap());
        let area = geo.area_band(1, 0, 6, 2);
        assert_eq!(area.y, i32::try_from(PAD + CELL_H).unwrap());
        assert_eq!(area.height, 2 * CELL_H);
        // Left/right content rows paint on their plain cell rows.
        let left_row = geo.row_band(BandEdge::Left, 1, 0, 6);
        assert_eq!((left_row.y, left_row.height), (second.y, second.height));
        assert_eq!(left_row.width, 6 * 9);
    }

    #[test]
    fn missing_surface_keeps_the_pre_fix_cell_mapping() {
        // No configured extent means no remainder to absorb: the mapping is
        // byte-identical to the cell mapping (this is what every pre-#3263
        // headless fixture sees).
        let geo = geometry(None, 1);
        assert_eq!(geo.bottom_remainder(), 0);
        let band = geo.row_band(BandEdge::Bottom, ROWS - 1, 0, 80);
        assert_eq!(
            band.y,
            i32::try_from(PAD + u32::from(ROWS - 1) * CELL_H).unwrap()
        );
        assert_eq!(band.height, CELL_H);
    }

    #[test]
    fn clamped_short_surface_stays_bounded_and_above_the_content() {
        // A surface shorter than one padded cell: the grid clamps to one row,
        // the strip under it is negative, so the anchor falls back to the cell
        // mapping and the rect never starts above the content it labels.
        let surface = 2 * PAD;
        let geo = BandPixelGeometry::new(PAD, (9, CELL_H), (0, 1), 1, Some(surface));
        let band = geo.row_band(BandEdge::Bottom, 0, 0, 80);
        assert_eq!(
            band.y,
            i32::try_from(PAD).unwrap(),
            "starts at the content top"
        );
        assert_eq!(band.height, CELL_H, "the cell mapping stands, as before");
    }

    #[test]
    fn clamped_grid_keeps_the_cell_mapping() {
        // A grid clamped to the row bound (here: two rows on a huge surface)
        // leaves far more than a cell. That strip is not a rounding remainder,
        // so nothing is absorbed and the band stays on its cell row — the
        // pre-#3263 behavior — instead of being dragged to the far edge.
        let geo = BandPixelGeometry::new(PAD, (9, CELL_H), (0, 2), 1, Some(4096));
        assert_eq!(geo.bottom_remainder(), 4096 - 16 - 2 * 19);
        let band = geo.row_band(BandEdge::Bottom, 1, 0, 80);
        assert_eq!(band.height, CELL_H);
        assert_eq!(band.y, i32::try_from(PAD + CELL_H).unwrap());
    }

    #[test]
    fn a_stale_extent_keeps_the_cell_mapping() {
        // Setting the window cells without resizing the surface (a headless
        // fixture, a layout-only caller) leaves an extent that says nothing
        // about this grid. The band must stay on its cell row, not jump to the
        // surface's far bottom edge.
        let geo = BandPixelGeometry::new(PAD, (9, CELL_H), (0, 9), 1, Some(472));
        let band = geo.row_band(BandEdge::Bottom, 8, 0, 80);
        assert_eq!(band.y, i32::try_from(PAD + 8 * CELL_H).unwrap());
        assert_eq!(band.height, CELL_H);
    }

    #[test]
    fn zero_bottom_bands_fall_back_to_the_cell_mapping() {
        let geo = geometry(Some(surface_for(7)), 0);
        let band = geo.row_band(BandEdge::Bottom, ROWS - 1, 0, 80);
        assert_eq!(
            band.y,
            i32::try_from(PAD + u32::from(ROWS - 1) * CELL_H).unwrap()
        );
        assert_eq!(band.height, CELL_H);
    }

    #[test]
    fn contains_is_half_open_on_both_axes() {
        let rect = BandPixels {
            x: 10,
            y: 20,
            width: 30,
            height: 19,
        };
        assert!(rect.contains(10.0, 20.0));
        assert!(rect.contains(39.9, 38.9));
        assert!(
            !rect.contains(40.0, 30.0),
            "right edge belongs to the next rect"
        );
        assert!(
            !rect.contains(20.0, 39.0),
            "bottom edge belongs to the next rect"
        );
        assert!(!rect.contains(9.9, 30.0));
    }

    #[test]
    fn window_origin_shifts_the_absolute_rows() {
        // Band rows arrive window-relative (`plugin_band_row` adds the grid
        // origin). A non-zero origin must shift the whole stack, not flip it.
        let window_y = 4;
        let surface = PAD * 2 + u32::from(window_y + ROWS) * CELL_H;
        let geo = BandPixelGeometry::new(PAD, (9, CELL_H), (window_y, ROWS), 1, Some(surface));
        let band = geo.row_band(BandEdge::Bottom, window_y + ROWS - 1, 0, 80);
        assert_eq!(
            band.y,
            i32::try_from(PAD + u32::from(window_y + ROWS - 1) * CELL_H).unwrap()
        );
        assert_eq!(band.height, CELL_H);
        assert_eq!(geo.bottom_remainder(), 0);
    }
}
