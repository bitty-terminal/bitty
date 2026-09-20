//! CTX-0577 (M1-16 / #1142) bell and notification policy behavior.
//!
//! Headless and deterministic; `cargo test` on CI without a display server.
//! These tests pin the accepted policy: visual-by-default `BEL`, default-deny
//! `OSC 9` / `OSC 777`, bounded display, and the RC-8 rate ceiling including
//! the negative (denied) path.

#![forbid(unsafe_code)]

use bitty_runtime::{
    BELL_FLASH_DURATION, BellMode, NOTIFICATION_BANNER_DURATION, NOTIFICATION_QUEUE_CAPACITY,
    NOTIFICATION_TEXT_MAX_CHARS, RC8_EVENTS_PER_WINDOW,
};
use bitty_runtime::{Runtime, RuntimeConfig};

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless runtime must build")
}

#[test]
fn bel_defaults_to_bounded_visual_flash() {
    let mut rt = runtime();
    assert_eq!(rt.bell_mode(), BellMode::Visual);
    rt.handle_pty_bytes(b"\x07");
    assert!(
        rt.visual_bell_active(),
        "default BEL must arm the bounded visual flash"
    );
}

#[test]
fn bell_off_and_audible_modes_do_not_paint() {
    let mut rt = runtime();
    rt.set_bell_mode(BellMode::Off);
    rt.handle_pty_bytes(b"\x07");
    assert!(!rt.visual_bell_active());

    let mut rt = runtime();
    rt.set_bell_mode(BellMode::Audible);
    rt.handle_pty_bytes(b"\x07");
    assert!(
        !rt.visual_bell_active(),
        "audible mode must not paint the visual flash"
    );
}

#[test]
fn bel_is_rate_limited_under_rc8() {
    // A hostile child can emit BEL far above the RC-8 ceiling; the visual
    // surface admits at most the ceiling per window and counts the rest.
    let mut rt = runtime();
    for _ in 0..(RC8_EVENTS_PER_WINDOW + 25) {
        rt.handle_pty_bytes(b"\x07");
    }
    assert!(rt.bell_rate_dropped() >= 25, "excess bells must be counted");
    assert!(rt.visual_bell_active());
}

#[test]
fn osc9_denied_by_default_and_counted() {
    let mut rt = runtime();
    assert!(!rt.osc_notification_allowed());
    rt.handle_pty_bytes(b"\x1b]9;build finished\x07");
    assert_eq!(rt.pending_notification_count(), 0, "denied -> not queued");
    assert_eq!(rt.notifications_denied(), 1);
    assert!(rt.notification_banner().is_none());
}

#[test]
fn osc777_denied_by_default_and_counted() {
    let mut rt = runtime();
    rt.handle_pty_bytes(b"\x1b]777;notify;Build;finished\x07");
    assert_eq!(rt.pending_notification_count(), 0);
    assert_eq!(rt.notifications_denied(), 1);
}

#[test]
fn consented_osc9_and_osc777_queue_and_present_one_bounded_banner() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    // The first admitted notification shows immediately (never silently
    // parked); the second queues behind it.
    rt.handle_pty_bytes(b"\x1b]9;first\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("first"));
    assert_eq!(rt.pending_notification_count(), 0);
    rt.handle_pty_bytes(b"\x1b]777;notify;Build;second\x07");
    assert_eq!(rt.pending_notification_count(), 1);

    // The present path shows exactly one banner at a time; advancing past
    // the first banner's window promotes the queued one.
    let _ = rt.tick();
    assert_eq!(rt.notification_banner().as_deref(), Some("first"));
    let later = std::time::Instant::now() + NOTIFICATION_BANNER_DURATION;
    assert!(rt.advance_notification_banner_at(later));
    assert_eq!(
        rt.notification_banner_at(later).as_deref(),
        Some("Build: second")
    );
    assert_eq!(rt.pending_notification_count(), 0);
}

#[test]
fn notification_queue_is_bounded_and_drops_newest() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    // The RC-8 limiter caps *admissions per window*; the queue bound is the
    // second bound. Fill past both and confirm the counters stay honest.
    let total = NOTIFICATION_QUEUE_CAPACITY + RC8_EVENTS_PER_WINDOW as usize + 8;
    for i in 0..total {
        rt.handle_pty_bytes(format!("\x1b]9;n{i}\x07").as_bytes());
    }
    assert!(
        rt.notifications_queue_dropped() > 0 || rt.bell_rate_dropped() > 0,
        "one of the two bounds must have dropped the excess"
    );
    assert!(
        rt.pending_notification_count() <= NOTIFICATION_QUEUE_CAPACITY,
        "queue must never exceed its capacity"
    );
}

#[test]
fn notification_text_is_sanitized_and_bounded() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    // Embedded control bytes and an oversized body must not reach the banner.
    let oversized = "x".repeat(NOTIFICATION_TEXT_MAX_CHARS + 128);
    let seq = format!("\x1b]9;a\u{1}b\u{7}{oversized}\x07");
    rt.handle_pty_bytes(seq.as_bytes());
    let _ = rt.tick();
    let banner = rt.notification_banner().expect("banner visible");
    assert!(banner.chars().count() <= NOTIFICATION_TEXT_MAX_CHARS + 4);
    assert!(!banner.contains('\u{1}'));
    assert!(!banner.contains('\u{7}'));
}

