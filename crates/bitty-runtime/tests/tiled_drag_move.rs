//! Issue #1694 (CTX-0966): Mod+Left-drag moves tiled panels (Hyprland-like).
//!
//! Headless pins: Mod(Alt/Super)+press on a tiled leaf grabs it (no
//! selection starts, focus follows the dragged pane), motion tracks the
//! advisory preview target live without mutating the tree, release
//! re-parents with position-based sizing (nearest-edge docking, ratio from
//! the drop position), and the release never desyncs into a selection
//! commit. Shift still forces selection; floating leaves stay on the
//! Alt+drag path; single-leaf trees fail soft to block selection.
//!
//! Issue #1811 (CTX-1070): the drag shows continuous feedback (ghost
//! outline on the dragged frame plus drop-target highlight under the
//! cursor, both via the Core-owned chrome-ring drag transition), corner
//! drops resolve through the accepted #1804 balanced policy at `0.5`, a
//! committed drop settles via the move transition, and `Esc` cancels
//! mid-drag restoring the pre-drag layout byte-identically.
//!
//! Issue #1981 (CTX-1167): motion holds the drag arms so ring progress
//! accumulates toward full visibility; repeat motion never restarts the
//! transition (which had pinned the ghost near-invisible).
use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{
    AnimationKind, AnimationPolicy, DropSpec, LayoutNode, ReducedMotionMode, Runtime,
    RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

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

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding, 9x19 cells, zero gaps). Matches the
/// `cursor_to_leaf_cell` / `cursor_to_layout_point` origin (no decoration
/// offset) so drop positions map to exact cells for position-based sizing.
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + f64::from(row) * 19.0 + 9.0,
    }
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

/// True painted position at fractional offsets inside the painted hit frame,
/// derived from public geometry only (no hard-coded pixel, padding, or
/// decoration constants): the headed-faithful probe shape (issue #1979).
fn frame_point(rt: &Runtime, view: ViewId, fx: f64, fy: f64) -> CursorPosition {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"));
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.frame.x) + f64::from(frame.frame.width) * fx,
        y: pad + f64::from(frame.frame.y) + f64::from(frame.frame.height) * fy,
    }
}

#[test]
fn mod_press_grabs_tiled_without_selection_and_focuses_dragged() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Press on the left pane with Mod held.
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.tiled_drag_active(),
        "Mod+press on tiled must grab for a tiled move"
    );
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    assert!(
        !rt.is_selection_dragging(),
        "grabbing press must not start selection"
    );
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Pressing the other pane grabs that pane instead (focus follows).
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(2)));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn super_press_grabs_tiled_for_hyprland_parity() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.tiled_drag_active(),
        "Super+press must also grab (Hyprland Mod muscle memory)"
    );
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    assert!(!rt.is_selection_dragging());
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

