#![forbid(unsafe_code)]
//! Background-image fit semantics at the runtime seam (CTX-1396, #2332).
//!
//! The pure cover/contain matrix lives in `bitty-rich`; this file proves the
//! runtime wires it to the right frame for both background tiers:
//!
//! - `window.background_image` is sized from the live surface extent, so it
//!   adapts to every window resize and never leaves edge gaps with `fill`;
//! - `decoration.background_image` is sized from each leaf's content frame,
//!   so a split, a divider move, and a window resize re-derive the destination;
//! - `background_opacity` dims the image alpha over the theme background and
//!   `window.opacity` scales the finished premultiplied surface once, with no
//!   double-dimming of a single knob.

use bitty_platform::PhysicalSize;
use bitty_runtime::{
    LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewAppearanceRule, ViewId,
};

/// 160x90 (16:9) opaque-red PNG.
const RED_16_9_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0xA0, 0x00, 0x00, 0x00, 0x5A, 0x08, 0x06, 0x00, 0x00, 0x00, 0x3F, 0xC6, 0x5D,
    0x52, 0x00, 0x00, 0x00, 0xBB, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0xED, 0xD2, 0x31, 0x11, 0x00,
    0x00, 0x08, 0xC4, 0xB0, 0x07, 0xFF, 0x9E, 0xC1, 0x01, 0x2B, 0x4B, 0x22, 0xA1, 0xD7, 0x9A, 0x64,
    0x02, 0x4F, 0x5A, 0x02, 0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10, 0x0C, 0x88, 0x01, 0xC1, 0x80,
    0x18, 0x10, 0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10, 0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10,
    0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10, 0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10, 0x0C, 0x88,
    0x01, 0xC1, 0x80, 0x18, 0x10, 0x0C, 0x88, 0x01, 0xC1, 0x80, 0x18, 0x10, 0x03, 0x82, 0x01, 0x31,
    0x20, 0x18, 0x10, 0x03, 0x82, 0x01, 0x31, 0x20, 0x18, 0x10, 0x03, 0x82, 0x01, 0x31, 0x20, 0x18,
    0x10, 0x03, 0x82, 0x01, 0x31, 0x20, 0x18, 0x10, 0x03, 0x82, 0x01, 0x31, 0x20, 0x18, 0x10, 0x03,
    0x82, 0x01, 0x31, 0x20, 0x18, 0x10, 0x03, 0x82, 0x01, 0x31, 0x20, 0x18, 0x10, 0x03, 0x82, 0x01,
    0x31, 0x20, 0x18, 0x10, 0x03, 0x62, 0x40, 0x30, 0x20, 0x06, 0x04, 0x03, 0x62, 0x40, 0x30, 0x20,
    0x06, 0x04, 0x03, 0x62, 0x40, 0x30, 0x20, 0x06, 0x04, 0x03, 0x62, 0x40, 0x30, 0x20, 0x06, 0x04,
    0x03, 0x62, 0x40, 0x30, 0x20, 0x06, 0x04, 0x03, 0x62, 0x40, 0x30, 0x20, 0x06, 0x84, 0xDB, 0x02,
    0x26, 0x4E, 0x02, 0xB2, 0xBE, 0xC2, 0x3F, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// 120x90 (4:3) opaque-red PNG.
