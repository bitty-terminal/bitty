#![forbid(unsafe_code)]
//! Issue #2939: focus-follows across mouse-capturing panes, and directional
//! focus resolved from the current focus point.
//!
//! Two owner-reported defects on 0.0.33 with
//! `mouse.focus_follows_mouse = true`:
//!
//! A. A pane running a mouse-tracking TUI (`1000`/`1002`/`1003`) pinned
//!    keyboard focus: hover activation returned early whenever the *focused*
//!    pane captured the mouse, so once the pointer left that pane nothing
//!    moved focus again. The focus decision must read the raw pointer
//!    position, resolved to a pane before capture routing, and it must never
//!    consult the capture state.
//! B. `Mod+l` in a tall-left + stacked-right layout picked the neighbour by
//!    the source pane's geometric centre, so a pane the user was not looking
//!    at won. The neighbour must be chosen from the focus point — the
//!    pointer under `focus_follows_mouse`, otherwise the pane's last focus
//!    point or centre.
//!
//! Every position derives from public geometry (`present_frames`,
//! `live_cell_size`, `window_padding_physical`), so the fixtures stay exact
//! at any cell metric.

use bitty_platform::CursorPosition;
use bitty_runtime::{
    FocusDirection, LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View,
    ViewId,
};

const LEFT: ViewId = ViewId::new(1);
const TOP_RIGHT: ViewId = ViewId::new(2);
const BOTTOM_RIGHT: ViewId = ViewId::new(3);

fn leaf(id: ViewId) -> LayoutNode {
    LayoutNode::leaf(View::new(id, 80, 24))
}

/// Side-by-side split at `ratio` (left child first).
fn columns(left: LayoutNode, right: LayoutNode, ratio: f32) -> LayoutNode {
    LayoutNode::split(SplitAxis::Horizontal, ratio, left, right)
}

/// Stacked split at `ratio` (top child first).
fn rows(top: LayoutNode, bottom: LayoutNode, ratio: f32) -> LayoutNode {
    LayoutNode::split(SplitAxis::Vertical, ratio, top, bottom)
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Physical cursor position at the centre of frame-local cell `(row, col)`
/// of `view`'s content frame.
fn cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cell_w, cell_h) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + (f64::from(col) + 0.5) * f64::from(cell_w),
        y: pad + f64::from(frame.content.y) + (f64::from(row) + 0.5) * f64::from(cell_h),
    }
}

/// Physical cursor position in the band between two vertically stacked
/// frames, centred horizontally inside `view`'s content frame.
fn seam_center(rt: &Runtime, view: ViewId, above: ViewId, below: ViewId) -> CursorPosition {
    let upper = frame_of(rt, above).frame;
    let lower = frame_of(rt, below).frame;
    let host = frame_of(rt, view).content;
    let pad = f64::from(rt.window_padding_physical());
    let top = f64::from(upper.y) + f64::from(upper.height);
    let bottom = f64::from(lower.y);
    CursorPosition {
        x: pad + f64::from(host.x) + f64::from(host.width) / 2.0,
        y: pad + (top + bottom) / 2.0,
    }
}

/// Tall left pane plus a right column split top/bottom: the owner's layout
/// after two `Mod+n` presses.
fn tall_left_stacked_right() -> LayoutNode {
    columns(
        leaf(LEFT),
        rows(leaf(TOP_RIGHT), leaf(BOTTOM_RIGHT), 0.5),
        0.5,
    )
}

/// Mirrored fixture for the `h` direction.
fn stacked_left_tall_right() -> LayoutNode {
    columns(
        rows(leaf(LEFT), leaf(BOTTOM_RIGHT), 0.5),
        leaf(TOP_RIGHT),
        0.5,
    )
}

fn hover_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: std::time::Duration::ZERO,
        ..RuntimeConfig::default()
    })
    .expect("hover runtime must build")
}

fn keyboard_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("keyboard runtime must build")
}

/// SGR any-event tracking: `1003` motion plus `1006` coordinates.
const SGR_ANY_MOTION: &[u8] = b"\x1b[?1003h\x1b[?1006h";

// ---------------------------------------------------------------------------
// A. Hover focus is pointer-driven, never capture-driven
// ---------------------------------------------------------------------------