#[test]
fn motion_tracks_preview_live_without_mutating() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    assert_eq!(rt.tiled_drag_preview(), None, "no hover recorded yet");
    // Hover the right pane: preview tracks it, tree untouched.
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    assert_eq!(*rt.layout(), before, "preview must never mutate the tree");
    // Hover back over the source: preview follows.
    rt.handle_cursor_moved(cell_pixels(10, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(1)));
    assert_eq!(*rt.layout(), before);
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn release_commits_position_based_reparent_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Drop near the right edge of the right pane (col 70 in an 80-col
    // container; right pane spans 40..80): docks after with ratio 0.75,
    // so the dragged pane lands small (25%).
    rt.handle_cursor_moved(cell_pixels(70, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active(), "release must end the drag");
    assert!(
        !rt.is_selection_dragging() && !rt.has_selection(),
        "release must not desync into a selection commit (no selection was started)"
    );
    // Source re-parented beside the target; focus stayed on the dragged pane.
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let ids = rt.layout().leaf_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&ViewId::new(1)) && ids.contains(&ViewId::new(2)));
    let ratio = rt.layout().split_ratio_at(&[]).expect("root split");
    assert!(
        (ratio - 0.75).abs() < 1e-6,
        "position-based sizing: drop x 70 -> ratio 0.75, got {ratio}"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn self_drop_is_noop_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Release back over the source leaf: no mutation, no selection.
    rt.handle_cursor_moved(cell_pixels(12, 12));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert_eq!(
        *rt.layout(),
        before,
        "self-drop must leave the tree untouched"
    );
    assert!(!rt.has_selection(), "self-drop release must not select");
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn shift_mod_press_forces_selection_not_drag() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello world");
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(0, 0));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "Shift must suppress the tiled grab (CTX-0181 escape)"
    );
    assert!(rt.is_selection_dragging(), "Shift+Mod still selects");
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn single_leaf_mod_press_falls_through_to_block_selection() {
    // A move would always be a self-drop no-op, so the press preserves
    // Alt+block selection instead of grabbing.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello world");
    rt.handle_cursor_moved(cell_pixels(0, 0));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "single-leaf Mod+press must not grab"
    );
    rt.handle_cursor_moved(cell_pixels(4, 0));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(rt.has_selection(), "single-leaf Mod+drag still selects");
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn cursor_left_cancels_without_committing() {
    use bitty_platform::{ModifiersState, PlatformEvent, WindowEventKind, WindowId};
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    });
    assert!(!rt.tiled_drag_active(), "leave must cancel the drag");
    assert_eq!(*rt.layout(), before, "cancel must not mutate the tree");
    assert!(!rt.has_selection());
    // A stale release after the cancel must not commit or select either.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), before);
    assert!(!rt.has_selection());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    let _ = ModifiersState {
        shift: false,
        control: false,
        alt: false,
        super_pressed: false,
    };
}

// -- Issue #1811 (CTX-1070) live preview, balanced corners, Esc cancel --