const RED_4_3_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00, 0x5A, 0x08, 0x06, 0x00, 0x00, 0x00, 0x73, 0x00, 0x92,
    0xEF, 0x00, 0x00, 0x00, 0x9E, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0xED, 0xD1, 0x01, 0x01, 0x00,
    0x00, 0x04, 0xC4, 0xC0, 0xA7, 0x7F, 0x67, 0x7A, 0x70, 0x8B, 0xB0, 0xAB, 0x49, 0x26, 0x3A, 0x5B,
    0x5B, 0x00, 0x58, 0x80, 0x05, 0x58, 0x80, 0x05, 0x58, 0x80, 0x05, 0x18, 0xB0, 0x00, 0x0B, 0xB0,
    0x00, 0x0B, 0xB0, 0x00, 0x0B, 0x30, 0x60, 0x01, 0x16, 0x60, 0x01, 0x16, 0x60, 0x01, 0x16, 0x60,
    0x01, 0x06, 0x2C, 0xC0, 0x02, 0x2C, 0xC0, 0x02, 0x2C, 0xC0, 0x02, 0x0C, 0x58, 0x80, 0x05, 0x58,
    0x80, 0x05, 0x58, 0x80, 0x05, 0x18, 0xB0, 0x00, 0x0B, 0xB0, 0x00, 0x0B, 0xB0, 0x00, 0x0B, 0xB0,
    0x00, 0x03, 0x16, 0x60, 0x01, 0x16, 0x60, 0x01, 0x16, 0x60, 0x01, 0x06, 0x2C, 0xC0, 0x02, 0x2C,
    0xC0, 0x02, 0x2C, 0xC0, 0x02, 0x0C, 0x58, 0x80, 0x05, 0x58, 0x80, 0x05, 0x58, 0x80, 0x05, 0x58,
    0x80, 0x01, 0x0B, 0xB0, 0x00, 0x0B, 0xB0, 0x00, 0x0B, 0xB0, 0x00, 0x03, 0x16, 0x60, 0x01, 0x16,
    0x60, 0x01, 0x16, 0x60, 0x01, 0x16, 0x60, 0xC0, 0x02, 0x2C, 0xC0, 0x02, 0x2C, 0xC0, 0x02, 0x2C,
    0xC0, 0xBF, 0x5A, 0x10, 0x52, 0x02, 0xB2, 0x5A, 0x49, 0x41, 0xFD, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 90x90 (1:1) opaque-red PNG.
const RED_1_1_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x5A, 0x00, 0x00, 0x00, 0x5A, 0x08, 0x06, 0x00, 0x00, 0x00, 0x38, 0xA8, 0x41,
    0x02, 0x00, 0x00, 0x00, 0x8B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0xED, 0xD0, 0x41, 0x0D, 0x00,
    0x00, 0x08, 0x04, 0xA0, 0xD3, 0xFE, 0x9D, 0x35, 0x85, 0x9B, 0x0F, 0x88, 0x40, 0x4D, 0x32, 0xE1,
    0x5C, 0x2B, 0x10, 0x2D, 0x1A, 0xD1, 0xA2, 0x45, 0x23, 0x5A, 0x34, 0xA2, 0x45, 0x8B, 0x46, 0xB4,
    0x68, 0x44, 0x8B, 0x16, 0x8D, 0x68, 0xD1, 0x88, 0x16, 0x2D, 0x1A, 0xD1, 0xA2, 0x11, 0x2D, 0x5A,
    0x34, 0xA2, 0x45, 0x23, 0x5A, 0xB4, 0x68, 0x44, 0x8B, 0x46, 0xB4, 0x68, 0xD1, 0x88, 0x16, 0x8D,
    0x68, 0xD1, 0xA2, 0x11, 0x2D, 0x1A, 0xD1, 0xA2, 0x45, 0x23, 0x5A, 0x34, 0xA2, 0x45, 0x8B, 0x46,
    0xB4, 0x68, 0x44, 0x8B, 0x16, 0x8D, 0x68, 0xD1, 0x88, 0x16, 0x2D, 0x1A, 0xD1, 0xA2, 0x11, 0x2D,
    0x5A, 0x34, 0xA2, 0x45, 0x23, 0x5A, 0xB4, 0x68, 0x44, 0x8B, 0x46, 0xB4, 0x68, 0xD1, 0x88, 0x16,
    0x8D, 0x68, 0xD1, 0xA2, 0x11, 0x2D, 0x1A, 0xD1, 0xA2, 0x45, 0x23, 0x5A, 0x34, 0xA2, 0xBF, 0x58,
    0xFF, 0xC6, 0x02, 0xB2, 0x1E, 0x13, 0x90, 0xD3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// 2x2 checkerboard PNG: (0,0) black, (1,0) white, (0,1) white, (1,1) black.
const CHECKER_2X2_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xB6, 0x0D,
    0x24, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x64, 0x60, 0x60, 0xF8,
    0xFF, 0xFF, 0xFF, 0x7F, 0x18, 0xC9, 0xF0, 0x1F, 0x00, 0x4C, 0xD7, 0x08, 0xF9, 0xC3, 0xC9, 0x39,
    0x9A, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1396-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write_image(root: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let path = root.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path.display().to_string()
}

