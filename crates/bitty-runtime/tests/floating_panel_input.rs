#![forbid(unsafe_code)]
//! Issue #2945: input for a workspace-detached floating panel.
//!
//! A panel floated with `toggle_floating` (default `Mod+a`) paints over the
//! active scene, but nothing used to reach it: the wheel and `Mod+u`/`Mod+i`
//! resolved the *focused* tiling leaf (the panel re-homes focus to the
//! survivor when it detaches, so the visible panel never scrolled while the
//! hidden pane behind it did), and a corner/edge drag fell through to the
//! Alt+drag *move* because only structural overlay bounds were resizable.
//! These tests pin the three reported symptoms headlessly:
//!
//! 1. a hit-tested floating leaf receives wheel and page scroll exactly like a
//!    tiled pane;
//! 2. corner and edge drags resize the anchored panel inside the window and
//!    its configured minimum;
//! 3. `Mod+Shift+<n>` (the DEC-0034 cross-workspace move, which also switches)
//!    works with a floating panel focused, and the panel follows its
//!    documented pin policy.
//!
//! Geometry is derived from public seams only ([`Runtime::present_frames`],
//! [`Runtime::live_cell_size`], [`Runtime::window_padding_physical`]) — no
//! hard-coded pixel, padding, or decoration constants.

use bitty_platform::{
    CursorPosition, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey,
    PressState, ScrollDelta,
};
use bitty_runtime::{
    LayoutNode, OverlayTier, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build headless")
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn feed_history(rt: &mut Runtime, lines: usize) {
    for i in 0..lines {
        rt.handle_pty_bytes(format!("history {i:03}\n").as_bytes());
    }
    rt.tick();
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Centre pixel of container cell `(col, row)`.
///
/// Overlay bounds are authored in container cells and placed at
/// `cell * live cell size` behind the window padding, so the centre of a cell
/// is unambiguous for both paint and hit-testing.
fn cell_center(rt: &Runtime, col: u16, row: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(col) * f64::from(cw) + f64::from(cw) / 2.0,
        y: pad + f64::from(row) * f64::from(ch) + f64::from(ch) / 2.0,
    }
}

fn named_key(named: NamedKey, state: PressState) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn mouse(button: MouseButton, state: PressState) -> MouseEvent {
    MouseEvent::new(button, state)
}

fn offset(rt: &Runtime, view: ViewId) -> usize {
    rt.presented_scroll_offset(view)
        .unwrap_or_else(|| panic!("{view:?} must be live"))
}

/// Detaches the primary leaf, leaving focus re-homed on the survivor — the
/// exact state a `toggle_floating` leaves behind and the reason "the focused
/// pane" and "the panel the user is looking at" disagree.
///
/// Hover activation is opted out (`focus_follows_mouse = false`, the
/// click-to-focus configuration) so the pointer can sit over the panel while
/// focus stays on the covered leaf — the state a keyboard toggle leaves
/// anyway, because the panel is composited under a *stationary* pointer and no
/// `CursorMoved` event ever re-evaluates hover focus.
fn detached_primary_runtime() -> (Runtime, ViewId, ViewId) {
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("click-to-focus runtime must build");
    install(&mut rt, two_pane());
    feed_history(&mut rt, 200);
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)), "primary seed");
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let float = ViewId::new(1);
    let base = rt.focused_view().expect("detach re-homes focus");
    assert_ne!(base, float, "focus re-homes off the detached panel");
    assert_eq!(rt.floated_views(), vec![float]);
    (rt, float, base)
}

