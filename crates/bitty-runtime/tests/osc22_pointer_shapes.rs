#![forbid(unsafe_code)]
//! `OSC 22` pointer shapes mapped to platform cursor icons (issue #1762).
//!
//! Locks the runtime half of the acceptance criteria: `OSC 22` parsing
//! (covered unit-wise in `bitty-vt`) reaches the per-pane presentation stacks
//! here, and the focused leaf's icon is exposed headlessly through
//! `cursor_icon_for_focused` / `focused_pointer_shape` for the app's
//! `WindowHandle::set_cursor_icon` dispatch.
//!
//! - Single-pane claims run everywhere (no PTY): set/reset/push/pop,
//!   `RIS` (`FullReset`) reset, unknown fail-open, query inertia.
//! - Multi-pane focus isolation needs real pane sessions and so stays
//!   Unix-gated behind `require_pty!` (mirrors `copy_search_view_bound.rs`).

use bitty_platform::CursorIcon;
use bitty_runtime::{Runtime, RuntimeConfig};
use bitty_vt::PointerShape;

fn headless() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless build")
}

/// Single-pane: a set reaches the focused icon, an empty set resets it.
#[test]
fn osc22_set_reaches_focused_icon_and_empty_resets() {
    let mut rt = headless();
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Default);
    assert_eq!(rt.focused_pointer_shape(), None);

    rt.handle_pty_bytes(b"\x1b]22;pointer\x1b\\");
    assert_eq!(rt.focused_pointer_shape(), Some(PointerShape::Pointer));
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);

    rt.handle_pty_bytes(b"\x1b]22;text\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Text);

    // Empty payload resets to the default pointer.
    rt.handle_pty_bytes(b"\x1b]22;\x07");
    assert_eq!(rt.focused_pointer_shape(), None);
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Default);
}

/// Single-pane: `RIS` (`ESC c` / `FullReset`) clears the pointer stack.
#[test]
fn osc22_ris_resets_the_focused_icon() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b]22;wait\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Wait);

    rt.handle_pty_bytes(b"\x1bc");
    assert_eq!(rt.focused_pointer_shape(), None);
    assert_eq!(
        rt.cursor_icon_for_focused(),
        CursorIcon::Default,
        "RIS must reset the pointer shape"
    );
}

/// Single-pane: push/pop follow stack semantics; queries stay inert.
#[test]
fn osc22_push_pop_and_queries() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b]22;pointer\x07");
    rt.handle_pty_bytes(b"\x1b]22;>wait,text\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Text);

    rt.handle_pty_bytes(b"\x1b]22;<\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Wait);

    rt.handle_pty_bytes(b"\x1b]22;<\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);

    // Queries synthesize no state change (deferred, no reply here).
    rt.handle_pty_bytes(b"\x1b]22;?__current__\x07");
    assert_eq!(
        rt.cursor_icon_for_focused(),
        CursorIcon::Pointer,
        "queries must not change the stack"
    );
}

/// Single-pane: unknown names fail open to the default pointer.
#[test]
fn osc22_unknown_names_fail_open_to_default() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b]22;pointer\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);

    rt.handle_pty_bytes(b"\x1b]22;no-such-cursor\x07");
    assert_eq!(
        rt.cursor_icon_for_focused(),
        CursorIcon::Default,
        "unknown names fail open to Default"
    );
}

/// Single-pane: X11 aliases reach the same icons as CSS names.
#[test]
fn osc22_x11_aliases_reach_css_icons() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b]22;hand2\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);
    rt.handle_pty_bytes(b"\x1b]22;xterm\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Text);
    rt.handle_pty_bytes(b"\x1b]22;watch\x07");
    assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Wait);
}

// ---------------------------------------------------------------------------
// Multi-pane focus isolation (Unix + real PTY sessions)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod panes {
    use super::*;
    use bitty_runtime::{LayoutNode, SplitAxis, View, ViewId};

    const PRIMARY: ViewId = ViewId::new(1);
    const PANE: ViewId = ViewId::new(2);

    fn split_runtime() -> Runtime {
        let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
            LayoutNode::leaf(View::new(PANE, 80, 24)),
        ));
        rt.force_headless_clipboard();
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == PANE)
            .expect("pane is presented");
        rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
            .expect("spawn pane shell");
        rt
    }

    #[test]
    fn focused_pane_owns_the_window_icon() {
        bitty_test_support::require_pty!();
        let mut rt = split_runtime();
        assert!(rt.set_focus(PRIMARY));
        rt.handle_pty_bytes(b"\x1b]22;pointer\x07");
        rt.handle_pane_bytes(PANE, b"\x1b]22;wait\x07");

        // Each pane keeps its own shape; the window shows the focused one.
        assert_eq!(rt.cursor_icon_for(PRIMARY), CursorIcon::Pointer);
        assert_eq!(rt.cursor_icon_for(PANE), CursorIcon::Wait);
        assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);

        assert!(rt.set_focus(PANE));
        assert_eq!(
            rt.cursor_icon_for_focused(),
            CursorIcon::Wait,
            "focus move must switch the window icon"
        );

        assert!(rt.set_focus(PRIMARY));
        assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Pointer);
    }

    #[test]
    fn pane_exit_drops_its_shape() {
        bitty_test_support::require_pty!();
        let mut rt = split_runtime();
        assert!(rt.set_focus(PANE));
        rt.handle_pane_bytes(PANE, b"\x1b]22;crosshair\x07");
        assert_eq!(rt.cursor_icon_for_focused(), CursorIcon::Crosshair);

        assert!(rt.close_pane_session(&PANE));
        assert_eq!(
            rt.cursor_icon_for(PANE),
            CursorIcon::Default,
            "closed pane reads back as default"
        );
    }

    #[test]
    fn ris_clears_only_the_emitting_pane() {
        bitty_test_support::require_pty!();
        let mut rt = split_runtime();
        rt.handle_pty_bytes(b"\x1b]22;pointer\x07");
        rt.handle_pane_bytes(PANE, b"\x1b]22;wait\x07");
        assert!(rt.set_focus(PANE));

        rt.handle_pane_bytes(PANE, b"\x1bc");
        assert_eq!(rt.cursor_icon_for(PANE), CursorIcon::Default);
        assert_eq!(
            rt.cursor_icon_for(PRIMARY),
            CursorIcon::Pointer,
            "RIS on one pane must not clear the other"
        );
    }

    #[test]
    fn respawn_clears_stale_shape() {
        bitty_test_support::require_pty!();
        let mut rt = split_runtime();
        rt.handle_pane_bytes(PANE, b"\x1b]22;wait\x07");
        assert_eq!(rt.cursor_icon_for(PANE), CursorIcon::Wait);

        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == PANE)
            .expect("pane is presented");
        rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 30"], frame.cols, frame.rows)
            .expect("respawn pane shell");
        assert_eq!(
            rt.cursor_icon_for(PANE),
            CursorIcon::Default,
            "respawned shell must not inherit the dead session's cursor"
        );
    }
}
