//! CTX-0754 (issue #1361) OS delivery for bells and notifications.
//!
//! Headless and deterministic; `cargo test` on CI without a display server.
//! Delivery goes through injectable sink seams with recording doubles, so no
//! test ever spawns a real backend: these tests pin that admitted `BEL`
//! rings the bell sink, admitted `OSC 9` / `OSC 777` reach the notification
//! sink at admission time (not banner-rotation time), denied/rate-dropped
//! events never reach a sink, and sink failures keep the in-grid surface
//! intact while counting the miss.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use bitty_platform::{
    BellSink, DesktopNotification, NotificationSink, OsDeliveryOutcome, OsDeliverySkip,
};
use bitty_runtime::{BellMode, RC8_EVENTS_PER_WINDOW, Runtime};

/// Recording [`BellSink`] double: counts rings, replays a fixed outcome.
#[derive(Clone)]
struct BellProbe {
    rings: Arc<Mutex<u32>>,
    outcome: Arc<Mutex<OsDeliveryOutcome>>,
}

impl BellProbe {
    fn delivered() -> Self {
        Self {
            rings: Arc::new(Mutex::new(0)),
            outcome: Arc::new(Mutex::new(OsDeliveryOutcome::Delivered)),
        }
    }

    fn failing() -> Self {
        Self {
            rings: Arc::new(Mutex::new(0)),
            outcome: Arc::new(Mutex::new(OsDeliveryOutcome::Failed(String::from(
                "probe backend down",
            )))),
        }
    }

    fn rings(&self) -> u32 {
        *self.rings.lock().expect("probe mutex must not be poisoned")
    }
}

impl BellSink for BellProbe {
    fn ring(&self) -> OsDeliveryOutcome {
        *self.rings.lock().expect("probe mutex must not be poisoned") += 1;
        self.outcome
            .lock()
            .expect("probe mutex must not be poisoned")
            .clone()
    }
}

/// Recording [`NotificationSink`] double: captures sanitized payloads.
#[derive(Clone)]
struct NotificationProbe {
    received: Arc<Mutex<Vec<(String, String)>>>,
    outcome: Arc<Mutex<OsDeliveryOutcome>>,
}

impl NotificationProbe {
    fn delivered() -> Self {
        Self::default()
    }

    fn received(&self) -> Vec<(String, String)> {
        self.received
            .lock()
            .expect("probe mutex must not be poisoned")
            .clone()
    }
}

impl Default for NotificationProbe {
    fn default() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
            outcome: Arc::new(Mutex::new(OsDeliveryOutcome::Delivered)),
        }
    }
}

impl NotificationSink for NotificationProbe {
    fn deliver(&self, notification: &DesktopNotification) -> OsDeliveryOutcome {
        self.received
            .lock()
            .expect("probe mutex must not be poisoned")
            .push((
                notification.title().to_owned(),
                notification.body().to_owned(),
            ));
        self.outcome
            .lock()
            .expect("probe mutex must not be poisoned")
            .clone()
    }
}

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless runtime must build")
}

#[test]
fn audible_bell_rings_sink_once_per_admitted_bel() {
    let mut rt = runtime();
    let probe = BellProbe::delivered();
    rt.set_bell_sink(Some(Box::new(probe.clone())));
    rt.set_bell_mode(BellMode::Both);
    rt.handle_pty_bytes(b"\x07");
    assert_eq!(probe.rings(), 1, "admitted BEL must ring exactly once");
    assert_eq!(rt.audible_bell_requests(), 1);
    assert_eq!(rt.audible_bell_delivered(), 1);
    assert_eq!(rt.audible_bell_undelivered(), 0);
    assert!(rt.visual_bell_active(), "Both keeps the visual flash");
}

#[test]
fn audible_without_sink_is_counted_and_silent() {
    // Fail-closed default: no sink installed means no sound, but the request
    // is still counted so the miss is observable.
    let mut rt = runtime();
    rt.set_bell_mode(BellMode::Audible);
    rt.handle_pty_bytes(b"\x07");
    assert_eq!(rt.audible_bell_requests(), 1);
    assert_eq!(rt.audible_bell_delivered(), 0);
    assert_eq!(rt.audible_bell_undelivered(), 1);
    assert!(!rt.visual_bell_active());
}

#[test]
fn bell_off_never_rings_and_visual_never_rings() {
    let mut rt = runtime();
    let probe = BellProbe::delivered();
    rt.set_bell_sink(Some(Box::new(probe.clone())));
    rt.set_bell_mode(BellMode::Off);
    rt.handle_pty_bytes(b"\x07");
    assert_eq!(probe.rings(), 0);
    assert_eq!(rt.audible_bell_requests(), 0);

    let mut rt = runtime();
    let probe = BellProbe::delivered();
    rt.set_bell_sink(Some(Box::new(probe.clone())));
    rt.set_bell_mode(BellMode::Visual);
    rt.handle_pty_bytes(b"\x07");
    assert_eq!(probe.rings(), 0, "visual mode must not ring");
    assert_eq!(rt.audible_bell_requests(), 0);
}

