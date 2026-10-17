#![forbid(unsafe_code)]
//! CTX-1173 (#2031): focused lower cursor under a higher float stays hidden.
//!
//! Residual of #1993: the CTX-1165 occluder table drops lower glyphs covered
//! by a higher detached float, and the same table must gate the deferred
//! cursor paint or the focused lower cursor leaks through the upper opaque
//! fill. Pinned headlessly on a blank grid (the cursor is the only non-bg
//! paint, so the probe is exact):
//!
//! - stacked floats with the lower focused and the cursor under the upper
//!   frame paints theme background only,
//! - the same setup with the cursor in the visible region still paints,
//! - a single float with the cursor visible still paints.

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

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let base = (y as usize * width as usize + x as usize) * 4;
    [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
}

fn cell_intersects_upper(
    cell_x: i64,
    cell_y: i64,
    cell_w: i64,
    cell_h: i64,
    upper: (i64, i64, i64, i64),
) -> bool {
    let (ux, uy, uw, uh) = upper;
    cell_x < ux + uw && ux < cell_x + cell_w && cell_y < uy + uh && uy < cell_y + cell_h
}

fn lower_geometry(rt: &Runtime, lower: ViewId) -> (i64, i64, i64, i64, u16, u16) {
    let frame = frame_of(rt, lower);
    let pad = i64::from(rt.window_padding_physical());
    let (cw, ch) = rt.live_cell_size();
    let ox = pad + i64::from(frame.content.x);
    let oy = pad + i64::from(frame.content.y);
    (ox, oy, i64::from(cw), i64::from(ch), frame.cols, frame.rows)
}

fn upper_frame_rect(rt: &Runtime, upper: ViewId) -> (i64, i64, i64, i64) {
    let frame = frame_of(rt, upper);
    let pad = i64::from(rt.window_padding_physical());
    let ux = pad + i64::from(frame.frame.x);
    let uy = pad + i64::from(frame.frame.y);
    (
        ux,
        uy,
        i64::from(frame.frame.width),
        i64::from(frame.frame.height),
    )
}

fn find_covered_cell(rt: &Runtime, lower: ViewId, upper: ViewId) -> (u16, u16) {
    let (ox, oy, cw, ch, cols, rows) = lower_geometry(rt, lower);
    let rect = upper_frame_rect(rt, upper);
    for row in 0..rows {
        for col in 0..cols {
            let x = ox + i64::from(col) * cw;
            let y = oy + i64::from(row) * ch;
            if cell_intersects_upper(x, y, cw, ch, rect) {
                return (row, col);
            }
        }
    }
    panic!("lower and upper floats must overlap for the cursor probe")
}

fn find_visible_cell(rt: &Runtime, lower: ViewId, upper: ViewId) -> (u16, u16) {
    let (ox, oy, cw, ch, cols, rows) = lower_geometry(rt, lower);
    let rect = upper_frame_rect(rt, upper);
    for row in 0..rows {
        for col in 0..cols {
            let x = ox + i64::from(col) * cw;
            let y = oy + i64::from(row) * ch;
            if !cell_intersects_upper(x, y, cw, ch, rect) {
                return (row, col);
            }
        }
    }
    panic!("lower float must keep a visible band outside the upper frame")
}

fn move_cursor(rt: &mut Runtime, row: u16, col: u16) {
    let seq = format!("\x1b[{};{}H", u32::from(row) + 1, u32::from(col) + 1);
    rt.handle_pty_bytes(seq.as_bytes());
}

fn cursor_center_pixel(rt: &Runtime, lower: ViewId, row: u16, col: u16) -> (u32, u32) {
    let (ox, oy, cw, ch, _, _) = lower_geometry(rt, lower);
    let x = (ox + i64::from(col) * cw + cw / 2).max(0) as u32;
    let y = (oy + i64::from(row) * ch + ch / 2).max(0) as u32;
    (x, y)
}

fn sample_cursor_cell(rt: &Runtime, lower: ViewId, row: u16, col: u16) -> [u8; 4] {
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.surface_extent().expect("extent");
    let (x, y) = cursor_center_pixel(rt, lower, row, col);
    assert!(
        x < extent.width() && y < extent.height(),
        "cursor sample must land inside the surface"
    );
    pixel(&rgba, extent.width(), x, y)
}

#[test]
fn stacked_float_focused_lower_cursor_raises_above_upper() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(1))
        .expect("detach lower must apply");
    rt.float_detach(ViewId::new(2))
        .expect("detach upper must apply");
    assert!(rt.set_focus(ViewId::new(1)), "lower float must focus");
    let (row, col) = find_covered_cell(&rt, ViewId::new(1), ViewId::new(2));
    move_cursor(&mut rt, row, col);
    rt.tick().expect("cursor frame must present");
    assert_ne!(
        sample_cursor_cell(&rt, ViewId::new(1), row, col),
        THEME_BG,
        "focused lower float must raise above the former upper float"
    );
    let (row2, col2) = find_covered_cell(&rt, ViewId::new(1), ViewId::new(2));
    move_cursor(&mut rt, row2, col2);
    rt.tick().expect("second cursor frame must present");
    assert_ne!(
        sample_cursor_cell(&rt, ViewId::new(1), row2, col2),
        THEME_BG,
        "raised float cursor must remain visible across retained reuse"
    );
}

#[test]
fn stacked_float_focused_lower_cursor_in_visible_region_paints() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(1))
        .expect("detach lower must apply");
    rt.float_detach(ViewId::new(2))
        .expect("detach upper must apply");
    assert!(rt.set_focus(ViewId::new(1)), "lower float must focus");
    let (row, col) = find_visible_cell(&rt, ViewId::new(1), ViewId::new(2));
    move_cursor(&mut rt, row, col);
    rt.tick().expect("cursor frame must present");
    assert_ne!(
        sample_cursor_cell(&rt, ViewId::new(1), row, col),
        THEME_BG,
        "cursor in the visible region must still paint"
    );
}

#[test]
fn single_float_cursor_in_visible_region_paints() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    assert!(rt.set_focus(ViewId::new(1)), "tiled survivor must focus");
    let (row, col) = find_visible_cell(&rt, ViewId::new(1), ViewId::new(2));
    move_cursor(&mut rt, row, col);
    rt.tick().expect("cursor frame must present");
    assert_ne!(
        sample_cursor_cell(&rt, ViewId::new(1), row, col),
        THEME_BG,
        "single-float baseline must keep painting the visible cursor"
    );
}
