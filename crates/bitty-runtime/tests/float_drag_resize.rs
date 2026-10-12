#![forbid(unsafe_code)]
//! CTX-1150 (issue #1980): pixel-smooth float Alt+drag with cell snap on
//! release, plus Alt+edge float resize wiring.
//!
//! Headless motion-sequence pins, virtual clock only, no PTY:
//! - Sub-cell Alt+drag moves the presented frame in pixels while the solver
//!   stays cell-quantized; release snaps to cells.
//! - Alt+press on a float border grabs an edge resize through the existing
//!   detect and rect helpers; motion changes dims within bounds. Pre-fix
//!   there was no resize path, so the grab assertion fails without the
//!   wiring.

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{LayoutNode, ResizeEdge, Runtime, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

/// Cursor pixels for the cell-mapping path (Alt+drag move): 8px padding,
/// 9x19 cells, plus the default decoration offset, matching the existing
/// Alt+drag headless geometry so drag deltas land on exact cells.
fn move_cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 14.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 14.0 + f64::from(row) * 19.0 + 9.0,
    }
}

/// Cursor pixels for the layout-point path (edge resize): 8px padding,
/// 9x19 cells, zero gaps, matching the split-handle origin so edge cells
/// map exactly.
fn layout_cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn overlay_tree() -> LayoutNode {
    LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 5)),
        UiRect::new(50, 10, 10, 5),
    )
}

fn overlay_bounds(rt: &Runtime) -> UiRect {
    match rt.layout() {
        LayoutNode::Overlay { bounds, .. } => *bounds,
        other => panic!("expected overlay tree, got {other:?}"),
    }
}

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn alt_press(rt: &mut Runtime, pos: CursorPosition) {
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
}