#[test]
fn hover_moves_focus_off_a_mouse_tracking_pane_issue_2939() {
    // Owner repro: opencode/btop holds focus, the pointer leaves, and focus
    // never moves again. The tracking pane is the primary grid here, which
    // is the same mode register the capture decision reads.
    let mut rt = hover_runtime();
    install(&mut rt, columns(leaf(LEFT), leaf(TOP_RIGHT), 0.5));
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    assert_eq!(rt.focused_view(), Some(LEFT));
    rt.handle_cursor_moved(cell_center(&rt, TOP_RIGHT, 5, 5));
    assert_eq!(
        rt.focused_view(),
        Some(TOP_RIGHT),
        "hover must move focus out of a pane that captures the mouse"
    );
    // ...and back into it: capture on the destination pane must not pin it
    // either (the btop repro: focus enters the tracking pane, then sticks).
    rt.handle_cursor_moved(cell_center(&rt, LEFT, 5, 5));
    assert_eq!(
        rt.focused_view(),
        Some(LEFT),
        "hover must move focus back into a capturing pane"
    );
    rt.handle_cursor_moved(cell_center(&rt, TOP_RIGHT, 5, 5));
    assert_eq!(
        rt.focused_view(),
        Some(TOP_RIGHT),
        "repeated hover must keep following the pointer"
    );
}

#[test]
fn hover_dwell_still_commits_out_of_a_mouse_tracking_pane_issue_2939() {
    // The dwell path must not consult capture either: the pointer stops on
    // another pane and the deadline alone commits focus.
    let delay = std::time::Duration::from_millis(120);
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: delay,
        ..RuntimeConfig::default()
    })
    .expect("hover runtime must build");
    install(&mut rt, columns(leaf(LEFT), leaf(TOP_RIGHT), 0.5));
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    let t0 = std::time::Instant::now();
    rt.handle_cursor_moved_at(cell_center(&rt, TOP_RIGHT, 5, 5), t0);
    assert_eq!(rt.focused_view(), Some(LEFT), "dwell defers the commit");
    assert_eq!(rt.hover_activation_deadline(), Some(t0 + delay));
    let _ = rt.tick_at(t0 + delay);
    assert_eq!(
        rt.focused_view(),
        Some(TOP_RIGHT),
        "the dwell deadline must move focus off a capturing pane"
    );
}

// ---------------------------------------------------------------------------
// B. Directional focus follows the focus point
// ---------------------------------------------------------------------------

#[test]
fn directional_focus_follows_the_pointer_in_a_stacked_column_issue_2939() {
    let mut rt = hover_runtime();
    install(&mut rt, tall_left_stacked_right());
    // Pointer at the top of the tall left pane: `l` lands top-right.
    rt.handle_cursor_moved(cell_center(&rt, LEFT, 2, 5));
    assert_eq!(rt.focused_view(), Some(LEFT));
    assert_eq!(
        rt.move_focus(FocusDirection::Right),
        Some(TOP_RIGHT),
        "l from the upper half must land on the upper right pane"
    );
    // Pointer at the bottom of the tall left pane: `l` lands bottom-right.
    // The pre-fix rule ranks by the source pane's centre, so it returns the
    // top-right pane here.
    rt.handle_cursor_moved(cell_center(&rt, LEFT, 21, 5));
    assert_eq!(rt.focused_view(), Some(LEFT));
    assert_eq!(
        rt.move_focus(FocusDirection::Right),
        Some(BOTTOM_RIGHT),
        "l from the lower half must land on the lower right pane"
    );
}

#[test]
fn directional_focus_h_uses_the_pointer_focus_point_issue_2939() {
    // Mirror fixture: stacked left column, tall right pane. From the tall
    // right pane, `h` must reach the left pane at the pointer's height.
    let mut rt = hover_runtime();
    install(&mut rt, stacked_left_tall_right());
    rt.handle_cursor_moved(cell_center(&rt, TOP_RIGHT, 2, 5));
    assert_eq!(rt.focused_view(), Some(TOP_RIGHT));
    assert_eq!(
        rt.move_focus(FocusDirection::Left),
        Some(LEFT),
        "h from the upper half must land on the upper left pane"
    );
    rt.handle_cursor_moved(cell_center(&rt, TOP_RIGHT, 21, 5));
    assert_eq!(rt.focused_view(), Some(TOP_RIGHT));
    assert_eq!(
        rt.move_focus(FocusDirection::Left),
        Some(BOTTOM_RIGHT),
        "h from the lower half must land on the lower left pane"
    );
}