#[test]
fn notification_banner_is_never_silent_and_self_expires() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;hello\x07");
    let now = std::time::Instant::now();
    assert!(rt.notification_banner_at(now).is_some());
    assert!(rt.notification_banner_at(now).is_some());
    assert!(
        rt.notification_banner_at(now + NOTIFICATION_BANNER_DURATION)
            .is_none(),
        "banner must expire rather than persist"
    );
    assert!(
        !rt.visual_bell_active_at(now + BELL_FLASH_DURATION),
        "flash must expire rather than persist"
    );
}

#[test]
fn bell_flash_expires_on_a_quiet_window_without_pty_or_layout_change() {
    // Review PX-3067: the bounded flash must self-expire on time (forcing a
    // frame) rather than persisting until unrelated activity happens to tick.
    let mut rt = runtime();
    rt.handle_pty_bytes(b"\x07");
    let now = std::time::Instant::now();
    assert!(rt.visual_bell_active_at(now));
    // Present the admitted flash, consuming the admission's redraw request.
    assert!(rt.tick_at(now).is_some(), "admitted flash must present");
    assert!(rt.visual_bell_active_at(now));
    // No PTY bytes and no layout/geometry change before the deadline.
    let later = now + BELL_FLASH_DURATION;
    let stats = rt.tick_at(later);
    assert!(
        stats.is_some(),
        "flash expiry must force a frame on a quiet window"
    );
    assert!(
        !rt.visual_bell_active_at(later),
        "flash must be cleared by the expiry tick"
    );
}

#[test]
fn notification_banner_expires_on_a_quiet_window_without_pty_or_layout_change() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;quiet\x07");
    let now = std::time::Instant::now();
    assert_eq!(rt.notification_banner_at(now).as_deref(), Some("quiet"));
    assert!(rt.tick_at(now).is_some(), "banner must present");
    let later = now + NOTIFICATION_BANNER_DURATION;
    let stats = rt.tick_at(later);
    assert!(
        stats.is_some(),
        "banner expiry must force a frame on a quiet window"
    );
    assert!(
        rt.notification_banner_at(later).is_none(),
        "banner must be cleared by the expiry tick"
    );
}

#[test]
fn bell_notification_deadline_tracks_live_surfaces() {
    // The app wake computation reads this deadline; it must be `Some` only
    // while a bounded surface is live, so an idle window keeps zero wakes.
    let mut rt = runtime();
    assert!(rt.bell_notification_deadline().is_none());
    rt.handle_pty_bytes(b"\x07");
    let deadline = rt.bell_notification_deadline().expect("flash arms a wake");
    assert!(deadline >= std::time::Instant::now());
    // Past the flash window the deadline clears once a tick expires it.
    let _ = rt.tick_at(std::time::Instant::now() + BELL_FLASH_DURATION);
    assert!(rt.bell_notification_deadline().is_none());
}

#[test]
fn osc9_conemu_subcommands_are_not_notifications() {
    // Review PX-3067: ConEmu's `OSC 9;<n>` commands share the code with the
    // notification form, so they must not surface a banner even with consent.
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    for seq in [
        &b"\x1b]9;1\x07"[..],    // sleep
        &b"\x1b]9;2;hi\x07"[..], // message box
        &b"\x1b]9;3;title\x07"[..],
        &b"\x1b]9;4;1;50\x07"[..], // progress
        &b"\x1b]9;9\x07"[..],
    ] {
        rt.handle_pty_bytes(seq);
    }
    assert_eq!(rt.pending_notification_count(), 0);
    assert!(rt.notification_banner().is_none());
    // The bare text form still surfaces.
    rt.handle_pty_bytes(b"\x1b]9;real notification\x07");
    assert_eq!(
        rt.notification_banner().as_deref(),
        Some("real notification")
    );
}

#[test]
fn malformed_notification_forms_stay_inert() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;\x07");
    rt.handle_pty_bytes(b"\x1b]777;notify;t\x07");
    rt.handle_pty_bytes(b"\x1b]777;other;t;b\x07");
    assert_eq!(
        rt.pending_notification_count(),
        0,
        "malformed forms must not queue a notification"
    );
    assert_eq!(rt.notifications_denied(), 0, "consent was granted");
}

#[test]
fn plugin_bell_observation_is_unchanged_by_policy() {
    // The policy controls the user-visible surface only; the bounded
    // plugin-visible `HostObservation::Bell` bridge stays intact.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("must build");
    rt.set_bell_mode(BellMode::Off);
    rt.handle_pty_bytes(b"\x07");
    let obs = rt.drain_plugin_observations();
    assert!(
        obs.iter()
            .any(|o| matches!(o, bitty_plugin_host::HostObservation::Bell)),
        "plugin observation must not be gated by the display policy"
    );
}
