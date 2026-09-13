//! Runtime Keyboard, mouse, wheel, clipboard, and paste tests.
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_platform::{
    CursorPosition, KeyEvent, MouseButton, PlatformEvent, PressState, WindowEventKind,
};
use bitty_runtime::{Runtime, RuntimeConfig, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn test_char_key(logical: &str, text: Option<&str>, state: PressState) -> KeyEvent {
    KeyEvent {
        logical_key: bitty_platform::LogicalKey::Character(logical.to_string()),
        text: text.map(|s| s.to_string()),
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn test_named_key(named: bitty_platform::NamedKey, state: PressState) -> KeyEvent {
    KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn mouse_headless_runtime(text: &str) -> Runtime {
    let mut rt = make_runtime();
    rt.force_headless_clipboard();
    rt.handle_pty_bytes(text.as_bytes());
    rt
}

fn mouse_headless_runtime_no_auto_copy(text: &str) -> Runtime {
    // CTX-0191: runtime with the copy-on-select toggle explicitly off.
    let mut rt = Runtime::new(RuntimeConfig {
        selection_auto_copy: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    rt.force_headless_clipboard();
    rt.handle_pty_bytes(text.as_bytes());
    rt
}

fn mouse_headless_runtime_auto_copy(text: &str) -> Runtime {
    // CTX-0191: runtime with the copy-on-select opt-in enabled.
    let mut rt = Runtime::new(RuntimeConfig {
        selection_auto_copy: true,
        ..RuntimeConfig::default()
    })
    .expect("auto-copy runtime must build");
    rt.force_headless_clipboard();
    rt.handle_pty_bytes(text.as_bytes());
    rt
}

fn mouse_press(button: bitty_platform::MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent {
        button,
        state: PressState::Pressed,
    }
}

fn mouse_release(button: bitty_platform::MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent {
        button,
        state: PressState::Released,
    }
}

#[test]
fn tracked_control_state_synthesizes_ctrl_bytes() {
    // CTX-0154: the legacy encoder must consult the tracked modifier
    // state, not winit text (None for Ctrl+letter on Wayland).
    let mut rt = make_runtime();
    // Pressing Control is modifier-only (no PTY bytes) but latches state.
    assert!(
        rt.handle_key_event(test_named_key(
            bitty_platform::NamedKey::Control,
            PressState::Pressed
        ))
        .is_none()
    );
    assert_eq!(rt.pending_input_len(), 0);
    // Wayland-style Ctrl+F (text=None) synthesizes 0x06.
    assert_eq!(
        rt.handle_key_event(test_char_key("f", None, PressState::Pressed)),
        Some(vec![0x06])
    );
    assert_eq!(rt.pending_input(), b"\x06");
    assert_eq!(rt.drain_pending_input(), b"\x06");
    // Bare-letter text under held Control synthesizes identically.
    assert_eq!(
        rt.handle_key_event(test_char_key("c", Some("c"), PressState::Pressed)),
        Some(vec![0x03])
    );
    assert_eq!(rt.drain_pending_input(), b"\x03");
    // Releasing Control unlatches: plain letters pass through again.
    assert!(
        rt.handle_key_event(test_named_key(
            bitty_platform::NamedKey::Control,
            PressState::Released
        ))
        .is_none()
    );
    assert_eq!(
        rt.handle_key_event(test_char_key("f", None, PressState::Pressed)),
        Some(b"f".to_vec())
    );
    assert_eq!(rt.drain_pending_input(), b"f");
    // Alt tracked via ModifiersChanged prefixes ESC (metaSendsEscape).
    let alt_on = PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(7),
        kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        }),
    };
    assert!(!rt.handle_platform_event(alt_on));
    assert_eq!(
        rt.handle_key_event(test_char_key("x", Some("x"), PressState::Pressed)),
        Some(vec![0x1b, b'x'])
    );
    assert_eq!(rt.drain_pending_input(), b"\x1bx");
}

#[test]
fn scroll_focused_page_moves_viewport_by_page() {
    // CTX-0178: Alt+U/I pages the focused pane by its viewport height,
    // clamped to scrollback bounds.
    let mut rt = make_runtime();
    for i in 0..200 {
        let line = format!("line {i:03}\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
    rt.tick();
    assert!(
        rt.state().scrollback_len() > 0,
        "history must exist to page through"
    );
    let rows = rt
        .layout()
        .find_leaf(ViewId::new(1))
        .expect("single leaf")
        .rows();
    assert!(rows > 1, "page must span more than one row");
    assert!(rt.scroll_focused_page(true), "page up must find the leaf");
    let offset = rt
        .layout()
        .find_leaf(ViewId::new(1))
        .expect("single leaf")
        .scroll_offset();
    assert_eq!(
        offset,
        usize::from(rows).min(rt.state().scrollback_len()),
        "one page up moves by viewport rows"
    );
    assert!(
        rt.scroll_focused_page(false),
        "page down must find the leaf"
    );
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("single leaf")
            .scroll_offset(),
        0,
        "one page down returns to live"
    );
}

#[test]
fn left_release_auto_copies_to_clipboard_and_primary() {
    let mut rt = mouse_headless_runtime_auto_copy("hello world");
    assert_eq!(rt.clipboard().headless_contents(), "");
    assert_eq!(rt.primary_contents(), "");
    // Drag cells (0,0)..(0,4) = "hello" via the mouse path (CTX-0223 +
    // CTX-0294/CTX-0333: coords include the default 8px window padding
    // inset and the decoration outer gap + border + content inset = 14px,
    // origin (22, 22)).
    rt.handle_cursor_moved(CursorPosition { x: 22.0, y: 22.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 22.0 + 9.0 * 4.0,
        y: 22.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    // Ghostty copy-on-select: both clipboards hold the selection.
    assert_eq!(rt.clipboard().headless_contents(), "hello");
    assert_eq!(rt.primary_contents(), "hello");
    // A successful copy clears any recorded clipboard failure.
    assert!(rt.last_clipboard_error().is_none());
}

#[test]
fn left_release_with_auto_copy_off_highlights_without_copying() {
    // CTX-0191: explicit opt-out leaves the highlight in place but touches
    // neither clipboard; the explicit chord path still copies.
    assert!(
        !RuntimeConfig {
            selection_auto_copy: false,
            ..RuntimeConfig::default()
        }
        .validate()
        .is_err()
    );
    let mut rt = mouse_headless_runtime_no_auto_copy("hello world");
    assert!(!rt.config().selection_auto_copy);
    // CTX-0223 + CTX-0294: mouse coords include the 8px window padding
    // inset and the 14px decoration origin shift.
    rt.handle_cursor_moved(CursorPosition { x: 22.0, y: 22.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 22.0 + 9.0 * 4.0,
        y: 22.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    // Highlight present, drag finished, clipboards untouched.
    assert!(!rt.is_selection_dragging());
    assert!(rt.has_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "");
    assert_eq!(rt.primary_contents(), "");
    // Explicit chord path (Ctrl+Shift+C arm) still copies on demand.
    let copied = rt
        .copy_selection_to_clipboard()
        .expect("explicit copy must not error");
    assert_eq!(copied.as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "hello");
}

#[test]
fn default_selection_does_not_clobber_clipboard() {
    // CTX-0371: the shipped default must not write the system clipboard (or
    // primary) when text is selected — kitty/ghostty semantics. Selecting
    // still highlights, and the explicit copy chord still works.
    let mut rt = mouse_headless_runtime("hello world");
    assert!(!rt.config().selection_auto_copy, "default is off");
    rt.clipboard_mut()
        .set_text("keep-me".to_string())
        .expect("headless set");
    rt.set_primary_text("keep-primary".to_string());
    // Drag cells (0,0)..(0,4) = "hello".
    rt.handle_cursor_moved(CursorPosition { x: 22.0, y: 22.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 22.0 + 9.0 * 4.0,
        y: 22.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    // Highlight present, both clipboards untouched.
    assert!(rt.has_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "keep-me");
    assert_eq!(rt.primary_contents(), "keep-primary");
    // Explicit chord still copies on demand.
    let copied = rt
        .copy_selection_to_clipboard()
        .expect("explicit copy must not error");
    assert_eq!(copied.as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "hello");
}

#[test]
fn left_release_auto_copy_overwrites_divergent_primary() {
    // Regression pin for the live Wayland gap (select-in-bitty never
    // reached `wl-paste --primary`): `auto_copy_selection` must replace
    // a stale/divergent primary with the new selection, not just write
    // the regular clipboard. The write itself is delivered by the
    // platform layer's wl-copy-first primary sync (CTX-0160 as fixed
    // here); the headless seam proves the contract deterministically.
    let mut rt = mouse_headless_runtime_auto_copy("hello world");
    rt.clipboard_mut()
        .set_text("zz".to_string())
        .expect("headless set");
    rt.set_primary_text("pq".to_string());
    assert_eq!(rt.clipboard().headless_contents(), "zz");
    assert_eq!(rt.primary_contents(), "pq");
    // Drag cells (0,0)..(0,4) = "hello" via the mouse path (CTX-0223 +
    // CTX-0294/CTX-0333: coords include the default 8px window padding
    // inset and the decoration outer gap + border + content inset = 14px,
    // origin (22, 22)).
    rt.handle_cursor_moved(CursorPosition { x: 22.0, y: 22.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 22.0 + 9.0 * 4.0,
        y: 22.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "hello");
    assert_eq!(rt.primary_contents(), "hello");
    assert!(rt.last_clipboard_error().is_none());
}

#[test]
fn left_release_without_drag_copies_nothing() {
    let mut rt = mouse_headless_runtime("hello world");
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert!(!rt.has_selection());
    assert_eq!(rt.clipboard().headless_contents(), "");
    assert_eq!(rt.primary_contents(), "");
}

#[test]
fn right_click_pastes_clipboard_bytes() {
    let mut rt = mouse_headless_runtime("hello world");
    rt.clipboard_mut()
        .set_text("hi".to_string())
        .expect("headless set");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Right));
    assert!(!rt.has_pending_paste(), "clean paste needs no confirm");
    assert_eq!(rt.pending_input(), b"hi");
    // A successful read leaves no recorded clipboard failure.
    assert!(rt.last_clipboard_error().is_none());
    // Release is a no-op: exactly one paste per click.
    rt.handle_mouse_input(mouse_release(MouseButton::Right));
    assert_eq!(rt.pending_input(), b"hi");
}

#[test]
fn right_click_with_empty_clipboard_pastes_nothing_without_error() {
    let mut rt = mouse_headless_runtime("hello world");
    assert!(rt.last_clipboard_error().is_none());
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Right));
    assert!(rt.pending_input().is_empty());
    assert!(!rt.has_pending_paste());
    assert!(rt.last_clipboard_error().is_none());
}

#[test]
fn middle_click_pastes_primary_bytes() {
    let mut rt = mouse_headless_runtime("hello world");
    // Standard clipboard holds something else: middle must read primary.
    // Order matters: the platform `set_text` best-effort syncs the
    // primary selection (CTX-0160 ghostty copy-on-select), so stage the
    // standard clipboard first and the primary second.
    rt.clipboard_mut()
        .set_text("zz".to_string())
        .expect("headless set");
    rt.set_primary_text("pq".to_string());
    assert_eq!(rt.clipboard().headless_contents(), "zz");
    assert_eq!(rt.primary_contents(), "pq");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Middle));
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"pq");
    assert!(rt.last_clipboard_error().is_none());
    rt.handle_mouse_input(mouse_release(MouseButton::Middle));
    assert_eq!(rt.pending_input(), b"pq");
    // Right-click still reads the standard clipboard, not primary.
    rt.drain_pending_input();
    rt.handle_mouse_input(mouse_press(MouseButton::Right));
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"zz");
}

#[test]
fn clipboard_copy_syncs_primary_like_ghostty() {
    // Platform CTX-0160 contract through the runtime seam: a standard
    // clipboard write also lands in the primary selection, so a
    // left-drag auto-copy is middle-pasteable without a second write.
    let mut rt = mouse_headless_runtime("hello world");
    rt.clipboard_mut()
        .set_text("synced".to_string())
        .expect("headless set");
    assert_eq!(rt.primary_contents(), "synced");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Middle));
    assert_eq!(rt.pending_input(), b"synced");
}

#[test]
fn middle_click_with_empty_primary_pastes_nothing() {
    // Fresh headless seam: both buffers start empty, so the primary read
    // succeeds empty (no error recorded) and pastes nothing.
    let mut rt = mouse_headless_runtime("hello world");
    assert_eq!(rt.primary_contents(), "");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Middle));
    assert!(rt.pending_input().is_empty());
    assert!(!rt.has_pending_paste());
    assert!(rt.last_clipboard_error().is_none());
}