#[test]
fn hit_tested_float_receives_wheel_scroll_not_the_focused_base() {
    let (mut rt, float, base) = detached_primary_runtime();
    // Point at the panel without focusing it (`focus_follows_mouse` is off by
    // default, and a pointer over a panel is not a focus decision).
    let frame = frame_of(&rt, float);
    assert_eq!(frame.tier, Some(OverlayTier::Float));
    let (cw, ch) = rt.live_cell_size();
    let centre_col = (frame.frame.x.max(0) as u32 + frame.frame.width / 2) / cw.max(1);
    let centre_row = (frame.frame.y.max(0) as u32 + frame.frame.height / 2) / ch.max(1);
    let pos = cell_center(&rt, centre_col as u16, centre_row as u16);
    rt.handle_cursor_moved(pos);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(float),
        "the pointer resolves the visible panel, not the covered base leaf"
    );
    assert_eq!(
        rt.focused_view(),
        Some(base),
        "focus stays on the tiling leaf the panel covers"
    );

    let float_before = offset(&rt, float);
    let base_before = offset(&rt, base);
    assert_eq!(float_before, 0, "the panel starts at live output");
    assert_eq!(base_before, 0);

    // Three notches of wheel-up: the panel under the pointer must scroll.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 3.0));
    assert!(
        offset(&rt, float) > float_before,
        "wheel over the floating panel must scroll the panel (got {})",
        offset(&rt, float)
    );
    assert_eq!(
        offset(&rt, base),
        base_before,
        "the covered tiling leaf must not scroll instead"
    );

    // Wheel-down returns to live, exactly like a tiled pane.
    let scrolled = offset(&rt, float);
    rt.handle_wheel(ScrollDelta::Lines(0.0, -3.0));
    assert!(
        offset(&rt, float) < scrolled,
        "wheel down must scroll the panel back toward live"
    );
}

#[test]
fn hit_tested_float_receives_page_scroll_like_a_tiled_pane() {
    let (mut rt, float, base) = detached_primary_runtime();
    let frame = frame_of(&rt, float);
    let (cw, ch) = rt.live_cell_size();
    let centre_col = (frame.frame.x.max(0) as u32 + frame.frame.width / 2) / cw.max(1);
    let centre_row = (frame.frame.y.max(0) as u32 + frame.frame.height / 2) / ch.max(1);
    let pos = cell_center(&rt, centre_col as u16, centre_row as u16);
    rt.handle_cursor_moved(pos);
    assert_eq!(rt.focused_view(), Some(base));

    assert!(rt.scroll_focused_page(true), "page up must find a pane");
    let paged = offset(&rt, float);
    assert!(
        paged > 0,
        "Mod+u must page the floating panel under the pointer (got {paged})"
    );
    assert_eq!(
        offset(&rt, base),
        0,
        "the covered tiling leaf must not page instead"
    );
    // A page is the configured share of the panel's own rows, so it moves more
    // than one line but never past the panel's history.
    let rows = usize::from(frame.rows.max(1));
    assert!(
        paged <= rows.max(1) + rows,
        "one page stays bounded: {paged}"
    );

    assert!(rt.scroll_focused_page(false), "page down must find a pane");
    assert_eq!(
        offset(&rt, float),
        0,
        "one page back returns to live output"
    );
}

/// Pixel position of a container cell's edge, used for corner/edge grabs.
fn cell_pixel(rt: &Runtime, col: u16, row: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(col) * f64::from(cw) + 1.0,
        y: pad + f64::from(row) * f64::from(ch) + 1.0,
    }
}

fn alt(rt: &mut Runtime, state: PressState) {
    rt.handle_key_event(named_key(NamedKey::Alt, state));
}

fn content_box(rt: &Runtime) -> (i64, i64) {
    let (cw, ch) = rt.live_cell_size();
    (
        i64::from(rt.container().width) * i64::from(cw),
        i64::from(rt.container().height) * i64::from(ch),
    )
}

