#![forbid(unsafe_code)]
//! CTX-1165 (#1993): stacked floats fully occlude lower text.
//!
//! The combined draw list paints all fills before all glyphs, so the CTX-1148
//! opaque float fill already covers lower fills but not lower glyphs. The
//! present path drops lower glyphs sharing area with a higher float frame,
//! extending the same opaque boundary to text rows. Pinned here:
//!
//! - two overlapping detached floats: the shared band shows the upper float
//!   only,
//! - float over tiled stays clean,
//! - the unstyled tiled baseline is unchanged.

use bitty_runtime::{
    AnimationPolicy, LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};
use std::collections::HashMap;

const THEME_BG: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];
const IMAGE_RED: [u8; 4] = [0xFF, 0x00, 0x00, 0xFF];

/// 4x4 opaque-red PNG (same bytes as the `background_images_present`
/// fixture, embedded so the window-background probe stays hermetic).
const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x06, 0x00, 0x00, 0x00, 0xA9, 0xF1, 0x9E,
    0x7E, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xFC, 0xCF, 0xC0, 0xF0,
    0x9F, 0x01, 0x09, 0x30, 0x21, 0x73, 0x88, 0x13, 0x00, 0x00, 0x83, 0xD1, 0x02, 0x06, 0x04, 0xBC,
    0x24, 0x47, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1181-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless harness runtime must build")
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        ),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let base = (y as usize * width as usize + x as usize) * 4;
    [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
}

fn win_content(rt: &Runtime, view: ViewId) -> (u32, u32, u32, u32) {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"));
    let pad = rt.window_padding_physical() as i64;
    let x0 = (pad + i64::from(frame.content.x)).max(0) as u32;
    let y0 = (pad + i64::from(frame.content.y)).max(0) as u32;
    (x0, y0, x0 + frame.content.width, y0 + frame.content.height)
}

fn win_frame(rt: &Runtime, view: ViewId) -> (u32, u32, u32, u32) {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"));
    let pad = rt.window_padding_physical() as i64;
    let x0 = (pad + i64::from(frame.frame.x)).max(0) as u32;
    let y0 = (pad + i64::from(frame.frame.y)).max(0) as u32;
    (x0, y0, x0 + frame.frame.width, y0 + frame.frame.height)
}

fn non_bg_colors(
    rgba: &[u8],
    width: u32,
    height: u32,
    rect: (u32, u32, u32, u32),
    cap: usize,
) -> (usize, HashMap<[u8; 4], usize>) {
    let (x0, y0, x1, y1) = rect;
    let mut total = 0usize;
    let mut colors: HashMap<[u8; 4], usize> = HashMap::new();
    for y in y0..y1 {
        for x in x0..x1 {
            if x >= width || y >= height {
                continue;
            }
            total += 1;
            let got = pixel(rgba, width, x, y);
            if got != THEME_BG {
                *colors.entry(got).or_insert(0) += 1;
            }
            if total >= cap {
                return (total, colors);
            }
        }
    }
    (total, colors)
}

fn non_bg_count(colors: &HashMap<[u8; 4], usize>) -> usize {
    colors.values().sum()
}

fn fill_primary(rt: &mut Runtime, cols: usize, rows: usize, color: &str, glyph: u8) {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b[2J\x1b[H");
    seq.extend_from_slice(color.as_bytes());
    seq.extend(std::iter::repeat_n(glyph, rows.saturating_mul(cols)));
    seq.extend_from_slice(b"\x1b[0m");
    rt.handle_pty_bytes(&seq);
}

#[test]
fn stacked_float_upper_owns_shared_band() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));
    rt.float_detach(ViewId::new(1))
        .expect("detach lower must apply");
    rt.float_detach(ViewId::new(2))
        .expect("detach upper must apply");
    let frames = rt.present_frames();
    assert_eq!(
        frames.last().expect("frames").view,
        ViewId::new(2),
        "later detach paints last"
    );
    let lower = *frames
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("lower presents");
    fill_primary(
        &mut rt,
        lower.cols as usize,
        lower.rows as usize,
        "\x1b[31m",
        b'L',
    );
    rt.tick().expect("text frame must present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let (lx0, ly0, lx1, ly1) = win_content(&rt, ViewId::new(1));
    let (ux0, uy0, ux1, uy1) = win_frame(&rt, ViewId::new(2));
    let (ox0, oy0, ox1, oy1) = (lx0.max(ux0), ly0.max(uy0), lx1.min(ux1), ly1.min(uy1));
    assert!(
        ox1 > ox0 && oy1 > oy0,
        "cascaded floats must overlap for the occlusion probe"
    );
    let (_, lower_only) =
        non_bg_colors(&rgba, width, height, (lx0, ly0, lx1.min(ux0), ly1), 40_000);
    assert!(
        non_bg_count(&lower_only) > 0,
        "lower-only content must carry text or the probe is vacuous"
    );
    let (ix0, iy0, ix1, iy1) = (
        ox0.saturating_add(4),
        oy0.saturating_add(4),
        ox1.saturating_sub(4),
        oy1.saturating_sub(4),
    );
    assert!(
        ix1 > ix0 && iy1 > iy0,
        "shrunk overlap must stay non-empty outside the ring edge"
    );
    let (_, overlap) = non_bg_colors(&rgba, width, height, (ix0, iy0, ix1, iy1), 120_000);
    assert_eq!(
        non_bg_count(&overlap),
        0,
        "shared band pixels belong to the upper float only"
    );
}

