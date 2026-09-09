//! Kitty placement + present-path tests (CTX-0248, headless).
//!
//! Placement math and rasterization live in `bitty-rich`; compositing
//! primitives in `bitty-render`. These tests prove the runtime seam:
//! completed transmissions route to stored images, display actions paint
//! RGBA into the presented frame topmost (never grid truth), and scroll /
//! alternate-screen behave as documented.

use bitty_runtime::{KittyDisplayOutcome, Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`).
fn red_2x2() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
}

fn default_geometry() -> (usize, usize, usize) {
    // Mirrors `tick_cursor_overlay_uses_theme_cursor_hue`: 9x19 cells,
    // 8px padding inset at scale 1.0, 80x24 grid.
    let cfg = RuntimeConfig::default();
    assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
    let rt = make_runtime();
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    assert_eq!(pad, 8);
    (9, 19, 8)
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn display_paints_image_pixels_topmost() {
    let (cw, ch, pad) = default_geometry();
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.tick().expect("display forces a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    // Cursor-anchored at (0,0), 2x2 cells: dest covers pad..pad+18 px.
    // Premultiplied opaque red is identity.
    assert_eq!(probe(&rgba, width, pad + 1, pad + 1), [0xFF, 0, 0, 0xFF]);
    assert_eq!(
        probe(&rgba, width, pad + 2 * cw - 1, pad + 2 * ch - 1),
        [0xFF, 0, 0, 0xFF]
    );
    // Outside the rect the theme background survives.
    let bg = bitty_render::grid::DEFAULT_BG;
    assert_eq!(
        probe(&rgba, width, pad + 2 * cw + 1, pad + 1),
        [bg[0], bg[1], bg[2], 0xFF]
    );
    assert_eq!(rt.tick(), None, "static image idles after present");
}

#[test]
fn image_covers_grid_text_where_they_overlap() {
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"A");
    // Cursor is now col 1; move back over the 'A' cell and cover it.
    rt.handle_pty_bytes(b"\x1b[1;1H");
    rt.kitty_display_image(32, Some(2), Some(2), None, 1, 1, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("display forces a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    // Cell (0,0) center carries opaque red, not the 'A' glyph gray.
    assert_eq!(probe(&rgba, width, pad + 4, pad + 9), [0xFF, 0, 0, 0xFF]);
}

#[test]
fn transmit_only_stores_without_painting() {
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), Some('t'), 2, 2, &red_2x2(), 0)
        .expect("transmit must succeed");
    assert!(matches!(outcome, KittyDisplayOutcome::Stored { .. }));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.tick().expect("first tick still presents the grid");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    // The cursor fill paints cell (0,0), so the pixel is theme-cursor, not
    // background — the assertion that matters is "not image red".
    assert_ne!(
        probe(&rgba, width, pad + 1, pad + 1),
        [0xFF, 0, 0, 0xFF],
        "transmit-only must paint no image pixels"
    );
}

#[test]
fn unsupported_action_stores_without_painting() {
    let mut rt = make_runtime();
    for action in ['p', 'd', 'q', 'f'] {
        let outcome = rt
            .kitty_display_image(32, Some(1), Some(1), Some(action), 1, 1, &[9, 9, 9, 9], 0)
            .expect("unsupported action must still store");
        assert!(
            matches!(outcome, KittyDisplayOutcome::StoredNotDisplayed { .. }),
            "a={action}"
        );
    }
    assert_eq!(rt.kitty_image_count(), 4);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn unknown_format_and_oversize_fail_closed() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_display_image(7, Some(2), Some(2), None, 1, 1, &red_2x2(), 0)
        .expect_err("f=7 must fail");
    assert!(matches!(
        err,
        bitty_runtime::KittyImageError::UnknownFormat(7)
    ));
    let err = rt
        .kitty_display_image(32, Some(9000), Some(1), None, 1, 1, &[0; 4], 0)
        .expect_err("9000px side must fail");
    assert!(matches!(err, bitty_runtime::KittyImageError::Decode(_)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn scroll_moves_image_with_content() {
    let (cw, _, pad) = default_geometry();
    let mut rt = make_runtime();
    // Anchor at the last row (24 rows: index 23).
    rt.handle_pty_bytes(b"\x1b[24;1H");
    rt.kitty_display_image(32, Some(2), Some(2), None, 1, 1, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("display forces a present");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    let ch = cfg.cell_height as usize;
    let before = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&before, width, pad + 4, 23 * ch + pad + 9),
        [0xFF, 0, 0, 0xFF]
    );
    // Three linefeeds at the bottom margin scroll three lines up.
    rt.handle_pty_bytes(b"\n\n\n");
    rt.tick().expect("scroll damage must present");
    let after = rt.headless_rgba().expect("rgba");
    // Placement is retained but now three rows higher.
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(
        probe(&after, width, pad + 4, 20 * ch + pad + 9),
        [0xFF, 0, 0, 0xFF]
    );
    // The cursor now rests on the old anchor row, so that pixel carries
    // the cursor fill — the assertion that matters is "not image red".
    assert_ne!(
        probe(&after, width, pad + 4, 23 * ch + pad + 9),
        [0xFF, 0, 0, 0xFF],
        "old anchor row must no longer carry the image"
    );
    let _ = cw;
}

#[test]
fn image_scrolled_off_top_paints_nothing_but_is_retained() {
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[24;1H");
    rt.kitty_display_image(32, Some(2), Some(2), None, 1, 1, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("display forces a present");
    // Scroll the anchor (row 23) fully off the top.
    rt.handle_pty_bytes(&[b'\n'; 30]);
    rt.tick().expect("scroll damage must present");
    assert_eq!(rt.kitty_placement_count(), 1, "placement retained");
    let rgba = rt.headless_rgba().expect("rgba");
    assert!(
        rgba.chunks_exact(4)
            .all(|px| px[0] != 0xFF || px[1] != 0 || px[2] != 0),
        "no opaque-red pixel may survive once scrolled off"
    );
    let _ = pad;
}

#[test]
fn alt_screen_clears_and_suppresses() {
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("display forces a present");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    assert_eq!(
        probe(&rt.headless_rgba().expect("rgba"), width, pad + 1, pad + 1),
        [0xFF, 0, 0, 0xFF,]
    );
    // Enter alternate screen: the layer clears and the repaint carries no
    // image pixels even though the grid generation may not advance.
    rt.handle_pty_bytes(b"\x1b[?1049h");
    rt.tick().expect("alt transition forces a present");
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    let rgba = rt.headless_rgba().expect("rgba");
    // Cursor rests at (0,0): cursor fill, not background — assert non-red.
    assert_ne!(
        probe(&rgba, width, pad + 1, pad + 1),
        [0xFF, 0, 0, 0xFF],
        "alt screen must carry no image pixels"
    );
    // Display while alt is active stores without placing.
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, &red_2x2(), 0)
        .expect("alt display must store");
    assert!(matches!(
        outcome,
        KittyDisplayOutcome::SuppressedAlternateScreen { .. }
    ));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    // Leave alternate screen: the stored-but-never-placed image survives
    // inertly (placements stay empty, nothing paints).
    rt.handle_pty_bytes(b"\x1b[?1049l");
    rt.tick();
    assert_eq!(rt.kitty_image_count(), 1, "stored image survives inertly");
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn pathological_placement_count_is_budgeted_per_frame() {
    // CTX-0252 F2: the layer retains up to 128 placements, but one frame
    // composites at most KITTY_PRESENT_MAX_BLITS_PER_FRAME blits. Each
    // `kitty_display_image` stores a fresh image, so the 64-image store cap
    // binds first here (64 placements); the unit-level budget test covers
    // the full 128-candidate shed.
    let mut rt = make_runtime();
    for z in 0..64 {
        rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, &red_2x2(), z)
            .expect("display must succeed");
    }
    assert_eq!(rt.kitty_image_count(), 64);
    assert_eq!(rt.kitty_placement_count(), 64);
    rt.tick().expect("display forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME,
        "frame blits stay within the per-frame budget"
    );
    // Shed-for-frame only: every placement is retained for later frames.
    assert_eq!(rt.kitty_placement_count(), 64);
}

#[test]
fn static_frame_reuses_cached_raster() {
    // CTX-0252 F2: a second present with identical placement geometry and
    // scrollback must not re-rasterize (hit), and the pixels stay red.
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("first present rasterizes");
    let after_first = rt.kitty_raster_stats();
    assert_eq!(after_first.misses, 1);
    assert_eq!(after_first.hits, 0);
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // New grid content forces a second present with an unchanged placement.
    rt.handle_pty_bytes(b"B");
    rt.tick().expect("new generation must present");
    let after_second = rt.kitty_raster_stats();
    assert_eq!(
        after_second.misses, 1,
        "identical frame must not re-rasterize"
    );
    assert_eq!(after_second.hits, 1);
    assert_eq!(rt.kitty_last_frame_images(), 1);
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    assert_eq!(probe(&rgba, width, pad + 1, pad + 1), [0xFF, 0, 0, 0xFF]);
}

#[test]
fn scroll_invalidates_cached_raster_without_stale_pixels() {
    // CTX-0252 F2: scrolling changes the scrollback sequence, so the cached
    // blit misses and the image repaints at its scrolled position (never
    // stale at the old anchor).
    let (_, _, pad) = default_geometry();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[24;1H");
    rt.kitty_display_image(32, Some(2), Some(2), None, 1, 1, &red_2x2(), 0)
        .expect("display must succeed");
    rt.tick().expect("display forces a present");
    let misses_before = rt.kitty_raster_stats().misses;
    assert!(misses_before >= 1);
    rt.handle_pty_bytes(b"\n\n\n");
    rt.tick().expect("scroll damage must present");
    assert!(
        rt.kitty_raster_stats().misses > misses_before,
        "scroll must invalidate the cached blit"
    );
    assert_eq!(rt.kitty_last_frame_images(), 1);
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    let ch = cfg.cell_height as usize;
    let rgba = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&rgba, width, pad + 4, 20 * ch + pad + 9),
        [0xFF, 0, 0, 0xFF],
        "image must track content three rows up"
    );
    assert_ne!(
        probe(&rgba, width, pad + 4, 23 * ch + pad + 9),
        [0xFF, 0, 0, 0xFF],
        "old anchor row must not keep a stale blit"
    );
}
