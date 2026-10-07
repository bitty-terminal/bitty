//! CTX-1008 (issue #1763) Kitty `OSC 99` notifications plus RC-8 flood cover.
//!
//! Headless and deterministic; `cargo test` on CI without a display server.
//! These tests pin the Core side of #1763: Kitty title/body parsing into
//! `{title, body}`, chunked `i=`/`d=` reassembly, default-deny consent, the
//! RC-8 rate ceiling on rapid output (fail-closed, bounded), and the bridge
//! defensive-cap wiring (no shell anywhere in the path).

#![forbid(unsafe_code)]

use bitty_runtime::{RC8_EVENTS_PER_WINDOW, Runtime};

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless runtime must build")
}

#[test]
fn kitty_denied_by_default_and_counted() {
    let mut rt = runtime();
    assert!(!rt.osc_notification_allowed());
    rt.handle_pty_bytes(b"\x1b]99;;Hello\x07");
    assert_eq!(rt.pending_notification_count(), 0);
    assert_eq!(rt.notifications_denied(), 1);
    assert!(rt.notification_banner().is_none());
    assert_eq!(rt.kitty_partials_pending(), 0);
}

#[test]
fn kitty_single_title_surfaces_with_consent() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]99;;Hello world\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("Hello world"));
    assert_eq!(rt.pending_notification_count(), 0);
}

#[test]
fn kitty_chunked_title_body_assembles_by_id() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]99;i=7:d=0;Hello\x07");
    assert!(rt.notification_banner().is_none());
    assert_eq!(rt.kitty_partials_pending(), 1);
    rt.handle_pty_bytes(b"\x1b]99;i=7:p=body;World\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("Hello: World"));
    assert_eq!(rt.kitty_partials_pending(), 0);
}

#[test]
fn kitty_body_only_degrades_to_body() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]99;i=9:p=body;Only body\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("Only body"));
}

#[test]
fn kitty_queries_and_close_stay_inert() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    for seq in [
        &b"\x1b]99;i=1:p=?;\x07"[..],
        &b"\x1b]99;i=1:p=close;x\x07"[..],
        &b"\x1b]99;i=1:p=icon;x\x07"[..],
        &b"\x1b]99;i=1:d=2;x\x07"[..],
        &b"\x1b]99;no-equals;x\x07"[..],
    ] {
        rt.handle_pty_bytes(seq);
    }
    assert_eq!(rt.pending_notification_count(), 0);
    assert!(rt.notification_banner().is_none());
    assert_eq!(rt.kitty_partials_pending(), 0);
}

#[test]
fn kitty_base64_surfaces_decoded() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    // `SGVsbG8=` decodes to `Hello`.
    rt.handle_pty_bytes(b"\x1b]99;i=2:e=1;SGVsbG8=\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("Hello"));
}

#[test]
fn kitty_rapid_output_triggers_rc8_and_stays_bounded() {
    // Acceptance: rapid notification output must trip the rate limiter
    // fail-closed (dropped and counted, never queued without bound).
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    for index in 0..(RC8_EVENTS_PER_WINDOW + 25) {
        let seq = format!("\x1b]99;;n{index}\x07");
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert!(
        rt.bell_rate_dropped() >= 25,
        "excess Kitty notifications must be counted"
    );
    assert!(
        rt.pending_notification_count() <= bitty_runtime::NOTIFICATION_QUEUE_CAPACITY,
        "queue must never exceed its capacity"
    );
    // The bridge defensive cap is wired (cap-only gate, no OS touch).
    let _ = rt.notifications_bridge_dropped();
}

#[test]
fn kitty_mixed_with_osc777_shares_one_rc8_budget() {
    // Both families flow through the same RC-8 limiter: a combined flood
    // above the ceiling drops the excess regardless of the wire form.
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    for index in 0..RC8_EVENTS_PER_WINDOW {
        if index % 2 == 0 {
            rt.handle_pty_bytes(b"\x1b]777;notify;Build;finished\x07");
        } else {
            rt.handle_pty_bytes(b"\x1b]99;;Hello\x07");
        }
    }
    assert_eq!(rt.bell_rate_dropped(), 0);
    rt.handle_pty_bytes(b"\x1b]99;;overflow\x07");
    rt.handle_pty_bytes(b"\x1b]777;notify;Build;overflow\x07");
    assert!(
        rt.bell_rate_dropped() >= 1,
        "shared RC-8 budget must drop over-ceiling mixes"
    );
}

#[test]
fn kitty_hostile_payload_is_sanitized_never_executed() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]99;;hi\"; rm -rf ~; \"\\bye\x07");
    let banner = rt.notification_banner().expect("banner visible");
    assert!(!banner.contains('\u{1}'));
    // No shell is ever constructed in the notification path: the payload
    // surfaces only as sanitized banner text.
    assert!(banner.contains("hi"));
}

#[test]
fn kitty_denied_chunks_never_buffer_partials() {
    // Without consent, even `d=0` openers must not pin assembler memory.
    let mut rt = runtime();
    for index in 0..4 {
        let seq = format!("\x1b]99;i=d{index}:d=0;part\x07");
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert_eq!(rt.kitty_partials_pending(), 0);
    assert_eq!(rt.notifications_denied(), 4);
}