fn single_leaf(rt: &mut Runtime, id: u64) {
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(id), 80, 24)));
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn theme_bg(rt: &Runtime) -> [u8; 4] {
    rt.config().theme.background
}

fn window_runtime(
    root: &std::path::Path,
    name: &str,
    bytes: &[u8],
    fit: &str,
    background_opacity: f32,
) -> Runtime {
    let path = write_image(root, name, bytes);
    Runtime::new(RuntimeConfig {
        window_background_image: Some(path),
        window_background_fit: fit.to_string(),
        window_background_opacity: background_opacity,
        window_background_position: "center".to_string(),
        background_image_roots: vec![root.display().to_string()],
        ..RuntimeConfig::default()
    })
    .expect("window background runtime builds")
}

fn assert_corners_red(rgba: &[u8], width: usize, height: usize, label: &str) {
    let red = [255, 0, 0, 255];
    assert_eq!(probe(rgba, width, 0, 0), red, "{label}: top-left gaps");
    assert_eq!(
        probe(rgba, width, width - 1, 0),
        red,
        "{label}: top-right gaps"
    );
    assert_eq!(
        probe(rgba, width, 0, height - 1),
        red,
        "{label}: bottom-left gaps"
    );
    assert_eq!(
        probe(rgba, width, width - 1, height - 1),
        red,
        "{label}: bottom-right gaps"
    );
    assert_eq!(
        probe(rgba, width, width / 2, 0),
        red,
        "{label}: top edge gaps"
    );
    assert_eq!(
        probe(rgba, width, width / 2, height - 1),
        red,
        "{label}: bottom edge gaps"
    );
    assert_eq!(
        probe(rgba, width, 0, height / 2),
        red,
        "{label}: left edge gaps"
    );
    assert_eq!(
        probe(rgba, width, width - 1, height / 2),
        red,
        "{label}: right edge gaps"
    );
}

