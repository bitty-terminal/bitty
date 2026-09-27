#![forbid(unsafe_code)]
//! Overlay bounds share one unit across the cell path and the present path
//! (CTX-0807, issue #1481).
//!
//! `LayoutNode::Overlay` bounds are authored in cells: the cell-path solver
//! (`layout_allocations`), `layout_cmd` (`overlay:5,5,20,10`), and Alt+drag
//! moves all read them that way. The decorated present solver received a
//! pixel area but used the raw cell numbers as pixels, so a float presented
//! as a sliver near the window origin and its pane grid followed a 1x1 frame.
//! This pins the present frame of a float to its cell bounds scaled by the
//! live cell size, with the geometry taken from public seams only.

use bitty_runtime::{LayoutNode, PresentFrame, Runtime, RuntimeConfig, UiRect, View, ViewId};

const BASE: ViewId = ViewId::new(1);
const FLOAT: ViewId = ViewId::new(2);

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

#[test]
fn float_presents_at_its_cell_bounds() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let bounds = UiRect::new(20, 6, 30, 8);
    rt.set_layout(LayoutNode::overlay(
        LayoutNode::leaf(View::new(BASE, 80, 24)),
        LayoutNode::leaf(View::new(FLOAT, 30, 8)),
        bounds,
    ));
    let (cw, ch) = rt.live_cell_size();
    let float = frame_of(&rt, FLOAT);

    assert_eq!(
        (float.frame.x, float.frame.y),
        (
            i32::from(bounds.x) * i32::try_from(cw).expect("cell width fits"),
            i32::from(bounds.y) * i32::try_from(ch).expect("cell height fits"),
        ),
        "the float's frame origin is its cell origin in pixels"
    );
    assert_eq!(
        (float.frame.width, float.frame.height),
        (u32::from(bounds.width) * cw, u32::from(bounds.height) * ch),
        "the float's frame spans its cell bounds in pixels"
    );
    // The content grid is the frame minus Core decoration, so it is a real
    // grid, not the 1x1 grid a pixel-sized sliver produced.
    assert!(
        float.cols > bounds.width / 2 && float.cols <= bounds.width,
        "float grid columns follow its bounds, got {}",
        float.cols
    );
    assert!(
        float.rows > bounds.height / 2 && float.rows <= bounds.height,
        "float grid rows follow its bounds, got {}",
        float.rows
    );
    // The float paints above the base, which still covers the container.
    let base = frame_of(&rt, BASE);
    assert!(base.frame.width > float.frame.width);
}

#[test]
fn float_past_the_container_edge_is_clipped_not_overflowing() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::overlay(
        LayoutNode::leaf(View::new(BASE, 80, 24)),
        LayoutNode::leaf(View::new(FLOAT, 30, 8)),
        UiRect::new(70, 20, 30, 8),
    ));
    let base = frame_of(&rt, BASE);
    let float = frame_of(&rt, FLOAT);
    let right = |frame: &PresentFrame| i64::from(frame.frame.x) + i64::from(frame.frame.width);
    let bottom = |frame: &PresentFrame| i64::from(frame.frame.y) + i64::from(frame.frame.height);
    assert!(
        right(&float) <= right(&base) && bottom(&float) <= bottom(&base),
        "an overlay clips to its parent area: float {float:?}, base {base:?}"
    );
}