#[test]
fn float_over_tiled_stays_clean() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    let frames = rt.present_frames();
    let tiled = *frames
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("tiled survivor presents");
    assert!(tiled.tier.is_none());
    fill_primary(
        &mut rt,
        tiled.cols as usize,
        tiled.rows as usize,
        "\x1b[33m",
        b'T',
    );
    rt.tick().expect("text frame must present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let (tx0, ty0, tx1, ty1) = win_content(&rt, ViewId::new(1));
    let (fx0, fy0, fx1, fy1) = win_frame(&rt, ViewId::new(2));
    let (ox0, oy0, ox1, oy1) = (tx0.max(fx0), ty0.max(fy0), tx1.min(fx1), ty1.min(fy1));
    assert!(
        ox1 > ox0 && oy1 > oy0,
        "float must cover tiled content for the probe"
    );
    let (ix0, iy0, ix1, iy1) = (
        ox0.saturating_add(4),
        oy0.saturating_add(4),
        ox1.saturating_sub(4),
        oy1.saturating_sub(4),
    );
    let (_, overlap) = non_bg_colors(&rgba, width, height, (ix0, iy0, ix1, iy1), 120_000);
    assert_eq!(
        non_bg_count(&overlap),
        0,
        "tiled text must not show through the float frame"
    );
}

#[test]
fn unstyled_steady_state_unchanged() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|frame| frame.tier.is_none()));
    let stats = rt.tick().expect("tiled baseline must present");
    assert!(stats.fills > 0);
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    assert_eq!(
        rgba.len(),
        extent.width() as usize * extent.height() as usize * 4
    );
    assert_eq!(pixel(&rgba, extent.width(), 0, 0), THEME_BG);
}

