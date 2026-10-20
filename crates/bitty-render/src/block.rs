//! Cell-box rasterization for Unicode block elements `U+2580-U+259F`.
//!
//! Font block glyphs are outline bitmaps chosen for text, not cell tiling:
//! their side bearings and antialiased edges do not have to land on the
//! terminal cell box. At a fractional DPI scale (for example 1.6) the live
//! JetBrainsMono Nerd Font full block rasterizes as a nominal 16x35 bitmap
//! inside a 16x35 cell, but its rightmost column carries only about 88/255
//! coverage and the baseline-relative top can sit one pixel above the cell.
//! Two adjacent block cells therefore meet with a partially covered boundary
//! column instead of two full-coverage edges: the 1 px grid seam visible in
//! figlet/lolcat art.
//!
//! Block elements are geometry, not typography. This module rasterizes their
//! Unicode box directly against the cell rectangle (eighth-block boundaries
//! are rounded once per axis, so complementary shapes share the same split),
//! giving every cell edge full coverage. Shades `U+2591-U+2593` keep their
//! documented 25/50/75% ink fraction as a uniform coverage level: unlike a
//! dither pattern, uniform coverage has no phase, so adjacent shade cells
//! tile without a pattern discontinuity at every cell boundary.
//!
//! Terminal Truth is untouched: this is a render-side substitution for the
//! glyph mask only, keyed by the cell's scalar. Selection, copy, search, and
//! accessibility still read the terminal-state cell.

use crate::fallback::is_block_element;

/// Upper bound on one cell-box mask allocation (width * height bytes).
///
/// A real cell is a few thousand pixels; the bound keeps a hostile or broken
/// cell metric from turning block emission into an unbounded allocation.
const MAX_BLOCK_MASK_BYTES: u64 = 1 << 20;

/// Light shade (`U+2591`) coverage: 25% of 255, rounded to nearest.
const LIGHT_SHADE_COVERAGE: u8 = 64;
/// Medium shade (`U+2592`) coverage: 50% of 255.
const MEDIUM_SHADE_COVERAGE: u8 = 128;
/// Dark shade (`U+2593`) coverage: 75% of 255, rounded to nearest.
const DARK_SHADE_COVERAGE: u8 = 191;

/// One rectangle in eighth-cell coordinates: `(x0, y0, x1, y1)` where `8`
/// is the right/bottom cell edge.
type Rect8 = (u8, u8, u8, u8);

/// Full-cell rectangle for `U+2588 FULL BLOCK`.
const FULL_BLOCK: &[Rect8] = &[(0, 0, 8, 8)];

/// Rectangles for one block scalar, in eighth-cell coordinates.
///
/// Every shape is expressed with the same edge function
/// ([`eighth_edge`]), so adjacent/competing shapes (for example upper half
/// and lower half, or left eighth and left seven eighths) split on exactly
/// the same pixel boundary and never leave an uncovered row or column.
const fn block_rects(character: char) -> &'static [Rect8] {
    match character {
        // U+2580-U+2587: horizontal bands.
        '\u{2580}' => &[(0, 0, 8, 4)], // upper half
        '\u{2581}' => &[(0, 7, 8, 8)], // lower one eighth
        '\u{2582}' => &[(0, 6, 8, 8)], // lower one quarter
        '\u{2583}' => &[(0, 5, 8, 8)], // lower three eighths
        '\u{2584}' => &[(0, 4, 8, 8)], // lower half
        '\u{2585}' => &[(0, 3, 8, 8)], // lower five eighths
        '\u{2586}' => &[(0, 2, 8, 8)], // lower three quarters
        '\u{2587}' => &[(0, 1, 8, 8)], // lower seven eighths
        // U+2588: full block.
        '\u{2588}' => FULL_BLOCK,
        // U+2589-U+258F: left-hand bands.
        '\u{2589}' => &[(0, 0, 7, 8)], // left seven eighths
        '\u{258A}' => &[(0, 0, 6, 8)], // left three quarters
        '\u{258B}' => &[(0, 0, 5, 8)], // left five eighths
        '\u{258C}' => &[(0, 0, 4, 8)], // left half
        '\u{258D}' => &[(0, 0, 3, 8)], // left three eighths
        '\u{258E}' => &[(0, 0, 2, 8)], // left one quarter
        '\u{258F}' => &[(0, 0, 1, 8)], // left one eighth
        // U+2590-U+2595: right-hand and upper bands.
        '\u{2590}' => &[(4, 0, 8, 8)], // right half
        '\u{2594}' => &[(0, 0, 8, 1)], // upper one eighth
        '\u{2595}' => &[(7, 0, 8, 8)], // right one eighth
        // U+2596-U+259F: quadrants and their complements.
        '\u{2596}' => &[(0, 4, 4, 8)],               // lower left
        '\u{2597}' => &[(4, 4, 8, 8)],               // lower right
        '\u{2598}' => &[(0, 0, 4, 4)],               // upper left
        '\u{2599}' => &[(0, 0, 4, 8), (4, 4, 8, 8)], // all but upper right
        '\u{259A}' => &[(0, 0, 4, 4), (4, 4, 8, 8)], // upper left + lower right
        '\u{259B}' => &[(0, 0, 8, 4), (0, 4, 4, 8)], // all but lower right
        '\u{259C}' => &[(0, 0, 8, 4), (4, 4, 8, 8)], // all but lower left
        '\u{259D}' => &[(4, 0, 8, 4)],               // upper right
        '\u{259E}' => &[(4, 0, 8, 4), (0, 4, 4, 8)], // upper right + lower left
        '\u{259F}' => &[(4, 0, 8, 8), (0, 4, 4, 8)], // all but upper left
        // Shades are not rectangles; handled by `coverage_mask`.
        _ => &[],
    }
}