#[test]
fn suspicious_right_paste_waits_for_confirmation() {
    let mut rt = mouse_headless_runtime("hello world");
    rt.clipboard_mut()
        .set_text("a\nb".to_string())
        .expect("headless set");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Right));
    assert!(rt.has_pending_paste(), "newline paste needs confirm");
    assert!(rt.pending_input().is_empty(), "no silent delivery");
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"a\nb");
}

#[test]
fn capture_mode_reports_sgr_and_never_pastes() {
    let mut rt = mouse_headless_runtime("hello world");
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.handle_pty_bytes(b"\x1b[?1006h");
    rt.clipboard_mut()
        .set_text("clip".to_string())
        .expect("headless set");
    rt.set_primary_text("prim".to_string());
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    // Right press in capture must report SGR, not paste.
    rt.handle_mouse_input(mouse_press(MouseButton::Right));
    assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M");
    assert!(!rt.has_pending_paste());
    assert!(!rt.has_selection(), "capture must not select");
    // Middle press likewise reports its own button code.
    rt.handle_mouse_input(mouse_press(MouseButton::Middle));
    assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M\x1b[<1;1;1M");
    assert!(!rt.has_pending_paste());
    // Left release in capture reports SGR and never auto-copies.
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M\x1b[<1;1;1M\x1b[<0;1;1m");
    assert_eq!(rt.clipboard().headless_contents(), "clip");
    assert_eq!(rt.primary_contents(), "prim");
    assert!(rt.last_clipboard_error().is_none());
}

#[test]
fn wheel_up_scrolls_into_history_and_down_returns_to_live() {
    // CTX-0155 (#251): winit LineDelta/PixelDelta y>0 = wheel up;
    // View::scroll_by positive = up into history. Wheel-up from live
    // must increase offset; wheel-down must decrease it.
    // CTX-0185: one notch now moves `scroll_lines_per_notch` (default 3),
    // not 1 — direction semantics unchanged, throughput fixed.
    let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;
    let pixels_per_notch = RuntimeConfig::default().scroll_pixels_per_notch as f64;
    let mut rt = make_runtime();
    for i in 0..60 {
        let line = format!("line {i:02}\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
    assert!(
        rt.state().scrollback_len() > 5,
        "need scrollback for wheel test, got {}",
        rt.state().scrollback_len()
    );
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    let offset = |rt: &Runtime| {
        rt.layout()
            .find_leaf(view_id)
            .map(|v| v.scroll_offset())
            .unwrap_or(usize::MAX)
    };
    assert_eq!(offset(&rt), 0, "must start at live");
    // Lines path.
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        offset(&rt),
        lines_per_notch,
        "wheel-up (Lines y>0) must go into history by one notch"
    );
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -1.0));
    assert_eq!(
        offset(&rt),
        0,
        "wheel-down (Lines y<0) must return toward live"
    );
    // Pixels path (accumulator threshold = configured pixels per notch).
    rt.handle_wheel(bitty_platform::ScrollDelta::Pixels(
        0.0,
        pixels_per_notch * 2.0,
    ));
    assert_eq!(
        offset(&rt),
        lines_per_notch * 2,
        "wheel-up (Pixels py>0) must go into history by two notches"
    );
    rt.handle_wheel(bitty_platform::ScrollDelta::Pixels(
        0.0,
        -(pixels_per_notch * 2.0),
    ));
    assert_eq!(
        offset(&rt),
        0,
        "wheel-down (Pixels py<0) must return toward live"
    );
}

#[test]
fn wheel_fractional_line_deltas_accumulate_instead_of_dropping() {
    // CTX-0185: high-resolution wheels emit fractional LineDelta notches
    // (|y| < 1.0). Truncating each event to `isize` dropped them outright
    // (read as lag); they must bank across events. Default 3 lines/notch:
    // 0.25 notch = 0.75 lines banked, second 0.25 completes 1.5 -> 1 line.
    let mut rt = make_runtime();
    for i in 0..60 {
        let line = format!("line {i:02}\n");
        rt.handle_pty_bytes(line.as_bytes());
    }
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    let offset = |rt: &Runtime| {
        rt.layout()
            .find_leaf(view_id)
            .map(|v| v.scroll_offset())
            .unwrap_or(usize::MAX)
    };
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 0.25));
    assert_eq!(offset(&rt), 0, "sub-line fraction must not scroll yet");
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 0.25));
    assert_eq!(offset(&rt), 1, "banked fractions must complete a line");
    // Opposite fractions walk back down (direction preserved).
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -0.25));
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -0.25));
    assert_eq!(offset(&rt), 0, "fractions must unwind toward live");
}