#[test]
fn drag_arms_live_preview_and_drop_settles() {
    use std::time::{Duration, Instant};
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    let source = ViewId::new(1);
    let target = ViewId::new(2);
    // Grab on the virtual clock so the motion sequence below is fully
    // deterministic (the press event path would arm on the wall clock).
    let t0 = Instant::now();
    assert!(rt.begin_tiled_drag_at(t0));
    assert!(rt.tiled_drag_active());
    assert_eq!(
        rt.animation_progress(AnimationKind::Drag, Some(source), t0),
        Some(0.0),
        "grab must arm the ghost outline at the gesture time"
    );
    // First motion onto the target arms the drop-target highlight at the
    // gesture time; the source ghost is already climbing (held, not
    // restarted).
    let t1 = t0 + Duration::from_millis(20);
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t1);
    assert_eq!(rt.tiled_drag_preview(), Some(target));
    assert_eq!(
        rt.animation_progress(AnimationKind::Drag, Some(target), t1),
        Some(0.0),
        "first hover must arm the drop-target highlight at the gesture time"
    );
    let p1 = rt
        .animation_progress(AnimationKind::Drag, Some(source), t1)
        .expect("ghost outline must stay live during motion");
    assert!(
        (0.0..1.0).contains(&p1),
        "ghost must be visibly climbing at 20 ms, got {p1}"
    );
    // Repeat motion on the same target accumulates instead of restarting:
    // both arms keep climbing from their first arm time. A restart policy
    // would read back near 0.0 here and hold the ghost near-invisible.
    let t2 = t1 + Duration::from_millis(40);
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t2);
    assert_eq!(rt.tiled_drag_preview(), Some(target));
    let p2 = rt
        .animation_progress(AnimationKind::Drag, Some(source), t2)
        .expect("ghost outline must stay live across repeat motion");
    assert!(
        p2 > p1,
        "repeat motion must accumulate the ghost ({p1} -> {p2}), never restart it"
    );
    assert!(
        p2 > 0.3,
        "ghost must be clearly visible 60 ms into the gesture, got {p2}"
    );
    let q2 = rt
        .animation_progress(AnimationKind::Drag, Some(target), t2)
        .expect("highlight must stay live across repeat motion");
    assert!(
        (0.0..1.0).contains(&q2),
        "highlight must be visibly climbing, got {q2}"
    );
    // The live plan tracks the hovered edge: the center tie docks left at
    // half, matching the position-based placement a release would commit.
    assert_eq!(
        rt.tiled_drag_drop_spec(),
        Some(DropSpec::new(target, SplitAxis::Horizontal, 0.5, false))
    );
    // Mid-transition the preview is still live.
    let mid = t0 + Duration::from_millis(75);
    assert!(
        rt.animation_progress(AnimationKind::Drag, Some(source), mid)
            .is_some(),
        "ghost outline must stay live mid-drag"
    );
    // Release commits the edge insert and settles via the move transition;
    // the preview clears with the gesture.
    rt.handle_cursor_moved_at(cell_pixels(70, 12), t0 + Duration::from_millis(90));
    let t4 = t0 + Duration::from_millis(100);
    assert!(rt.end_tiled_drag_at(t4));
    assert_eq!(
        rt.animation_progress(AnimationKind::Move, Some(source), t4),
        Some(0.0),
        "committed drop must settle, not snap"
    );
    assert!(!rt.tiled_drag_active());
    assert_eq!(rt.tiled_drag_preview(), None);
    assert_eq!(rt.tiled_drag_drop_spec(), None);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn motion_sequence_accumulates_ghost_monotonically() {
    use std::time::{Duration, Instant};
    // CTX-1167: a five-step motion burst on the virtual clock must show a
    // monotonically climbing ghost — every step advances the same arm, so
    // the ring is visibly present through the whole gesture instead of
    // pinned near 0 by per-motion restarts.
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let source = ViewId::new(1);
    let target = ViewId::new(2);
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    let t0 = Instant::now();
    assert!(rt.begin_tiled_drag_at(t0));
    let mut prev = 0.0f32;
    let mut step = 0u64;
    while step < 5 {
        step += 1;
        let at = t0 + Duration::from_millis(10 * step);
        rt.handle_cursor_moved_at(cell_pixels(60, 12), at);
        assert_eq!(rt.tiled_drag_preview(), Some(target));
        let p = rt
            .animation_progress(AnimationKind::Drag, Some(source), at)
            .expect("ghost must stay live through the burst");
        assert!(
            p > prev,
            "motion step {step} must advance the ghost ({prev} -> {p}), never restart it"
        );
        prev = p;
    }
    assert!(
        prev > 0.3,
        "ghost must be clearly visible 50 ms into the burst, got {prev}"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn tiled_drag_instant_policies_arm_nothing() {
    use std::time::{Duration, Instant};
    // CTX-1167: disabled, reduced-motion-always, safe-mode, and 0 ms
    // policies arm no tiled-drag transition, while the gesture itself still
    // tracks the preview and still commits on release.
    for policy in [
        AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        },
        AnimationPolicy {
            duration_ms: [0; AnimationKind::COUNT],
            ..AnimationPolicy::default()
        },
    ] {
        let mut rt = Runtime::new(RuntimeConfig {
            animations: policy,
            ..RuntimeConfig::default()
        })
        .expect("instant runtime must build");
        let source = ViewId::new(1);
        let target = ViewId::new(2);
        rt.set_layout(two_pane());
        rt.handle_cursor_moved(cell_pixels(10, 12));
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
        let t0 = Instant::now();
        assert!(rt.begin_tiled_drag_at(t0));
        assert!(
            rt.animation_progress(AnimationKind::Drag, Some(source), t0)
                .is_none(),
            "suppressed grab must not arm: {policy:?}"
        );
        // Motion still tracks the preview; it just never animates.
        let t1 = t0 + Duration::from_millis(20);
        rt.handle_cursor_moved_at(cell_pixels(60, 12), t1);
        assert_eq!(rt.tiled_drag_preview(), Some(target));
        let t2 = t1 + Duration::from_millis(40);
        rt.handle_cursor_moved_at(cell_pixels(60, 12), t2);
        assert!(
            rt.animation_progress(AnimationKind::Drag, Some(source), t2)
                .is_none(),
            "suppressed motion must not arm the ghost: {policy:?}"
        );
        assert!(
            rt.animation_progress(AnimationKind::Drag, Some(target), t2)
                .is_none(),
            "suppressed motion must not arm the highlight: {policy:?}"
        );
        assert!(!rt.animations_active());
        assert_eq!(rt.animation_deadline(), None);
        // Release still ends the gesture; the drop settles instantly.
        rt.handle_cursor_moved_at(cell_pixels(70, 12), t2 + Duration::from_millis(10));
        let t4 = t2 + Duration::from_millis(20);
        assert!(rt.end_tiled_drag_at(t4));
        assert!(
            rt.animation_progress(AnimationKind::Move, Some(source), t4)
                .is_none(),
            "suppressed drop must settle instantly: {policy:?}"
        );
        assert!(!rt.tiled_drag_active());
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    }
}

#[test]
fn corner_drop_inserts_balanced_cell() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Top-right corner of the right pane (col 76, row 2): the physically
    // tall target stacks the dragged pane above it at a balanced half
    // instead of a position-sized sliver.
    rt.handle_cursor_moved(cell_pixels(76, 2));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    let spec = rt
        .tiled_drag_drop_spec()
        .expect("corner hover must plan a drop");
    assert_eq!(spec.target, ViewId::new(2));
    assert_eq!(spec.axis, SplitAxis::Vertical);
    assert!(
        (spec.ratio - 0.5).abs() < 1e-6,
        "corner drops split balanced, got {}",
        spec.ratio
    );
    assert!(!spec.after, "top corner docks before");
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert!(!rt.has_selection());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(rt.tiled_drag_preview(), None);
    assert_eq!(rt.tiled_drag_drop_spec(), None);
    // Balanced vertical insert: the dragged pane sits above the target at
    // half height, both leaves retained exactly once.
    assert_eq!(rt.layout().split_axis_at(&[]), Some(SplitAxis::Vertical));
    assert!(
        (rt.layout().split_ratio_at(&[]).expect("root split") - 0.5).abs() < 1e-6,
        "corner drop must land balanced"
    );
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn esc_cancels_mid_drag_byte_identical_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Hover the right pane: preview and drop plan appear mid-drag, tree
    // untouched.
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    assert!(rt.tiled_drag_drop_spec().is_some());
    assert_eq!(*rt.layout(), before);
    // Esc cancels the gesture and is consumed (never reaches the shell).
    let delivered = rt.handle_key_event(named_key(NamedKey::Escape, PressState::Pressed));
    assert_eq!(delivered, None, "Esc cancel must not reach the PTY");
    assert!(
        rt.pending_input().is_empty(),
        "consumed Esc must leave no input bytes"
    );
    assert!(!rt.tiled_drag_active(), "Esc must end the drag");
    assert_eq!(rt.tiled_drag_preview(), None);
    assert_eq!(rt.tiled_drag_drop_spec(), None);
    assert_eq!(
        *rt.layout(),
        before,
        "cancel must restore the pre-drag layout byte-identically"
    );
    assert!(!rt.has_selection(), "cancel must not select");
    // A stale release after the cancel commits nothing either.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), before);
    assert!(!rt.has_selection());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn esc_without_drag_still_reaches_shell() {
    // Non-regression guard for the CTX-0475 scope: with no drag active the
    // same Esc still encodes for the shell.
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    assert_eq!(
        rt.handle_key_event(named_key(NamedKey::Escape, PressState::Pressed)),
        Some(vec![27])
    );
    assert!(!rt.tiled_drag_active());
}

#[test]
fn cursor_left_cancel_clears_preview_and_repaints() {
    use bitty_platform::{PlatformEvent, WindowEventKind, WindowId};
    use std::time::Instant;
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    assert!(rt.tiled_drag_drop_spec().is_some());
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    });
    assert!(!rt.tiled_drag_active(), "leave must cancel the drag");
    assert_eq!(rt.tiled_drag_preview(), None);
    assert_eq!(rt.tiled_drag_drop_spec(), None);
    assert_eq!(*rt.layout(), before, "cancel must not mutate the tree");
    assert!(
        rt.tick_at(Instant::now()).is_some(),
        "cancel must repaint so the ghost outline and highlight clear"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

// -- Issue #1979 (CTX-1149) headed-breakage regressions --------------------

#[test]
fn gaps_out_grab_hits_painted_leaf_not_shifted_neighbour() {
    // The grab hit-test subtracted the outer gap a second time, so with
    // nonzero gaps_out a Mod+press on the first gaps_out cells of a pane
    // grabbed the neighbour while motion preview and release aimed at the
    // painted leaf. Grab, preview, and drop must agree on the painted leaf
    // at every position, probed through painted geometry (no hard-coded
    // pixels).
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 0,
        gaps_out: 2,
        ..RuntimeConfig::default()
    })
    .expect("gapped runtime must build");
    rt.set_layout(two_pane());
    for fx in [0.02, 0.05, 0.1, 0.25, 0.5, 0.9] {
        let pos = frame_point(&rt, ViewId::new(2), fx, 0.5);
        rt.handle_cursor_moved(pos);
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
        rt.handle_mouse_input(press(MouseButton::Left));
        assert_eq!(
            rt.tiled_drag_source(),
            Some(ViewId::new(2)),
            "painted position fx {fx} must grab the painted leaf"
        );
        rt.handle_cursor_moved(pos);
        assert_eq!(
            rt.tiled_drag_preview(),
            Some(ViewId::new(2)),
            "preview must agree with the grab at fx {fx}"
        );
        rt.handle_mouse_input(release(MouseButton::Left));
        assert!(!rt.tiled_drag_active());
        assert!(!rt.has_selection());
        rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    }
}

