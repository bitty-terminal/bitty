//! Headless/degraded-startup verification for `App::run`.
//!
//! This target uses `harness = false` so it executes as a real process entry
//! point: winit requires the event loop to be created on the **main thread**,
//! which the normal test harness (worker threads) cannot provide. The crate
//! unit tests therefore cannot exercise `App::run`; this binary can.
//!
//! Behavior matrix:
//!
//! - Headless machine (CI): loop creation fails; `App::run` must return
//!   `PlatformError::DisplayUnavailable` instead of panicking or aborting.
//! - Machine with a live window system: the handler exits the loop on the
//!   first delivered event; `App::run` must return `Ok(())`.
//!
//! Both outcomes are accepted; anything else fails the gate.

use bitty_platform::{App, AppHandler, EventContext, PlatformError, PlatformEvent};

struct ExitOnFirstEvent;

impl AppHandler for ExitOnFirstEvent {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, _event: PlatformEvent) {
        ctx.exit();
    }
}

fn main() {
    let outcome = App::run(ExitOnFirstEvent);
    match outcome {
        Ok(()) => {
            println!("ok: event loop ran and exited cleanly");
        }
        Err(PlatformError::DisplayUnavailable(detail)) => {
            println!("ok: headless environment reported owned error: {detail}");
        }
        Err(other) => {
            eprintln!("fail: unexpected platform failure: {other:?}");
            std::process::exit(1);
        }
    }
}