#[cfg(unix)]
#[test]
fn stacked_floats_colored_text_upper_wins_across_reuse() {
    use std::collections::HashSet;

    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(1))
        .expect("detach lower must apply");
    rt.float_detach(ViewId::new(2))
        .expect("detach upper must apply");
    let frames = rt.present_frames();
    let lower = *frames
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("lower presents");
    let upper = *frames
        .iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("upper presents");
    let (lcols, lrows) = (lower.cols, lower.rows);
    let (ucols, urows) = (upper.cols, upper.rows);
    rt.spawn_shell_for_view(ViewId::new(1), "/bin/sh", &["-c", "sleep 30"], lcols, urows)
        .expect("lower shell must spawn");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], ucols, urows)
        .expect("upper shell must spawn");
    let _ = (lrows, ucols);
    let mut low_seq = Vec::new();
    low_seq.extend_from_slice(b"\x1b[2J\x1b[H\x1b[31m");
    low_seq.extend(std::iter::repeat_n(
        b'L',
        usize::from(lcols).saturating_mul(usize::from(lrows)),
    ));
    low_seq.extend_from_slice(b"\x1b[0m");
    let mut up_seq = Vec::new();
    up_seq.extend_from_slice(b"\x1b[2J\x1b[H\x1b[32m");
    up_seq.extend(std::iter::repeat_n(
        b'U',
        usize::from(ucols).saturating_mul(usize::from(urows)),
    ));
    up_seq.extend_from_slice(b"\x1b[0m");
    rt.handle_pane_bytes(ViewId::new(1), &low_seq);
    rt.handle_pane_bytes(ViewId::new(2), &up_seq);
    rt.tick().expect("both floats must present");
    let check_overlap = |rt: &Runtime, label: &str| {
        let rgba = rt.headless_rgba().expect("rgba must exist");
        let extent = rt.surface_extent().expect("extent must exist");
        let (width, height) = (extent.width(), extent.height());
        let (lx0, ly0, lx1, ly1) = win_content(rt, ViewId::new(1));
        let (ux0, uy0, ux1, uy1) = win_frame(rt, ViewId::new(2));
        let (ox0, oy0, ox1, oy1) = (lx0.max(ux0), ly0.max(uy0), lx1.min(ux1), ly1.min(uy1));
        assert!(ox1 > ox0 && oy1 > oy0, "floats must overlap");
        let (_, lower_only) =
            non_bg_colors(&rgba, width, height, (lx0, ly0, lx1.min(ux0), ly1), 40_000);
        assert!(
            non_bg_count(&lower_only) > 0,
            "{label}: lower-only must carry text"
        );
        let (uxc0, uyc0, uxc1, uyc1) = win_content(rt, ViewId::new(2));
        let (_, upper_only) = non_bg_colors(
            &rgba,
            width,
            height,
            (uxc0.max(lx1), uyc0, uxc1, uyc1.min(uyc0 + 200)),
            40_000,
        );
        let upper_set: HashSet<[u8; 4]> = upper_only.keys().copied().collect();
        let lower_set: HashSet<[u8; 4]> = lower_only.keys().copied().collect();
        let lower_unique: HashSet<[u8; 4]> = lower_set.difference(&upper_set).copied().collect();
        let (ix0, iy0, ix1, iy1) = (
            ox0.saturating_add(4),
            oy0.saturating_add(4),
            ox1.saturating_sub(4),
            oy1.saturating_sub(4),
        );
        let (_, overlap) = non_bg_colors(&rgba, width, height, (ix0, iy0, ix1, iy1), 120_000);
        let overlap_set: HashSet<[u8; 4]> = overlap.keys().copied().collect();
        let leaked: Vec<[u8; 4]> = overlap_set.intersection(&lower_unique).copied().collect();
        assert!(
            leaked.is_empty(),
            "{label}: shared band must not carry lower colors, leaked {leaked:?}"
        );
        let upper_hit = overlap_set.intersection(&upper_set).count();
        assert!(
            upper_hit > 0 || non_bg_count(&overlap) == 0,
            "{label}: shared band belongs to the upper float"
        );
    };
    check_overlap(&rt, "re-render");
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b[1;1HMORE");
    rt.tick()
        .expect("upper damage must present with lower reuse");
    check_overlap(&rt, "reuse");
}

