#![forbid(unsafe_code)]
//! Chrome-style S3 present path (CTX-1162, DEC-0111 as corrected by DEC-0113).
//!
//! Proves headlessly, without a display server or GPU, that the resolved
//! per-`View` style paints through the existing primitives only:
//!
//! * glow: one `RoundedFill { border: 0 }` halo (`frame + glow_width`);
//! * shadow: one `RoundedFill { border: 0 }` at the frame offset with the
//!   frame radius (never a square `FillRect`);
//! * tint: a raster-time wash of the per-`View` background blits;
//! * fade: the global `fade_unfocused` baked into unfocused leaves'
//!   fills/glyphs/chrome, animated through the existing
//!   `animation_progress(Focus)` value.
//!
//! Envelope style (per the frame-capture harness precedent): geometry keys
//! and grid digests pin the content-metrics invariant, raw RGBA probes pin
//! paint presence, and `cells_examined` deltas pin retained reuse. No pinned
//! golden hashes: hashes churn on every present-path change while the
//! envelopes pin the contract.
//!
//! Deterministic on all platforms: virtual clock only for animation
//! asserts, no sleeps, no PTY spawn (grid bytes enter through
//! `handle_pty_bytes`), no window. The image fixture is embedded so the
//! tint tests are hermetic.

use std::time::{Duration, Instant};

use bitty_runtime::{
    AnimationPolicy, ChromeStyle, Decoration, LayoutNode, PresentFrame, Runtime, RuntimeConfig,
    SplitAxis, UiRect, View, ViewId,
};

/// 4x4 opaque-red PNG (generated once; embedded so the test is hermetic).
const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x06, 0x00, 0x00, 0x00, 0xA9, 0xF1, 0x9E,
    0x7E, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xFC, 0xCF, 0xC0, 0xF0,
    0x9F, 0x01, 0x09, 0x30, 0x21, 0x73, 0x88, 0x13, 0x00, 0x00, 0x83, 0xD1, 0x02, 0x06, 0x04, 0xBC,
    0x24, 0x47, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1162-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn harness(cfg: RuntimeConfig) -> Runtime {
    let mut rt =
        Runtime::with_deterministic_rasterizer(cfg).expect("headless harness runtime must build");
    assert!(
        !rt.is_crossfont(),
        "harness requires the deterministic headless rasterizer"
    );
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    assert!(rt.set_focus(ViewId::new(1)));
    rt
}

fn two_pane(rt: &mut Runtime) {
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    assert!(rt.set_focus(ViewId::new(1)));
}

fn instant_policy() -> AnimationPolicy {
    AnimationPolicy {
        enabled: false,
        ..Default::default()
    }
}

fn present_bytes(rt: &mut Runtime, input: &[u8]) -> Vec<u8> {
    rt.handle_pty_bytes(input);
    rt.tick().expect("present must draw");
    rt.headless_rgba().expect("rgba after tick")
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn surface_width(rt: &Runtime) -> usize {
    let extent = rt.config().window_extent();
    usize::try_from(extent.width()).expect("width fits usize")
}

fn pad_px(rt: &Runtime) -> usize {
    usize::try_from(rt.window_padding_physical()).expect("pad fits")
}

/// Comparable geometry key for one presented leaf: any style-driven layout
/// motion changes at least one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GeomKey {
    view: u64,
    frame_x: i32,
    frame_y: i32,
    frame_w: u32,
    frame_h: u32,
    content_x: i32,
    content_y: i32,
    content_w: u32,
    content_h: u32,
    cols: u16,
    rows: u16,
    border: u16,
    radius: u16,
}

fn geom_key(frame: &PresentFrame) -> GeomKey {
    GeomKey {
        view: frame.view.0,
        frame_x: frame.frame.x,
        frame_y: frame.frame.y,
        frame_w: frame.frame.width,
        frame_h: frame.frame.height,
        content_x: frame.content.x,
        content_y: frame.content.y,
        content_w: frame.content.width,
        content_h: frame.content.height,
        cols: frame.cols,
        rows: frame.rows,
        border: frame.border,
        radius: frame.radius,
    }
}