#[test]
fn corner_and_edge_drags_resize_the_floating_panel() {
    let (mut rt, float, _base) = detached_primary_runtime();
    let home = frame_of(&rt, float);
    let (cw, ch) = rt.live_cell_size();

    // The panel's cell rect, derived from its painted frame (the anchor is
    // authored in container cells and painted at `cell * live cell size`).
    let left = (home.frame.x.max(0) as u32 / cw.max(1)) as u16;
    let top = (home.frame.y.max(0) as u32 / ch.max(1)) as u16;

    // Top-left corner: grab one cell inside the corner, drag two cells left
    // and one cell up. The panel must grow toward the origin.
    let grab = cell_center(&rt, left, top);
    rt.handle_cursor_moved(grab);
    assert_eq!(
        rt.float_resize_hover_at(grab),
        Some(bitty_runtime::ResizeEdge::TopLeft),
        "the painted corner must hit-test as a corner handle"
    );
    alt(&mut rt, PressState::Pressed);
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
    assert!(
        rt.float_resize_active(),
        "Alt+press on the panel corner must start a resize"
    );
    assert!(
        !rt.alt_drag_active(),
        "the corner grab must not fall through to the Alt+drag move"
    );
    assert_eq!(rt.float_resize_leaf(), Some(float));
    rt.handle_cursor_moved(cell_pixel(&rt, left - 2, top - 1));
    let grown = frame_of(&rt, float);
    assert!(
        grown.frame.width > home.frame.width && grown.frame.height > home.frame.height,
        "top-left drag must grow the panel, got {grown:?} from {home:?}"
    );
    assert!(
        grown.frame.x < home.frame.x && grown.frame.y < home.frame.y,
        "the grabbed corner is the one that moves"
    );
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
    alt(&mut rt, PressState::Released);
    assert!(!rt.float_resize_active(), "release ends the resize");

    // Bottom-right corner: same gesture on the far side shrinks it back.
    let current = frame_of(&rt, float);
    let cur_left = (current.frame.x.max(0) as u32 / cw.max(1)) as u16;
    let cur_top = (current.frame.y.max(0) as u32 / ch.max(1)) as u16;
    let cur_right = cur_left + (current.frame.width / cw.max(1)) as u16 - 1;
    let cur_bottom = cur_top + (current.frame.height / ch.max(1)) as u16 - 1;
    let grab = cell_center(&rt, cur_right, cur_bottom);
    rt.handle_cursor_moved(grab);
    assert_eq!(
        rt.float_resize_hover_at(grab),
        Some(bitty_runtime::ResizeEdge::BottomRight)
    );
    alt(&mut rt, PressState::Pressed);
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
    assert!(rt.float_resize_active());
    rt.handle_cursor_moved(cell_pixel(&rt, cur_right - 4, cur_bottom - 2));
    let shrunk = frame_of(&rt, float);
    assert!(
        shrunk.frame.width < current.frame.width && shrunk.frame.height < current.frame.height,
        "bottom-right drag must shrink the panel"
    );
    assert_eq!(
        (shrunk.frame.x, shrunk.frame.y),
        (current.frame.x, current.frame.y),
        "the opposite corner stays pinned"
    );
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
    alt(&mut rt, PressState::Released);

    // Right edge: a mid-height grab on the right border resizes width only.
    let current = frame_of(&rt, float);
    let cur_left = (current.frame.x.max(0) as u32 / cw.max(1)) as u16;
    let cur_top = (current.frame.y.max(0) as u32 / ch.max(1)) as u16;
    let cur_right = cur_left + (current.frame.width / cw.max(1)) as u16 - 1;
    let mid_row = cur_top + (current.frame.height / ch.max(1)) as u16 / 2;
    let grab = cell_center(&rt, cur_right, mid_row);
    rt.handle_cursor_moved(grab);
    assert_eq!(
        rt.float_resize_hover_at(grab),
        Some(bitty_runtime::ResizeEdge::Right),
        "the right border must hit-test as a single-axis edge"
    );
    alt(&mut rt, PressState::Pressed);
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
    rt.handle_cursor_moved(cell_pixel(&rt, cur_right + 3, mid_row));
    let wider = frame_of(&rt, float);
    assert!(
        wider.frame.width > current.frame.width,
        "right-edge drag must widen the panel"
    );
    assert_eq!(
        wider.frame.height, current.frame.height,
        "a single-axis edge must not change the other axis"
    );
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
    alt(&mut rt, PressState::Released);

    // Every committed extent stays inside the window.
    let (box_w, box_h) = content_box(&rt);
    let final_frame = frame_of(&rt, float);
    assert!(
        i64::from(final_frame.frame.x) >= 0 && i64::from(final_frame.frame.y) >= 0,
        "the panel keeps its container origin: {final_frame:?}"
    );
    assert!(
        i64::from(final_frame.frame.x) + i64::from(final_frame.frame.width) <= box_w,
        "the panel must not spill past the window's right edge: {final_frame:?}"
    );
    assert!(
        i64::from(final_frame.frame.y) + i64::from(final_frame.frame.height) <= box_h,
        "the panel must not spill past the window's bottom edge: {final_frame:?}"
    );
}

