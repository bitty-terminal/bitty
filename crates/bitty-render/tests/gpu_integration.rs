//! GPU-dependent integration tests.
//!
//! These tests exercise the only paths this crate contains that require a
//! real graphics stack. They are **skipped unless the environment variable
//! `BITTY_RENDER_GPU_TESTS=1` is set**, because CI runners have no GPU: a
//! plain `cargo test` on CI compiles this file but every test returns early
//! with a skip notice.
//!
//! To run for real, on a machine with a working driver:
//!
//! ```text
//! BITTY_RENDER_GPU_TESTS=1 cargo test -p bitty-render --test gpu_integration -- --nocapture
//! ```
//!
//! What is therefore *not* verified by default CI (see crate docs): adapter
//! enumeration, device creation, and — once later slices add them — surface
//! present paths.

use bitty_render::gpu::GpuContext;

const ENABLE_ENV: &str = "BITTY_RENDER_GPU_TESTS";

fn gpu_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

/// Minimal executor for driving wgpu's futures without adding a blocking
/// runtime dependency to the crate. Safe code only: `std::task::Wake` plus
/// a condvar; no `RawWaker` vtables.
mod block_on {
    use std::future::Future;
    use std::sync::{Arc, Condvar, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    struct Notify(Arc<(Mutex<bool>, Condvar)>);

    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let (flag, cv) = &*self.0;
            *flag.lock().expect("notify flag poisoned") = true;
            cv.notify_all();
        }
    }

    pub(super) fn run<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let waker = Waker::from(Arc::new(Notify(Arc::clone(&state))));
        let mut cx = Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    let (flag, cv) = &*state;
                    let mut signaled = flag.lock().expect("notify flag poisoned");
                    while !*signaled {
                        signaled = cv.wait(signaled).expect("condvar poisoned");
                    }
                    *signaled = false;
                }
            }
        }
    }
}

#[test]
fn gpu_context_initializes_on_a_real_adapter() {
    if !gpu_tests_enabled() {
        eprintln!("skipped: {ENABLE_ENV} != 1 (CI has no GPU; run locally with {ENABLE_ENV}=1)");
        return;
    }

    let ctx = block_on::run(GpuContext::initialize())
        .expect("GPU context should initialize when BITTY_RENDER_GPU_TESTS=1");

    let summary = ctx.adapter_summary();
    eprintln!(
        "adapter: {} ({:?}, {:?})",
        summary.name, summary.backend, summary.class
    );
    assert!(
        !summary.name.is_empty(),
        "driver must report an adapter name"
    );
}
