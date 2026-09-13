//! Window padding + opacity wiring (CTX-0223): the config-plane dead knobs.
//!
//! Proves headlessly, without a display server or GPU:
//!
//! - `window.padding` insets the grid within the window: the default
//!   headless surface spans the window extent (grid + twice the padding),
//!   tick translates content by the inset origin, the padding band keeps the
//!   theme background, grid derivation removes the inset before dividing,
//!   and mouse hit-testing subtracts it.
//! - `window.padding` is live-reconcilable: `Runtime::set_window_padding`
//!   adopts a new value without restart (window keeps its size; the grid
//!   absorbs the inset) and rejects out-of-range values fail-closed.
//! - `window.opacity` reaches the platform: the mapping onto winit
//!   transparency is total and fail-soft (opaque stays on the fast path;
//!   unsupported platforms ignore the flag and stay opaque). The live
//!   `WindowHandle::set_opacity` itself needs a real window, so the
//!   headless half asserts the exact mapping it implements.

#![forbid(unsafe_code)]

use bitty_platform::{CursorPosition, PhysicalSize};
use bitty_runtime::{Runtime, RuntimeConfig};

/// Theme background as composited into headless RGBA (premultiplied;
/// `DEFAULT_BG` is fully opaque so straight == premultiplied here).
const BG_PIXEL: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let base = (u64::from(y) * u64::from(width) + u64::from(x)) as usize * 4;
    [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
}

fn count_non_bg(rgba: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut count = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if pixel(rgba, width, x, y) != BG_PIXEL {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn default_surface_spans_window_extent_with_padding() {
    // 80x24 at 9x19 = 720x456 grid; default 8px padding => 736x472 window.
    let rt = make_runtime();
    assert_eq!(rt.window_padding(), 8);
    assert_eq!(rt.config().window_padding, 8);
    assert_eq!(rt.config().pixel_extent(), PhysicalSize::new(720, 456));
    assert_eq!(rt.config().window_extent(), PhysicalSize::new(736, 472));
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(736, 472)));
}

#[test]
fn padding_band_keeps_background_and_content_shifts() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"HELLO");
    rt.tick().expect("first present must draw");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    assert_eq!(rgba.len(), 736 * 472 * 4);
    // Padding bands (top 8 rows, bottom 8 rows, left/right 8 cols) are
    // untouched background: no fills or glyphs land there.
    assert_eq!(count_non_bg(&rgba, 736, 0, 0, 736, 8), 0, "top band");
    assert_eq!(count_non_bg(&rgba, 736, 0, 464, 736, 472), 0, "bottom band");
    assert_eq!(count_non_bg(&rgba, 736, 0, 0, 8, 472), 0, "left band");
    assert_eq!(count_non_bg(&rgba, 736, 728, 0, 736, 472), 0, "right band");
    // The first row of cells (now at y 8..27) carries the HELLO glyphs.
    assert!(
        count_non_bg(&rgba, 736, 8, 8, 8 + 5 * 9, 8 + 19) > 0,
        "shifted content must paint non-background pixels"
    );
}

#[test]
fn zero_padding_restores_legacy_origin() {
    let mut rt = make_runtime();
    rt.set_window_padding(0).expect("zero padding is valid");
    assert_eq!(rt.window_padding(), 0);
    // Window keeps its size; the grid absorbs the freed inset. CTX-0375: the
    // content grid also removes the 14px per-side decoration: the 81x24
    // container leaves 77x22 content cells.
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(736, 472)));
    assert_eq!(rt.snapshot().width, 77, "736/9 container minus decoration");
    assert_eq!(
        rt.snapshot().height,
        22,
        "472/19 container minus decoration"
    );
    rt.handle_pty_bytes(b"HELLO");
    rt.tick().expect("present after padding change");
    let rgba = rt.headless_rgba().expect("rgba");
    // No inset: content starts at the window origin.
    assert!(count_non_bg(&rgba, 736, 0, 0, 5 * 9, 19) > 0);
}