fn grid_digest(rt: &Runtime) -> u64 {
    let snap = rt.snapshot();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for cell in snap.cells.iter() {
        let mut buf = [0u8; 4];
        for byte in cell.glyph.encode_utf8(&mut buf).as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn leaf_frame(rt: &Runtime, view: u64) -> PresentFrame {
    rt.present_frames()
        .iter()
        .find(|f| f.view == ViewId::new(view))
        .copied()
        .expect("leaf must present")
}

/// Top-edge center pixel of one leaf's ring band (container space plus the
/// window padding inset). The band is at least 1px under the default
/// decoration, far from the rounded corners, and covered by no glyph.
fn ring_probe(rt: &Runtime, view: u64) -> (usize, usize) {
    let frame = leaf_frame(rt, view);
    let pad = isize::try_from(pad_px(rt)).expect("pad fits");
    let x = pad
        + isize::try_from(frame.frame.x).expect("x fits")
        + isize::try_from(frame.frame.width / 2).expect("half fits");
    let y = pad + isize::try_from(frame.frame.y).expect("y fits");
    (
        usize::try_from(x).expect("probe x"),
        usize::try_from(y).expect("probe y"),
    )
}

fn styled_glow() -> RuntimeConfig {
    RuntimeConfig {
        style_glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
        style_glow_width: 8,
        ..RuntimeConfig::default()
    }
}

#[test]
fn unstyled_configs_present_byte_identical() {
    // Defaults byte-identical: an explicitly defaulted style surface
    // (including a per-View rule that resolves to the defaults) presents
    // exactly like the bare default — the S3 consumer adds no paint for
    // inert values.
    let mut plain = harness(RuntimeConfig::default());
    let mut explicit = harness(RuntimeConfig {
        style_glow_color: None,
        style_glow_width: 0,
        style_shadow_color: None,
        style_shadow_offset_x: 0,
        style_shadow_offset_y: 0,
        style_fade_unfocused: 1.0,
        background_tint: None,
        background_tint_strength: 0.0,
        view_appearance: vec![bitty_runtime::ViewAppearanceRule {
            selector: String::from("*"),
            glow_width: Some(0),
            background_tint_strength: Some(0.0),
            ..Default::default()
        }],
        ..RuntimeConfig::default()
    });
    for frame in explicit.present_frames() {
        assert!(frame.style.is_inert());
    }
    let a_stats = plain.tick().expect("plain presents");
    let b_stats = explicit.tick().expect("explicit presents");
    assert_eq!(a_stats.fills, b_stats.fills);
    assert_eq!(a_stats.rounded_fills, b_stats.rounded_fills);
    assert_eq!(a_stats.glyphs, b_stats.glyphs);
    assert_eq!(a_stats.backgrounds, b_stats.backgrounds);
    let a = present_bytes(&mut plain, b"HELLO style S3");
    let b = present_bytes(&mut explicit, b"HELLO style S3");
    assert_eq!(a.len(), b.len());
    assert_eq!(a, b, "inert style must render byte-identically");
}

#[test]
fn glow_halo_paints_outside_frame_and_preserves_metrics() {
    let mut plain = harness(RuntimeConfig::default());
    let mut styled = harness(styled_glow());
    assert!(!styled.present_frames()[0].style.is_inert());

    let a_stats = plain.tick().expect("plain presents");
    let b_stats = styled.tick().expect("styled presents");
    assert_eq!(
        b_stats.rounded_fills,
        a_stats.rounded_fills + 1,
        "glow adds exactly one halo primitive"
    );
    // Geometry, cell metrics, and grid truth are untouched by paint.
    let a_geom: Vec<GeomKey> = plain.present_frames().iter().map(geom_key).collect();
    let b_geom: Vec<GeomKey> = styled.present_frames().iter().map(geom_key).collect();
    assert_eq!(a_geom, b_geom);
    assert_eq!(grid_digest(&plain), grid_digest(&styled));

    let a = present_bytes(&mut plain, b"HELLO glow S3");
    let b = present_bytes(&mut styled, b"HELLO glow S3");
    assert_ne!(a, b, "glow must paint");
    let width = surface_width(&styled);
    let frame = leaf_frame(&styled, 1);
    let pad = pad_px(&styled);
    // Mid-halo on the left band: outside the frame, inside the halo.
    let x = pad + usize::try_from(frame.frame.x).expect("x fits") - 4;
    let y = pad
        + usize::try_from(frame.frame.y).expect("y fits")
        + usize::try_from(frame.frame.height / 2).expect("half fits");
    assert_eq!(
        probe(&b, width, x, y),
        [0xFF, 0x00, 0x00, 0xFF],
        "opaque halo must stamp the glow color over the gap band"
    );
    assert_eq!(
        probe(&a, width, x, y),
        [0x1E, 0x1E, 0x2E, 0xFF],
        "unstyled twin keeps the theme background there"
    );
}

#[test]
fn shadow_offsets_with_frame_radius_and_no_square_overhang() {
    // DEC-0113 Fix 1 acceptance: rounded-corner shadow golden — the shadow
    // follows the frame radius at max radius + max offset, with no square
    // overhang in the corner boxes.
    let decoration = Decoration::new(6, 6, 1, 16, 6);
    let mut plain = harness(RuntimeConfig {
        decoration,
        ..RuntimeConfig::default()
    });
    let mut styled = harness(RuntimeConfig {
        decoration,
        style_shadow_color: Some([0x00, 0x00, 0x80, 0xFF]),
        style_shadow_offset_x: bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        style_shadow_offset_y: bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        ..RuntimeConfig::default()
    });
    let a_stats = plain.tick().expect("plain presents");
    let b_stats = styled.tick().expect("styled presents");
    assert_eq!(
        b_stats.rounded_fills,
        a_stats.rounded_fills + 1,
        "shadow adds exactly one primitive"
    );
    let a_geom: Vec<GeomKey> = plain.present_frames().iter().map(geom_key).collect();
    let b_geom: Vec<GeomKey> = styled.present_frames().iter().map(geom_key).collect();
    assert_eq!(a_geom, b_geom);
    assert_eq!(grid_digest(&plain), grid_digest(&styled));

    let a = present_bytes(&mut plain, b"HELLO shadow S3");
    let b = present_bytes(&mut styled, b"HELLO shadow S3");
    assert_ne!(a, b, "shadow must paint");
    let width = surface_width(&styled);
    let frame = leaf_frame(&styled, 1);
    let pad = pad_px(&styled);
    let fx = pad + usize::try_from(frame.frame.x).expect("x fits");
    let fy = pad + usize::try_from(frame.frame.y).expect("y fits");
    let off =
        usize::try_from(bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX).expect("offset fits");
    // Straight left edge of the offset shadow: the shadow color stamps.
    assert_eq!(
        probe(&b, width, fx + off + 2, fy + off + 30),
        [0x00, 0x00, 0x80, 0xFF]
    );
    // Corner box of the offset shadow: a square FillRect would paint here;
    // the rounded shadow clips it back to the theme background.
    assert_eq!(
        probe(&b, width, fx + off + 1, fy + off + 1),
        [0x1E, 0x1E, 0x2E, 0xFF],
        "shadow corner must clip (no square overhang)"
    );
    assert_eq!(
        probe(&a, width, fx + off + 2, fy + off + 30),
        [0x1E, 0x1E, 0x2E, 0xFF],
        "unstyled twin keeps the background on the shadow band"
    );
}

#[test]
fn tint_washes_background_blits_and_is_inert_without_an_image() {
    let root = scratch("tint");
    let path = root.join("red.png");
    std::fs::write(&path, RED_PNG).expect("write fixture");
    let path = path.display().to_string();
    let roots = vec![root.display().to_string()];

    let image_cfg = || RuntimeConfig {
        background_image: Some(path.clone()),
        background_fit: String::from("stretch"),
        background_image_roots: roots.clone(),
        ..RuntimeConfig::default()
    };
    let mut plain = harness(image_cfg());
    let mut tinted = harness(RuntimeConfig {
        background_tint: Some([0x00, 0x00, 0xFF, 0xFF]),
        background_tint_strength: 1.0,
        ..image_cfg()
    });
    // Tint without an image is inert but valid: byte-identical to unstyled.
    let mut tint_no_image = harness(RuntimeConfig {
        background_tint: Some([0x00, 0x00, 0xFF, 0xFF]),
        background_tint_strength: 1.0,
        ..RuntimeConfig::default()
    });
    let mut bare = harness(RuntimeConfig::default());

    let p_stats = plain.tick().expect("plain presents");
    assert_eq!(p_stats.backgrounds, 1);
    let t_stats = tinted.tick().expect("tinted presents");
    assert_eq!(t_stats.backgrounds, 1);
    let frame = leaf_frame(&tinted, 1);
    let pad = pad_px(&tinted);
    let width = surface_width(&tinted);
    // Inside the content rect, below the first text line: the blit covers
    // the cell backgrounds (background_images_present probe).
    let x = pad + usize::try_from(frame.content.x).expect("x fits") + 20;
    let y = pad + usize::try_from(frame.content.y).expect("y fits") + 300;

    let p = present_bytes(&mut plain, b"HELLO tint S3");
    let t = present_bytes(&mut tinted, b"HELLO tint S3");
    assert_ne!(p, t, "tint must wash the blit");
    assert_eq!(
        probe(&p, width, x, y),
        [0xFF, 0x00, 0x00, 0xFF],
        "untinted blit shows the image"
    );
    assert_eq!(
        probe(&t, width, x, y),
        [0x00, 0x00, 0xFF, 0xFF],
        "opaque full-strength tint stamps the tint color"
    );

    let n = present_bytes(&mut tint_no_image, b"HELLO tint S3");
    let q = present_bytes(&mut bare, b"HELLO tint S3");
    assert_eq!(n, q, "tint without an image must stay inert");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fade_dims_unfocused_leaf_and_reuses_retained() {
    // DEC-0113 Fix 2 invariant (i): a steady-state frame with a clean
    // unfocused leaf reuses its retained primitives AND carries the faded
    // alpha.
    let mut faded = harness(RuntimeConfig {
        style_fade_unfocused: 0.5,
        animations: instant_policy(),
        ..RuntimeConfig::default()
    });
    let mut plain = harness(RuntimeConfig {
        animations: instant_policy(),
        ..RuntimeConfig::default()
    });
    two_pane(&mut faded);
    two_pane(&mut plain);
    faded.tick().expect("first present draws");
    plain.tick().expect("first present draws");

    let width = surface_width(&faded);
    let (rx, ry) = ring_probe(&faded, 2);
    let faded_ring = probe(&faded.headless_rgba().expect("rgba"), width, rx, ry);
    let (px, py) = ring_probe(&plain, 2);
    let plain_ring = probe(&plain.headless_rgba().expect("rgba"), width, px, py);
    assert_ne!(
        faded_ring, plain_ring,
        "the unfocused ring must dim under fade"
    );
    // The focused leaf is unaffected: same pixel both runtimes.
    let (fx, fy) = ring_probe(&faded, 1);
    let (gx, gy) = ring_probe(&plain, 1);
    assert_eq!(
        probe(&faded.headless_rgba().expect("rgba"), width, fx, fy),
        probe(&plain.headless_rgba().expect("rgba"), width, gx, gy),
        "the focused ring must ignore fade"
    );

    // Damage only the focused pane: the clean unfocused leaf must reuse its
    // retained (faded) list instead of re-rendering.
    faded.handle_pty_bytes(b"reuse me");
    plain.handle_pty_bytes(b"reuse me");
    let stats = faded.tick().expect("damage must present");
    plain.tick().expect("twin presents");
    let total_cells: usize = faded
        .present_frames()
        .iter()
        .map(|f| usize::from(f.cols) * usize::from(f.rows))
        .sum();
    assert!(
        stats.cells_examined < total_cells as u64,
        "the clean leaf must reuse retained primitives (examined {} of {total_cells})",
        stats.cells_examined
    );
    let (rx2, ry2) = ring_probe(&faded, 2);
    assert_eq!(
        probe(&faded.headless_rgba().expect("rgba"), width, rx2, ry2),
        faded_ring,
        "the reused leaf must keep its faded alpha (no stale bright frame)"
    );
}

#[test]
fn focus_flip_repaints_without_grid_damage() {
    // DEC-0113 Fix 2 invariant (ii): a focus flip with no grid damage still
    // repaints the affected leaves with the new factor.
    let mut rt = harness(RuntimeConfig {
        style_fade_unfocused: 0.5,
        animations: instant_policy(),
        ..RuntimeConfig::default()
    });
    two_pane(&mut rt);
    rt.handle_pty_bytes(b"flip me");
    rt.tick().expect("first present draws");
    let width = surface_width(&rt);
    let grid_before = grid_digest(&rt);
    let geom_before: Vec<GeomKey> = rt.present_frames().iter().map(geom_key).collect();
    let (x1, y1) = ring_probe(&rt, 1);
    let (x2, y2) = ring_probe(&rt, 2);
    let rgba = rt.headless_rgba().expect("rgba");
    let bright = probe(&rgba, width, x1, y1);
    let dim = probe(&rgba, width, x2, y2);
    assert_ne!(bright, dim);

    // No PTY bytes between: the flip alone must repaint with the new factor.
    assert!(rt.set_focus(ViewId::new(2)));
    let stats = rt.tick().expect("focus flip must present");
    assert!(stats.cells_examined > 0, "affected leaves must repaint");
    let rgba = rt.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&rgba, width, x1, y1),
        dim,
        "the leaf that lost focus must dim"
    );
    assert_eq!(
        probe(&rgba, width, x2, y2),
        bright,
        "the newly focused leaf must brighten"
    );
    assert_eq!(
        grid_digest(&rt),
        grid_before,
        "terminal truth must not move"
    );
    assert_eq!(
        rt.present_frames().iter().map(geom_key).collect::<Vec<_>>(),
        geom_before,
        "geometry must not move"
    );
    assert_eq!(rt.tick(), None, "must idle after the flip commits");
}

#[test]
fn fade_animates_with_focus_transition_on_virtual_clock() {
    // The focus-transition ANIMATION of the fade reads the existing
    // animation_progress(Focus) value: virtual-clock drive only.
    let start = Instant::now();
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
        style_fade_unfocused: 0.5,
        ..RuntimeConfig::default()
    })
    .expect("harness runtime must build");
    two_pane(&mut rt);
    rt.tick_at(start).expect("first present draws");
    // No-fade twin: leaf 1 unfocused reads the idle ring unfaded — the value
    // the flip frame must start from (only the fade animates; the ring color
    // switches to idle immediately).
    let mut twin =
        Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("twin must build");
    two_pane(&mut twin);
    assert!(twin.set_focus(ViewId::new(2)));
    twin.tick_at(start).expect("twin draws");
    let (tx, ty) = ring_probe(&twin, 1);
    let twin_width = surface_width(&twin);
    let idle_unfaded = probe(&twin.headless_rgba().expect("rgba"), twin_width, tx, ty);
    let width = surface_width(&rt);
    let grid_before = grid_digest(&rt);
    let geom_before: Vec<GeomKey> = rt.present_frames().iter().map(geom_key).collect();
    let (x1, y1) = ring_probe(&rt, 1);
    let pixel = |rt: &Runtime| probe(&rt.headless_rgba().expect("rgba"), width, x1, y1);
    let bright = pixel(&rt);

    assert!(rt.set_focus(ViewId::new(2)));
    rt.tick_at(start).expect("flip frame presents");
    assert_eq!(
        pixel(&rt),
        idle_unfaded,
        "at flip instant the fade still reads progress 0 (starts unfaded)"
    );
    rt.tick_at(start + Duration::from_millis(50))
        .expect("mid-transition presents");
    let mid = pixel(&rt);
    rt.tick_at(start + Duration::from_secs(5))
        .expect("commit frame presents");
    let dim = pixel(&rt);
    assert_ne!(mid, bright, "mid-transition must dim (not a snap)");
    assert_ne!(mid, dim, "mid-transition must not skip to the end state");
    assert_ne!(bright, dim, "the flip must settle faded");
    assert_eq!(
        grid_digest(&rt),
        grid_before,
        "terminal truth must not move"
    );
    assert_eq!(
        rt.present_frames().iter().map(geom_key).collect::<Vec<_>>(),
        geom_before,
        "geometry must not move mid-transition"
    );
    assert_eq!(
        rt.tick_at(start + Duration::from_secs(6)),
        None,
        "must idle after the transition commits"
    );
}

