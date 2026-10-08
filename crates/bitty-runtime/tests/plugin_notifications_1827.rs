//! CTX-1033 (issue #1827) plugin-notification admission into the bounded
//! banner surface.
//!
//! Headless and deterministic; `cargo test` on CI without a display server.
//! These tests pin the application-facing half of #1827 at the `Runtime`
//! seam: grant-gated plugin notices (`platform.notify`, already enforced
//! when queued) admit into the same one-banner-at-a-time in-grid surface
//! plus the installed OS sink that terminal-originated notifications use —
//! with no OSC-consent gate, with the shared `RC-8` rate ceiling and the
//! bounded queue enforced, and with every refusal counted.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bitty_platform::{DesktopNotification, NotificationSink, OsDeliveryOutcome};
use bitty_runtime::{NOTIFICATION_BANNER_DURATION, NOTIFICATION_TEXT_MAX_CHARS, Runtime};

/// Recording [`NotificationSink`] double: captures payloads.
#[derive(Clone, Default)]
struct NotificationProbe {
    received: Arc<Mutex<Vec<(String, String)>>>,
}

impl NotificationProbe {
    fn received(&self) -> Vec<(String, String)> {
        self.received
            .lock()
            .expect("probe mutex must not be poisoned")
            .clone()
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
        OsDeliveryOutcome::Delivered
    }
}

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("headless runtime must build")
}

#[test]
fn first_plugin_notice_shows_banner_without_osc_consent() {
    let mut rt = runtime();
    // Default-deny for terminal output must not gate plugin notices: the
    // grant was enforced when the plugin queued the entry.
    assert!(!rt.osc_notification_allowed());
    let now = Instant::now();
    assert!(rt.admit_plugin_notification("Sample", "ok", now));
    assert_eq!(
        rt.notification_banner_at(now).as_deref(),
        Some("Sample: ok")
    );
    assert_eq!(rt.pending_notification_count(), 0);
    assert_eq!(rt.plugin_notifications_dropped(), 0);
}

#[test]
fn empty_plugin_notice_refused_without_counting() {
    let mut rt = runtime();
    let now = Instant::now();
    assert!(!rt.admit_plugin_notification("", "", now));
    assert!(!rt.admit_plugin_notification("  \n ", " \t", now));
    assert!(rt.notification_banner_at(now).is_none());
    assert_eq!(rt.plugin_notifications_dropped(), 0);
}

#[test]
fn plugin_rate_and_queue_overflow_counted_never_blocks() {
    let mut rt = runtime();
    let now = Instant::now();
    // One `RC-8` window admits at most `RC8_EVENTS_PER_WINDOW` notices at a
    // fixed instant: the first paints immediately, the next eight fill the
    // bounded queue, the tenth finds the queue full, and the eleventh trips
    // the rate ceiling.
    let mut admitted = 0u32;
    // Eleven attempts: one past the `RC-8` ceiling of ten per window.
    for _ in 0..11u32 {
        if rt.admit_plugin_notification("Burst", "n", now) {
            admitted += 1;
        }
    }
    assert_eq!(
        admitted, 9,
        "one banner plus one full queue admit; the rest refuse"
    );
    assert_eq!(
        rt.pending_notification_count(),
        8,
        "the bounded queue holds the rest"
    );
    assert_eq!(
        rt.plugin_notifications_dropped(),
        2,
        "queue-full plus rate refusals are counted"
    );
    assert_eq!(rt.notifications_queue_dropped(), 1);
    assert_eq!(rt.bell_rate_dropped(), 1);
    // The window re-opens admission; the surface keeps working (past the
    // expired banner, so the recovery paints immediately).
    let later = now + NOTIFICATION_BANNER_DURATION;
    assert!(rt.admit_plugin_notification("Burst", "recovered", later));
    assert_eq!(
        rt.notification_banner_at(later).as_deref(),
        Some("Burst: recovered")
    );
}

#[test]
fn plugin_notice_reaches_os_sink_at_admission_time() {
    let mut rt = runtime();
    let probe = NotificationProbe::default();
    rt.set_notification_sink(Some(Box::new(probe.clone())));
    let now = Instant::now();
    assert!(rt.admit_plugin_notification("Sample", "ok", now));
    assert_eq!(
        probe.received(),
        vec![(String::from("Sample"), String::from("ok"))]
    );
    assert_eq!(rt.notifications_os_delivered(), 1);
    assert_eq!(rt.notifications_os_undelivered(), 0);
}

#[test]
fn plugin_notice_without_sink_still_paints_and_counts_miss() {
    let mut rt = runtime();
    let now = Instant::now();
    assert!(rt.admit_plugin_notification("Sample", "ok", now));
    assert_eq!(
        rt.notification_banner_at(now).as_deref(),
        Some("Sample: ok")
    );
    assert_eq!(rt.notifications_os_delivered(), 0);
    assert_eq!(rt.notifications_os_undelivered(), 1);
}

#[test]
fn plugin_notice_text_is_sanitized_and_bounded() {
    let mut rt = runtime();
    let now = Instant::now();
    assert!(rt.admit_plugin_notification("A\x01B", "x\n\ty\x07z", now));
    assert_eq!(rt.notification_banner_at(now).as_deref(), Some("AB: xyz"));
    let long = "w".repeat(600);
    let later = now + NOTIFICATION_BANNER_DURATION;
    assert!(rt.admit_plugin_notification(&long, &long, later));
    let banner = rt.notification_banner_at(later).expect("banner visible");
    assert!(
        banner.chars().count() <= 2 * NOTIFICATION_TEXT_MAX_CHARS + 2,
        "banner stays bounded, got {} chars",
        banner.chars().count()
    );
}
