//! Selection and clipboard integration test (headless, no display).
//!
//! Proves the CTX-0059 path without a window server or PTY spawn:
//!
//! - `Selection` drag via `Runtime::start_selection` / `update` / `end`
//!   with wide-char snapping (bitty-ui)
//! - `Runtime::cursor_to_cell` physical→cell mapping via `CellMetrics`
//! - `Runtime::handle_platform_event` mouse flow (`MouseInput` + `CursorMoved`)
//! - Copy/paste via `Clipboard` (arboard with headless fallback)
//! - OSC 52 write bridging (`handle_pty_bytes` → clipboard)
//! - Headless determinism and boundedness (8192 byte cap)
//!
//! All assertions run on CI without X11/Wayland; `Clipboard::new_headless`
//! is forced where determinism matters so the test never depends on a
//! live display server.

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PhysicalSize, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::Runtime;
use bitty_ui::{CellPos, Selection};

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed_text(rt: &mut Runtime, text: &str) {
    // Feed raw bytes through parser → terminal state so snapshot reflects text.
    rt.handle_pty_bytes(text.as_bytes());
}

#[test]
fn selection_drag_extracts_text_headlessly() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    // Snapshot grid is 80x24; "hello world" at row 0 cols 0..10.
    rt.start_selection(CellPos::new(0, 0));
    rt.update_selection(CellPos::new(0, 4));
    rt.end_selection(CellPos::new(0, 4));
    assert!(rt.has_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    // Single-cell drag should clear (no selection).
    rt.start_selection(CellPos::new(1, 1));
    rt.end_selection(CellPos::new(1, 1));
    assert!(!rt.has_selection());
    assert!(rt.selection_text().is_none());
}

#[test]
fn selection_wide_char_never_splits_pair() {
    let mut rt = make_runtime();
    // Write a wide character: '中' occupies two cells (lead + spacer).
    feed_text(&mut rt, "A\u{4e2d}B");
    // Layout: 'A' col0, '中' lead col1 spacer col2, 'B' col3.
    // Drag from spacer (col2) should snap to leading col1.
    rt.start_selection(CellPos::new(0, 2));
    rt.update_selection(CellPos::new(0, 3));
    rt.end_selection(CellPos::new(0, 3));
    let text = rt.selection_text().expect("wide selection");
    assert_eq!(text, "\u{4e2d}B", "spacer snap must not split wide pair");
    // Full row selection includes wide char as single glyph.
    rt.clear_selection();
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 3));
    assert_eq!(rt.selection_text().as_deref(), Some("A\u{4e2d}B"));
}

#[test]
fn selection_across_rows_includes_newline() {
    let mut rt = make_runtime();
    // Fill two rows: row0 "abc", row1 "def" via linefeed.
    rt.handle_pty_bytes(b"abc\r\ndef");
    rt.start_selection(CellPos::new(0, 1));
    rt.end_selection(CellPos::new(1, 1));
    let text = rt.selection_text().expect("multi-row");
    // CTX-0168 (#270): rows running to the grid edge trim blank padding,
    // so the copied text is exactly the newline-joined lines.
    assert_eq!(text, "bc\nde");
}

#[test]
fn copy_selection_to_clipboard_headless() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "copy me");
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 3));
    let copied = rt
        .copy_selection_to_clipboard()
        .expect("copy must succeed")
        .expect("text");
    assert_eq!(copied, "copy");
    // Headless buffer must reflect copy.
    rt.clipboard_mut().get_text().expect("get must succeed");
    assert_eq!(rt.clipboard().headless_contents(), "copy");
    // CTX-0160 platform contract: the standard write best-effort syncs the
    // primary selection, so the copy is middle-pasteable without a second
    // write. A successful copy also leaves no recorded clipboard failure.
    assert_eq!(rt.clipboard().primary_contents(), "copy");
    assert!(rt.last_clipboard_error().is_none());
    // Paste should inject into pending_input (clean text: no confirmation gate).
    rt.clear_selection();
    // Clear pending before paste.
    rt.drain_pending_input();
    let insp = rt
        .paste_from_clipboard()
        .expect("paste must succeed")
        .expect("text");
    assert!(!insp, "clean paste must not need confirmation");
    assert!(
        !rt.has_pending_paste(),
        "clean paste must not leave pending"
    );
    assert_eq!(rt.pending_input(), b"copy");
    assert_eq!(rt.drain_pending_input(), b"copy");
    // No selection → copy returns None without touching clipboard.
    assert_eq!(rt.copy_selection_to_clipboard().expect("no sel"), None);
    assert_eq!(rt.clipboard().headless_contents(), "copy");
}

