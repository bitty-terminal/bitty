//! CTX-1280 (#2168): tiled chrome must hide behind a detached float.
//!
//! Follow-up to #2138/#2160: those fixed lower chrome leaking OVER the float
//! interior. This is the behind-float variant: with a detached float up, the
//! surviving tiled leaves' own chrome (focus/idle rings, shadows, glows)
//! still draws divider/outline bands in the background AROUND the float. The
//! accepted design is HIDE, not dim: the surface already clears to theme
//! background in the gap bands, so suppressing tiled chrome leaves a clean
//! backdrop while tiled text stays live. The float keeps its own chrome,
//! the tiled baseline (no detached float) is unchanged, and restoring the
//! float brings tiled chrome back.

use bitty_runtime::{
    AnimationPolicy, LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

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

/// Non-background pixel count over the whole surface.
fn non_bg_count(rt: &Runtime) -> usize {
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let mut count = 0usize;
    for y in 0..height {
        for x in 0..width {
            if pixel(&rgba, width, x, y) != THEME_BG {
                count += 1;
            }
        }
    }
    count
}

fn float_frame_inflated(rt: &Runtime, view: ViewId, margin: u32) -> (u32, u32, u32, u32) {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"));
    let pad = rt.window_padding_physical() as i64;
    let x0 = (pad + i64::from(frame.frame.x)).max(0) as u32;
    let y0 = (pad + i64::from(frame.frame.y)).max(0) as u32;
    (
        x0.saturating_sub(margin),
        y0.saturating_sub(margin),
        x0.saturating_add(frame.frame.width).saturating_add(margin),
        y0.saturating_add(frame.frame.height).saturating_add(margin),
    )
}

/// Non-background pixels on the tiled leaves' frame-edge bands (where the
/// Core-owned rings paint), skipping anything under the float frame plus
/// margin. Frame-edge bands carry only chrome: tiled cell content and the
/// focused cursor cell live strictly inside the content rect, so live text
/// never samples here.
fn tiled_edge_chrome_count(rt: &Runtime, float_exclude: (u32, u32, u32, u32)) -> (usize, usize) {
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let (rx0, ry0, rx1, ry1) = float_exclude;
    let pad = rt.window_padding_physical() as i64;
    let mut leaked = 0usize;
    let mut sampled = 0usize;
    for frame in rt.present_frames() {
        if frame.tier.is_some() || frame.frame.width == 0 || frame.frame.height == 0 {
            continue;
        }
        let fx0 = (pad + i64::from(frame.frame.x)).max(0) as u32;
        let fy0 = (pad + i64::from(frame.frame.y)).max(0) as u32;
        let fx1 = fx0.saturating_add(frame.frame.width);
        let fy1 = fy0.saturating_add(frame.frame.height);
        // Outermost 2px band of the frame rect: the ring plus its AA edge.
        let mut probe = |x: u32, y: u32| {
            if x >= rx0 && x < rx1 && y >= ry0 && y < ry1 {
                return;
            }
            if x >= width || y >= height {
                return;
            }
            sampled += 1;
            if pixel(&rgba, width, x, y) != THEME_BG {
                leaked += 1;
            }
        };
        let mut x = fx0;
        while x < fx1 {
            for dy in 0..2 {
                probe(x, fy0.saturating_add(dy));
                probe(x, fy1.saturating_sub(1).saturating_sub(dy));
            }
            x += 3;
        }
        let mut y = fy0;
        while y < fy1 {
            for dx in 0..2 {
                probe(fx0.saturating_add(dx), y);
                probe(fx1.saturating_sub(1).saturating_sub(dx), y);
            }
            y += 3;
        }
    }
    (leaked, sampled)
}

#[test]
fn tiled_chrome_hidden_behind_detached_float_issue_2168() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    // Baseline: tiled rings paint, so the backdrop probe below is
    // non-vacuous once the float goes up.
    let stats = rt.tick().expect("tiled baseline must present");
    assert!(
        stats.rounded_fills > 0,
        "tiled baseline must paint rings (probe is vacuous otherwise)"
    );
    assert!(
        non_bg_count(&rt) > 0,
        "tiled baseline must show chrome pixels"
    );
    // Detach the left leaf: the survivors refill the container, so their
    // divider/gap bands run underneath and around the centered float.
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let stats = rt.tick().expect("float frame must present");
    assert!(
        stats.rounded_fills > 0,
        "the float keeps its own ring while tiled chrome hides"
    );
    let exclude = float_frame_inflated(&rt, ViewId::new(1), 4);
    let (leaked, sampled) = tiled_edge_chrome_count(&rt, exclude);
    assert!(
        sampled > 100,
        "edge probe is vacuous unless many tiled edge pixels are sampled outside the float"
    );
    assert_eq!(
        leaked, 0,
        "tiled chrome (rings, shadows, glows) must not show around a detached float (issue #2168)"
    );
    // Round trip: restoring the float brings tiled chrome back.
    rt.float_restore(ViewId::new(1))
        .expect("restore must apply");
    let stats = rt.tick().expect("restored frame must present");
    assert!(
        stats.rounded_fills > 0,
        "tiled rings return once the float is restored"
    );
    let (leaked, _) = tiled_edge_chrome_count(&rt, (0, 0, 0, 0));
    assert!(
        leaked > 0,
        "tiled edge chrome returns once the float is restored"
    );
}

#[test]
fn tiled_baseline_keeps_chrome_without_float_issue_2168() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    let stats = rt.tick().expect("tiled baseline must present");
    assert!(
        stats.rounded_fills > 0,
        "no detached float: tiled rings still paint"
    );
    assert!(
        non_bg_count(&rt) > 0,
        "no detached float: tiled chrome still shows"
    );
}