#[test]
fn mod_press_under_capture_starts_tiled_drag() {
    // With a mouse-tracking app focused, the capture path reported the
    // Mod+Left press to the app and no tiled drag ever started headed. The
    // grab probe now runs before the report: a press that grabs is
    // consumed, the app sees nothing of the gesture, and the release
    // commits the drop instead of stranding it.
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_pty_bytes(b"primary text");
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();
    assert!(rt.mouse_capture_active(), "tracking must be active");
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active(), "Mod+press must grab under capture");
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    assert!(
        rt.pending_input().is_empty(),
        "grabbing press must not report to the app"
    );
    rt.handle_cursor_moved(cell_pixels(70, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert!(
        rt.pending_input().is_empty(),
        "release must commit, not report (the app never saw the press)"
    );
    assert!(!rt.has_selection());
    let ratio = rt.layout().split_ratio_at(&[]).expect("root split");
    assert!(
        (ratio - 0.75).abs() < 1e-6,
        "drop must commit position-based sizing, got {ratio}"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn super_press_over_pinned_cover_grabs_no_hidden_tile() {
    // Store-based floats are invisible to the allocation hit-test, so a
    // Super+press over a pinned cover grabbed the tiled base beneath it
    // (the Alt path was already safe: its present-based grab consumes the
    // press first). The cover owns the press: no tiled grab starts and the
    // release touches nothing.
    let mut rt = make_runtime();
    rt.set_layout(three_pane());
    rt.pin_floating(ViewId::new(3)).expect("pin must apply");
    let before = rt.layout().clone();
    let pos = frame_point(&rt, ViewId::new(3), 0.5, 0.5);
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "press over a pinned cover must not grab the tile beneath"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), before, "release must touch nothing");
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

#[test]
fn press_over_detached_float_grabs_no_hidden_tile() {
    // Detach-model twin of the pin cover test: a workspace-detached float
    // (CTX-1147) is composited over the tiling but absent from it. Alt+press
    // stays on the float path (Alt+drag owns it) and Super+press must not
    // grab the tile beneath either.
    let mut rt = make_runtime();
    rt.set_layout(three_pane());
    rt.float_detach(ViewId::new(3)).expect("detach must apply");
    let before = rt.layout().clone();
    let pos = frame_point(&rt, ViewId::new(3), 0.5, 0.5);
    // Alt+press: the float path owns the gesture, never the tiled drag.
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "Alt+press on a float is not a tiled grab"
    );
    assert!(
        rt.alt_drag_active(),
        "Alt+press on a float stays on the float path"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    // Super+press: no float grab exists for Super, and the hidden tile is
    // covered — nothing grabs.
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "Super+press over a detached cover must not grab the tile beneath"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), before, "release must touch nothing");
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

// -- Issue #1710 (CTX-0995) tiled-drag follow-ups -------------------------

#[test]
fn tracking_release_ends_drag_instead_of_stranding() {
    // Capture-strand: the focused app enables mouse tracking mid-drag.
    // The terminating left release must end the tiled drag (committing
    // the drop) instead of being captured and stranding the gesture.
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Mid-drag the app enables tracking (1000): capture becomes active.
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();
    assert!(
        rt.mouse_capture_active(),
        "tracking must be active mid-drag"
    );
    // Motion still tracks the preview (drag owns motion before capture).
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    // Release ends the drag and commits — it must not strand, must not
    // report to the app (which never saw the press), and must not select.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "tracking release must end the drag, not strand it"
    );
    assert!(
        rt.pending_input().is_empty(),
        "stranded release must not report to the app"
    );
    assert!(!rt.has_selection());
    // A later ordinary release must not commit the old source again.
    let after = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), after);
    assert!(!rt.tiled_drag_active());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn outer_gaps_drop_selects_correct_target() {
    // Outer-gaps shift: with nonzero gaps_out the drop point must stay in
    // container coordinates (the hit test applies the outer inset itself).
    // Grab well inside the left pane, then drop on the first cols of the
    // right pane: the shifted mapping would self-drop (None) or hit left.
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 0,
        gaps_out: 2,
        ..RuntimeConfig::default()
    })
    .expect("gapped runtime must build");
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    // Container 80x24 with outer 2: left (2,2,38,20), right (40,2,38,20).
    // Col 41 is the second col of the right pane.
    rt.handle_cursor_moved(cell_pixels(41, 12));
    assert_eq!(
        rt.tiled_drag_preview(),
        Some(ViewId::new(2)),
        "outer-gaps drop must hit the right pane, not shift to left"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert!(!rt.has_selection());
    let ids = rt.layout().leaf_ids();
    assert!(ids.contains(&ViewId::new(1)) && ids.contains(&ViewId::new(2)));
    assert_ne!(
        rt.layout().split_ratio_at(&[]),
        Some(0.5),
        "drop beside the target must re-parent with position sizing"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn overlay_covered_drop_is_noop_without_tier_flip() {
    // Overlay-tier drop: a floating overlay covering the base gap is never
    // a tiled drop target. Hovering it clears the preview; release leaves
    // the tree untouched instead of re-parenting into the overlay subtree.
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 0,
        ..RuntimeConfig::default()
    })
    .expect("gapped runtime must build");
    let overlay = LayoutNode::overlay(
        two_pane(),
        LayoutNode::leaf(View::new(ViewId::new(3), 10, 5)),
        UiRect::new(38, 0, 4, 24),
    );
    rt.set_layout(overlay);
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    // Gap band (39..41) under the overlay: no tiled target.
    rt.handle_cursor_moved(cell_pixels(40, 12));
    assert_eq!(
        rt.tiled_drag_preview(),
        None,
        "overlay hover must clear the preview, not target the float"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert_eq!(
        *rt.layout(),
        before,
        "overlay drop must leave the tree untouched (no tier-flip)"
    );
    assert!(!rt.has_selection());
    // The overlay leaf is still floating; the source stayed tiled.
    let tiers: std::collections::HashMap<ViewId, Option<bitty_runtime::OverlayTier>> =
        rt.layout().leaf_overlay_tiers().into_iter().collect();
    assert_eq!(tiers.get(&ViewId::new(1)), Some(&None));
    assert!(tiers.get(&ViewId::new(3)).is_some_and(|t| t.is_some()));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}
