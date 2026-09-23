//! CTX-0756 (issue #1359) cursor style / bell runtime adoption.
//!
//! Headless and deterministic; `cargo test` on CI without a display server.
//! These tests pin the `terminal.cursor_style` / `terminal.bell` chain at
//! the runtime boundary: `RuntimeConfig` defaults stay fail-closed,
//! construction seeds the terminal-state default style, and the configured
//! bell mode governs the `BEL` surface without a compositor.

#![forbid(unsafe_code)]

use bitty_runtime::config::{DEFAULT_BELL_MODE, DEFAULT_CURSOR_STYLE};
use bitty_runtime::{BellMode, Runtime, RuntimeConfig};

#[test]
fn runtime_config_cursor_and_bell_defaults_are_fail_closed() {
    // `default` (renderer block fallback) and `visual` (bounded flash):
    // a fresh config never silences the bell nor reshapes the cursor.
    assert_eq!(DEFAULT_CURSOR_STYLE, bitty_vt::CursorStyle::Default);
    assert_eq!(DEFAULT_BELL_MODE, BellMode::Visual);
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.cursor_style, bitty_vt::CursorStyle::Default);
    assert_eq!(cfg.bell_mode, BellMode::Visual);
    cfg.validate().expect("default runtime config valid");
}

#[test]
fn construction_seeds_configured_cursor_style_and_bell_mode() {
    let cfg = RuntimeConfig {
        cursor_style: bitty_vt::CursorStyle::SteadyBar,
        bell_mode: BellMode::Off,
        ..RuntimeConfig::default()
    };
    let rt = Runtime::new(cfg).expect("headless runtime must build");
    assert_eq!(rt.bell_mode(), BellMode::Off);
    assert_eq!(
        rt.state().default_cursor_style(),
        bitty_vt::CursorStyle::SteadyBar
    );
    assert_eq!(
        rt.state().snapshot().cursor.cursor_style,
        bitty_vt::CursorStyle::SteadyBar
    );
    // `BEL` under `Off` arms no flash.
    let mut rt = rt;
    rt.handle_pty_bytes(b"\x07");
    assert!(!rt.visual_bell_active());
}

#[test]
fn configured_bell_both_still_paints_the_bounded_flash() {
    let cfg = RuntimeConfig {
        bell_mode: BellMode::Both,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("headless runtime must build");
    assert_eq!(rt.bell_mode(), BellMode::Both);
    rt.handle_pty_bytes(b"\x07");
    assert!(
        rt.visual_bell_active(),
        "both mode must arm the bounded visual flash"
    );
}

#[test]
fn app_decscusr_reset_resolves_to_configured_cursor_style() {
    // End to end over the PTY path: `CSI 0 SP q` (app reset) lands back on
    // the configured shape instead of the hardcoded block fallback.
    let cfg = RuntimeConfig {
        cursor_style: bitty_vt::CursorStyle::SteadyUnderline,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("headless runtime must build");
    rt.handle_pty_bytes(b"\x1b[5 q");
    assert_eq!(
        rt.state().snapshot().cursor.cursor_style,
        bitty_vt::CursorStyle::BlinkingBar
    );
    rt.handle_pty_bytes(b"\x1b[0 q");
    assert_eq!(
        rt.state().snapshot().cursor.cursor_style,
        bitty_vt::CursorStyle::SteadyUnderline
    );
}
