#![forbid(unsafe_code)]
//! Issue #2787: returning to the window restores the internal focus owner.
//!
//! Reported on 0.0.30 under Hyprland: switching to another window or workspace
//! and coming back left the focus on a different pane than the one the user
//! left. The runtime focus model was not losing the target (the pane survives
//! `Focused(false)` -> `Focused(true)` and a bare `Occluded(true)`); the return
//! path overwrote it with pointer state:
//!
//! - a workspace switch-away unmaps the Wayland surface, so the pointer leaves
//!   it (`CursorLeft`) and re-enters on switch-back (`wl_pointer.enter`);
//! - winit pushes `CursorEntered` **and** one `CursorMoved` at the pointer's
//!   current position for that enter, even though the pointer did not move;
//! - with `mouse.focus_follows_mouse` on (the default) that synthetic move ran
//!   hover activation and focused whichever pane sat under the resting pointer.
//!
//! The contract pinned here: a focus-out/focus-in cycle and a bare occlusion
//! edge never move the focus owner (pane or panel), and the entry re-sync is
//! position-only — real motion still activates hover focus.

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, PlatformEvent, WindowEventKind, WindowId};
use bitty_runtime::{
    LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

fn win(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    }
}

/// Horizontal split: primary grid on the left, second pane on the right.
fn split_runtime(config: RuntimeConfig) -> Runtime {
    let mut rt = Runtime::new(config).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Centre of frame-local cell `(row, col)` of `view`'s content frame.
fn cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y) + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

/// The exact platform sequence a window focus cycle delivers.
fn focus_cycle(rt: &mut Runtime) {
    assert!(!rt.handle_platform_event(win(WindowEventKind::Focused(false))));
    assert!(!rt.handle_platform_event(win(WindowEventKind::Focused(true))));
}

/// Focus-out then focus-in keeps the focused pane, and typing still reaches
/// that pane's own shell writer.
#[test]
fn focus_cycle_keeps_the_focused_pane_and_its_input_route() {
    bitty_test_support::require_pty!();
    let mut rt = split_runtime(RuntimeConfig::default());
    let frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
        .expect("spawn pane shell");
    assert!(rt.set_focus(PANE));

    focus_cycle(&mut rt);

    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "focus-out/focus-in must not move the internal focus owner"
    );
    assert!(
        rt.push_input_bytes(b"x"),
        "input still routes to the pane focused before the cycle"
    );
}

/// The same cycle keeps a focused detached panel (float) focused, and a
/// pointer entry over a tiled pane does not steal focus from it.
#[test]
fn focus_cycle_keeps_the_focused_panel() {
    let mut rt = split_runtime(RuntimeConfig::default());
    rt.float_detach(PANE).expect("detach the panel");
    assert!(rt.set_focus(PANE));

    focus_cycle(&mut rt);
    assert_eq!(rt.focused_view(), Some(PANE), "focus cycle keeps the panel");

    // Switch-away/switch-back: the pointer re-enters over the tiled pane.
    let over_primary = cell_center(&rt, PRIMARY, 4, 4);
    rt.handle_platform_event(win(WindowEventKind::CursorLeft));
    rt.handle_platform_event(win(WindowEventKind::CursorEntered));
    rt.handle_platform_event(win(WindowEventKind::CursorMoved(over_primary)));
    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "the entry re-sync must not move focus off the panel"
    );
}

/// A bare occlusion edge is presentation-only: no focus, no window-focus
/// state change.
#[test]
fn occlusion_alone_does_not_touch_focus() {
    let mut rt = split_runtime(RuntimeConfig::default());
    assert!(rt.set_focus(PANE));
    let focused_before = rt.is_window_focused();

    assert!(!rt.handle_platform_event(win(WindowEventKind::Occluded(true))));
    assert_eq!(rt.focused_view(), Some(PANE), "occlusion keeps the pane");
    assert_eq!(
        rt.is_window_focused(),
        focused_before,
        "occlusion is not a focus edge"
    );

    assert!(!rt.handle_platform_event(win(WindowEventKind::Occluded(false))));
    assert_eq!(rt.focused_view(), Some(PANE), "the reveal keeps the pane");
    assert_eq!(
        rt.is_window_focused(),
        focused_before,
        "the reveal is not a focus edge"
    );
}

/// The entry re-sync winit synthesizes after `CursorEntered` updates the
/// pointer position but is not motion: it must not run hover activation.
/// Real motion afterwards still moves focus (focus-follows-mouse works).
#[test]
fn pointer_entry_re_sync_does_not_steal_focus() {
    let mut rt = split_runtime(RuntimeConfig::default());
    assert!(
        RuntimeConfig::default().focus_follows_mouse,
        "this regression is about the default focus-follows-mouse policy"
    );

    // The pointer rests over the primary pane and hover already moved focus
    // there; the user then focuses the right pane with the keyboard while the
    // mouse stays put.
    let over_primary = cell_center(&rt, PRIMARY, 4, 4);
    rt.handle_cursor_moved(over_primary);
    assert_eq!(rt.focused_view(), Some(PRIMARY));
    assert!(rt.set_focus(PANE));

    // Workspace switch-away: the surface unmaps under the pointer.
    rt.handle_platform_event(win(WindowEventKind::CursorLeft));
    // Switch-back: the surface re-maps under the stationary pointer and winit
    // pushes CursorEntered plus one CursorMoved at the same position.
    rt.handle_platform_event(win(WindowEventKind::CursorEntered));
    rt.handle_platform_event(win(WindowEventKind::CursorMoved(over_primary)));
    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "the entry re-sync must not move focus to the pane under the pointer"
    );

    // Genuine motion is still hover activation.
    let moved = cell_center(&rt, PRIMARY, 5, 5);
    rt.handle_platform_event(win(WindowEventKind::CursorMoved(moved)));
    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "real motion still activates the hovered pane"
    );

    // An entry that lands at a new position is real motion, not a re-sync.
    assert!(rt.set_focus(PANE));
    rt.handle_platform_event(win(WindowEventKind::CursorLeft));
    rt.handle_platform_event(win(WindowEventKind::CursorEntered));
    let elsewhere = cell_center(&rt, PRIMARY, 6, 6);
    assert_ne!(elsewhere, moved);
    rt.handle_platform_event(win(WindowEventKind::CursorMoved(elsewhere)));
    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "an entry move that changes position still follows the pointer"
    );
}

/// A hover dwell armed before the window lost focus never commits while the
/// window is away or when it returns; the remembered pane wins.
#[test]
fn focus_loss_drops_a_pending_hover_dwell() {
    let mut rt = split_runtime(RuntimeConfig {
        focus_follows_mouse_delay: Duration::from_millis(120),
        ..RuntimeConfig::default()
    });
    assert!(rt.set_focus(PANE));
    let start = Instant::now();

    // Motion over the primary pane arms the dwell (focus still on the pane).
    let over_primary = cell_center(&rt, PRIMARY, 4, 4);
    rt.handle_cursor_moved_at(over_primary, start);
    assert_eq!(rt.focused_view(), Some(PANE));
    assert!(
        rt.hover_activation_deadline().is_some(),
        "a dwell is armed before the focus loss"
    );

    focus_cycle(&mut rt);
    assert!(
        rt.hover_activation_deadline().is_none(),
        "the focus loss drops the pointer-armed dwell"
    );
    // Tick past the original deadline: nothing commits.
    let _ = rt.tick_at(start + Duration::from_millis(400));
    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "a stale dwell must not move focus after the window returns"
    );
}
