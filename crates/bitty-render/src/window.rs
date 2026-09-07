//! Window-presentation math: padding inset and opacity sanitize helpers.
//!
//! CTX-0223 wires the config-plane `window.padding` / `window.opacity` knobs
//! into the live path. This module owns the pure, total, headless-testable
//! half of that wiring:
//!
//! - [`padded_content_rect`] insets the terminal grid inside the window by
//!   `window.padding` logical pixels on every side (ghostty/alacritty-class
//!   window padding). The padding band keeps the theme background (the
//!   surface clear color); grid content is translated by the inset origin.
//! - Opacity itself is a platform concern (`bitty-platform` maps it onto
//!   winit transparency, which has no render-side knob in winit 0.30 —
//!   per-pixel alpha compositing in the pipelines is a documented
//!   follow-up). Nothing here invents an opacity rendering path.
//!
//! Bounds mirror `bitty-config` `WindowConfig` (`0..=64` logical pixels;
//! opacity is not carried here). All arithmetic is overflow-safe
//! (`u64` intermediates, saturating conversion), so hostile inputs are total
//! and can never panic or wrap.

use crate::geometry::{ExtentPx, RectPx};

/// Largest window padding honored, in logical pixels.
///
/// Mirrors `bitty-config` `WindowConfig::validate` (`must be <= 64`) and
/// `bitty-runtime` `MAX_WINDOW_PADDING` by value: `bitty-render` must not
/// depend on `bitty-config`, so the pairing is pinned by tests on the
/// consuming side. Larger inputs are clamped, never rejected, so geometry
/// stays total even for unvalidated callers.
pub const MAX_WINDOW_PADDING_PX: u32 = 64;

/// Clamps a raw padding to the honored range.
///
/// Total for every `u32` input; values above [`MAX_WINDOW_PADDING_PX`]
/// saturate down. Validated callers (config + runtime) never exceed the
/// bound — this is defense-in-depth for direct users of the geometry.
#[must_use]
pub const fn clamp_window_padding(padding: u32) -> u32 {
    if padding > MAX_WINDOW_PADDING_PX {
        MAX_WINDOW_PADDING_PX
    } else {
        padding
    }
}

/// Insets `window` by `padding` logical pixels on every side.
///
/// Returns the content rectangle (origin = inset offset, span = drawable
/// grid area). Returns `None` when no drawable content remains, i.e. twice
/// the clamped padding covers either dimension — callers keep the previous
/// geometry in that case (fail-closed, never a zero/negative surface).
/// Zero padding yields the full-window rectangle.
///
/// The consumer contract (`bitty-runtime` tick + resize):
///
/// - surface extent = window extent (grid pixels + twice the padding);
/// - grid origin = the returned `x`/`y` (content is translated there);
/// - grid derivation subtracts twice the padding before dividing by the
///   cell metrics, so the window — not the grid — absorbs the inset.
#[must_use]
pub fn padded_content_rect(window: &ExtentPx, padding: u32) -> Option<RectPx> {
    let pad = u64::from(clamp_window_padding(padding));
    let inset = pad.saturating_mul(2);
    let width = u64::from(window.width);
    let height = u64::from(window.height);
    if inset >= width || inset >= height {
        return None;
    }
    // `width - inset` is positive and below `u32::MAX` (it shrank), and
    // `pad <= 64` always fits `i32`.
    let content_width = (width - inset) as u32;
    let content_height = (height - inset) as u32;
    Some(RectPx::new(
        pad as i32,
        pad as i32,
        content_width,
        content_height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_matches_config_window_padding() {
        // Pinned by value (see module docs): config rejects `> 64`.
        assert_eq!(MAX_WINDOW_PADDING_PX, 64);
        assert_eq!(clamp_window_padding(0), 0);
        assert_eq!(clamp_window_padding(8), 8);
        assert_eq!(clamp_window_padding(64), 64);
        assert_eq!(clamp_window_padding(65), 64);
        assert_eq!(clamp_window_padding(u32::MAX), 64);
    }

    #[test]
    fn zero_padding_is_the_full_window() {
        let window = ExtentPx::new(736, 472);
        assert_eq!(
            padded_content_rect(&window, 0),
            Some(RectPx::new(0, 0, 736, 472))
        );
    }

    #[test]
    fn default_padding_insets_grid_within_window() {
        // Default 8px padding: an 80x24 grid at 9x19 cells (720x456) sits
        // inside a 736x472 window with origin (8, 8).
        let window = ExtentPx::new(736, 472);
        assert_eq!(
            padded_content_rect(&window, 8),
            Some(RectPx::new(8, 8, 720, 456))
        );
    }

    #[test]
    fn oversized_padding_is_fail_closed() {
        // Twice the padding covering either dimension leaves no content.
        assert_eq!(padded_content_rect(&ExtentPx::new(16, 16), 8), None);
        assert_eq!(padded_content_rect(&ExtentPx::new(100, 10), 8), None);
        assert_eq!(padded_content_rect(&ExtentPx::new(10, 100), 8), None);
        assert_eq!(padded_content_rect(&ExtentPx::new(0, 0), 0), None);
        // Clamped hostile padding still reports honestly on small windows.
        assert_eq!(
            padded_content_rect(&ExtentPx::new(100, 100), u32::MAX),
            None
        );
    }

    #[test]
    fn single_pixel_content_survives() {
        // 2*8 + 1 keeps exactly one drawable pixel row/column.
        assert_eq!(
            padded_content_rect(&ExtentPx::new(17, 17), 8),
            Some(RectPx::new(8, 8, 1, 1))
        );
    }

    #[test]
    fn extremes_do_not_panic_or_wrap() {
        let max = ExtentPx::new(u32::MAX, u32::MAX);
        let rect = padded_content_rect(&max, u32::MAX).expect("max window fits max padding");
        assert_eq!(rect.x, 64);
        assert_eq!(rect.y, 64);
        assert_eq!(rect.width, u32::MAX - 128);
        assert_eq!(rect.height, u32::MAX - 128);
    }
}
