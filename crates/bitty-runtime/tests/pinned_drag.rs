#![forbid(unsafe_code)]
//! CTX-1081: pinned (sticky) floats answer Alt+drag.
//!
//! A pinned leaf is detached from the layout tree, so the pre-fix
//! drag-grab probe (`leaf_is_floating` over the live tree only) failed
//! soft on it: Alt+press neither grabbed a float nor started a selection
//! path owned by the sticky leaf, while the Mod tiled-drag below could
//! grab the covered base leaf instead. The probe now reads the pinned
//! store, so a press on a presented pinned frame arms an Alt+drag for
//! that leaf.
//!
//! Drag semantic (documented here and on the touched paths): a pinned leaf
//! keeps the anchored contract — its present bounds recompute from
//! `float_frame` plus the cascade offset — and the drag accumulates a
//! Runtime-owned re-anchor offset on top. The drop keeps the offset, so
//! the drop point becomes the new anchor (clamped into the container at
//! present time); unpin drops the offset and returns the still-floating
//! panel to the active workspace with default anchored geometry.

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{
    AnimationKind, AnimationPolicy, LayoutNode, OverlayTier, PresentationMode, Runtime,
    RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

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

fn toggle(rt: &mut Runtime, id: ViewId) {
    let mut tree = rt.layout().clone();
    bitty_ui::presentation::toggle_floating(&mut tree, id).expect("toggle must apply");
    rt.set_layout(tree);
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding inset, 9x19 cells, zero cell gaps, plus
/// the unified CTX-0294/CTX-0333 default decoration outer gap + border +
/// content inset = 14px at scale 1.0). Mirrors the mouse_chrome helper so
/// drag deltas land on exact cells.
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 14.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 14.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Physical cursor position at fractional (`fx`, `fy`) offsets inside
/// `view`'s present hit-test frame, derived from public geometry only (no
/// hard-coded pixel, padding, or decoration constants).
fn frame_point(rt: &Runtime, view: ViewId, fx: f64, fy: f64) -> CursorPosition {
    let frame = frame_of(rt, view);
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.frame.x) + f64::from(frame.frame.width) * fx,
        y: pad + f64::from(frame.frame.y) + f64::from(frame.frame.height) * fy,
    }
}

/// Alt+Left-press at `pos`: moves the cursor, holds Alt, and presses.
/// The caller releases both.
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
fn alt_drag_grabs_pinned_leaf_not_the_covered_base() {
    // Discrimination: before the fix the probe failed soft on the
    // detached pinned leaf, so this press grabbed nothing float-like —
    // the tiled-drag path below could instead lift the covered base leaf.
    // Now the press arms an Alt+drag for the pinned leaf itself.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    assert!(rt.set_focus(ViewId::new(2)), "park focus on the base leaf");

    let pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    alt_press(&mut rt, pos);
    assert!(
        rt.alt_drag_active(),
        "Alt+press on a pinned float must grab it"
    );
    assert!(
        !rt.tiled_drag_active(),
        "a pinned leaf must never start a tiled move"
    );
    assert!(
        !rt.is_selection_dragging(),
        "the grabbing press must not start selection"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "grabbing focuses the dragged pin"
    );
    alt_release(&mut rt);
    assert!(!rt.alt_drag_active());
    assert!(
        !rt.has_selection(),
        "drag release must not commit a selection (desync guard)"
    );
    // The tree is untouched: the leaf is still pinned, the live layout
    // still holds only the base leaf.
    assert_eq!(rt.pinned_views(), vec![ViewId::new(1)]);
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(2)]);
}

