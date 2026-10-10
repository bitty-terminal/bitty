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
//! - CTX-1076 (`window.background_image` + placement, issue #1815) adds the
//!   pure placement half for the window background image: [`sanitize_window_background_opacity`]
//!   (dim factor), [`reposition_window_background`] (letterbox position), and
//!   [`dim_rgba_alpha`] (in-place alpha multiply). All are total,
//!   allocation-free except the in-place dim (caller-owned buffer), and
//!   headless-testable.
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

/// Window background-image position (CTX-1076, issue #1815).
///
/// Mirrors `bitty-config::BackgroundPosition` by value (`bitty-render` must
/// not depend on `bitty-config` for this helper; the pairing is pinned by
/// the app-level test): nine closed CSS `background-position` values.
/// Only `fit`/`center` letterbox observes it; `fill`/`stretch`/`tile`
/// ignore it (documented, never an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WindowBackgroundPosition {
    /// Centered both axes (default).
    #[default]
    Center,
    /// Top-left corner.
    TopLeft,
    /// Top edge, horizontally centered.
    Top,
    /// Top-right corner.
    TopRight,
    /// Left edge, vertically centered.
    Left,
    /// Right edge, vertically centered.
    Right,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom edge, horizontally centered.
    Bottom,
    /// Bottom-right corner.
    BottomRight,
}

impl WindowBackgroundPosition {
    /// Parses a canonical position name (exact, case-sensitive).
    ///
    /// Total: returns `None` for anything but the nine accepted spellings.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "center" => Some(Self::Center),
            "top-left" => Some(Self::TopLeft),
            "top" => Some(Self::Top),
            "top-right" => Some(Self::TopRight),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "bottom-left" => Some(Self::BottomLeft),
            "bottom" => Some(Self::Bottom),
            "bottom-right" => Some(Self::BottomRight),
            _ => None,
        }
    }

    /// Canonical spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::TopLeft => "top-left",
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::Left => "left",
            Self::Right => "right",
            Self::BottomLeft => "bottom-left",
            Self::Bottom => "bottom",
            Self::BottomRight => "bottom-right",
        }
    }
}

/// Sanitizes a window background dim factor (CTX-1076
/// `window.background_opacity`).
///
/// Total for every `f32`: non-finite saturates to `1.0` (opaque), finite
/// values clamp to `[0.0, 1.0]`. Validated callers (config + runtime) never
/// exceed the bound — this is defense-in-depth for direct users of the
/// raster path.
#[must_use]
pub fn sanitize_window_background_opacity(opacity: f32) -> f32 {
    if !opacity.is_finite() {
        1.0
    } else {
        opacity.clamp(0.0, 1.0)
    }
}

/// Repositions a letterboxed background blit inside its outer window rect
/// (CTX-1076 `window.background_position`).
///
/// `outer` is the window rect, `inner` the rasterized blit dest (already
/// clipped to `outer` by the fit: `inner.width <= outer.width` and
/// `inner.height <= outer.height` for `fit`/`center`; `fill`/`stretch`/`tile`
/// cover `outer` so the result equals `inner`). Returns the repositioned
/// rect with the same size, moved so the empty bands sit per `position`.
/// All arithmetic is saturating, so hostile inputs are total and can never
/// panic or wrap.
#[must_use]
pub fn reposition_window_background(
    outer: &RectPx,
    inner: &RectPx,
    position: WindowBackgroundPosition,
) -> RectPx {
    let dw = u64::from(outer.width);
    let dh = u64::from(outer.height);
    let iw = u64::from(inner.width);
    let ih = u64::from(inner.height);
    // `fill`/`stretch`/`tile` cover: no letterbox, position ignored.
    if iw >= dw && ih >= dh {
        return *inner;
    }
    let free_w = dw.saturating_sub(iw);
    let free_h = dh.saturating_sub(ih);
    // Horizontal offset: 0 = left, half = center, full = right.
    let ox = match position {
        WindowBackgroundPosition::TopLeft
        | WindowBackgroundPosition::Left
        | WindowBackgroundPosition::BottomLeft => 0,
        WindowBackgroundPosition::Top
        | WindowBackgroundPosition::Center
        | WindowBackgroundPosition::Bottom => free_w / 2,
        WindowBackgroundPosition::TopRight
        | WindowBackgroundPosition::Right
        | WindowBackgroundPosition::BottomRight => free_w,
    };
    // Vertical offset: 0 = top, half = center, full = bottom.
    let oy = match position {
        WindowBackgroundPosition::TopLeft
        | WindowBackgroundPosition::Top
        | WindowBackgroundPosition::TopRight => 0,
        WindowBackgroundPosition::Left
        | WindowBackgroundPosition::Center
        | WindowBackgroundPosition::Right => free_h / 2,
        WindowBackgroundPosition::BottomLeft
        | WindowBackgroundPosition::Bottom
        | WindowBackgroundPosition::BottomRight => free_h,
    };
    let x = outer
        .x
        .saturating_add(i32::try_from(ox).unwrap_or(i32::MAX));
    let y = outer
        .y
        .saturating_add(i32::try_from(oy).unwrap_or(i32::MAX));
    RectPx::new(x, y, inner.width, inner.height)
}