#[test]
fn float_occludes_window_background_gap() {
    // CTX-1181 (#2041): the window background image paints whole-window in
    // the backgrounds layer, above the CTX-1148 opaque float fill in fills.
    // A gap band between tiled survivors behind a float must not render
    // through the float frame.
    let root = scratch_dir("window-gap");
    let path = root.join("red.png");
    std::fs::write(&path, RED_PNG).expect("write fixture");
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        window_background_image: Some(path.display().to_string()),
        window_background_fit: "stretch".to_string(),
        background_image_roots: vec![root.display().to_string()],
        ..RuntimeConfig::default()
    })
    .expect("window image must load");
    install(&mut rt, three_pane());
    rt.handle_pty_bytes(b"\x1b[?25l");
    let stats = rt.tick().expect("baseline must present");
    assert!(
        stats.backgrounds >= 1,
        "window blit must present or the probe is vacuous"
    );
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    rt.tick().expect("float frame must present");

    let frames = rt.present_frames();
    let frame_of = |view: ViewId| {
        *frames
            .iter()
            .find(|frame| frame.view == view)
            .unwrap_or_else(|| panic!("{view:?} must be presented"))
    };
    let left_survivor = frame_of(ViewId::new(2));
    let right_survivor = frame_of(ViewId::new(3));
    let float_frame = frame_of(ViewId::new(1));
    let (gap_left, gap_right) = if left_survivor.frame.x <= right_survivor.frame.x {
        (
            left_survivor.frame.x as i64 + i64::from(left_survivor.frame.width),
            right_survivor.frame.x as i64,
        )
    } else {
        (
            right_survivor.frame.x as i64 + i64::from(right_survivor.frame.width),
            left_survivor.frame.x as i64,
        )
    };
    assert!(
        gap_right > gap_left,
        "tiled survivors must leave a gap band for the probe"
    );
    let pad = i64::from(rt.window_padding_physical());
    let gap_top =
        (pad + i64::from(left_survivor.frame.y)).max(pad + i64::from(right_survivor.frame.y));
    let gap_bottom =
        (pad + i64::from(left_survivor.frame.y) + i64::from(left_survivor.frame.height))
            .min(pad + i64::from(right_survivor.frame.y) + i64::from(right_survivor.frame.height));
    assert!(
        gap_bottom > gap_top,
        "survivor gap band must span rows for the probe"
    );
    let gap_x = (pad + (gap_left + gap_right) / 2) as u32;
    let float_x0 = pad + i64::from(float_frame.frame.x);
    let float_x1 = float_x0 + i64::from(float_frame.frame.width);
    assert!(
        i64::from(gap_x) >= float_x0 && i64::from(gap_x) < float_x1,
        "float must cover the gap column for the probe"
    );
    let float_y0 = pad + i64::from(float_frame.frame.y);
    let float_y1 = float_y0 + i64::from(float_frame.frame.height);
    let mut under: Option<u32> = None;
    let mut free: Option<u32> = None;
    for y in gap_top..gap_bottom {
        if y >= float_y0 + 4 && y < float_y1 - 4 {
            under.get_or_insert(y as u32);
        } else {
            free.get_or_insert(y as u32);
        }
    }
    let under_y = under.expect("float must cover the gap band for the probe");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    assert!(
        gap_x < width && under_y < height,
        "gap-under-float probe stays inside the window"
    );
    assert_eq!(
        pixel(&rgba, width, gap_x, under_y),
        THEME_BG,
        "gap band under the float must stay opaque theme background"
    );
    if let Some(free_y) = free {
        assert_eq!(
            pixel(&rgba, width, gap_x, free_y),
            IMAGE_RED,
            "gap band past the float still shows the window image"
        );
    } else {
        assert!(pad > 2, "padding band must exist for the fallback probe");
        let (px, py) = (1u32, 1u32);
        assert_eq!(
            pixel(&rgba, width, px, py),
            IMAGE_RED,
            "padding past the float still shows the window image"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn float_occludes_kitty_image_behind() {
    // CTX-1181 (#2041): kitty image blits composite topmost, above the
    // CTX-1148 opaque float fill and the CTX-1165 glyph filter. An image
    // placed by a behind tiled panel must not show through the float frame.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));
    rt.handle_pty_bytes(b"\x1b[?25l");
    let payload = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16);
    rt.kitty_display_image(32, Some(4), Some(4), None, 48, 16, 0, &payload, 0)
        .expect("display must store and place");
    rt.tick().expect("image frame must present");
    assert!(
        rt.kitty_last_frame_images() >= 1,
        "behind image must composite or the probe is vacuous"
    );
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let (tx0, ty0, _tx1, _ty1) = win_content(&rt, ViewId::new(1));
    let (fx0, fy0, fx1, fy1) = win_frame(&rt, ViewId::new(2));
    let (cell_w, cell_h) = rt.live_cell_size();
    assert!(cell_w > 0 && cell_h > 0, "cell metrics must be live");
    let (ix1, iy1) = (
        tx0.saturating_add(48u32.saturating_mul(cell_w)),
        ty0.saturating_add(16u32.saturating_mul(cell_h)),
    );
    let (ox0, oy0, ox1, oy1) = (tx0.max(fx0), ty0.max(fy0), ix1.min(fx1), iy1.min(fy1));
    assert!(
        ox1 > ox0 && oy1 > oy0,
        "float must cover the behind image for the probe"
    );
    let probe = (tx0, ty0, tx0.saturating_add(20), ty0.saturating_add(20));
    assert!(
        probe.2 <= fx0 || probe.3 <= fy0,
        "behind-only probe must sit outside the float frame"
    );
    let (_, behind_only) = non_bg_colors(&rgba, width, height, probe, 4_000);
    assert!(
        behind_only.contains_key(&IMAGE_RED),
        "behind image must stay visible past the float"
    );
    let (ix0, iy0, ix1, iy1) = (
        ox0.saturating_add(4),
        oy0.saturating_add(4),
        ox1.saturating_sub(4),
        oy1.saturating_sub(4),
    );
    assert!(
        ix1 > ix0 && iy1 > iy0,
        "shrunk overlap must stay non-empty outside the ring edge"
    );
    let (_, overlap) = non_bg_colors(&rgba, width, height, (ix0, iy0, ix1, iy1), 120_000);
    assert_eq!(
        non_bg_count(&overlap),
        0,
        "behind kitty image must not show through the float frame"
    );
}