/// Rounds one eighth-cell coordinate to a pixel edge.
///
/// All shapes on one axis share this function, so the boundary between
/// complementary eighths is identical for both shapes even when the cell
/// pixel count is not divisible by eight.
fn eighth_edge(eighths: u8, extent: u32) -> u32 {
    let scaled = u64::from(eighths)
        .saturating_mul(u64::from(extent))
        .saturating_add(4)
        / 8;
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// Builds the coverage mask for one block element at `width` x `height`,
/// or `None` when `character` is outside `U+2580-U+259F`.
///
/// Coverage is `0` (cell background) or the shade level for full ink; solid
/// shapes use `255`. The mask is row-major and exactly `width * height`
/// bytes, so an atlas slot built from it spans the whole cell box.
#[must_use]
pub(crate) fn coverage_mask(character: char, width: u32, height: u32) -> Option<Vec<u8>> {
    if !is_block_element(character) || width == 0 || height == 0 {
        return None;
    }
    let len = u64::from(width).checked_mul(u64::from(height))?;
    if len > MAX_BLOCK_MASK_BYTES {
        return None;
    }
    let len = usize::try_from(len).ok()?;
    let mut mask = vec![0u8; len];

    let shade = match character {
        '\u{2591}' => Some(LIGHT_SHADE_COVERAGE),
        '\u{2592}' => Some(MEDIUM_SHADE_COVERAGE),
        '\u{2593}' => Some(DARK_SHADE_COVERAGE),
        _ => None,
    };
    if let Some(level) = shade {
        mask.fill(level);
        return Some(mask);
    }

    let stride = usize::try_from(width).ok()?;
    for &(x0, y0, x1, y1) in block_rects(character) {
        let px0 = eighth_edge(x0, width);
        let px1 = eighth_edge(x1, width);
        let py0 = eighth_edge(y0, height);
        let py1 = eighth_edge(y1, height);
        if px0 >= px1 || py0 >= py1 {
            continue;
        }
        let row_start = usize::try_from(px0).ok()?;
        let row_end = usize::try_from(px1).ok()?;
        for y in py0..py1 {
            let start = usize::try_from(y).ok()?.checked_mul(stride)?;
            let start = start.checked_add(row_start)?;
            let end = start.checked_add(row_end.checked_sub(row_start)?)?;
            mask.get_mut(start..end)?.fill(255);
        }
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_block_scalar_has_a_mask() {
        for cp in 0x2580u32..=0x259F {
            let character = char::from_u32(cp).expect("block range is valid");
            let mask = coverage_mask(character, 16, 35).expect("block scalar covered");
            assert_eq!(mask.len(), 16 * 35, "U+{cp:04X}");
            assert!(mask.iter().any(|&v| v > 0), "U+{cp:04X} must paint ink");
        }
    }

    #[test]
    fn non_block_scalars_are_not_rasterized_here() {
        for character in ['A', '\u{2500}', '\u{2502}', '\u{2800}', '\u{28FF}'] {
            assert!(coverage_mask(character, 16, 35).is_none(), "{character:?}");
        }
    }

    #[test]
    fn full_block_covers_every_pixel() {
        let mask = coverage_mask('\u{2588}', 16, 35).expect("full block");
        assert!(mask.iter().all(|&v| v == 255));
    }

    #[test]
    fn complementary_bands_share_their_split() {
        // Upper and lower halves, and left/right halves, must split at the
        // same pixel even for odd cell dimensions (no gap, no double row).
        for (width, height) in [(16, 35), (13, 27), (15, 34), (7, 9)] {
            let upper = coverage_mask('\u{2580}', width, height).expect("upper half");
            let lower = coverage_mask('\u{2584}', width, height).expect("lower half");
            for y in 0..height {
                for x in 0..width {
                    let i = (y * width + x) as usize;
                    assert_eq!(
                        u16::from(upper[i]) + u16::from(lower[i]),
                        255,
                        "vertical split at {width}x{height} ({x},{y})"
                    );
                }
            }
            let left = coverage_mask('\u{258C}', width, height).expect("left half");
            let right = coverage_mask('\u{2590}', width, height).expect("right half");
            for y in 0..height {
                for x in 0..width {
                    let i = (y * width + x) as usize;
                    assert_eq!(
                        u16::from(left[i]) + u16::from(right[i]),
                        255,
                        "horizontal split at {width}x{height} ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn shades_keep_quarter_half_three_quarter_coverage() {
        for (character, expected) in [
            ('\u{2591}', 0.25f64),
            ('\u{2592}', 0.50f64),
            ('\u{2593}', 0.75f64),
        ] {
            let mask = coverage_mask(character, 16, 35).expect("shade");
            let total: u64 = mask.iter().map(|&v| u64::from(v)).sum();
            let coverage = total as f64 / (255.0 * f64::from(16 * 35));
            assert!(
                (coverage - expected).abs() <= 0.02,
                "{character:?} coverage {coverage:.3} != {expected:.2}"
            );
        }
    }
}
