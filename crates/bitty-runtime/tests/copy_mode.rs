//! Keyboard-driven copy mode (CTX-0384, issue #640).
//!
//! Vi-style modal copy mode with visual selection plus yank, built on the
//! CTX-0385 multi-click seams (`SelectionKind`, word/line/block expansion,
//! block text dispatch). All tests are headless and deterministic.

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey,
    PressState,
};
use bitty_runtime::Runtime;
use bitty_ui::SelectionKind;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed_text(rt: &mut Runtime, text: &str) {
    rt.handle_pty_bytes(text.as_bytes());
}

fn char_key(ch: &str) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: Some(ch.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn named_key(named: NamedKey) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

#[test]
fn enter_sets_active_with_status_and_cursor() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello");
    assert!(!rt.is_copy_mode());
    assert!(rt.copy_mode_label().is_none());
    rt.enter_copy_mode();
    assert!(rt.is_copy_mode());
    assert!(rt.copy_mode_cursor().is_some());
    assert_eq!(rt.copy_mode_label(), Some("COPY"));
}

#[test]
fn char_movement_hjkl_clamped_no_pty() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    rt.enter_copy_mode();
    rt.drain_pending_input();
    let start = rt.copy_mode_cursor().expect("cursor");
    rt.handle_key_event(char_key("l"));
    let right = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(right.col, start.col.saturating_add(1));
    assert_eq!(rt.pending_input_len(), 0);
    rt.handle_key_event(char_key("h"));
    assert_eq!(rt.copy_mode_cursor(), Some(start));
    assert_eq!(rt.pending_input_len(), 0);
    // Clamp at origin: h at col 0 stays.
    for _ in 0..100 {
        rt.handle_key_event(char_key("h"));
    }
    let clamped = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(clamped.col, 0);
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn arrows_page_g_movement_no_pty() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "line1\r\nline2\r\nline3");
    rt.enter_copy_mode();
    rt.drain_pending_input();
    rt.handle_key_event(named_key(NamedKey::ArrowDown));
    assert_eq!(rt.pending_input_len(), 0);
    rt.handle_key_event(named_key(NamedKey::ArrowUp));
    assert_eq!(rt.pending_input_len(), 0);
    rt.handle_key_event(char_key("G"));
    let bottom = rt.copy_mode_cursor().expect("cursor");
    assert!(bottom.row >= 2);
    rt.handle_key_event(char_key("g"));
    let top = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(top.row, 0);
    rt.handle_key_event(named_key(NamedKey::PageDown));
    assert_eq!(rt.pending_input_len(), 0);
    rt.handle_key_event(named_key(NamedKey::PageUp));
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn word_movement_w_b_e() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "foo bar baz");
    rt.enter_copy_mode();
    // Jump to line start then word-forward across foo.
    rt.handle_key_event(char_key("0"));
    rt.handle_key_event(char_key("w"));
    let after_w = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(after_w.col, 4, "w from 0 must land on bar");
    rt.handle_key_event(char_key("e"));
    let after_e = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(after_e.col, 6, "e must land on bar end");
    rt.handle_key_event(char_key("b"));
    let after_b = rt.copy_mode_cursor().expect("cursor");
    assert_eq!(after_b.col, 4, "b must return to bar start");
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn line_ends_zero_dollar() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello");
    rt.enter_copy_mode();
    rt.handle_key_event(char_key("$"));
    let end = rt.copy_mode_cursor().expect("cursor");
    assert!(end.col >= 4);
    rt.handle_key_event(char_key("0"));
    assert_eq!(rt.copy_mode_cursor().expect("cursor").col, 0);
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn visual_v_expands_simple_selection_and_kind() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    rt.enter_copy_mode();
    rt.handle_key_event(char_key("0"));
    rt.handle_key_event(char_key("v"));
    assert_eq!(rt.copy_mode_visual_kind(), Some(SelectionKind::Simple));
    rt.handle_key_event(char_key("l"));
    rt.handle_key_event(char_key("l"));
    rt.handle_key_event(char_key("l"));
    rt.handle_key_event(char_key("l"));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Simple));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
}

#[test]
fn visual_line_expands_line_selection() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "line1\r\nline2");
    rt.enter_copy_mode();
    rt.handle_key_event(char_key("g"));
    rt.handle_key_event(char_key("V"));
    assert_eq!(rt.copy_mode_visual_kind(), Some(SelectionKind::Line));
    rt.handle_key_event(char_key("j"));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Line));
    let text = rt.selection_text().expect("line visual text");
    assert!(
        text.contains("line1") && text.contains("line2"),
        "text: {text}"
    );
}