#[test]
fn window_fill_covers_every_source_and_window_aspect_without_edges() {
    // 3x3 matrix: 16:9 / 4:3 / 1:1 images against 16:9 / 4:3 / 1:1 windows.
    // `fill` (cover) is the default and must fill the whole surface; the pure
    // crop math is pinned in `bitty-rich`.
    let cases = [
        ("red-169.png", RED_16_9_PNG, "16:9"),
        ("red-43.png", RED_4_3_PNG, "4:3"),
        ("red-11.png", RED_1_1_PNG, "1:1"),
    ];
    let windows = [
        (PhysicalSize::new(800, 450), "16:9"),
        (PhysicalSize::new(640, 480), "4:3"),
        (PhysicalSize::new(500, 500), "1:1"),
    ];
    let root = scratch("window-fill-matrix");
    for (name, bytes, image_label) in cases {
        let mut rt = window_runtime(&root, name, bytes, "fill", 1.0);
        single_leaf(&mut rt, 1);
        for (extent, window_label) in windows {
            rt.handle_resize(extent).expect("resize applies");
            let stats = rt.tick().expect("presents");
            assert_eq!(
                stats.backgrounds, 1,
                "{image_label}/{window_label}: one blit"
            );
            let shown = rt.surface_extent().expect("surface extent");
            assert_eq!(shown, extent, "surface follows the resize");
            let rgba = rt.headless_rgba().expect("rgba after tick");
            assert_corners_red(
                &rgba,
                extent.width() as usize,
                extent.height() as usize,
                &format!("{image_label} image in {window_label} window"),
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn window_fit_letterboxes_full_image_and_honors_position() {
    let root = scratch("window-fit");
    let mut rt = window_runtime(&root, "red-169.png", RED_16_9_PNG, "fit", 1.0);
    single_leaf(&mut rt, 1);
    rt.handle_resize(PhysicalSize::new(500, 500))
        .expect("resize applies");
    assert_eq!(rt.tick().expect("presents").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let bg = theme_bg(&rt);
    // 16:9 inside 500x500: 500x281, centered at y=110 (109.5 rounds up).
    assert_eq!(probe(&rgba, 500, 250, 50), bg, "top letterbox band");
    assert_eq!(
        probe(&rgba, 500, 250, 250),
        [255, 0, 0, 255],
        "image center"
    );
    assert_eq!(probe(&rgba, 500, 250, 480), bg, "bottom letterbox band");
    // Top-left position moves the same letterbox to the top edge.
    rt.set_background_appearance(
        Vec::new(),
        None,
        "fill".to_string(),
        vec![root.display().to_string()],
        Some(root.join("red-169.png").display().to_string()),
        "fit".to_string(),
        1.0,
        "top-left".to_string(),
    )
    .expect("position adopts live");
    assert_eq!(rt.tick().expect("repaints").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&rgba, 500, 250, 0),
        [255, 0, 0, 255],
        "top-left position removes the top band"
    );
    assert_eq!(probe(&rgba, 500, 250, 499), bg, "bottom band remains");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn window_center_keeps_native_pixels_and_window_stretch_fills() {
    let root = scratch("window-center-stretch");
    let mut rt = window_runtime(&root, "red-11.png", RED_1_1_PNG, "center", 1.0);
    std::fs::write(root.join("red-169.png"), RED_16_9_PNG).expect("second fixture");
    single_leaf(&mut rt, 1);
    rt.handle_resize(PhysicalSize::new(500, 500))
        .expect("resize applies");
    assert_eq!(rt.tick().expect("presents").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let bg = theme_bg(&rt);
    // 90x90 native, centered in 500x500: origin (205, 205).
    assert_eq!(probe(&rgba, 500, 100, 100), bg, "center leaves a band");
    assert_eq!(
        probe(&rgba, 500, 250, 250),
        [255, 0, 0, 255],
        "native center"
    );
    assert_eq!(
        probe(&rgba, 500, 294, 294),
        [255, 0, 0, 255],
        "native corner"
    );
    assert_eq!(probe(&rgba, 500, 296, 250), bg, "native edge is exact");

    rt.set_background_appearance(
        Vec::new(),
        None,
        "fill".to_string(),
        vec![root.display().to_string()],
        Some(root.join("red-169.png").display().to_string()),
        "stretch".to_string(),
        1.0,
        "center".to_string(),
    )
    .expect("stretch adopts live");
    assert_eq!(rt.tick().expect("repaints").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    assert_corners_red(&rgba, 500, 500, "stretch");
}

#[test]
fn window_tile_repeats_native_checker_from_the_top_left() {
    let root = scratch("window-tile");
    let mut rt = window_runtime(&root, "checker.png", CHECKER_2X2_PNG, "tile", 1.0);
    single_leaf(&mut rt, 1);
    rt.handle_resize(PhysicalSize::new(64, 64))
        .expect("resize applies");
    assert_eq!(rt.tick().expect("presents").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let black = [0, 0, 0, 255];
    let white = [255, 255, 255, 255];
    for (x, y, expected) in [
        (0, 0, black),
        (1, 0, white),
        (0, 1, white),
        (1, 1, black),
        (2, 0, black),
        (3, 1, black),
        (32, 32, black),
        (33, 32, white),
    ] {
        assert_eq!(
            probe(&rgba, 64, x, y),
            expected,
            "tile repeats the 2x2 checker at ({x}, {y})"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn window_fill_rekeys_on_resize_and_repaints_the_new_extent() {
    let root = scratch("window-resize");
    let mut rt = window_runtime(&root, "red-169.png", RED_16_9_PNG, "fill", 1.0);
    single_leaf(&mut rt, 1);
    assert_eq!(rt.tick().expect("first tick").backgrounds, 1);
    let (_, misses_before, _, _) = rt.background_raster_stats();
    rt.handle_resize(PhysicalSize::new(1280, 720))
        .expect("resize applies");
    assert_eq!(rt.tick().expect("second tick").backgrounds, 1);
    let (_, misses_after, _, _) = rt.background_raster_stats();
    assert!(
        misses_after > misses_before,
        "resize must rasterize a new destination ({misses_before} -> {misses_after})"
    );
    let rgba = rt.headless_rgba().expect("rgba");
    assert_corners_red(&rgba, 1280, 720, "post-resize");
    // A forced repaint at the same extent reuses the cached blit.
    let (hits_before, misses_before, _, _) = rt.background_raster_stats();
    rt.set_window_opacity(0.999).expect("forces a repaint");
    let _ = rt.tick();
    let (hits_after, misses_after, _, _) = rt.background_raster_stats();
    assert!(hits_after > hits_before, "static frame reuses the raster");
    assert_eq!(
        misses_after, misses_before,
        "no re-raster on an idle resize"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn per_view_background_tracks_split_resize_and_divider_move() {
    let root = scratch("view-split");
    let path = write_image(&root, "red-43.png", RED_4_3_PNG);
    let mut config = RuntimeConfig {
        view_appearance: vec![ViewAppearanceRule {
            selector: "view:7".to_string(),
            background_image: Some(path),
            background_fit: Some("fill".to_string()),
            ..Default::default()
        }],
        background_image_roots: vec![root.display().to_string()],
        ..RuntimeConfig::default()
    };
    // Pin committed geometry: this test asserts the destination, not the
    // RFC-0002 interpolation (which intentionally paints intermediate rects).
    config.animations.duration_ms = [0; bitty_runtime::runtime::animations::AnimationKind::COUNT];
    let mut rt = Runtime::new(config).expect("per-view background runtime builds");
    single_leaf(&mut rt, 7);
    let red = [255, 0, 0, 255];
    let bg = theme_bg(&rt);
    let width = rt.surface_extent().expect("surface").width() as usize;
    let height = rt.surface_extent().expect("surface").height() as usize;

    let full = rt.tick().expect("single tick");
    assert_eq!(full.backgrounds, 1, "per-view blit presents");
    let rgba = rt.headless_rgba().expect("rgba");
    let frame = rt.present_frames()[0];
    let pad = rt.window_padding_physical() as i32;
    // The image covers exactly the content frame, never the padding band.
    assert_eq!(
        probe(
            &rgba,
            width,
            usize::try_from(pad + frame.content.x).expect("x"),
            usize::try_from(pad + frame.content.y + frame.content.height as i32 / 2).expect("y"),
        ),
        red,
        "content frame is covered"
    );
    assert_eq!(
        probe(&rgba, width, 0, height / 2),
        bg,
        "padding band stays outside the per-view image"
    );

    // Split horizontally: view 7 keeps the left half, view 8 opens right.
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(7), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(8), 40, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    assert_eq!(rt.tick().expect("split tick").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let frames = rt.present_frames();
    let left = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(7))
        .expect("view 7 frame");
    let right = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(8))
        .expect("view 8 frame");
    let left_mid_y =
        usize::try_from(pad + left.content.y + left.content.height as i32 / 2).expect("y");
    let left_edge_x = usize::try_from(pad + left.content.x).expect("x");
    let right_mid_y =
        usize::try_from(pad + right.content.y + right.content.height as i32 / 2).expect("y");
    let right_mid_x =
        usize::try_from(pad + right.content.x + right.content.width as i32 / 2).expect("x");
    assert_eq!(
        probe(&rgba, width, left_edge_x, left_mid_y),
        red,
        "split left edge keeps the image"
    );
    assert_eq!(
        probe(&rgba, width, right_mid_x, right_mid_y),
        bg,
        "the sibling leaf has no image"
    );

    // Divider move: view 7 shrinks to 30%, so the image must follow the
    // content frame and stop exactly at its new right edge.
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.3,
        LayoutNode::leaf(View::new(ViewId::new(7), 24, 24)),
        LayoutNode::leaf(View::new(ViewId::new(8), 56, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    assert_eq!(rt.tick().expect("moved tick").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let frames = rt.present_frames();
    let narrow = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(7))
        .expect("view 7 frame");
    let narrow_right =
        usize::try_from(pad + narrow.content.x + narrow.content.width as i32 - 1).expect("x");
    let beyond =
        usize::try_from(pad + narrow.content.x + narrow.content.width as i32 + 2).expect("x");
    let y = usize::try_from(pad + narrow.content.y + narrow.content.height as i32 / 2).expect("y");
    assert_eq!(
        probe(&rgba, width, narrow_right, y),
        red,
        "image reaches the moved right edge"
    );
    assert_eq!(
        probe(&rgba, width, beyond, y),
        bg,
        "image does not bleed past the moved frame"
    );

    // Window resize re-derives every frame from the new surface extent.
    rt.handle_resize(PhysicalSize::new(1000, 700))
        .expect("resize applies");
    assert_eq!(rt.tick().expect("resize tick").backgrounds, 1);
    let rgba = rt.headless_rgba().expect("rgba");
    let width = rt.surface_extent().expect("surface").width() as usize;
    let resized = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == ViewId::new(7))
        .expect("view 7 frame");
    assert!(
        resized.content.width > narrow.content.width,
        "window resize widens the split content frame"
    );
    let x = usize::try_from(pad + resized.content.x).expect("x");
    let right =
        usize::try_from(pad + resized.content.x + resized.content.width as i32 - 1).expect("x");
    let beyond_right =
        usize::try_from(pad + resized.content.x + resized.content.width as i32 + 2).expect("x");
    let y =
        usize::try_from(pad + resized.content.y + resized.content.height as i32 / 2).expect("y");
    assert_eq!(
        probe(&rgba, width, x, y),
        red,
        "the resized frame's left edge is covered"
    );
    assert_eq!(
        probe(&rgba, width, right, y),
        red,
        "the resized frame's right edge is covered"
    );
    assert_eq!(
        probe(&rgba, width, beyond_right, y),
        bg,
        "the image stops at the resized content frame"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Integer expectations mirror the headless compositor:
/// `dim_rgba_alpha` (background_opacity) → source-over theme background →
/// `scale_premultiplied_rgba` (window.opacity).
fn expected_dimmed_then_scaled(bg: [u8; 4], window_opacity: f32) -> [u8; 4] {
    // Solid red image at background_opacity 0.5 has alpha 128.
    let sa: u32 = 128;
    let inv = 255 - sa;
    let blended = [
        (255 * sa / 255) + u32::from(bg[0]) * inv / 255,
        u32::from(bg[1]) * inv / 255,
        u32::from(bg[2]) * inv / 255,
        sa + u32::from(bg[3]) * inv / 255,
    ];
    let factor = (window_opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
    [
        (blended[0] * factor / 255) as u8,
        (blended[1] * factor / 255) as u8,
        (blended[2] * factor / 255) as u8,
        (blended[3] * factor / 255) as u8,
    ]
}

#[test]
fn background_opacity_and_window_opacity_compose_in_that_order() {
    let root = scratch("opacity");
    let mut rt = window_runtime(&root, "red-169.png", RED_16_9_PNG, "fill", 1.0);
    single_leaf(&mut rt, 1);
    assert_eq!(rt.tick().expect("opaque tick").backgrounds, 1);
    let width = rt.surface_extent().expect("surface").width() as usize;
    let opaque = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&opaque, width, 4, 4),
        [255, 0, 0, 255],
        "default: image is opaque over the theme background"
    );
    let bg = theme_bg(&rt);

    // background_opacity 0.5 dims only the image: half red over the theme bg.
    rt.set_background_appearance(
        Vec::new(),
        None,
        "fill".to_string(),
        vec![root.display().to_string()],
        Some(root.join("red-169.png").display().to_string()),
        "fill".to_string(),
        0.5,
        "center".to_string(),
    )
    .expect("background opacity adopts live");
    assert_eq!(rt.tick().expect("dimmed tick").backgrounds, 1);
    let dimmed = rt.headless_rgba().expect("rgba");
    let expected_dimmed = expected_dimmed_then_scaled(bg, 1.0);
    assert_eq!(
        probe(&dimmed, width, 4, 4),
        expected_dimmed,
        "background_opacity is source-over theme background, alpha 128"
    );

    // window.opacity 0.5 then scales the finished surface once: no second
    // alpha multiply on the image alone.
    rt.set_window_opacity(0.5).expect("window opacity adopts");
    assert_eq!(rt.tick().expect("window dim tick").backgrounds, 1);
    let composed = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&composed, width, 4, 4),
        expected_dimmed_then_scaled(bg, 0.5),
        "window.opacity scales the whole premultiplied buffer after the blit"
    );

    // Orthogonality: window.opacity alone (background_opacity back to 1.0)
    // halves the opaque image exactly once.
    rt.set_background_appearance(
        Vec::new(),
        None,
        "fill".to_string(),
        vec![root.display().to_string()],
        Some(root.join("red-169.png").display().to_string()),
        "fill".to_string(),
        1.0,
        "center".to_string(),
    )
    .expect("opacity reset adopts");
    assert_eq!(rt.tick().expect("reset tick").backgrounds, 1);
    let only_window = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&only_window, width, 4, 4),
        [128, 0, 0, 128],
        "window.opacity halves the opaque image once"
    );
    let _ = std::fs::remove_dir_all(&root);
}
