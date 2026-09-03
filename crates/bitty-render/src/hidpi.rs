//! HiDPI scale-aware cell, font, and extent math.
//!
//! All items here are pure, total, and deterministic: no window system,
//! adapter, or filesystem is contacted, so every behavior runs headlessly on
//! CI. The module implements the reference-learned contract (DEC-0004): the
//! surface extent is always physical window pixels, while glyph
//! rasterization matches the *scaled* cell so the compositor never upscales
//! terminal content in the common path.
//!
//! # Reference model
//!
//! Mainstream emulators keep three quantities consistent on every
//! resize/scale change:
//!
//! 1. **Surface extent** = physical window pixels (`logical x scale`,
//!    rounded; see [`surface_extent_for_grid`] and
//!    [`bitty_platform::map_resize_to_surface_extent`]).
//! 2. **Cell metrics** = base (design) cell scaled by the DPI factor and
//!    rounded to whole physical pixels ([`scaled_cell_metrics`]).
//! 3. **Font size** = base point size scaled by the same factor
//!    ([`scaled_point_size`]), with the glyph cache and atlas invalidated so
//!    subsequent frames rasterize at the scaled size
//!    ([`GridRenderer::apply_dpi_scale`](crate::grid::GridRenderer::apply_dpi_scale)).
//!
//! The grid then derives from `physical / scaled cell`
//! ([`grid_from_surface_extent`]), which keeps the per-frame NDC factor
//! ([`crate::batch::derive_scale`]) near 1.0 instead of magnifying 1x
//! texels with linear filtering (soft text) or leaving a stale small
//! surface for the compositor to upscale (blurry text).
//!
//! # Scale-factor handling
//!
//! [`sanitize_dpi_scale`] coerces invalid input (zero, negative, NaN,
//! infinite) to `1.0` and clamps to `[MIN_DPI_SCALE, MAX_DPI_SCALE]`, so
//! hostile or stale factors cannot produce degenerate or gigantic geometry.
//! Fractional factors such as Hyprland's 1.6 pass through unchanged.

use bitty_platform::PhysicalSize;

use crate::grid::CellMetrics;

/// Smallest DPI scale honored after sanitizing.
pub const MIN_DPI_SCALE: f64 = 0.25;

/// Largest DPI scale honored after sanitizing.
pub const MAX_DPI_SCALE: f64 = 4.0;

/// Largest font point size accepted after scaling (mirrors
/// [`crate::glyph::FontQuery::validate`]).
pub const MAX_SCALED_POINT_SIZE: f32 = 3999.0;

/// Coerces any scale factor into the honored range.
///
/// Non-finite and non-positive inputs become `1.0` (no scaling); finite
/// positives clamp to `[MIN_DPI_SCALE, MAX_DPI_SCALE]`. Fractional factors
/// such as `1.6` pass through unchanged.
#[must_use]
pub fn sanitize_dpi_scale(scale: f64) -> f64 {
    if !(scale.is_finite() && scale > 0.0) {
        1.0
    } else {
        scale.clamp(MIN_DPI_SCALE, MAX_DPI_SCALE)
    }
}

/// Scales one cell side to whole physical pixels.
///
/// Rounding is half away from zero with a floor of one pixel, so a scaled
/// cell is never degenerate; saturation keeps hostile inputs overflow-free.
#[must_use]
pub fn scaled_cell_side(base_px: u32, scale: f64) -> u32 {
    let scaled = f64::from(base_px) * sanitize_dpi_scale(scale);
    let rounded = scaled.round();
    if rounded < 1.0 {
        1
    } else if rounded >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        rounded as u32
    }
}

/// Scales base (design) cell metrics to whole physical pixels at `scale`.
///
/// Cell sides are non-zero by construction, and [`scaled_cell_side`] never
/// yields zero, so the result is always valid.
#[must_use]
pub fn scaled_cell_metrics(base: CellMetrics, scale: f64) -> CellMetrics {
    CellMetrics {
        width: scaled_cell_side(base.width, scale),
        height: scaled_cell_side(base.height, scale),
    }
}

/// Scales a base point size by `scale`, clamped to `(0, 3999]`.
///
/// An invalid base (non-finite or non-positive, only reachable from
/// unvalidated callers — [`crate::glyph::FontQuery::validate`] rejects such
/// sizes) falls back to `1.0` before scaling so the result stays total.
#[must_use]
pub fn scaled_point_size(base_pt: f32, scale: f64) -> f32 {
    let base = if base_pt.is_finite() && base_pt > 0.0 {
        base_pt
    } else {
        1.0
    };
    let scaled = base * sanitize_dpi_scale(scale) as f32;
    scaled.clamp(f32::MIN_POSITIVE, MAX_SCALED_POINT_SIZE)
}

/// Surface extent covering a `cols x rows` grid at `cell` metrics.
///
/// Returns `None` when either dimension is zero (nothing to configure —
/// callers skip surface configuration until a non-zero size arrives, per
/// [`bitty_platform::map_resize_to_surface_extent`]).
#[must_use]
pub fn surface_extent_for_grid(
    cols: usize,
    rows: usize,
    cell: CellMetrics,
) -> Option<PhysicalSize> {
    let extent = cell.extent_for(cols, rows);
    bitty_platform::map_resize_to_surface_extent(PhysicalSize::new(extent.width, extent.height))
}