#[test]
fn yank_y_copies_to_clipboard_and_primary_and_exits() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    rt.enter_copy_mode();
    rt.handle_key_event(char_key("0"));
    rt.handle_key_event(char_key("v"));
    for _ in 0..4 {
        rt.handle_key_event(char_key("l"));
    }
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    rt.drain_pending_input();
    rt.handle_key_event(char_key("y"));
    assert!(!rt.is_copy_mode(), "yank must exit copy mode");
    assert_eq!(rt.clipboard().headless_contents(), "hello");
    assert_eq!(rt.primary_contents(), "hello");
    assert_eq!(rt.pending_input_len(), 0, "yank must not reach PTY");
}

#[test]
fn esc_exits_without_pty() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello");
    rt.enter_copy_mode();
    assert!(rt.is_copy_mode());
    rt.drain_pending_input();
    rt.handle_key_event(named_key(NamedKey::Escape));
    assert!(!rt.is_copy_mode());
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn typing_while_active_never_reaches_pty() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello");
    rt.enter_copy_mode();
    rt.drain_pending_input();
    // Plain typing keys are copy-mode motions, never PTY bytes.
    for key in ["x", "z", "q", "y"] {
        // y with no visual selection is a no-op yank (stays active, no bytes).
        rt.handle_key_event(char_key(key));
        assert_eq!(
            rt.pending_input_len(),
            0,
            "key {key} must not reach PTY in copy mode"
        );
    }
    // Still active after no-op yank with no selection.
    assert!(rt.is_copy_mode());
    rt.handle_key_event(named_key(NamedKey::Escape));
}

#[test]
fn chrome_action_enter_copy_mode_parses() {
    let action = bitty_config::ChromeAction::parse("enter_copy_mode").expect("must parse");
    assert_eq!(action, bitty_config::ChromeAction::EnterCopyMode);
    assert_eq!(action.canonical(), "enter_copy_mode");
}

#[test]
fn fail_closed_yank_without_selection_no_crash() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello");
    rt.enter_copy_mode();
    // No visual: y is a no-op that stays active and touches no clipboard.
    rt.handle_key_event(char_key("y"));
    assert!(rt.is_copy_mode());
    assert_eq!(rt.clipboard().headless_contents(), "");
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn default_binding_enters_copy_mode() {
    use bitty_config::{KeyName, KeyRef, default_keymaps, match_keymap};
    let maps = default_keymaps().expect("defaults valid");
    let space = KeyRef {
        key: KeyName::Space,
        ctrl: true,
        alt: false,
        shift: true,
        super_held: false,
    };
    assert_eq!(
        match_keymap(&maps, space),
        Some(bitty_config::ChromeAction::EnterCopyMode),
        "ctrl+shift+space must enter copy mode"
    );
}

#[test]
fn ctrl_v_toggles_block_visual() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "ab\r\ncd");
    rt.enter_copy_mode();
    rt.handle_key_event(char_key("g"));
    // Hold Control, press v for block visual.
    rt.handle_key_event(named_key(NamedKey::Control));
    rt.handle_key_event(char_key("v"));
    assert_eq!(rt.copy_mode_visual_kind(), Some(SelectionKind::Block));
    // Release Control, extend the rectangle.
    let mut release = named_key(NamedKey::Control);
    release.state = PressState::Released;
    rt.handle_key_event(release);
    rt.handle_key_event(char_key("l"));
    rt.handle_key_event(char_key("j"));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Block));
    let text = rt.selection_text().expect("block visual text");
    assert!(text.contains('a'), "block text: {text}");
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn mouse_selection_suppressed_while_active() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    rt.enter_copy_mode();
    rt.drain_pending_input();
    // Physical cell for grid (0,0) with the readable 9x19 metrics plus the
    // default 8px padding and 14px decoration insets.
    let pos = CursorPosition {
        x: 8.0 + 14.0,
        y: 8.0 + 14.0,
    };
    rt.handle_cursor_moved(pos);
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Released));
    assert!(
        rt.is_copy_mode(),
        "mouse must not exit copy mode while modal"
    );
    assert!(rt.selection_text().is_none());
    assert_eq!(rt.pending_input_len(), 0);
}

