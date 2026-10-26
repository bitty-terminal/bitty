//! CTX-1609 `terminal.ambiguous_width` runtime adoption.
//!
//! Headless and deterministic: no display server is needed. These tests pin
//! the policy at the runtime boundary — the config default stays narrow and
//! a configured wide policy reaches terminal state, so a printed Ambiguous
//! scalar occupies two columns plus a spacer.

#![forbid(unsafe_code)]

use bitty_runtime::config::DEFAULT_AMBIGUOUS_WIDTH;
use bitty_runtime::{Runtime, RuntimeConfig};
use bitty_term_state::AmbiguousWidth;

#[test]
fn default_policy_is_narrow() {
    assert_eq!(DEFAULT_AMBIGUOUS_WIDTH, AmbiguousWidth::Narrow);
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.ambiguous_width, AmbiguousWidth::Narrow);
    cfg.validate().expect("default runtime config valid");
    let rt = Runtime::new(cfg).expect("headless runtime builds");
    assert_eq!(rt.state().ambiguous_width(), AmbiguousWidth::Narrow);
}

#[test]
fn construction_seeds_wide_policy_into_terminal_state() {
    let cfg = RuntimeConfig {
        ambiguous_width: AmbiguousWidth::Wide,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("headless runtime builds");
    assert_eq!(rt.state().ambiguous_width(), AmbiguousWidth::Wide);
    rt.handle_pty_bytes("\u{03B1}".as_bytes());
    let snapshot = rt.state().snapshot();
    assert_eq!(snapshot.cells[0].glyph, '\u{03B1}');
    assert_eq!(snapshot.cells[0].width, 2);
    assert!(snapshot.cells[1].spacer);
    assert_eq!(snapshot.cursor.position.col, 2);
}

#[test]
fn narrow_policy_keeps_ambiguous_scalar_narrow() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime builds");
    rt.handle_pty_bytes("\u{03B1}".as_bytes());
    let snapshot = rt.state().snapshot();
    assert_eq!(snapshot.cells[0].width, 1);
    assert!(!snapshot.cells[1].spacer);
    assert_eq!(snapshot.cursor.position.col, 1);
}