#[test]
fn oversized_inward_drag_clamps_to_the_pane_minimum() {
    let (mut rt, float, _base) = detached_primary_runtime();
    let home = frame_of(&rt, float);
    let (cw, ch) = rt.live_cell_size();
    let left = (home.frame.x.max(0) as u32 / cw.max(1)) as u16;
    let top = (home.frame.y.max(0) as u32 / ch.max(1)) as u16;
    let right = left + (home.frame.width / cw.max(1)) as u16;

    // Grab the left edge and drag it far past the right edge: the step must
    // clamp at the pane minimum instead of collapsing to nothing.
    let grab = cell_center(&rt, left, top + (home.frame.height / ch.max(1)) as u16 / 2);
    rt.handle_cursor_moved(grab);
    assert_eq!(
        rt.float_resize_hover_at(grab),
        Some(bitty_runtime::ResizeEdge::Left)
    );
    alt(&mut rt, PressState::Pressed);
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
    assert!(rt.float_resize_active());
    rt.handle_cursor_moved(cell_pixel(&rt, right, top + 1));
    let squeezed = frame_of(&rt, float);
    assert!(
        squeezed.cols >= 1 && squeezed.rows >= 1,
        "a degenerate drag keeps at least one cell: {squeezed:?}"
    );
    assert!(
        squeezed.frame.width < home.frame.width,
        "the inward drag shrank the panel: {squeezed:?}"
    );
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
    alt(&mut rt, PressState::Released);

    // The committed extent is durable: it survives a further present pass.
    rt.tick();
    let after_tick = frame_of(&rt, float);
    assert_eq!(
        (after_tick.frame.width, after_tick.frame.height),
        (squeezed.frame.width, squeezed.frame.height),
        "a tick must not reset the resized extent"
    );
}

#[test]
fn resize_extent_is_dropped_when_the_panel_returns_to_tiling() {
    let (mut rt, float, _base) = detached_primary_runtime();
    let home = frame_of(&rt, float);
    let (cw, ch) = rt.live_cell_size();
    let left = (home.frame.x.max(0) as u32 / cw.max(1)) as u16;
    let top = (home.frame.y.max(0) as u32 / ch.max(1)) as u16;
    let grab = cell_center(&rt, left, top);
    rt.handle_cursor_moved(grab);
    alt(&mut rt, PressState::Pressed);
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
    rt.handle_cursor_moved(cell_pixel(
        &rt,
        left.saturating_sub(2),
        top.saturating_sub(1),
    ));
    rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
    alt(&mut rt, PressState::Released);
    let resized = frame_of(&rt, float);
    assert_ne!(
        (resized.frame.width, resized.frame.height),
        (home.frame.width, home.frame.height)
    );

    // Restore drops the extent with the rest of the anchored geometry: the
    // panel comes back as a plain tiled split, and a later detach starts at the
    // default anchored share again.
    rt.float_restore(float).expect("restore must apply");
    rt.float_detach(float).expect("re-detach must apply");
    let again = frame_of(&rt, float);
    assert_eq!(
        (again.frame.width, again.frame.height),
        (home.frame.width, home.frame.height),
        "a fresh detach must not inherit a past resize"
    );
}

