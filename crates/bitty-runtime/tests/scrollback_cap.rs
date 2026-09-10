//! CTX-0297: `terminal.scrollback` bounds retained history headlessly.
//!
//! Effect-level proof for the config knob: the effective
//! `terminal.scrollback` value is captured by terminal creation and bounds
//! the retained scrollback lines (`RuntimeConfig::scrollback` ->
//! `State::with_scrollback_lines`). Feeds a fixed byte stream and asserts
//! the retained length, so the cap (not the feed size) is the bound.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

/// Feed `lines` full rows; the first `GRID_ROWS - 1` linefeeds just move the
/// cursor and every later linefeed scrolls one line into scrollback.
fn feed_lines(rt: &mut Runtime, lines: usize) {
    for i in 0..lines {
        rt.handle_pty_bytes(format!("line {i:03}\r\n").as_bytes());
    }
}

#[test]
fn configured_scrollback_cap_bounds_retained_lines() {
    const CAP: usize = 5;
    let cfg = RuntimeConfig {
        scrollback: CAP,
        ..RuntimeConfig::default()
    };
    cfg.validate().expect("cap within bounds");
    let mut rt = Runtime::new(cfg).expect("runtime builds");
    assert_eq!(rt.config().scrollback, CAP);

    feed_lines(&mut rt, 80);

    assert_eq!(
        rt.state().scrollback_len(),
        CAP,
        "retention must stop at the configured cap"
    );
    assert!(rt.state().check_invariants().is_ok());
    // Oldest-first pruning: retained ids stay strictly monotonic and the
    // oldest id proves earlier lines were evicted.
    let ids: Vec<u64> = rt.state().scrollback().map(|line| line.id).collect();
    assert_eq!(ids.len(), CAP);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(ids[0] > 0, "oldest lines must have been pruned");
}

#[test]
fn zero_cap_disables_scrollback_retention() {
    let cfg = RuntimeConfig {
        scrollback: 0,
        ..RuntimeConfig::default()
    };
    let mut rt = Runtime::new(cfg).expect("runtime builds");
    feed_lines(&mut rt, 80);
    assert_eq!(rt.state().scrollback_len(), 0);
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn default_runtime_keeps_full_stream_below_the_default_cap() {
    // Same stream as the capped test: the default cap is not reached, so
    // every scrolled line is retained. This separates the cap effect from
    // the feed size.
    let mut rt = Runtime::with_defaults().expect("defaults");
    feed_lines(&mut rt, 80);
    assert_eq!(
        rt.state().scrollback_len(),
        80 - (bitty_term_state::GRID_ROWS - 1),
        "default cap retains every scrolled line of this stream"
    );
}
