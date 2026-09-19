//! CTX-0577 (M1-17 / #1143) OSC 8 click-to-open live consumer.
//!
//! Headless and deterministic: a recording [`UrlOpener`] captures the exact
//! URI sequence the click path would hand to the OS, so no browser is
//! launched and the gate behavior (gesture required, scheme allowlist,
//! single-use) is asserted directly.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use bitty_platform::{CursorPosition, MouseButton, PlatformEvent, PressState, WindowEventKind};
use bitty_plugin_host::InterceptionDecision;
use bitty_runtime::{Runtime, UrlActivation, UrlOpener};

#[derive(Default)]
struct RecordingOpener {
    opened: Arc<Mutex<Vec<String>>>,
    fail: bool,
}

impl UrlOpener for RecordingOpener {
    fn open_url(&self, activation: UrlActivation) -> Result<(), bitty_platform::PlatformError> {
        if self.fail {
            return Err(bitty_platform::PlatformError::UrlLaunch(
                "recording opener failure".to_string(),
            ));
        }
        self.opened
            .lock()
            .expect("poison-free")
            .push(activation.uri().to_owned());
        Ok(())
    }
}

fn window_event(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind,
    }
}

fn click_link(rt: &mut Runtime) {
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(CursorPosition {
        x: 1.0,
        y: 1.0,
    })));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(
        bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released),
    )));
}

fn runtime_with_recorder() -> (Runtime, Arc<Mutex<Vec<String>>>) {
    let mut rt = Runtime::with_defaults().expect("must build");
    let opened = Arc::new(Mutex::new(Vec::new()));
    rt.set_url_opener(Box::new(RecordingOpener {
        opened: Arc::clone(&opened),
        fail: false,
    }));
    (rt, opened)
}

#[test]
fn click_on_safe_hyperlink_opens_through_the_live_consumer() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    click_link(&mut rt);
    assert!(rt.has_pending_hyperlink_activation());
    let uri = rt
        .activate_pending_hyperlink(&[], false)
        .expect("safe hyperlink must open");
    assert_eq!(uri, "https://example.test");
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://example.test"]
    );
    assert_eq!(rt.url_activations(), 1);
    assert_eq!(rt.url_activation_refusals(), 0);
    // Single use: the gesture is gone and a second call refuses.
    assert!(!rt.has_pending_hyperlink_activation());
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert_eq!(rt.url_activation_refusals(), 1);
    assert_eq!(opened.lock().expect("poison-free").len(), 1);
}

#[test]
fn no_gesture_means_nothing_opens() {
    let (mut rt, opened) = runtime_with_recorder();
    // Terminal output alone can never mint a gesture.
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert!(opened.lock().expect("poison-free").is_empty());
    assert_eq!(rt.url_activation_refusals(), 1);
}

#[test]
fn veto_and_timeout_are_fail_closed() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    click_link(&mut rt);
    assert!(
        rt.activate_pending_hyperlink(&[InterceptionDecision::Veto], false)
            .is_err()
    );
    assert!(opened.lock().expect("poison-free").is_empty());

    click_link(&mut rt);
    assert!(rt.activate_pending_hyperlink(&[], true).is_err());
    assert!(opened.lock().expect("poison-free").is_empty());
}

#[test]
fn hostile_scheme_never_mints_a_gesture() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
    click_link(&mut rt);
    assert!(!rt.has_pending_hyperlink_activation());
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert!(opened.lock().expect("poison-free").is_empty());
}

#[test]
fn opener_failure_is_counted_as_a_refusal() {
    let mut rt = Runtime::with_defaults().expect("must build");
    rt.set_url_opener(Box::new(RecordingOpener {
        opened: Arc::new(Mutex::new(Vec::new())),
        fail: true,
    }));
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    click_link(&mut rt);
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert_eq!(rt.url_activations(), 0);
    assert_eq!(rt.url_activation_refusals(), 1);
}