#[test]
fn clipboard_is_bounded_and_truncates() {
    let mut rt = make_runtime();
    let long = "x".repeat(9000);
    // Direct clipboard write truncates to CLIPBOARD_MAX_BYTES (8192) at char boundary.
    rt.clipboard_mut()
        .set_text(long.clone())
        .expect("set must succeed headless");
    assert_eq!(rt.clipboard().headless_contents().len(), 8192);
    assert!(rt.clipboard().headless_contents().chars().all(|c| c == 'x'));
    // Copy of a long selection also bounded via clipboard primitive.
    // Fill snapshot with long text? Instead test paste bounded.
    let long_paste = "y".repeat(9000);
    rt.clipboard_mut()
        .set_text(long_paste)
        .expect("set long paste");
    rt.drain_pending_input();
    let insp = rt.paste_from_clipboard().expect("paste").expect("insp");
    // Long clean paste (all 'y') is delivered immediately, no pending.
    assert!(!insp);
    assert!(!rt.has_pending_paste());
    // Length is bounded to CLIPBOARD_MAX_BYTES via clipboard primitive.
    assert_eq!(rt.pending_input().len(), 8192);
    assert_eq!(rt.clipboard().headless_contents().len(), 8192);
}

#[test]
fn select_all_covers_whole_grid() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "abc");
    rt.select_all();
    assert!(rt.has_selection());
    let text = rt.selection_text().expect("select_all text");
    // First row should contain "abc" at start, rest blanks (but trimmed? selection text includes blanks as spaces).
    assert!(text.starts_with("abc"));
    // Clearing and resize should clamp selection.
    rt.clear_selection();
    assert!(!rt.has_selection());
    rt.select_all();
    assert!(rt.has_selection());
    // Resize to smaller grid clamps selection.
    rt.handle_resize(PhysicalSize::new(8 * 4, 16 * 2))
        .expect("resize small");
    assert!(rt.has_selection());
    // New selection should be within new bounds (4 cols x 2 rows).
    let sel = rt.selection().expect("selection after resize");
    assert!(sel.anchor.col < 4 && sel.focus.col < 4);
}

#[test]
fn cursor_to_cell_mapping_is_headless_and_clamped() {
    let rt = make_runtime();
    // Default cell 8x16.
    let pos = CursorPosition { x: 16.0, y: 32.0 };
    let cell = rt.cursor_to_cell(pos);
    assert_eq!(cell, CellPos::new(2, 2));
    // Negative and far-outside clamp.
    let neg = CursorPosition {
        x: -100.0,
        y: -10.0,
    };
    assert_eq!(rt.cursor_to_cell(neg), CellPos::new(0, 0));
    let far = CursorPosition {
        x: 10000.0,
        y: 10000.0,
    };
    let far_cell = rt.cursor_to_cell(far);
    let snap = rt.snapshot();
    assert_eq!(far_cell.row as usize, snap.height - 1);
    assert_eq!(far_cell.col as usize, snap.width - 1);
}

#[test]
fn mouse_event_flow_drives_selection_via_platform_event() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "drag via winit");
    // Simulate winit event flow: cursor moved, then mouse down, drag, up.
    let start = CursorPosition { x: 0.0, y: 0.0 }; // col0 row0
    let end = CursorPosition {
        x: 8.0 * 4.0,
        y: 0.0,
    }; // col4 row0
    // Move to start before press (last_cursor needed)
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorMoved(start),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Pressed,
        }),
    });
    // Drag
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorMoved(end),
    });
    assert!(rt.is_selection_dragging());
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    assert!(!rt.is_selection_dragging());
    assert_eq!(rt.selection_text().as_deref(), Some("drag "));
    // Copy via API after mouse selection.
    let copied = rt.copy_selection_lossy().expect("copy");
    assert_eq!(copied, "drag ");
    assert_eq!(rt.clipboard().headless_contents(), "drag ");
}

// CTX-0168 (#270) multi-line mouse selection: drag across rows through the
// real platform-event path (CursorMoved + MouseInput), with per-row columns,
// CJK width handling, auto-copy into both clipboards, and paste back through
// the repeat-confirm gate. No display server needed; headless clipboard seam.