#[test]
fn directional_focus_j_and_k_use_the_pointer_focus_point_issue_2939() {
    // A full-width pane over a split row (and its mirror): `j`/`k` must
    // cross at the pointer's column, not at the source pane's centre (which
    // sits in the vertical gap band between the two lower panes).
    let mut rt = hover_runtime();
    install(
        &mut rt,
        rows(
            leaf(LEFT),
            columns(leaf(TOP_RIGHT), leaf(BOTTOM_RIGHT), 0.5),
            0.5,
        ),
    );
    rt.handle_cursor_moved(cell_center(&rt, LEFT, 2, 55));
    assert_eq!(rt.focused_view(), Some(LEFT));
    assert_eq!(
        rt.move_focus(FocusDirection::Down),
        Some(BOTTOM_RIGHT),
        "j from the right half of the full-width pane lands on the bottom-right pane"
    );
    // Same row on the left half keeps the left-column answer.
    rt.handle_cursor_moved(cell_center(&rt, LEFT, 2, 5));
    assert_eq!(rt.focused_view(), Some(LEFT));
    assert_eq!(
        rt.move_focus(FocusDirection::Down),
        Some(TOP_RIGHT),
        "j from the left half lands on the bottom-left pane"
    );

    // Mirror fixture for `k`.
    let mut rt = hover_runtime();
    install(
        &mut rt,
        rows(
            columns(leaf(LEFT), leaf(TOP_RIGHT), 0.5),
            leaf(BOTTOM_RIGHT),
            0.5,
        ),
    );
    rt.handle_cursor_moved(cell_center(&rt, BOTTOM_RIGHT, 8, 55));
    assert_eq!(rt.focused_view(), Some(BOTTOM_RIGHT));
    assert_eq!(
        rt.move_focus(FocusDirection::Up),
        Some(TOP_RIGHT),
        "k from the right half of the full-width pane lands on the top-right pane"
    );
    rt.handle_cursor_moved(cell_center(&rt, BOTTOM_RIGHT, 8, 5));
    assert_eq!(rt.focused_view(), Some(BOTTOM_RIGHT));
    assert_eq!(
        rt.move_focus(FocusDirection::Up),
        Some(LEFT),
        "k from the left half lands on the top-left pane"
    );
}

#[test]
fn directional_focus_without_a_pointer_uses_the_pane_centre_issue_2939() {
    // Keyboard-only focus has no pointer intent: the source pane's centre
    // is the documented fallback, so the neighbour that contains it wins.
    let mut rt = keyboard_runtime();
    install(
        &mut rt,
        columns(
            leaf(LEFT),
            rows(leaf(TOP_RIGHT), leaf(BOTTOM_RIGHT), 0.25),
            0.5,
        ),
    );
    assert_eq!(rt.focused_view(), Some(LEFT));
    assert_eq!(
        rt.move_focus(FocusDirection::Right),
        Some(BOTTOM_RIGHT),
        "the tall lower right pane contains the source centre"
    );
}

#[test]
fn directional_focus_ties_are_deterministic_issue_2939() {
    // Pointer in the gap band of the right column: both candidates are the
    // same facing-edge distance away and equally far from the focus point,
    // so stable pane order decides — every time, from any history.
    let mut rt = hover_runtime();
    install(&mut rt, tall_left_stacked_right());
    let seam = seam_center(&rt, LEFT, TOP_RIGHT, BOTTOM_RIGHT);
    for previous in [LEFT, TOP_RIGHT, BOTTOM_RIGHT] {
        rt.set_focus(previous);
        rt.handle_cursor_moved(seam);
        assert_eq!(rt.focused_view(), Some(LEFT), "hover keeps the source");
        assert_eq!(
            rt.move_focus(FocusDirection::Right),
            Some(TOP_RIGHT),
            "the tie must resolve to the first pane in stable order"
        );
    }
    for _ in 0..3 {
        rt.set_focus(LEFT);
        rt.handle_cursor_moved(seam);
        assert_eq!(rt.move_focus(FocusDirection::Right), Some(TOP_RIGHT));
    }
}