fn alt_release(rt: &mut Runtime) {
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn float_alt_drag_is_pixel_smooth_then_snaps_to_cells() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    assert_eq!(overlay_bounds(&rt), UiRect::new(50, 10, 10, 5));
    let home_frame = frame_of(&rt, ViewId::new(2));

    // Grab the float interior (cell 55,12 sits inside bounds 50..60 x 10..15).
    let grab = move_cell_pixels(55, 12);
    alt_press(&mut rt, grab);
    assert!(rt.alt_drag_active(), "Alt+press on float must grab");
    assert!(
        !rt.float_resize_active(),
        "interior press must move, never resize"
    );

    // Sub-cell motion (a few physical pixels, staying in the same cell):
    // probe for a delta that keeps the cell mapping put so the solver must
    // stay while the frame follows. The grab may sit anywhere inside its
    // cell, so try +4 first and fall back to -4.
    let grab_cell = rt.cursor_to_cell(grab);
    let sub = [4.0, -4.0, 2.0, -2.0, 1.0, -1.0]
        .into_iter()
        .map(|dx| CursorPosition {
            x: grab.x + dx,
            y: grab.y,
        })
        .find(|pos| rt.cursor_to_cell(*pos) == grab_cell)
        .expect("a sub-cell step must exist inside the 9px live cell");
    let sub_dx = (sub.x - grab.x).round() as i32;
    let t = Instant::now();
    assert!(
        rt.update_alt_drag_at(sub, t),
        "sub-cell motion keeps the drag armed",
    );
    assert_eq!(
        overlay_bounds(&rt),
        UiRect::new(50, 10, 10, 5),
        "solver stays cell-quantized on sub-cell motion",
    );
    let smooth = frame_of(&rt, ViewId::new(2));
    assert_eq!(
        smooth.frame.x - home_frame.frame.x,
        sub_dx,
        "presented frame must follow sub-cell motion in pixels",
    );
    assert_eq!(
        smooth.frame.y, home_frame.frame.y,
        "no vertical motion means no vertical shift",
    );

    // Whole-cell motion (+5 cols, +2 rows via exact cells): bounds follow
    // by cells and the pixel remainder stays bounded (one live cell each
    // way). Virtual clock throughout: no wall-clock sleep.
    let t_move = t + Duration::from_millis(10);
    rt.handle_cursor_moved_at(move_cell_pixels(60, 14), t_move);
    assert_eq!(
        overlay_bounds(&rt),
        UiRect::new(55, 12, 10, 5),
        "whole-cell motion moves the solver bounds",
    );

    // Release snaps to cells: the frame lands exactly on the solver bounds
    // with no pixel remainder (the drop keeps whole cells only).
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.alt_drag_active());
    let snapped = frame_of(&rt, ViewId::new(2));
    assert_eq!(
        overlay_bounds(&rt),
        UiRect::new(55, 12, 10, 5),
        "release keeps the moved bounds",
    );
    assert_eq!(
        snapped.frame.x - home_frame.frame.x,
        5 * 9,
        "snapped frame must sit on whole cells (no pixel remainder)",
    );
    assert_eq!(
        snapped.frame.y - home_frame.frame.y,
        2 * 19,
        "snapped frame must sit on whole cells vertically",
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn float_alt_drag_pixel_remainder_is_bounded_and_virtual_clocked() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    let grab = move_cell_pixels(55, 12);
    alt_press(&mut rt, grab);
    assert!(rt.alt_drag_active());

    // A second sub-cell step accumulates but stays within one live cell.
    // Probe like above so the solver must stay put.
    let grab_cell = rt.cursor_to_cell(grab);
    let sub = [7.0, -7.0, 5.0, -5.0, 3.0, -3.0, 1.0, -1.0]
        .into_iter()
        .map(|dx| CursorPosition {
            x: grab.x + dx,
            y: grab.y,
        })
        .find(|pos| rt.cursor_to_cell(*pos) == grab_cell)
        .expect("a sub-cell step must exist");
    let t = Instant::now();
    assert!(
        rt.update_alt_drag_at(sub, t),
        "second sub-cell step stays armed",
    );
    assert_eq!(
        overlay_bounds(&rt),
        UiRect::new(50, 10, 10, 5),
        "still no solver move below one cell",
    );
    let home = {
        let mut fresh = make_runtime();
        fresh.set_layout(overlay_tree());
        frame_of(&fresh, ViewId::new(2))
    };
    let moved = frame_of(&rt, ViewId::new(2));
    assert!(
        (moved.frame.x - home.frame.x).abs() <= 9,
        "pixel remainder stays within one live cell width",
    );
    assert!(
        (moved.frame.y - home.frame.y).abs() <= 19,
        "pixel remainder stays within one live cell height",
    );

    // Virtual clock: motion arms the drag transition deterministically.
    let leaf = ViewId::new(2);
    assert_eq!(
        rt.animation_progress(bitty_runtime::AnimationKind::Drag, Some(leaf), t),
        Some(0.0),
        "drag motion must arm at the gesture time",
    );
    let mid = t + Duration::from_millis(75);
    let p = rt
        .animation_progress(bitty_runtime::AnimationKind::Drag, Some(leaf), mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid drag progress {p}");
    alt_release(&mut rt);
    assert!(!rt.alt_drag_active());
}

#[test]
fn float_edge_resize_grabs_border_and_grows_within_bounds() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    assert_eq!(overlay_bounds(&rt), UiRect::new(50, 10, 10, 5));

    // Alt+press on the right border (cell 59,12 is the bounds right edge).
    alt_press(&mut rt, layout_cell_pixels(59, 12));
    assert!(
        rt.float_resize_active(),
        "Alt+press on a float border must grab a resize (pre-fix: no path)",
    );
    assert!(!rt.alt_drag_active(), "edge press must resize, never move",);
    assert_eq!(rt.float_resize_edge(), Some(ResizeEdge::Right));
    assert_eq!(rt.float_resize_leaf(), Some(ViewId::new(2)));

    // Drag +5 cols right: width grows by cells, origin stays, height stays.
    let t = Instant::now();
    assert!(
        rt.update_float_resize_at(layout_cell_pixels(64, 12), t),
        "resize motion stays armed",
    );
    assert_eq!(
        overlay_bounds(&rt),
        UiRect::new(50, 10, 15, 5),
        "right-edge drag must grow the width",
    );

    // Corner grabs move both axes: release, then grab the bottom-right.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.float_resize_active());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    assert_eq!(
        rt.float_resize_hover_at(layout_cell_pixels(64, 14)),
        Some(ResizeEdge::BottomRight),
        "hover query must report the corner edge",
    );

    // Release ends with no selection, like the move path.
    assert!(!rt.has_selection());
}

#[test]
fn float_edge_resize_clamps_to_minima_and_container() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());

    // Shrink from the left far past the minima: width pins instead of
    // inverting, and the right edge stays put.
    alt_press(&mut rt, layout_cell_pixels(50, 12));
    assert_eq!(rt.float_resize_edge(), Some(ResizeEdge::Left));
    let t = Instant::now();
    assert!(rt.update_float_resize_at(layout_cell_pixels(79, 12), t));
    let shrunk = overlay_bounds(&rt);
    assert_eq!(
        shrunk.width,
        View::MIN_COLS,
        "collapse must pin to the view minima",
    );
    assert_eq!(
        shrunk.x + shrunk.width,
        50 + 10,
        "left shrink keeps the right edge",
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));

    // Grow past the container clips: the float never leaves the container.
    // Fresh layout so the shrink above cannot interfere with the edge cells.
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    alt_press(&mut rt, layout_cell_pixels(59, 12));
    assert_eq!(rt.float_resize_edge(), Some(ResizeEdge::Right));
    let t = Instant::now();
    assert!(rt.update_float_resize_at(layout_cell_pixels(79, 12), t));
    let grown = overlay_bounds(&rt);
    assert!(
        grown.x + grown.width <= 80,
        "resized float must stay inside the 80-col container",
    );
    assert_eq!(grown.x, 50, "right grow keeps the left edge");
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    assert!(!rt.has_selection());
}
