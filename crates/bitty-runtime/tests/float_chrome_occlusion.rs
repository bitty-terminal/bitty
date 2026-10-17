//! CTX-1271 (#2138): detached floats must fully occlude lower chrome.
//!
//! Headed evidence (recording/ctx-1271, run 20261016T102103Z shot
//! `04-2138-float`): tiled text is hidden behind a detached float, but the
//! tiled panels' divider/border rings paint over the float interior —
//! `DrawList` order is fills, then rounded, then backgrounds, so the
//! CTX-1148 opaque float fill (a `FillRect`) sits beneath lower leaves'
//! `RoundedFill` rings. Pixel probe: the overlap interior must be exactly
//! theme background once tiled text is blanked.

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

#[test]
fn float_over_tiled_hides_lower_chrome_issue_2138() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    // Detach the left leaf: the two surviving tiled panes refill the
    // container, so their divider/gap band runs underneath the centered
    // detached float (the headed #2138 scene).
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let stats = rt.tick().expect("float frame must present");
    assert!(
        stats.rounded_fills > 0,
        "probe is vacuous unless rings paint (rounded_fills must be non-zero)"
    );
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (width, height) = (extent.width(), extent.height());
    let (fx0, fy0, fx1, fy1) = win_frame(&rt, ViewId::new(1));
    // Interior of the float frame, away from its own ring: tiled text is
    // blank (session-less leaf) so any non-background pixel here is lower
    // chrome (divider/border rings, shadows, glows) leaking over the float.
    let (ix0, iy0, ix1, iy1) = (
        fx0.saturating_add(8),
        fy0.saturating_add(8),
        fx1.saturating_sub(8),
        fy1.saturating_sub(8),
    );
    assert!(ix1 > ix0 && iy1 > iy0, "float interior must be non-empty");
    let mut leaked = 0usize;
    for y in iy0..iy1.min(height) {
        for x in ix0..ix1.min(width) {
            if pixel(&rgba, width, x, y) != THEME_BG {
                leaked += 1;
            }
        }
    }
    assert_eq!(
        leaked, 0,
        "lower chrome must not paint over the float interior (issue #2138)"
    );
}
