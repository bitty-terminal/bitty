#![forbid(unsafe_code)]
//! CTX-1148 (#1978): detached floats paint opaque.
//!
//! A detached floating panel must occlude tiled content beneath it with the
//! theme background. The grid fills cover only the content rect; without an
//! explicit frame fill the decoration inset band shows tiled content through
//! (siblings refill the container after detach). This pins headlessly:
//!
//! - tiled baseline presents opaque without floats,
//! - a detached float frame is opaque theme background over tiled content,
//! - the three-pane owner layout (float third) shows no bleed-through.

use bitty_runtime::{
    LayoutNode, OverlayTier, PresentFrame, Runtime, SplitAxis, UiRect, View, ViewId,
};

const THEME_BG: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];
const RED_BG: [u8; 4] = [0xF3, 0x8B, 0xA8, 0xFF];

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
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

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let base = (u64::from(y) * u64::from(width) + u64::from(x)) as usize * 4;
    [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
}

fn fill_primary_red(rt: &mut Runtime) {
    rt.handle_pty_bytes(b"\x1b[41m\x1b[2J\x1b[0m");
}

fn inset_band_point(rt: &Runtime, view: ViewId) -> (u32, u32) {
    let frame = frame_of(rt, view);
    let pad = rt.window_padding_physical();
    assert!(frame.frame.width > 20 && frame.frame.height > 10);
    let x = pad
        .saturating_add(frame.frame.x.max(0) as u32)
        .saturating_add(frame.frame.width / 2);
    let y = pad
        .saturating_add(frame.frame.y.max(0) as u32)
        .saturating_add(2);
    (x, y)
}

#[test]
fn tiled_baseline_presents_opaque_without_floats() {
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
    let width = extent.width();
    assert_eq!(
        pixel(&rgba, width, 0, 0),
        THEME_BG,
        "padding band keeps opaque theme background"
    );
    assert_eq!(pixel(&rgba, width, 0, 0)[3], 0xFF);
}

#[test]
fn detached_float_frame_is_opaque_over_tiled_content() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    fill_primary_red(&mut rt);
    rt.tick().expect("red baseline must present");
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 2);
    let float_frame = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("float presents");
    assert_eq!(float_frame.tier, Some(OverlayTier::Float));
    assert_eq!(
        frames.last().expect("frames").view,
        ViewId::new(2),
        "float paints last"
    );
    rt.tick().expect("float present must draw");
    let rgba = rt.headless_rgba().expect("rgba after float");
    let extent = rt.surface_extent().expect("extent");
    let width = extent.width();
    let (x, y) = inset_band_point(&rt, ViewId::new(2));
    assert!(
        x + 1 < width && y + 1 < extent.height(),
        "inset probe stays inside the window"
    );
    assert_eq!(
        pixel(&rgba, width, x, y),
        THEME_BG,
        "float inset band must be opaque theme background, not tiled content beneath"
    );
    assert_ne!(
        pixel(&rgba, width, x, y),
        RED_BG,
        "tiled red must not bleed through the float frame"
    );
    let frame = frame_of(&rt, ViewId::new(2));
    let pad = rt.window_padding_physical();
    let base_x = pad.saturating_add(frame.frame.x.max(0) as u32);
    let base_y = pad.saturating_add(frame.frame.y.max(0) as u32);
    let mut bleed = 0usize;
    for dy in 1..frame.frame.height.min(12) {
        for dx in 1..frame.frame.width.min(12) {
            let px = base_x.saturating_add(dx);
            let py = base_y.saturating_add(dy);
            if px >= width || py >= extent.height() {
                continue;
            }
            if pixel(&rgba, width, px, py) == RED_BG {
                bleed += 1;
            }
        }
    }
    assert_eq!(bleed, 0, "no tiled red may show through the float frame");
}

#[test]
fn three_pane_float_third_has_no_bleed_through() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    fill_primary_red(&mut rt);
    rt.tick().expect("red baseline must present");
    rt.float_detach(ViewId::new(3))
        .expect("detach third must apply");
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 3);
    let float_frame = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(3))
        .expect("third presents as float");
    assert_eq!(float_frame.tier, Some(OverlayTier::Float));
    rt.tick().expect("float present must draw");
    let rgba = rt.headless_rgba().expect("rgba after float");
    let extent = rt.surface_extent().expect("extent");
    let width = extent.width();
    let frame = frame_of(&rt, ViewId::new(3));
    let pad = rt.window_padding_physical();
    let base_x = pad.saturating_add(frame.frame.x.max(0) as u32);
    let base_y = pad.saturating_add(frame.frame.y.max(0) as u32);
    let (cx, cy) = inset_band_point(&rt, ViewId::new(3));
    assert_eq!(
        pixel(&rgba, width, cx, cy),
        THEME_BG,
        "detached third inset band stays opaque theme background"
    );
    let mut opaque = 0usize;
    let mut bleed = 0usize;
    for dy in 1..frame.frame.height.min(12) {
        for dx in 1..frame.frame.width.min(12) {
            let px = base_x.saturating_add(dx);
            let py = base_y.saturating_add(dy);
            if px >= width || py >= extent.height() {
                continue;
            }
            let got = pixel(&rgba, width, px, py);
            if got == RED_BG {
                bleed += 1;
            } else if got == THEME_BG {
                opaque += 1;
            }
        }
    }
    assert_eq!(bleed, 0, "no tiled red may show through the detached third");
    assert!(opaque > 0, "float frame carries opaque theme background");
}
