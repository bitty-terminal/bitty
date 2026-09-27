//! Regression test for issue #1433: selection drag must stay within originating panel.
//!
//! Tests that when dragging a selection starting in one narrow panel,
//! the selection does not leak into adjacent panels.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState};
use bitty_runtime::Runtime;
use bitty_ui::{CellPos, LayoutNode, Selection, SelectionKind, SplitAxis, View, ViewId};

fn make_runtime_with_split() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();

    // Create a split layout with two side-by-side panels
    // Left panel: columns 0-19, rows 0-23 (20x24)
    // Right panel: columns 20-39, rows 0-23 (20x24)
    let left_view = View::new(ViewId(1), 20, 24);
    let right_view = View::new(ViewId(2), 20, 24);

    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5, // 50% split
        LayoutNode::leaf(left_view),
        LayoutNode::leaf(right_view),
    );

    rt.set_layout(split);
    rt.reflow_layout();

    // Feed some text to both panels
    rt.handle_pty_bytes(b"Left panel text 1234567890\r\n");
    rt.handle_pty_bytes(b"Right panel text ABCDEFGHIJ\r\n");

    rt
}

fn cell_pos(col: u16, row: u16) -> CursorPosition {
    const PAD: f64 = 8.0;
    const DECORATION: f64 = 14.0;
    CursorPosition {
        x: PAD + DECORATION + f64::from(col) * 9.0,
        y: PAD + DECORATION + f64::from(row) * 19.0,
    }
}

fn press_at(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
    rt.handle_mouse_input_at(MouseEvent::new(MouseButton::Left, PressState::Pressed), now);
}

fn drag_to(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
}

fn release_at(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
    rt.handle_mouse_input_at(
        MouseEvent::new(MouseButton::Left, PressState::Released),
        now,
    );
}

#[test]
fn selection_stays_within_originating_panel() {
    let mut rt = make_runtime_with_split();
    let now = Instant::now();

    // Start selection in left panel (col 5, row 1)
    press_at(&mut rt, 5, 1, now);

    // Drag into right panel (col 25, row 1) - should be clamped to left panel bounds
    drag_to(&mut rt, 25, 1, now + Duration::from_millis(10));

    // Check that selection is still within left panel bounds (cols 0-19)
    let selection = rt.selection();
    assert!(selection.is_some(), "Should have a selection");
    let sel = selection.unwrap();

    // The focus should be clamped to the right edge of left panel (col 19)
    // since we dragged from col 5 to col 25 (which is in right panel)
    assert_eq!(
        sel.focus.col, 19,
        "Selection should be clamped to left panel right edge"
    );
    assert!(
        sel.focus.col < 20,
        "Selection should stay in left panel (cols 0-19)"
    );

    // Release in right panel - should also be clamped
    release_at(&mut rt, 30, 1, now + Duration::from_millis(20));

    // Final selection should still be within left panel
    let final_selection = rt.selection();
    assert!(final_selection.is_some(), "Should have final selection");
    let final_sel = final_selection.unwrap();
    assert!(
        final_sel.focus.col < 20,
        "Final selection should stay in left panel"
    );
    assert!(
        final_sel.anchor.col < 20,
        "Anchor should stay in left panel"
    );
}

#[test]
fn selection_within_single_panel_works_normally() {
    let mut rt = make_runtime_with_split();
    let now = Instant::now();

    // Start and drag entirely within left panel
    press_at(&mut rt, 5, 1, now);
    drag_to(&mut rt, 15, 1, now + Duration::from_millis(10));
    release_at(&mut rt, 15, 1, now + Duration::from_millis(20));

    let selection = rt.selection();
    assert!(selection.is_some(), "Should have a selection");
    let sel = selection.unwrap();

    // Should have normal selection within left panel
    assert!(sel.anchor.col < 20);
    assert!(sel.focus.col < 20);
    assert!(sel.anchor.col >= 0 && sel.anchor.col <= 19);
    assert!(sel.focus.col >= 0 && sel.focus.col <= 19);
}

#[test]
fn selection_in_right_panel_clamped_to_right_panel() {
    let mut rt = make_runtime_with_split();
    let now = Instant::now();

    // Start selection in right panel (col 25, row 1)
    press_at(&mut rt, 25, 1, now);

    // Drag left into left panel (col 5, row 1) - should be clamped to right panel bounds
    drag_to(&mut rt, 5, 1, now + Duration::from_millis(10));

    let selection = rt.selection();
    assert!(selection.is_some(), "Should have a selection");
    let sel = selection.unwrap();

    // The focus should be clamped to the left edge of right panel (col 20)
    // since we dragged from col 25 to col 5 (which is in left panel)
    assert_eq!(
        sel.focus.col, 20,
        "Selection should be clamped to right panel left edge"
    );
    assert!(
        sel.focus.col >= 20,
        "Selection should stay in right panel (cols 20-39)"
    );
}

#[test]
fn word_selection_clamped_to_panel() {
    let mut rt = make_runtime_with_split();
    let now = Instant::now();

    // Double-click for word selection in left panel
    press_at(&mut rt, 5, 1, now);
    release_at(&mut rt, 5, 1, now + Duration::from_millis(1));
    press_at(&mut rt, 5, 1, now + Duration::from_millis(2)); // Double-click

    // Drag into right panel
    drag_to(&mut rt, 25, 1, now + Duration::from_millis(10));

    let selection = rt.selection();
    assert!(selection.is_some(), "Should have word selection");
    let sel = selection.unwrap();
    assert_eq!(sel.kind, SelectionKind::Word, "Should be word selection");

    // Should be clamped to left panel
    assert!(
        sel.focus.col < 20,
        "Word selection should stay in left panel"
    );
}

#[test]
fn clear_selection_resets_origin_tracking() {
    let mut rt = make_runtime_with_split();
    let now = Instant::now();

    // Start selection in left panel
    press_at(&mut rt, 5, 1, now);

    // Clear selection
    rt.clear_selection();

    // Start new selection in right panel
    press_at(&mut rt, 25, 1, now + Duration::from_millis(10));
    drag_to(&mut rt, 5, 1, now + Duration::from_millis(20));

    let selection = rt.selection();
    assert!(selection.is_some(), "Should have selection");
    let sel = selection.unwrap();

    // New selection should be clamped to right panel (where it started)
    assert!(
        sel.focus.col >= 20,
        "New selection should be clamped to right panel"
    );
}
