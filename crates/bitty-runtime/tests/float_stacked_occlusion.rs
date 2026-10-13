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