/// Derives grid dimensions from a physical surface extent and cell metrics.
///
/// Divides with a floor and saturates to at least 1x1 (a surface that cannot
/// fit a full cell still addresses one). Cell sides are non-zero by
/// construction, so no division by zero is possible. The caller caps absurd
/// dimensions for its own grid memory; this helper reports the raw quotient.
#[must_use]
pub fn grid_from_surface_extent(extent: PhysicalSize, cell: CellMetrics) -> (usize, usize) {
    let cols = (extent.width() / cell.width).max(1) as usize;
    let rows = (extent.height() / cell.height).max(1) as usize;
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cell() -> CellMetrics {
        CellMetrics::new(8, 16).expect("valid base cell")
    }

    #[test]
    fn sanitize_keeps_valid_and_fractional_scales() {
        assert_eq!(sanitize_dpi_scale(1.0), 1.0);
        assert_eq!(sanitize_dpi_scale(1.6), 1.6);
        assert_eq!(sanitize_dpi_scale(2.0), 2.0);
        assert_eq!(sanitize_dpi_scale(0.25), 0.25);
        assert_eq!(sanitize_dpi_scale(4.0), 4.0);
    }

    #[test]
    fn sanitize_repairs_invalid_and_clamps_hostile() {
        assert_eq!(sanitize_dpi_scale(0.0), 1.0);
        assert_eq!(sanitize_dpi_scale(-1.6), 1.0);
        assert_eq!(sanitize_dpi_scale(f64::NAN), 1.0);
        assert_eq!(sanitize_dpi_scale(f64::INFINITY), 1.0);
        assert_eq!(sanitize_dpi_scale(f64::NEG_INFINITY), 1.0);
        assert_eq!(sanitize_dpi_scale(100.0), MAX_DPI_SCALE);
        assert_eq!(sanitize_dpi_scale(0.01), MIN_DPI_SCALE);
    }

    #[test]
    fn scaled_cells_match_fractional_scale() {
        // Hyprland 1.6: 8x16 design cells rasterize at 13x26 physical pixels.
        let scaled = scaled_cell_metrics(base_cell(), 1.6);
        assert_eq!(scaled, CellMetrics::new(13, 26).expect("valid"));
        assert_eq!(scaled_cell_side(8, 1.0), 8);
        assert_eq!(scaled_cell_side(16, 2.0), 32);
        // Invalid scales fall back to unscaled cells, never zero.
        assert_eq!(scaled_cell_metrics(base_cell(), 0.0), base_cell());
        assert_eq!(scaled_cell_metrics(base_cell(), f64::NAN), base_cell());
        assert_eq!(scaled_cell_side(8, -3.0), 8);
        assert_eq!(scaled_cell_side(1, 0.01), 1, "clamped scale keeps >= 1px");
    }

    #[test]
    fn scaled_point_size_tracks_scale_and_clamps() {
        assert_eq!(scaled_point_size(12.0, 1.0), 12.0);
        assert!((scaled_point_size(12.0, 1.6) - 19.2).abs() < 1e-4);
        assert_eq!(scaled_point_size(12.0, 2.0), 24.0);
        assert_eq!(scaled_point_size(12.0, 0.0), 12.0);
        assert_eq!(scaled_point_size(12.0, f64::NAN), 12.0);
        assert_eq!(scaled_point_size(3999.0, 4.0), MAX_SCALED_POINT_SIZE);
        assert!(scaled_point_size(1.0, 0.25) > 0.0);
        // Invalid bases stay total instead of producing NaN.
        assert_eq!(scaled_point_size(f32::NAN, 1.6), 1.6);
        assert_eq!(scaled_point_size(0.0, 2.0), 2.0);
        assert_eq!(scaled_point_size(-5.0, 2.0), 2.0);
    }

    #[test]
    fn surface_extent_for_grid_round_trips_with_grid_derivation() {
        // 1566x935 logical at 1.6x: physical 2506x1496 holds 192x57 scaled cells.
        let cell = scaled_cell_metrics(base_cell(), 1.6);
        let extent = surface_extent_for_grid(192, 57, cell).expect("non-zero");
        assert_eq!(extent, PhysicalSize::new(192 * 13, 57 * 26));
        let (cols, rows) = grid_from_surface_extent(extent, cell);
        assert_eq!((cols, rows), (192, 57));
        assert!(surface_extent_for_grid(0, 24, cell).is_none());
        assert!(surface_extent_for_grid(80, 0, cell).is_none());
    }

    #[test]
    fn grid_derivation_floors_and_saturates_to_one() {
        let cell = scaled_cell_metrics(base_cell(), 1.6);
        // Physical tiled extent 2506x1496 at 13x26 cells.
        let (cols, rows) = grid_from_surface_extent(PhysicalSize::new(2506, 1496), cell);
        assert_eq!((cols, rows), (192, 57));
        // Unscaled cells reproduce the legacy quotients.
        let (cols, rows) = grid_from_surface_extent(PhysicalSize::new(2506, 1496), base_cell());
        assert_eq!((cols, rows), (313, 93));
        // Degenerate extents still address one cell.
        let (cols, rows) = grid_from_surface_extent(PhysicalSize::new(0, 0), cell);
        assert_eq!((cols, rows), (1, 1));
    }
}
