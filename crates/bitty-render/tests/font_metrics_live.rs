//! Live resolved-font metric regression tests for #2520.
//!
//! The owner report (`font = { family = "DejaVu Sans Mono", size = 11 }`)
//! depends on two things the deterministic fake rasterizers cannot observe:
//!
//! - the point size actually handed to the platform rasterizer is the
//!   configured size scaled by the live DPI factor (`11pt * 1.6 = 17.6pt`),
//!   and
//! - the measured advance at that size still fits the integer cell the
//!   config derives (`9x20` base, `14x32` at 1.6).
//!
//! A fractional resolution (`11pt * 96/72 = 14.6667px`; `17.6pt = 23.4667px`)
//! is normal for FreeType, but the *rounded* advance must match the cell or
//! neighbours overlap and stems collide. These tests pin that contract on a
//! real host font stack.
//!
//! Gated behind `BITTY_RENDER_FONT_TESTS=1` (the same gate as
//! `glyph_fallback_live.rs`) and skipped when the host has no DejaVu face, so
//! CI stays deterministic (the deterministic half of this contract lives in
//! `bitty-config`'s `default_effective_cell` tests).
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test font_metrics_live
//! ```

use bitty_config::types::FontConfig;
use bitty_render::grid::{CellMetrics, resolve_baseline_offset};
use bitty_render::{
    CrossFontRasterizer, FontMetrics, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, RasterKey,
};

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const FAMILY: &str = "DejaVu Sans Mono";
const POINT_SIZE: f32 = 11.0;
const SCALE: f64 = 1.6;

fn live_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

fn query(point_size: f32) -> FontQuery {
    FontQuery {
        family: FAMILY.to_string(),
        style: FontStyle::Normal,
        point_size,
    }
}

/// Loads the owner's face at `point_size`; `None` when the gate is off or
/// the host has no DejaVu Sans Mono face.
fn load(point_size: f32) -> Option<(CrossFontRasterizer, bitty_render::FontId)> {
    if !live_tests_enabled() {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run live font tests");
        return None;
    }
    let mut raster = CrossFontRasterizer::new().ok()?;
    match raster.load_font(&query(point_size)) {
        Ok(font) => Some((raster, font)),
        Err(err) => {
            eprintln!("skipped: {FAMILY} at {point_size}pt unavailable: {err:?}");
            None
        }
    }
}

fn metrics(
    raster: &mut CrossFontRasterizer,
    font: bitty_render::FontId,
    point_size: f32,
) -> FontMetrics {
    let measured = raster
        .font_metrics(font, point_size)
        .expect("metrics query must not fail on a live stack");
    measured.expect("crossfont reports metrics for a scalable face")
}

fn rasterize(
    raster: &mut CrossFontRasterizer,
    font: bitty_render::FontId,
    c: char,
    size: f32,
) -> GlyphBitmap {
    raster
        .rasterize(RasterKey::new(c, font, size).unwrap())
        .expect("rasterization must not fail")
        .expect("DejaVu Sans Mono covers ASCII")
}

#[test]
fn size_11_metrics_fit_the_derived_cell() {
    let Some((mut raster, font)) = load(POINT_SIZE) else {
        return;
    };
    let m = metrics(&mut raster, font, POINT_SIZE);
    assert!(m.is_usable(), "resolved metrics must be usable: {m:?}");

    // 11pt at 96 DPI is 14.6667px; DejaVu's 0.602 em advance is 8.83px and
    // rounds to the 9px cell width.
    let cell = FontConfig {
        family: FAMILY.to_string(),
        size: POINT_SIZE,
        ..FontConfig::default()
    }
    .default_effective_cell();
    assert_eq!(cell, (9, 20), "config-derived cell at 11pt changed");

    let glyph = rasterize(&mut raster, font, 'M', POINT_SIZE);
    assert_eq!(
        glyph.metrics.advance[0], 9,
        "rounded advance must equal the cell width or cells overlap"
    );
    assert!(
        glyph.metrics.width <= 10,
        "glyph bitmap width {} exceeds the cell (clipping/overlap)",
        glyph.metrics.width
    );

    // The metric-aware baseline (line height + descent) must stay inside the
    // 20px cell instead of falling back to the 3/4 rule.
    let basis = resolve_baseline_offset(cell.1, Some(m));
    assert!(
        (1..=i64::from(cell.1)).contains(&basis),
        "baseline {basis} outside the {}px cell",
        cell.1
    );
}

#[test]
fn size_11_at_scale_1_6_keeps_advance_and_cell_consistent() {
    let Some((mut raster, font)) = load(POINT_SIZE * SCALE as f32) else {
        return;
    };
    let scaled = POINT_SIZE * SCALE as f32; // 17.6pt => 23.4667px
    let m = metrics(&mut raster, font, scaled);
    assert!(
        m.is_usable(),
        "resolved scaled metrics must be usable: {m:?}"
    );

    // Runtime cell derivation: 9x20 * 1.6 rounds to 14x32 (hidpi path).
    let base = FontConfig {
        family: FAMILY.to_string(),
        size: POINT_SIZE,
        ..FontConfig::default()
    }
    .default_effective_cell();
    let live = CellMetrics::new(
        bitty_render::hidpi::scaled_cell_side(base.0, SCALE),
        bitty_render::hidpi::scaled_cell_side(base.1, SCALE),
    )
    .expect("scaled cell is non-zero");
    assert_eq!((live.width, live.height), (14, 32));

    // 23.4667px * 0.602 em = 14.13px, rounded to the 14px cell.
    let glyph = rasterize(&mut raster, font, 'M', scaled);
    assert_eq!(
        glyph.metrics.advance[0], live.width as i32,
        "scaled advance must match the scaled cell; fractional point sizes \
         must not drift a pixel off the cell"
    );
    assert!(
        glyph.metrics.width <= live.width as i32 + 1,
        "scaled glyph bitmap width {} exceeds cell {}",
        glyph.metrics.width,
        live.width
    );
}

#[test]
fn fractional_point_size_is_not_truncated_to_whole_points() {
    // 11.9pt crosses a whole-pixel advance boundary (9.54px -> 10) while
    // 11pt stays at 9. A truncating conversion would report 9 for both.
    // (The owner's 17.6pt happens to round to the same 14px advance as
    // 17pt, so it cannot distinguish truncation by advance alone.)
    let Some((mut whole_raster, whole_font)) = load(11.0) else {
        return;
    };
    let Some((mut fractional_raster, fractional_font)) = load(11.9) else {
        return;
    };
    let whole = metrics(&mut whole_raster, whole_font, 11.0);
    let fractional = metrics(&mut fractional_raster, fractional_font, 11.9);
    assert_eq!(whole.average_advance_px, 9.0);
    assert_eq!(
        fractional.average_advance_px, 10.0,
        "11.9pt must not truncate to 11pt (fractional advance 9.54px rounds to 10)"
    );
    assert!(whole.is_usable() && fractional.is_usable());
}
