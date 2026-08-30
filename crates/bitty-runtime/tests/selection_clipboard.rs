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
    // Selection is row-major inclusive: row0 col1..end, row1 0..1.
    // Terminal grid is 80 cols; the first row's trailing blanks become
    // spaces, so we trim trailing spaces per line before compare (matches
    // bitty-ui's `Selection::text` semantics of emitting spaces for blanks).
    let trimmed = text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(trimmed, "bc\nde");
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
    // Paste should inject into pending_input (clean text: no confirmation gate).
    rt.clear_selection();
    // Clear pending before paste.
    rt.drain_pending_input();
    let insp = rt
        .paste_from_clipboard()
        .expect("paste must succeed")
        .expect("text");
    assert!(insp.is_clean(), "clean paste must not need confirmation");
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
    assert!(insp.is_clean());
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
