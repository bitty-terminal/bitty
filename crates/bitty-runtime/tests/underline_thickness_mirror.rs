//! Cross-crate mirror pin for underline/strikethrough thickness.
//!
//! `bitty-render` (paint pipeline) and `bitty-rich` (headless hyperlink
//! overlay geometry) both compute `height / 8` clamped to `1..=2`, and
//! `bitty-rich` deliberately avoids a `bitty-render` dependency, so the
//! formula is duplicated by value. `bitty-runtime` consumes both crates and
//! is therefore the place where the two copies are compared: drift in either
//! copy fails here instead of silently painting a different bar than the
//! headless overlay reports.

#[test]
fn underline_thickness_matches_across_render_and_rich() {
    for height in [0u32, 1, 7, 8, 9, 15, 16, 17, 22, 32, 64, 127, u32::MAX] {
        assert_eq!(
            bitty_render::grid::underline_thickness(height),
            bitty_rich::hyperlink::underline_thickness(height),
            "mirrored underline thickness drifted at cell height {height}"
        );
    }
}