/// Dims straight-alpha RGBA8 bytes in place by multiplying the alpha channel
/// (CTX-1076 `window.background_opacity`).
///
/// `rgba` is `width * height * 4` straight-alpha bytes; `opacity` is
/// sanitized first (non-finite becomes opaque). `1.0` is a no-op (early-out,
/// no write); `0.0` clears every alpha to transparent. RGB is preserved so
/// the image blends over the theme background. Total for every input length
/// (trailing partial pixels are ignored, never panicking).
pub fn dim_rgba_alpha(rgba: &mut [u8], opacity: f32) {
    let opacity = sanitize_window_background_opacity(opacity);
    if (opacity - 1.0).abs() < f32::EPSILON {
        return;
    }
    if opacity <= 0.0 {
        for chunk in rgba.chunks_exact_mut(4) {
            chunk[3] = 0;
        }
        return;
    }
    for chunk in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(chunk[3]);
        // Widening `as` casts are lossless (u8 -> u16); the product of
        // `alpha * opacity` stays in u8 range via the 0..255 scale.
        let dimmed = ((f64::from(alpha) * f64::from(opacity)).round() as u16).min(255) as u8;
        chunk[3] = dimmed;
    }
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

    #[test]
    fn background_opacity_sanitize_is_total() {
        assert_eq!(sanitize_window_background_opacity(1.0), 1.0);
        assert_eq!(sanitize_window_background_opacity(0.0), 0.0);
        assert_eq!(sanitize_window_background_opacity(0.5), 0.5);
        assert_eq!(sanitize_window_background_opacity(-0.5), 0.0);
        assert_eq!(sanitize_window_background_opacity(2.0), 1.0);
        assert_eq!(sanitize_window_background_opacity(f32::NAN), 1.0);
        assert_eq!(sanitize_window_background_opacity(f32::INFINITY), 1.0);
        assert_eq!(
            WindowBackgroundPosition::parse("center"),
            Some(WindowBackgroundPosition::Center)
        );
        assert_eq!(
            WindowBackgroundPosition::parse("top-left"),
            Some(WindowBackgroundPosition::TopLeft)
        );
        assert_eq!(WindowBackgroundPosition::parse("cover"), None);
        assert_eq!(WindowBackgroundPosition::parse(""), None);
    }

    #[test]
    fn background_position_moves_letterbox_inside_window() {
        let outer = RectPx::new(0, 0, 100, 100);
        let inner = RectPx::new(40, 40, 20, 20);
        // Center (default) keeps the centered dest.
        assert_eq!(
            reposition_window_background(&outer, &inner, WindowBackgroundPosition::Center),
            RectPx::new(40, 40, 20, 20)
        );
        assert_eq!(
            reposition_window_background(&outer, &inner, WindowBackgroundPosition::TopLeft),
            RectPx::new(0, 0, 20, 20)
        );
        assert_eq!(
            reposition_window_background(&outer, &inner, WindowBackgroundPosition::BottomRight),
            RectPx::new(80, 80, 20, 20)
        );
        assert_eq!(
            reposition_window_background(&outer, &inner, WindowBackgroundPosition::Top),
            RectPx::new(40, 0, 20, 20)
        );
        // Covering dest ignores position.
        let cover = RectPx::new(0, 0, 100, 100);
        assert_eq!(
            reposition_window_background(&outer, &cover, WindowBackgroundPosition::TopLeft),
            cover
        );
    }

    #[test]
    fn background_dim_multiplies_alpha_only() {
        let mut rgba = vec![200, 100, 50, 255, 10, 20, 30, 128];
        dim_rgba_alpha(&mut rgba, 1.0);
        assert_eq!(rgba, vec![200, 100, 50, 255, 10, 20, 30, 128]);
        dim_rgba_alpha(&mut rgba, 0.5);
        assert_eq!(rgba[0..3], [200, 100, 50]);
        assert_eq!(rgba[3], 128);
        assert_eq!(rgba[7], 64);
        dim_rgba_alpha(&mut rgba, 0.0);
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[7], 0);
    }
}
