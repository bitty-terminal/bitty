//! CTX-0367 headless IME contract tests.
//!
//! Proves the composition path without a display server or PTY:
//!
//! 1. `Ime::Preedit` updates presentation-only preedit state, bounded to
//!    `IME_PREEDIT_MAX_CHARS` scalars, and never mutates grid truth.
//! 2. `Ime::Commit` routes UTF-8 through the same bounded input path as
//!    typed text, exactly once, including winit's documented
//!    `Preedit("")`-then-`Commit` sequence.
//! 3. Raw key presses during an active composition are consumed (no double
//!    input); the keyboard frees again after commit/cancel.
//! 4. `Ime::Disabled` clears the overlay without PTY bytes.
//! 5. Commit truncation is bounded to 256 chars / 1024 bytes at a char
//!    boundary.
//! 6. The caret rect forwarded to the platform IME tracks the DPI-scaled
//!    cursor cell and stays inside the surface.
//!
//! All headless and deterministic (no wall clock, no PTY spawn).

#![forbid(unsafe_code)]

use bitty_platform::{
    ImeEvent, KeyEvent, KeyLocation, LogicalKey, PhysicalSize, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::Runtime;

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

fn ime_event(event: ImeEvent) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Ime(event),
    }
}

#[test]
fn preedit_state_is_bounded_and_never_touches_grid_truth() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let before = rt.snapshot();

    // 300 hostile scalars must truncate to the 128-scalar preedit bound.
    let long: String = "你".repeat(300);
    rt.handle_ime_preedit(Some(long), Some(0));
    let preedit = rt.ime_preedit().expect("preedit must be set");
    assert_eq!(
        preedit.chars().count(),
        128,
        "preedit must truncate to IME_PREEDIT_MAX_CHARS at a char boundary"
    );

    assert!(rt.tick().is_some(), "preedit must force a present");
    let after = rt.snapshot();
    assert_eq!(
        before.cells, after.cells,
        "preedit must not mutate grid truth"
    );
    assert_eq!(before.width, after.width);
    assert_eq!(before.height, after.height);

    // The platform caret rect is armed for the focused cursor and in-bounds.
    let area = rt.ime_cursor_area().expect("focused caret must be armed");
    assert!(area.width >= 1 && area.height >= 1);
    assert!(area.x >= 0 && area.y >= 0);
    let extent = rt.surface_extent().expect("surface extent after build");
    assert!(area.x + area.width as i32 <= extent.width() as i32);
    assert!(area.y + area.height as i32 <= extent.height() as i32);
}

#[test]
fn empty_preedit_then_commit_inserts_exactly_once() {
    // winit's `Ime::Commit` doc guarantee: an empty `Preedit` arrives right
    // before the commit. The empty preedit clears the overlay and emits no
    // bytes; the commit is the single insertion.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("nihao".to_string()), Some(5));
    assert!(rt.ime_preedit().is_some());

    rt.handle_ime_preedit(None, None);
    assert!(rt.ime_preedit().is_none(), "empty preedit clears overlay");
    assert_eq!(
        rt.pending_input_len(),
        0,
        "empty preedit must not emit input bytes"
    );

    rt.handle_ime_commit("你好".to_string());
    assert_eq!(rt.pending_input(), "你好".as_bytes(), "commit UTF-8 bytes");
    assert_eq!(rt.drain_pending_input(), "你好".as_bytes());
    assert_eq!(
        rt.pending_input_len(),
        0,
        "commit must land exactly once (no double input)"
    );
    assert!(rt.ime_preedit().is_none(), "commit clears preedit");
}

#[test]
fn raw_keys_during_composition_do_not_double_input() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("n".to_string()), Some(1));

    // Platform quirk: the raw key for the composition reaches the key path.
    // It must be consumed by the composition guard, not inserted.
    assert!(rt.handle_key_event(char_key("n")).is_none());
    assert!(rt.handle_key_event_ref(&char_key("i")).is_none());
    assert_eq!(
        rt.pending_input_len(),
        0,
        "composition owns the keyboard: no raw Latin insertion"
    );

    // Commit delivers the composed text once; the keyboard frees again.
    rt.handle_ime_preedit(None, None);
    rt.handle_ime_commit("你".to_string());
    assert_eq!(rt.pending_input(), "你".as_bytes());
    rt.drain_pending_input();
    assert!(rt.handle_key_event(char_key("a")).is_some());
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn ime_disabled_clears_preedit_without_bytes() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("zhong".to_string()), Some(0));

    let exit = rt.handle_platform_event(ime_event(ImeEvent::Disabled));
    assert!(!exit, "IME Disabled must not request exit");
    assert!(rt.ime_preedit().is_none(), "Disabled clears the overlay");
    assert_eq!(rt.pending_input_len(), 0, "Disabled emits no bytes");

    // The full platform seam also routes preedit/commit identically.
    let exit =
        rt.handle_platform_event(ime_event(ImeEvent::Preedit("ni".to_string(), Some((0, 2)))));
    assert!(!exit);
    assert_eq!(rt.ime_preedit(), Some("ni"));
    let exit = rt.handle_platform_event(ime_event(ImeEvent::Commit("你".to_string())));
    assert!(!exit);
    assert_eq!(rt.pending_input(), "你".as_bytes());
    assert!(rt.ime_preedit().is_none());
}

#[test]
fn commit_is_bounded_256_chars_and_1024_bytes() {
    // 300 one-byte scalars: the character cap bites.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_commit("a".repeat(300));
    assert_eq!(rt.drain_pending_input().len(), 256);

    // 300 four-byte scalars: the byte cap bites first and stays valid UTF-8.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_commit("\u{20000}".repeat(300));
    let bytes = rt.drain_pending_input();
    assert!(bytes.len() <= 1024, "commit bytes bounded: {}", bytes.len());
    assert!(
        std::str::from_utf8(&bytes).is_ok(),
        "bounded commit must stay valid UTF-8"
    );
}

#[test]
fn preedit_cursor_is_char_indexed_against_hostile_byte_offsets() {
    // A cursor byte offset inside the second scalar of a 3-byte char must
    // snap down to a char boundary (never panic, never split).
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("你好a".to_string()), Some(4));
    assert_eq!(rt.ime_preedit(), Some("你好a"));
    assert!(rt.tick().is_some(), "hostile cursor must still present");
}

#[test]
fn caret_area_scales_with_dpi() {
    let extent = PhysicalSize::new(1600, 1000);

    let mut base = Runtime::with_defaults().expect("headless runtime must build");
    base.handle_resize(extent).expect("resize");
    assert!(base.tick().is_some(), "base first present");
    let base_area = base.ime_cursor_area().expect("base caret");

    let mut hidpi = Runtime::with_defaults().expect("headless runtime must build");
    hidpi.handle_resize(extent).expect("resize");
    hidpi.apply_dpi_scale(2.0, Some(extent));
    assert!(hidpi.tick().is_some(), "hidpi first present");
    let hidpi_area = hidpi.ime_cursor_area().expect("hidpi caret");

    assert!(
        hidpi_area.width > base_area.width && hidpi_area.height > base_area.height,
        "DPI adoption must scale the caret cell: base={base_area:?} hidpi={hidpi_area:?}"
    );
}