#[test]
fn alt_drag_moves_pinned_drop_reanchors_and_unpin_returns() {
    // Drop re-anchors: motion shifts the presented pinned frame by the
    // exact cell delta, the release keeps the new anchor (sticky across a
    // workspace switch), and unpin-after-drag still returns the
    // still-floating panel to the active workspace with default anchored
    // geometry (a re-pin restarts at the cascade anchor).
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    // Pin a tiled leaf directly: pinning converges on Floating.
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    let home = frame_of(&rt, ViewId::new(1));
    assert_eq!(home.tier, Some(OverlayTier::Float));

    // Grab at the frame center, then move exactly +5 cols / +2 rows.
    let grab_pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    alt_press(&mut rt, grab_pos);
    assert!(rt.alt_drag_active());
    let grab_cell = rt.cursor_to_cell(grab_pos);
    rt.handle_cursor_moved(cell_pixels(grab_cell.col + 5, grab_cell.row + 2));
    assert!(rt.alt_drag_active(), "motion keeps a pinned drag armed");
    assert!(!rt.has_selection(), "owned motion selects nothing");

    // The presented frame follows by exactly the cell delta (9x19 headless
    // cells); size is unchanged — the drag re-anchors, never resizes.
    let moved = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        moved.frame.x - home.frame.x,
        5 * 9,
        "frame must move 5 cols"
    );
    assert_eq!(
        moved.frame.y - home.frame.y,
        2 * 19,
        "frame must move 2 rows"
    );
    assert_eq!(
        (moved.frame.width, moved.frame.height),
        (home.frame.width, home.frame.height),
        "drag re-anchors, never resizes"
    );

    // Drop: the gesture ends with no selection, and the frame stays where
    // it was dropped — the drop point is the new anchor.
    alt_release(&mut rt);
    assert!(!rt.alt_drag_active());
    assert!(!rt.has_selection());
    let dropped = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        (dropped.frame.x, dropped.frame.y),
        (moved.frame.x, moved.frame.y),
        "drop must keep the dragged anchor"
    );

    // The re-anchor is window-global like the pin: it follows a workspace
    // switch, and the live tree never gains the leaf meanwhile.
    rt.workspace_new().expect("second workspace must open");
    let away = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        (away.frame.x, away.frame.y),
        (moved.frame.x, moved.frame.y),
        "dragged anchor must follow the switch"
    );
    assert!(
        !rt.layout().leaf_ids().contains(&ViewId::new(1)),
        "pinned leaf stays out of every live tree"
    );

    // Unpin-after-drag returns the still-floating panel to the active
    // workspace; it no longer follows switches.
    let restored = rt.unpin_floating(ViewId::new(1)).expect("unpin must apply");
    assert_eq!(restored, ViewId::new(1));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating,
        "unpin returns the floating panel, it does not re-tile it"
    );
    rt.workspace_prev();
    assert!(
        rt.present_frames()
            .iter()
            .all(|frame| frame.view != ViewId::new(1)),
        "unpinned float stays in its workspace"
    );

    // Back home, a re-pin restarts at the cascade anchor: the drag offset
    // was present-only and died with the unpin.
    rt.workspace_next();
    rt.pin_floating(ViewId::new(1)).expect("re-pin must apply");
    let repinned = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        (repinned.frame.x, repinned.frame.y),
        (home.frame.x, home.frame.y),
        "re-pin must restart at the cascade anchor, not the old drag"
    );
    rt.unpin_floating(ViewId::new(1)).expect("cleanup unpin");
}

#[test]
fn pinned_drag_clamps_into_the_container() {
    // A wild pointer teleport can never push a dragged pin off-screen:
    // the composed origin clamps into the container at present time.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");

    let grab_pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    alt_press(&mut rt, grab_pos);
    assert!(rt.alt_drag_active());
    // Teleport far down-right (past the grid edge; the cell mapping
    // clamps, and the present path clamps the rest).
    rt.handle_cursor_moved(cell_pixels(79, 23));
    assert!(rt.alt_drag_active());
    let frame = frame_of(&rt, ViewId::new(1));
    assert!(frame.frame.x >= 0, "dragged pin stays on-screen left");
    assert!(frame.frame.y >= 0, "dragged pin stays on-screen top");
    assert!(
        frame.frame.x + frame.frame.width as i32 <= 80 * 9,
        "dragged pin stays on-screen right"
    );
    assert!(
        frame.frame.y + frame.frame.height as i32 <= 24 * 19,
        "dragged pin stays on-screen bottom"
    );
    alt_release(&mut rt);
    let settled = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        (settled.frame.x, settled.frame.y),
        (frame.frame.x, frame.frame.y),
        "clamped drop point is the new anchor"
    );
    rt.unpin_floating(ViewId::new(1)).expect("cleanup unpin");
}