/// Drives a left-drag across rows via platform events (press, motion, release).
fn mouse_drag(rt: &mut Runtime, start: CursorPosition, waypoints: &[CursorPosition]) {
    let window_id = WindowId::from_raw_public(1);
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(start),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Pressed,
        }),
    });
    for pos in waypoints {
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(*pos),
        });
    }
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
}

/// Physical position for a grid cell with the default 8x16 cell metrics.
fn cell_pos(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: f64::from(col) * 8.0,
        y: f64::from(row) * 16.0,
    }
}

#[test]
fn multiline_mouse_drag_selects_row_range_with_newlines() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "line1\r\nline2\r\nline3");
    // Drag from row 0 col 0 down to row 2 col 4.
    mouse_drag(&mut rt, cell_pos(0, 0), &[cell_pos(2, 1), cell_pos(4, 2)]);
    assert!(!rt.is_selection_dragging());
    let range = rt.selection().expect("selection").normalized();
    assert_eq!(range.start, CellPos::new(0, 0));
    assert_eq!(range.end, CellPos::new(2, 4));
    assert_eq!(range.row_span(), 3);
    // Newline-joined lines with no blank padding (exact clipboard bytes).
    assert_eq!(rt.selection_text().as_deref(), Some("line1\nline2\nline3"));
    // Ghostty copy-on-select: release auto-copies to both selections.
    assert_eq!(rt.clipboard().headless_contents(), "line1\nline2\nline3");
    assert_eq!(rt.primary_contents(), "line1\nline2\nline3");
    // Highlight must cover the range: the next tick presents with fills.
    let stats = rt.tick().expect("selection must force a present");
    assert!(stats.fills > 0);
}

#[test]
fn multiline_mouse_drag_respects_partial_columns() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "abcdef\r\nghijkl");
    // Drag from row 0 col 2 to row 1 col 3: first row runs to the edge
    // (padding trimmed), last row ends exactly at col 3.
    mouse_drag(&mut rt, cell_pos(2, 0), &[cell_pos(3, 1)]);
    let range = rt.selection().expect("selection").normalized();
    assert_eq!(range.start, CellPos::new(0, 2));
    assert_eq!(range.end, CellPos::new(1, 3));
    assert_eq!(rt.selection_text().as_deref(), Some("cdef\nghij"));
    assert_eq!(rt.clipboard().headless_contents(), "cdef\nghij");
}

#[test]
fn multiline_mouse_drag_upward_normalizes() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "abcdef\r\nghijkl");
    // Press on the lower row, drag upward: same range as downward.
    mouse_drag(&mut rt, cell_pos(3, 1), &[cell_pos(2, 0)]);
    let range = rt.selection().expect("selection").normalized();
    assert_eq!(range.start, CellPos::new(0, 2));
    assert_eq!(range.end, CellPos::new(1, 3));
    assert_eq!(rt.selection_text().as_deref(), Some("cdef\nghij"));
}

#[test]
fn multiline_mouse_drag_with_wide_chars_keeps_columns() {
    let mut rt = make_runtime();
    // Row 0: 'a' col0, '中' lead col1 + spacer col2, 'b' col3.
    // Row 1: 'c' col0, '好' lead col1 + spacer col2, 'd' col3.
    feed_text(&mut rt, "a\u{4e2d}b\r\nc\u{597d}d");
    // Full two-row drag: wide glyphs emitted once, no split pairs.
    mouse_drag(&mut rt, cell_pos(0, 0), &[cell_pos(3, 1)]);
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("a\u{4e2d}b\nc\u{597d}d")
    );
    // Drag starting on a spacer snaps to its leader, per row.
    rt.clear_selection();
    mouse_drag(&mut rt, cell_pos(2, 0), &[cell_pos(0, 1)]);
    let range = rt.selection().expect("selection").normalized();
    assert_eq!(range.start, CellPos::new(0, 1));
    assert_eq!(range.end, CellPos::new(1, 0));
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("\u{4e2d}b\nc"),
        "spacer snap must not split the wide pair"
    );
}