#[test]
fn rate_limited_bells_do_not_ring() {
    let mut rt = runtime();
    let probe = BellProbe::delivered();
    rt.set_bell_sink(Some(Box::new(probe.clone())));
    rt.set_bell_mode(BellMode::Audible);
    for _ in 0..(RC8_EVENTS_PER_WINDOW + 5) {
        rt.handle_pty_bytes(b"\x07");
    }
    assert_eq!(
        probe.rings(),
        RC8_EVENTS_PER_WINDOW,
        "only RC-8-admitted bells ring"
    );
    assert_eq!(rt.audible_bell_requests(), u64::from(RC8_EVENTS_PER_WINDOW));
    assert!(rt.bell_rate_dropped() >= 5);
}

#[test]
fn failing_bell_sink_is_counted_and_keeps_visual_flash() {
    let mut rt = runtime();
    rt.set_bell_sink(Some(Box::new(BellProbe::failing())));
    rt.set_bell_mode(BellMode::Both);
    rt.handle_pty_bytes(b"\x07");
    assert_eq!(rt.audible_bell_requests(), 1);
    assert_eq!(rt.audible_bell_delivered(), 0);
    assert_eq!(rt.audible_bell_undelivered(), 1);
    assert!(
        rt.visual_bell_active(),
        "sink failure must not kill the in-grid flash"
    );
}

#[test]
fn denied_notification_never_reaches_sink() {
    let mut rt = runtime();
    let probe = NotificationProbe::delivered();
    rt.set_notification_sink(Some(Box::new(probe.clone())));
    // Default-deny: no consent, no delivery, no undelivered miss either
    // (the request died at the consent gate, not at the sink).
    rt.handle_pty_bytes(b"\x1b]9;build finished\x07");
    assert!(probe.received().is_empty());
    assert_eq!(rt.notifications_denied(), 1);
    assert_eq!(rt.notifications_os_delivered(), 0);
    assert_eq!(rt.notifications_os_undelivered(), 0);
}

#[test]
fn admitted_notifications_deliver_at_admission_not_banner_rotation() {
    let mut rt = runtime();
    let probe = NotificationProbe::delivered();
    rt.set_notification_sink(Some(Box::new(probe.clone())));
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;first\x07");
    rt.handle_pty_bytes(b"\x1b]777;notify;Build;second\x07");
    // The second notification is still queued behind the live banner, but
    // desktop delivery already happened for both.
    assert_eq!(rt.pending_notification_count(), 1);
    let received = probe.received();
    assert_eq!(received.len(), 2, "both admissions must deliver at once");
    assert_eq!(received[0], (String::new(), String::from("first")));
    assert_eq!(received[1], (String::from("Build"), String::from("second")));
    assert_eq!(rt.notifications_os_delivered(), 2);
    assert_eq!(rt.notifications_os_undelivered(), 0);
}

#[test]
fn sink_failure_keeps_banner_and_counts_miss() {
    let mut rt = runtime();
    let failing = NotificationProbe {
        received: Arc::new(Mutex::new(Vec::new())),
        outcome: Arc::new(Mutex::new(OsDeliveryOutcome::Failed(String::from(
            "probe backend down",
        )))),
    };
    rt.set_notification_sink(Some(Box::new(failing)));
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;hello\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("hello"));
    assert_eq!(rt.notifications_os_delivered(), 0);
    assert_eq!(rt.notifications_os_undelivered(), 1);
}

#[test]
fn skipped_outcome_counts_undelivered() {
    let mut rt = runtime();
    let skipped = NotificationProbe {
        received: Arc::new(Mutex::new(Vec::new())),
        outcome: Arc::new(Mutex::new(OsDeliveryOutcome::Skipped(
            OsDeliverySkip::BackendMissing,
        ))),
    };
    rt.set_notification_sink(Some(Box::new(skipped)));
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;hello\x07");
    assert_eq!(rt.notifications_os_delivered(), 0);
    assert_eq!(rt.notifications_os_undelivered(), 1);
}

#[test]
fn no_sink_counts_undelivered_but_banner_survives() {
    let mut rt = runtime();
    rt.set_osc_notification_allowed(true);
    rt.handle_pty_bytes(b"\x1b]9;hello\x07");
    assert_eq!(rt.notification_banner().as_deref(), Some("hello"));
    assert_eq!(rt.notifications_os_delivered(), 0);
    assert_eq!(rt.notifications_os_undelivered(), 1);
}

#[test]
fn notification_payload_is_sanitized_before_sink() {
    let mut rt = runtime();
    let probe = NotificationProbe::delivered();
    rt.set_notification_sink(Some(Box::new(probe.clone())));
    rt.set_osc_notification_allowed(true);
    let oversized = "y".repeat(600);
    let seq = format!("\x1b]9;a\u{1}b\u{7}{oversized}\x07");
    rt.handle_pty_bytes(seq.as_bytes());
    let received = probe.received();
    assert_eq!(received.len(), 1);
    let (_, body) = &received[0];
    assert!(!body.contains('\u{1}'));
    assert!(!body.contains('\u{7}'));
    assert!(
        body.chars().count() <= bitty_platform::NOTIFICATION_BODY_MAX_CHARS,
        "sink body must respect the OS bound"
    );
}