#[test]
fn chrome_style_adopt_repaints_unfocused_leaf() {
    // A `fade_unfocused` live-adopt repaints the affected leaves with the
    // new factor (DEC-0113 Fix 2a trigger) and matches a fresh build.
    let mut live = harness(RuntimeConfig {
        animations: instant_policy(),
        ..RuntimeConfig::default()
    });
    two_pane(&mut live);
    assert!(live.set_focus(ViewId::new(2)));
    live.tick().expect("steady state draws");
    let mut fresh = harness(RuntimeConfig {
        style_fade_unfocused: 0.5,
        animations: instant_policy(),
        ..RuntimeConfig::default()
    });
    two_pane(&mut fresh);
    assert!(fresh.set_focus(ViewId::new(2)));
    fresh.tick().expect("steady state draws");

    live.set_chrome_style(ChromeStyle {
        fade_unfocused: 0.5,
        ..ChromeStyle::default()
    })
    .expect("adopt");
    live.tick().expect("adopt must repaint");
    let width = surface_width(&live);
    let (x1, y1) = ring_probe(&live, 1);
    let (x2, y2) = ring_probe(&fresh, 1);
    assert_eq!(
        probe(&live.headless_rgba().expect("rgba"), width, x1, y1),
        probe(&fresh.headless_rgba().expect("rgba"), width, x2, y2),
        "adopted fade must equal a fresh build (no stale alpha)"
    );
}

