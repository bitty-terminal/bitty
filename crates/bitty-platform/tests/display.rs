//! Display-dependent integration tests for `bitty-platform`.
//!
//! These tests open real windows through the system window manager and are
//! therefore **gated behind the default-off `gui-tests` feature**:
//!
//! ```sh
//! cargo test -p bitty-platform --features gui-tests
//! ```
//!
//! They never run in CI (headless) and are local verification evidence only.
//! Everything they cover beyond the headless unit tests is limited to the
//! thin OS-glue paths that cannot be exercised with constructed inputs:
//! actual event-loop creation, real window creation/resize/redraw delivery,
//! and live DPI scale reporting.

#![cfg(feature = "gui-tests")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bitty_platform::{
    App, AppHandler, EventContext, LogicalSize, PhysicalSize, PlatformEvent, WindowConfig,
    WindowEventKind,
};

#[derive(Default)]
struct Record {
    resized: Option<PhysicalSize>,
    redraw: bool,
}

/// Creates one window on resume, requests a redraw, and exits once a resize
/// or redraw request arrives — or after a bounded number of loop iterations
/// as a watchdog so a misbehaving compositor cannot hang the run.
struct SmokeTest {
    config: WindowConfig,
    record: Arc<std::sync::Mutex<Record>>,
    watchdog: Arc<AtomicU32>,
    finished: Arc<AtomicBool>,
}

impl AppHandler for SmokeTest {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
        match event {
            PlatformEvent::Resumed => {
                let handle = ctx
                    .create_window(self.config.clone())
                    .expect("window creation succeeds on a live display");
                assert!(handle.inner_size().height() > 0);
                assert!(handle.scale_factor().get() > 0.0);
                handle.request_redraw();
            }
            PlatformEvent::Window { kind, .. } => {
                let mut record = self.record.lock().expect("poison-free test mutex");
                match kind {
                    WindowEventKind::Resized(size) => {
                        assert!(size.width() > 0 && size.height() > 0);
                        record.resized = Some(size);
                        self.finished.store(true, Ordering::SeqCst);
                        ctx.exit();
                    }
                    WindowEventKind::RedrawRequested => {
                        record.redraw = true;
                        self.finished.store(true, Ordering::SeqCst);
                        ctx.exit();
                    }
                    _ => {}
                }
            }
            PlatformEvent::AboutToWait => {
                if self.watchdog.fetch_add(1, Ordering::SeqCst) > 600 {
                    panic!("no resize/redraw within watchdog budget");
                }
            }
            _ => {}
        }
    }
}

#[test]
fn creates_window_and_receives_resize_or_redraw() {
    let record = Arc::new(std::sync::Mutex::new(Record::default()));
    let handler = SmokeTest {
        config: WindowConfig::new()
            .with_title("bitty-platform gui-tests")
            .with_inner_size(LogicalSize::new(320.0, 240.0).expect("valid"))
            .with_visible(false),
        record: Arc::clone(&record),
        watchdog: Arc::new(AtomicU32::new(0)),
        finished: Arc::new(AtomicBool::new(false)),
    };

    App::run(handler).expect("live display expected under gui-tests");

    let record = record.lock().expect("poison-free test mutex");
    assert!(
        record.redraw || record.resized.is_some(),
        "expected at least one redraw request or resize event"
    );
}