#[test]
fn set_window_padding_is_live_and_fail_closed() {
    let mut rt = make_runtime();
    // Out-of-range values never apply (previous value retained).
    assert!(rt.set_window_padding(65).is_err());
    assert!(rt.set_window_padding(u32::MAX).is_err());
    assert_eq!(rt.window_padding(), 8);
    // Same value is a no-op success.
    rt.set_window_padding(8).expect("same value ok");
    // Growing the inset shrinks the grid in place, no restart.
    rt.set_window_padding(16)
        .expect("valid padding applies live");
    assert_eq!(rt.window_padding(), 16);
    // (736-32)/9 = 78 container cells; minus decoration -> 74 content cells.
    // (472-32)/19 = 23 container rows; minus decoration -> 21 content rows.
    assert_eq!(rt.snapshot().width, 74, "grid absorbs inset + decoration");
    assert_eq!(rt.snapshot().height, 21, "grid absorbs inset + decoration");
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(736, 472)));
    assert!(rt.tick().is_some(), "padding change forces full redraw");
}

#[test]
fn resize_derives_grid_minus_padding() {
    let mut rt = make_runtime();
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("valid resize");
    // (800-16)/9 x (600-16)/19 = 87x30 container cells; minus the 14px
    // per-side decoration inset -> 83x28 content. The window keeps its size.
    assert_eq!((rt.snapshot().width, rt.snapshot().height), (83, 28));
    assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
}

#[test]
fn hit_testing_subtracts_the_inset() {
    let rt = make_runtime();
    // A click inside the padding band is background, not a cell.
    assert!(
        rt.cursor_to_leaf_cell(CursorPosition { x: 4.0, y: 4.0 })
            .is_none(),
        "padding band must not resolve to a leaf cell"
    );
    // The first grid cell starts at the inset origin.
    let (id, cell) = rt
        .cursor_to_leaf_cell(CursorPosition { x: 8.0, y: 8.0 })
        .expect("inset origin is cell (0, 0)");
    assert_eq!((cell.row, cell.col), (0, 0));
    let _ = id;
    // Second column starts one cell past the inset.
    let (_, cell) = rt
        .cursor_to_leaf_cell(CursorPosition { x: 17.0, y: 8.0 })
        .expect("second column");
    assert_eq!((cell.row, cell.col), (0, 1));
}

#[test]
fn opacity_mapping_is_total_and_fail_soft() {
    // Headless half of the opacity wiring: the exact mapping the live
    // `WindowHandle::set_opacity` implements (the handle itself needs a
    // real window, which the seat-holding probe covers).
    use bitty_platform::{opacity_requests_transparency, sanitize_opacity};
    assert_eq!(sanitize_opacity(0.9), 0.9);
    assert_eq!(sanitize_opacity(1.0), 1.0);
    assert_eq!(sanitize_opacity(-1.0), 0.0);
    assert_eq!(sanitize_opacity(99.0), 1.0);
    assert_eq!(sanitize_opacity(f32::NAN), 1.0);
    // Opaque (and invalid, which sanitizes to opaque) keeps the fast
    // non-transparent path; sub-1.0 requests blending where supported and
    // is ignored (still opaque, no error) where not.
    assert!(!opacity_requests_transparency(1.0));
    assert!(!opacity_requests_transparency(f32::NAN));
    assert!(opacity_requests_transparency(0.9));
    assert!(opacity_requests_transparency(0.0));
    // Config default rides through to the platform default (opaque).
    let platform_default = bitty_platform::WindowConfig::default();
    assert!((platform_default.opacity() - 1.0).abs() < f32::EPSILON);
    assert!(!platform_default.is_transparent());
    // A runtime built from defaults carries the matching padding default.
    assert_eq!(
        RuntimeConfig::default().window_padding,
        bitty_runtime::config::DEFAULT_WINDOW_PADDING
    );
}
