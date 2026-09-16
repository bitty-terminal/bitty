//! `bitty --test-mode` headless E2E servo loop (CTX-0506, research 043).
//!
//! Test mode runs the real [`Runtime`] plus the real `BITTY_SOCKET` servo
//! without a display, GPU, winit event loop, or VM, so native E2E tests can
//! drive panels and assert state over the accepted `bitty.debug/*` IPC
//! surface. It reuses the existing architecture end-to-end: the same socket
//! framing, dispatcher, scope authorization (`ScopeSet::cli_default()` plus
//! the explicit `BITTY_CTL_ELEVATE` allowlist), and the same cross-thread
//! control queue that the graphical app drains.
//!
//! Bounds:
//! - No new authority: test mode changes lifecycle only. It grants no scope,
//!   issues no automation bearer, widens no rate/redaction bound, and opens
//!   no transport beyond the same-UID `0600` Unix socket.
//! - Fail-closed: a test-mode process that cannot serve the socket exits
//!   non-zero instead of running a harness against no surface.
//! - Flag-gated surface: `bitty.debug/testInfo` / `bitty.debug/testExit`
//!   exist only while this loop serves (see `Dispatcher::with_test_mode`).
//! - Bounded loop: the tick interval is fixed, and the process exits 0 only
//!   on the elevated `testExit` control verb.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bitty_runtime::Runtime;

use crate::ctl;
use crate::ipc_serve::{self, ServerDescriptor};

/// Deterministic tick period for the headless loop (~60 Hz).
pub(crate) const TEST_MODE_TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Set only by the `bitty.debug/testExit` apply arm (main thread).
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request a clean test-mode shutdown (called from the control apply path).
pub(crate) fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Whether `testExit` has been applied.
#[must_use]
pub(crate) fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Run the test-mode loop; returns the process exit code.
///
/// The caller owns `runtime` (already configured, laid out, and spawned by
/// the normal startup path) and exits with this code.
pub(crate) fn run(runtime: &mut Runtime) -> i32 {
    let descriptor = ServerDescriptor {
        cols: runtime.config().cols,
        rows: runtime.config().rows,
        test_mode: true,
    };
    let ipc_serve = ipc_serve::serve_in_background(descriptor);
    if !ipc_serve.is_enabled() {
        eprintln!("bitty: test-mode requires a servable IPC socket — aborting");
        return 1;
    }
    eprintln!("bitty: test-mode ready socket={}", ipc_serve.socket_path());
    while !exit_requested() {
        // Same order as `TerminalApp::drive_tick`: pump the PTY, apply
        // queued control verbs on the runtime owner thread, present. The
        // pre-mutation zoom hook is a no-op: test mode has no chrome/zoom.
        let _ = runtime.poll_pty();
        let _ = ctl::drain_global_control_queue_with(
            runtime,
            &ctl::granted_scopes_for_servo(),
            |_, _| {},
        );
        let _ = runtime.tick();
        // `tick` publishes the inspect store only when it presents; publish
        // unconditionally so `getGridText` observes the latest state even on
        // an idle loop.
        runtime.publish_inspect_snapshot();
        std::thread::sleep(TEST_MODE_TICK_INTERVAL);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_flag_round_trips() {
        assert!(!exit_requested());
        request_exit();
        assert!(exit_requested());
    }
}
