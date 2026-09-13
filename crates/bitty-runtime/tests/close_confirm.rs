//! CTX-0370 close confirmation: window-close gate + overlay paint.
//!
//! Headless, bounded, deterministic; `cargo test` on CI without X11/Wayland;
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use bitty_platform::{
    KeyEvent, KeyLocation, LogicalKey, NamedKey, PlatformEvent, PressState, WindowEventKind,
    WindowId,
};
use bitty_runtime::{CloseConfirmMode, Runtime, RuntimeConfig};

fn runtime_with(mode: CloseConfirmMode) -> Runtime {
    let cfg = RuntimeConfig {
        close_confirm: mode,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn close_request() -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CloseRequested,
    }
}

fn esc_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

#[test]
fn window_close_proceeds_immediately_under_defaults() {
    // Default `when_busy` with no running job: the window close request
    // exits on the first gesture (pre-0370 behavior).
    let mut rt = runtime_with(CloseConfirmMode::WhenBusy);
    assert!(rt.handle_platform_event(close_request()));
    assert!(!rt.has_pending_close_confirm());
}

#[test]
fn window_close_arms_then_repeat_exits_under_always() {
    let mut rt = runtime_with(CloseConfirmMode::Always);
    // First request arms and keeps the loop alive.
    assert!(!rt.handle_platform_event(close_request()));
    assert!(rt.has_pending_close_confirm());
    let banner = rt.close_confirm_banner_text().expect("armed banner");
    assert!(
        banner.chars().count() <= bitty_runtime::CLOSE_CONFIRM_BANNER_MAX_CHARS,
        "bounded: {banner}"
    );
    assert!(banner.contains("close again to confirm"), "{banner}");
    assert!(banner.contains("Esc cancels"), "{banner}");
    // The repeated OS close request is the confirm gesture.
    assert!(rt.handle_platform_event(close_request()));
    assert!(!rt.has_pending_close_confirm());
}

#[test]
fn window_close_esc_cancels_the_arm_and_keeps_running() {
    let mut rt = runtime_with(CloseConfirmMode::Always);
    assert!(!rt.handle_platform_event(close_request()));
    assert!(rt.has_pending_close_confirm());
    // Esc cancels without exiting; the key never reaches the PTY.
    assert!(rt.handle_key_event(esc_press()).is_none());
    assert!(!rt.has_pending_close_confirm());
    assert!(rt.close_confirm_banner_text().is_none());
    // After cancel the next close request arms again (never silently exits).
    assert!(!rt.handle_platform_event(close_request()));
    assert!(rt.has_pending_close_confirm());
}

#[test]
fn pending_close_banner_paints_overlay_delta_and_clears() {
    // Reviewer wiring proof (same shape as the CTX-0186 paste banner test):
    // while a close arm holds, tick() paints a presentation-only pill
    // (fills+glyphs delta) without mutating grid cells; cancelling removes
    // it and the frame idles again.
    let mut rt = runtime_with(CloseConfirmMode::Always);
    let base = rt.tick().expect("initial frame must present");
    assert!(rt.tick().is_none(), "clean grid must idle after present");
    let cells_before = rt.snapshot().cells.clone();

    assert!(!rt.handle_platform_event(close_request()));
    assert!(rt.has_pending_close_confirm());
    let pending = rt.tick().expect("armed close must force a present");
    assert!(
        pending.fills > base.fills,
        "banner must add fills: base={} pending={}",
        base.fills,
        pending.fills
    );
    assert!(
        pending.glyphs > base.glyphs,
        "banner must add glyphs: base={} pending={}",
        base.glyphs,
        pending.glyphs
    );
    assert_eq!(
        rt.snapshot().cells,
        cells_before,
        "banner must not leak into grid cells"
    );

    // Esc-cancel clears the banner: one repaint back at baseline, then idle.
    assert!(rt.handle_key_event(esc_press()).is_none());
    let cleared = rt.tick().expect("cancel must repaint without banner");
    assert_eq!(cleared.fills, base.fills, "banner fill must clear");
    assert_eq!(cleared.glyphs, base.glyphs, "banner glyphs must clear");
    assert_eq!(rt.snapshot().cells, cells_before);
    assert!(rt.tick().is_none(), "must idle once banner clears");
}