#[test]
fn wheel_scroll_speed_config_scales_per_notch_distance() {
    // CTX-0185: per-notch distance is configurable and validated.
    fn runtime_with_scroll(lines: u32, pixels: u32) -> Runtime {
        Runtime::new(RuntimeConfig {
            scroll_lines_per_notch: lines,
            scroll_pixels_per_notch: pixels,
            ..RuntimeConfig::default()
        })
        .expect("custom scroll speed must build")
    }
    fn fill(rt: &mut Runtime) {
        for i in 0..60 {
            let line = format!("line {i:02}\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
    }
    fn offset_of(rt: &Runtime) -> usize {
        let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
        rt.layout()
            .find_leaf(view_id)
            .map(|v| v.scroll_offset())
            .unwrap_or(usize::MAX)
    }
    // 1 line/notch restores the pre-CTX-0185 feel exactly.
    let mut slow = runtime_with_scroll(1, 16);
    fill(&mut slow);
    slow.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(offset_of(&slow), 1);
    // 5 lines/notch moves five times further per event.
    let mut fast = runtime_with_scroll(5, 16);
    fill(&mut fast);
    fast.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(offset_of(&fast), 5);
    // Pixels threshold is configurable: 8px/notch means 16px = 2 notches.
    let mut touchy = runtime_with_scroll(2, 8);
    fill(&mut touchy);
    touchy.handle_wheel(bitty_platform::ScrollDelta::Pixels(0.0, 16.0));
    assert_eq!(offset_of(&touchy), 4);
}

#[test]
fn wheel_sgr_capture_scales_with_scroll_speed() {
    // CTX-0185: mouse-mode SGR wheel emission scales with the configured
    // lines/notch (one SGR event per line, still capped at 32/frame) and
    // never scrolls the viewport.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.handle_pty_bytes(b"\x1b[?1006h");
    rt.drain_pending_input();
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
    let pending = String::from_utf8_lossy(rt.pending_input()).into_owned();
    let ups = pending.matches("\x1b[<64;").count();
    assert_eq!(
        ups,
        RuntimeConfig::default().scroll_lines_per_notch as usize,
        "one SGR 64 per line in the notch, got {pending:?}"
    );
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    let offset = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX);
    assert_eq!(offset, 0, "capture scroll must not move the viewport");
}