#[test]
fn mod_shift_number_moves_the_floating_panel_and_switches_workspace() {
    // DEC-0034: `Mod+Shift+<n>` moves the focused pane to workspace N and
    // follows focus, which is how the workspace switch reads to the user.
    // A focused floating panel used to fail this closed ("focused pane not in
    // layout"), so neither the panel nor the workspace moved.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.workspace_new().expect("second workspace");
    rt.workspace_switch(0);
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let float = ViewId::new(1);
    assert!(rt.set_focus(float), "the panel is a focus target");

    let (moved, from, to) = rt
        .workspace_move_focused_to_one_based(2)
        .expect("Mod+Shift+2 must move the panel");
    assert_eq!((moved, from, to), (float, 1, 2));
    assert_eq!(
        rt.active_workspace_index(),
        1,
        "the gesture follows focus to the target workspace"
    );
    assert_eq!(
        rt.focused_view(),
        Some(float),
        "the panel keeps keyboard focus after the move"
    );
    assert_eq!(
        rt.floated_views(),
        vec![float],
        "the panel travels into the target workspace's float store"
    );
    let frame = frame_of(&rt, float);
    assert_eq!(
        frame.tier,
        Some(OverlayTier::Float),
        "the moved panel still paints as a float"
    );

    // A detached float is workspace-local: switching back hides it.
    rt.workspace_switch(0);
    assert!(
        rt.present_frames().iter().all(|frame| frame.view != float),
        "the panel stays with the workspace it was moved into"
    );
    rt.workspace_switch(1);
    assert_eq!(rt.floated_views(), vec![float]);
}

#[test]
fn pinned_panel_keeps_the_pin_policy_across_the_same_gesture() {
    // A pin is window-global: there is nothing to reparent, so the gesture
    // switches the workspace and the panel stays composited and focused.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.workspace_new().expect("second workspace");
    rt.workspace_switch(0);
    rt.pin_floating(ViewId::new(2)).expect("pin must apply");
    let pinned = ViewId::new(2);
    assert!(rt.set_focus(pinned), "the pin is a focus target");

    let (moved, _, _) = rt
        .workspace_move_focused_to_one_based(2)
        .expect("the gesture must not refuse a pinned panel");
    assert_eq!(moved, pinned);
    assert_eq!(
        rt.active_workspace_index(),
        1,
        "the pin gesture still switches the workspace"
    );
    assert_eq!(
        rt.pinned_views(),
        vec![pinned],
        "the pin stays window-global"
    );
    assert_eq!(
        rt.focused_view(),
        Some(pinned),
        "focus stays on the pinned panel"
    );
    assert!(
        rt.present_frames().iter().any(|frame| frame.view == pinned),
        "a pinned panel follows the pin policy: visible in every workspace"
    );
}

#[test]
fn plain_workspace_switch_is_not_blocked_by_a_focused_float() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.workspace_new().expect("second workspace");
    rt.workspace_switch(0);
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert!(rt.set_focus(ViewId::new(1)));

    assert_eq!(
        rt.workspace_focus_clamped(2),
        Some(1),
        "Mod+<n> must still switch with a floating panel focused"
    );
    assert_eq!(rt.active_workspace_index(), 1);
    assert!(
        !rt.floated_occupied(),
        "the detached panel stays in the workspace it was floated in"
    );
    assert_eq!(
        rt.workspace_focus_clamped(1),
        Some(0),
        "switching back works the same way"
    );
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);
}

#[test]
fn tiled_panes_keep_focused_pane_scroll_routing() {
    // The pointer rule is an overlay rule: tiling leaves never overlap, so a
    // wheel with the pointer over a non-focused tile keeps scrolling the
    // focused pane exactly as before (no silent behaviour change).
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime");
    install(&mut rt, two_pane());
    feed_history(&mut rt, 200);
    rt.set_focus(ViewId::new(2));
    let frame = frame_of(&rt, ViewId::new(1));
    let (cw, ch) = rt.live_cell_size();
    let col = (frame.frame.width / 2) / cw.max(1);
    let row = (frame.frame.height / 2) / ch.max(1);
    let over_first = cell_center(&rt, col as u16, row as u16);
    rt.handle_cursor_moved(over_first);
    assert_eq!(
        rt.cursor_to_present_cell(over_first).map(|(view, _)| view),
        Some(ViewId::new(1))
    );
    rt.handle_wheel(ScrollDelta::Lines(0.0, 3.0));
    assert!(
        offset(&rt, ViewId::new(2)) > 0,
        "the focused pane owns the wheel over a plain tile"
    );
    assert_eq!(
        offset(&rt, ViewId::new(1)),
        0,
        "the tile under the pointer is not scrolled instead"
    );
}