#[test]
fn pinned_drag_back_has_no_dead_zone() {
    // CodeRabbit 1893: the STORED offset clamps at update time into what
    // present permits. In the 80x24 container the depth-0 pin anchors at
    // cell x=8 with a 64-wide float (max x=16: an 8-cell right margin), so
    // dragging 30 cells right must store only +8; dragging back one cell
    // then moves the frame one cell. Pre-fix the store kept +30 and the
    // frame sat still in a 22-cell dead zone.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    let home = frame_of(&rt, ViewId::new(1));

    let grab_pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    alt_press(&mut rt, grab_pos);
    assert!(rt.alt_drag_active());
    let grab_cell = rt.cursor_to_cell(grab_pos);
    // +30 cols stays on the 80-col grid from the frame center (~col 40).
    rt.handle_cursor_moved(cell_pixels(grab_cell.col + 30, grab_cell.row));
    assert!(rt.alt_drag_active());
    let past_edge = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        past_edge.frame.x - home.frame.x,
        8 * 9,
        "only the 8-cell permitted margin may apply"
    );
    // Drag back exactly one cell: the frame must follow by one cell, not
    // sit in the dead zone of the clamped-away excess.
    rt.handle_cursor_moved(cell_pixels(grab_cell.col + 29, grab_cell.row));
    assert!(rt.alt_drag_active());
    let back_one = frame_of(&rt, ViewId::new(1));
    assert_eq!(
        back_one.frame.x - past_edge.frame.x,
        -9,
        "dragging back one cell must move the frame one cell"
    );
    assert_eq!(
        back_one.frame.x - home.frame.x,
        7 * 9,
        "stored offset must be the clamped +7"
    );
    alt_release(&mut rt);
    assert!(!rt.has_selection());
    rt.unpin_floating(ViewId::new(1)).expect("cleanup unpin");
}

#[test]
fn pinned_drag_arms_drag_transition_on_virtual_clock() {
    // The pinned motion arms the same Drag chrome transition a structural
    // float move arms. Virtual clock only: `tick_at` advances time, so no
    // wall-clock sleep or real-time active assertion is needed.
    let mut rt = Runtime::new(RuntimeConfig {
        animations: AnimationPolicy::default(),
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("animation runtime must build");
    install(&mut rt, two_pane());
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");

    let grab_pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    alt_press(&mut rt, grab_pos);
    assert!(rt.alt_drag_active());

    let t = Instant::now();
    let grab_cell = rt.cursor_to_cell(grab_pos);
    assert!(
        rt.update_alt_drag_at(cell_pixels(grab_cell.col + 3, grab_cell.row + 1), t),
        "pinned drag motion must apply"
    );
    let leaf = ViewId::new(1);
    assert_eq!(
        rt.animation_progress(AnimationKind::Drag, Some(leaf), t),
        Some(0.0),
        "pinned drag must arm at the gesture time"
    );
    let mid = t + Duration::from_millis(75);
    let p = rt
        .animation_progress(AnimationKind::Drag, Some(leaf), mid)
        .expect("mid progress");
    assert!((0.0..1.0).contains(&p), "mid drag progress {p}");
    assert!(rt.tick_at(mid).is_some(), "active drag must present");
    let end = t + Duration::from_millis(150);
    assert!(rt.tick_at(end).is_some(), "final frame commits");
    assert!(!rt.animations_active(), "drag must complete");
    assert!(rt.tick_at(end).is_none(), "idle after drag completes");
    assert_eq!(rt.animation_deadline(), None, "no deadline when idle");
    alt_release(&mut rt);
}