#[test]
fn multiline_selection_pastes_through_repeat_confirm_gate() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "line1\r\nline2\r\nline3");
    mouse_drag(&mut rt, cell_pos(0, 0), &[cell_pos(4, 2)]);
    assert_eq!(rt.clipboard().headless_contents(), "line1\nline2\nline3");
    // Right-click paste of newline text waits on the confirmation gate:
    // held pending with a visible summary, nothing delivered silently.
    rt.drain_pending_input();
    rt.handle_cursor_moved(cell_pos(0, 0));
    rt.handle_mouse_input(MouseEvent {
        button: MouseButton::Right,
        state: PressState::Pressed,
    });
    assert!(rt.has_pending_paste());
    assert!(rt.pending_input().is_empty());
    let summary = rt.pending_paste_summary().expect("summary");
    assert!(summary.contains("3 lines"), "summary: {summary}");
    // Repeat-confirm (identical right-click, unchanged clipboard) delivers
    // every line with newlines intact.
    rt.handle_mouse_input(MouseEvent {
        button: MouseButton::Right,
        state: PressState::Pressed,
    });
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"line1\nline2\nline3");
}

#[test]
fn multiline_primary_pastes_through_repeat_confirm_gate() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "alpha\r\nbeta");
    mouse_drag(&mut rt, cell_pos(0, 0), &[cell_pos(3, 1)]);
    // Auto-copy synced the primary selection (ghostty copy-on-select).
    assert_eq!(rt.primary_contents(), "alpha\nbeta");
    // Middle-click reads the primary selection through the same gate.
    rt.drain_pending_input();
    rt.handle_cursor_moved(cell_pos(0, 0));
    rt.handle_mouse_input(MouseEvent {
        button: MouseButton::Middle,
        state: PressState::Pressed,
    });
    assert!(rt.has_pending_paste(), "newline primary needs confirm");
    assert!(rt.pending_input().is_empty());
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"alpha\nbeta");
}

#[test]
fn osc52_write_is_bridged_to_clipboard() {
    let mut rt = make_runtime();
    // Default: writes are denied without explicit capability grant (P0-AC-007).
    let osc = b"\x1b]52;c;hello\x07";
    rt.handle_pty_bytes(osc);
    assert_eq!(
        rt.clipboard().headless_contents(),
        "",
        "write must be denied without grant"
    );
    // Grant write, then it forwards.
    rt.set_osc_clipboard_write_allowed(true);
    rt.handle_pty_bytes(osc);
    assert_eq!(rt.clipboard().headless_contents(), "hello");
    // Read query must be denied without consent (no clipboard change).
    let before = rt.clipboard().headless_contents().to_owned();
    let osc_read = b"\x1b]52;c;?\x07";
    rt.handle_pty_bytes(osc_read);
    assert_eq!(rt.clipboard().headless_contents(), before);
    // Even with read consent, no data leaves clipboard without explicit flow —
    // the consent flag only controls whether a reply would be synthesized,
    // not whether the headless buffer is mutated. Denied vs allowed is at least the gate:
    assert!(!rt.osc_clipboard_read_allowed());
    rt.set_osc_clipboard_read_allowed(true);
    assert!(rt.osc_clipboard_read_allowed());
}

#[test]
fn deterministic_copy_paste_across_runtimes() {
    let mut a = make_runtime();
    let mut b = make_runtime();
    for rt in [&mut a, &mut b] {
        feed_text(rt, "deterministic 42");
        rt.start_selection(CellPos::new(0, 0));
        rt.end_selection(CellPos::new(0, 3));
        rt.copy_selection_lossy().expect("copy");
    }
    assert_eq!(
        a.clipboard().headless_contents(),
        b.clipboard().headless_contents()
    );
    assert_eq!(a.selection_text(), b.selection_text());
    // Paste deterministically.
    a.drain_pending_input();
    b.drain_pending_input();
    a.paste_from_clipboard().expect("paste");
    b.paste_from_clipboard().expect("paste");
    assert_eq!(a.pending_input(), b.pending_input());
}

#[test]
fn selection_text_uses_snapshot_wide_snap() {
    let mut rt = make_runtime();
    // Directly craft snapshot via State: write "a中b"
    rt.handle_pty_bytes("a\u{4e2d}b".as_bytes());
    let sel = Selection::simple(CellPos::new(0, 1), CellPos::new(0, 1));
    rt.set_selection(sel);
    // Single cell selection on leading wide char should emit one glyph.
    assert_eq!(rt.selection_text().as_deref(), Some("\u{4e2d}"));
    // Selection starting at spacer should snap to leader.
    let sel_spacer = Selection::simple(CellPos::new(0, 2), CellPos::new(0, 2));
    rt.set_selection(sel_spacer);
    assert_eq!(rt.selection_text().as_deref(), Some("\u{4e2d}"));
}