#[test]
fn max_style_values_keep_content_metrics() {
    // DEC-0113 Fix 2 invariant (iii): content metrics, clips, and insets are
    // unchanged under max style values — style paint never moves the grid.
    let mut plain = harness(RuntimeConfig::default());
    let mut styled = harness(RuntimeConfig {
        style_glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
        style_glow_width: bitty_runtime::config::MAX_STYLE_GLOW_WIDTH_PX,
        style_shadow_color: Some([0x00, 0x00, 0x00, 0x80]),
        style_shadow_offset_x: bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        style_shadow_offset_y: -bitty_runtime::config::MAX_STYLE_SHADOW_OFFSET_PX,
        style_fade_unfocused: 0.0,
        background_tint: Some([0x11, 0x22, 0x33, 0xFF]),
        background_tint_strength: 1.0,
        view_appearance: vec![bitty_runtime::ViewAppearanceRule {
            selector: String::from("view:1"),
            glow_width: Some(3),
            background_tint_strength: Some(0.5),
            ..Default::default()
        }],
        ..RuntimeConfig::default()
    });
    two_pane(&mut plain);
    two_pane(&mut styled);
    plain.tick().expect("plain presents");
    styled.tick().expect("styled presents");
    let a_geom: Vec<GeomKey> = plain.present_frames().iter().map(geom_key).collect();
    let b_geom: Vec<GeomKey> = styled.present_frames().iter().map(geom_key).collect();
    assert_eq!(a_geom, b_geom, "metrics must ignore max style");
    assert!(
        styled.present_frames().iter().any(|f| !f.style.is_inert()),
        "the styled run must actually carry style"
    );
    assert_eq!(grid_digest(&plain), grid_digest(&styled));
    let a = present_bytes(&mut plain, b"HELLO metrics S3");
    let b = present_bytes(&mut styled, b"HELLO metrics S3");
    assert_ne!(a, b, "max style must paint (comparison is not vacuous)");
}