// M1-15 scrolled-history selection (CTX-0665): the copy cursor travels
// into scrollback with the viewport and yank reads history text.

fn feed_history(rt: &mut Runtime, rows: usize) {
    for i in 0..rows {
        feed_text(rt, &format!("cmd{i:02} output\r\n"));
    }
    feed_text(rt, "live tail");
}

fn view_offset(rt: &Runtime) -> usize {
    let vid = rt.focused_view().expect("focused view");
    rt.layout().find_leaf(vid).expect("leaf").scroll_offset()
}

fn cursor_is_visible(rt: &Runtime) -> bool {
    let cur = rt.copy_mode_cursor().expect("cursor");
    let vid = rt.focused_view().expect("focused view");
    let view = rt.layout().find_leaf(vid).expect("leaf");
    cur.row < view.rows() && cur.col < view.cols()
}

#[test]
fn page_up_enters_history_with_cursor_visible_no_pty() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    feed_history(&mut rt, h + 8);
    assert!(rt.state().scrollback_len() > 0);
    rt.enter_copy_mode();
    assert!(rt.is_copy_mode());
    rt.drain_pending_input();
    assert_eq!(view_offset(&rt), 0);
    rt.handle_key_event(named_key(NamedKey::PageUp));
    assert!(rt.is_copy_mode(), "paging keeps copy mode");
    assert!(view_offset(&rt) > 0, "page up must scroll into history");
    assert!(cursor_is_visible(&rt), "cursor must stay in view");
    assert_eq!(rt.pending_input_len(), 0);
    // Paging back down returns to live with the cursor visible.
    rt.handle_key_event(named_key(NamedKey::PageDown));
    assert_eq!(view_offset(&rt), 0, "page down returns to live");
    assert!(cursor_is_visible(&rt));
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn history_line_visual_yank_copies_history_text() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    feed_history(&mut rt, h + 4);
    rt.enter_copy_mode();
    rt.drain_pending_input();
    rt.handle_key_event(named_key(NamedKey::PageUp));
    assert!(view_offset(&rt) > 0);
    // Word motion works on history content (no panic, stays visible).
    rt.handle_key_event(char_key("0"));
    rt.handle_key_event(char_key("w"));
    assert!(cursor_is_visible(&rt));
    assert_eq!(rt.pending_input_len(), 0);
    // Line visual over two history rows, then yank.
    rt.handle_key_event(char_key("V"));
    assert_eq!(rt.copy_mode_visual_kind(), Some(SelectionKind::Line));
    // No stale live-grid highlight while scrolled.
    assert!(rt.selection().is_none());
    rt.handle_key_event(char_key("j"));
    rt.handle_key_event(char_key("y"));
    assert!(!rt.is_copy_mode(), "yank must exit copy mode");
    let clip = rt.clipboard().headless_contents();
    assert!(
        clip.contains("cmd") && clip.contains('\n'),
        "yanked history lines: {clip:?}"
    );
    assert_eq!(rt.primary_contents(), clip);
    assert_eq!(rt.pending_input_len(), 0, "yank must not reach PTY");
}

#[test]
fn history_simple_visual_yank_copies_word() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    feed_history(&mut rt, h + 4);
    rt.enter_copy_mode();
    rt.drain_pending_input();
    rt.handle_key_event(named_key(NamedKey::PageUp));
    assert!(view_offset(&rt) > 0);
    rt.handle_key_event(char_key("0"));
    rt.handle_key_event(char_key("v"));
    rt.handle_key_event(char_key("w"));
    rt.handle_key_event(char_key("y"));
    assert!(!rt.is_copy_mode(), "yank must exit copy mode");
    let clip = rt.clipboard().headless_contents();
    assert!(
        !clip.is_empty() && clip.contains("cmd"),
        "yanked history word: {clip:?}"
    );
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn history_yank_without_visual_stays_active() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    feed_history(&mut rt, h + 4);
    rt.enter_copy_mode();
    rt.drain_pending_input();
    rt.handle_key_event(named_key(NamedKey::PageUp));
    assert!(view_offset(&rt) > 0);
    // No visual: y is a no-op that stays active (live-path parity).
    rt.handle_key_event(char_key("y"));
    assert!(rt.is_copy_mode());
    assert_eq!(rt.clipboard().headless_contents(), "");
    assert_eq!(rt.pending_input_len(), 0);
}
