//! Issue #2788: a Mod+drag panel move must not shift pane content.
//!
//! Reported on 0.0.30: moving a panel with Mod+left-drag accumulated blank
//! rows at the top of the panel, and each further move kept adding them.
//! A move is a pure re-tile: the pane keeps its terminal truth (cells,
//! scrollback, cursor) and only its slot changes. Moving a pane into a
//! shorter slot and back must therefore be a round trip, not a one-way
//! content shift.
//!
//! The drag commits through `Runtime::set_layout`, which resizes the moved
//! pane to its new slot (`State::resize_with_report`). Height-only resizes
//! absorb unused bottom padding before scrolling content out (`#2529`), and
//! the matching grow must restore that padding below the content instead of
//! inserting blank filler above the pulled history. This test pins the
//! round trip through the real pointer gesture, with the primary pane
//! carrying the content (so no PTY is needed).

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, View, ViewId};

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

/// A true point inside `view`'s committed frame, derived from public
/// geometry only (no hard-coded padding or cell constants).
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

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Two stacked panes: `ViewId(1)` on top, `ViewId(2)` below. With `ratio`
/// 0.7 the slots differ in height, so a swap between them resizes the moved
/// pane in both directions.
fn stacked(ratio: f32) -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Vertical,
        ratio,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

/// One Mod (Alt) + left drag from `from`'s centre onto `to`'s centre, which
/// commits the center-target swap.
fn mod_drag(rt: &mut Runtime, from: ViewId, to: ViewId) {
    rt.handle_cursor_moved(frame_point(rt, from, 0.5, 0.5));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active(), "Mod+press must grab {from:?}");
    rt.handle_cursor_moved(frame_point(rt, to, 0.5, 0.5));
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    assert!(!rt.tiled_drag_active(), "release must end the drag");
}

/// Visible primary rows as trimmed text, newest last.
fn rows(rt: &Runtime) -> Vec<String> {
    let snap = rt.snapshot();
    (0..snap.height)
        .map(|r| {
            let mut line = String::new();
            for c in 0..snap.width {
                line.push(snap.cells[r * snap.width + c].glyph);
            }
            line.trim_end().to_string()
        })
        .collect()
}

#[test]
fn repeated_mod_drag_move_keeps_content_origin_rows_and_scrollback() {
    let mut rt = Runtime::with_defaults().expect("defaults must build");
    rt.set_layout(stacked(0.7));
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(1)),
        "the top pane owns the primary state"
    );
    // Content that fills part of the top slot and parks the cursor inside
    // it: the shrink to the bottom slot must absorb bottom padding *and*
    // scroll content out, which is the asymmetric case.
    for i in 1..=10 {
        rt.handle_pty_bytes(format!("L{i:02}\r\n").as_bytes());
    }

    let baseline_rows = rows(&rt);
    assert_eq!(
        baseline_rows.first().map(String::as_str),
        Some("L01"),
        "fixture content starts at the top row: {baseline_rows:?}"
    );
    let baseline_scrollback = rt.state().scrollback_len();
    let baseline_frame = frame_of(&rt, ViewId::new(1));
    let baseline_cursor = rt.state().cursor().position.row;

    for cycle in 0..3 {
        // Move the primary pane onto the shorter bottom slot, then move it
        // back to its original slot. Both commits resize the pane.
        mod_drag(&mut rt, ViewId::new(1), ViewId::new(2));
        mod_drag(&mut rt, ViewId::new(1), ViewId::new(2));

        let frame = frame_of(&rt, ViewId::new(1));
        assert_eq!(
            frame.content, baseline_frame.content,
            "cycle {cycle}: the move round trip must restore the content origin"
        );
        assert_eq!(
            frame.rows, baseline_frame.rows,
            "cycle {cycle}: the move round trip must restore the pane rows"
        );
        assert_eq!(
            rt.state().scrollback_len(),
            baseline_scrollback,
            "cycle {cycle}: the move round trip must restore the scrollback viewport"
        );
        assert_eq!(
            rt.state().cursor().position.row,
            baseline_cursor,
            "cycle {cycle}: the cursor must ride its content"
        );
        assert_eq!(
            rows(&rt),
            baseline_rows,
            "cycle {cycle}: a move must never insert blank rows at the top"
        );
    }
}
