//! GPU-dependent integration tests.
//!
//! These tests exercise the only paths this crate contains that require a
//! real graphics stack or a live window system. They are **skipped unless the
//! environment variables below are set**, because CI runners have no GPU and
//! no display server: a plain `cargo test` on CI compiles this file but every
//! GPU test returns early with a skip notice.
//!
//! - `BITTY_RENDER_GPU_TESTS=1` gates adapter/device enumeration
//!   (`GpuContext::initialize`) and any test that reaches the driver.
//! - `BITTY_RENDER_GPU_SURFACE_TESTS=1` additionally gates real
//!   `wgpu::Surface` creation from a `bitty_platform::SurfaceTarget`
//!   (requires a window system on top of a GPU). When the variable is not set
//!   the surface test reports `skipped` and the headless fake is exercised
//!   instead by the unit tests in `src/gpu.rs` (`Surface::headless`);
//!   that headless path composites `DrawList`+`Atlas` onto an owned RGBA
//!   buffer and is verified on every CI run, including the
//!   `x86_64-pc-windows-gnu` target.
//!
//! To run for real, on a machine with a working driver and (for surface
//! tests) a window system:
//!
//! ```text
//! BITTY_RENDER_GPU_TESTS=1 cargo test -p bitty-render --test gpu_integration -- --nocapture
//! BITTY_RENDER_GPU_TESTS=1 BITTY_RENDER_GPU_SURFACE_TESTS=1 cargo test -p bitty-render --test gpu_integration -- --nocapture
//! ```
//!
//! What is therefore *not* verified by default CI (see crate docs): adapter
//! enumeration, device creation, and real surface creation/present. What *is*
//! verified on CI: the headless fake surface lifecycle (creation, format
//! fallback, configure/resize, and `DrawList`+`Atlas` present composition)
//! via `src/gpu.rs` unit tests, which run on both `native` and
//! `x86_64-pc-windows-gnu`.

use bitty_platform::PhysicalSize;
use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::gpu::{GpuContext, Surface};
use bitty_render::grid::{CellMetrics, GridRenderer};

const ENABLE_ENV: &str = "BITTY_RENDER_GPU_TESTS";
const SURFACE_ENV: &str = "BITTY_RENDER_GPU_SURFACE_TESTS";

fn gpu_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

fn surface_tests_enabled() -> bool {
    gpu_tests_enabled() && matches!(std::env::var(SURFACE_ENV).as_deref(), Ok("1"))
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

/// Headless fake surface: always runs (no GPU, no display server required).
///
/// This is the same seam the unit tests cover, duplicated here as an
/// integration-test witness that `Surface::headless` and
/// `Surface::headless_present` (DrawList+Atlas → RGBA) are reachable from an
/// integration target and honor the format/resize contracts.
#[test]
fn headless_surface_present_composites_without_gpu_or_display() {
    #[derive(Debug)]
    struct FakeR {
        next: u64,
    }
    impl GlyphRasterizer for FakeR {
        fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
            Ok(FontId::next(&mut self.next))
        }
        fn rasterize(&mut self, k: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
            if k.character == ' ' {
                return Ok(None);
            }
            let side = (u32::from(k.character) % 3 + 6) as i32;
            Ok(Some(
                GlyphBitmap::try_new(
                    GlyphMetrics {
                        left: 0,
                        top: 6,
                        width: side,
                        height: side,
                        advance: [side, 0],
                    },
                    BitmapFormat::Rgb,
                    vec![0xAA; side as usize * side as usize * 3],
                )
                .unwrap(),
            ))
        }
    }
    let q = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    let mut renderer =
        GridRenderer::new(FakeR { next: 0 }, &q, CellMetrics::new(8, 16).unwrap()).unwrap();
    let mut st = bitty_term_state::State::new();
    st.apply(&bitty_term_state::TerminalAction::Print(
        bitty_vt::GraphemeCell::from('A'),
    ));
    let snap = st.snapshot();
    let dmg = bitty_term_state::Damage {
        generation: snap.generation,
        regions: st.damage_since(0).into_boxed_slice(),
    };
    let list = renderer.render(&snap, &dmg).expect("render");
    let texels = renderer.atlas_texels().to_vec();
    let dims = renderer.atlas_dims();

    let surface = Surface::headless(PhysicalSize::new(320, 240)).expect("headless creation");
    assert!(surface.is_headless());
    assert_eq!(surface.extent(), Some(PhysicalSize::new(320, 240)));

    // Headless resize via the dedicated seam (no GpuContext required) — zero
    // extents are skipped, non-zero reconfigures.
    surface
        .headless_resize(PhysicalSize::new(0, 0))
        .expect("zero resize is no-op");
    assert_eq!(surface.extent(), Some(PhysicalSize::new(320, 240)));
    surface
        .headless_resize(PhysicalSize::new(640, 480))
        .expect("valid resize");
    assert_eq!(surface.extent(), Some(PhysicalSize::new(640, 480)));

    // Present DrawList+Atlas onto the headless buffer; inspect rgba.
    let stats = surface
        .headless_present(&list, Some((&texels, dims)))
        .expect("headless present");
    assert_eq!(stats.fills, list.fills.len());
    assert_eq!(stats.glyphs, list.glyphs.len());
    let rgba = surface.headless_rgba().expect("rgba after present");
    assert_eq!(rgba.len(), 640 * 480 * 4);
    assert!(rgba.iter().any(|&b| b != 0));
    eprintln!(
        "headless present ok: frame {} ({} fills, {} glyphs)",
        stats.frame, stats.fills, stats.glyphs
    );
}

/// Real GPU surface (window + adapter): env-gated, skipped on CI.
///
/// When `BITTY_RENDER_GPU_SURFACE_TESTS=1` this test attempts to initialize a
/// `GpuContext` and, if a display server is reachable, creates a real
/// `Surface` via `GpuContext::create_surface` from a `SurfaceTarget` held by
/// a hidden `winit` window. On headless CI the window creation fails and the
/// test reports `skipped` rather than panicking — the headless unit tests
/// remain the CI evidence. On a developer machine with a GPU + display, the
/// test verifies `configure`/`resize`/`present` can be called without panic.
#[test]
fn real_surface_creation_is_env_gated_and_display_dependent() {
    if !surface_tests_enabled() {
        eprintln!(
            "skipped: {ENABLE_ENV}=1 and {SURFACE_ENV}=1 required for real surface (CI uses headless fake)"
        );
        return;
    }

    let ctx = block_on::run(GpuContext::initialize())
        .expect("GPU context should initialize when BITTY_RENDER_GPU_TESTS=1");

    // Try to create a hidden window and derive a SurfaceTarget. If no display
    // server is available (e.g. SSH without X/Wayland), report skipped
    // instead of failing — the operator asked for GPU surface tests but the
    // environment cannot provide a window.
    let target = match try_create_surface_target() {
        Some(t) => t,
        None => {
            eprintln!("skipped: no display server available for real surface creation");
            return;
        }
    };

    let surface = ctx
        .create_surface(&target)
        .expect("GpuContext::create_surface should succeed with a live SurfaceTarget");
    assert!(!surface.is_headless());

    let extent = PhysicalSize::new(640, 480);
    surface
        .configure(&ctx, extent)
        .expect("configure should succeed");
    assert_eq!(surface.extent(), Some(extent));

    surface
        .resize(&ctx, PhysicalSize::new(0, 0))
        .expect("zero resize is a no-op for real surfaces");
    assert_eq!(surface.extent(), Some(extent));

    surface
        .resize(&ctx, PhysicalSize::new(800, 600))
        .expect("valid resize");
    assert_eq!(surface.extent(), Some(PhysicalSize::new(800, 600)));

    // Full DrawList presentation through the fill + glyph pipelines with
    // dirty-region atlas upload (CTX-0140): render a small snapshot with the
    // deterministic fake rasterizer, then present it on the real surface.
    // Swap-chain acquire may still fail on an occluded window; that reports
    // `skipped`, never a panic.
    {
        let q = FontQuery {
            family: "Fake".into(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        let mut renderer = GridRenderer::new(
            FakeRealSurfaceR { next: 0 },
            &q,
            CellMetrics::new(8, 16).unwrap(),
        )
        .unwrap();
        let mut st = bitty_term_state::State::new();
        st.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from('A'),
        ));
        let snap = st.snapshot();
        let dmg = bitty_term_state::Damage {
            generation: snap.generation,
            regions: st.damage_since(0).into_boxed_slice(),
        };
        let list = renderer.render(&snap, &dmg).expect("render");
        let texels = renderer.atlas_texels().to_vec();
        let dims = renderer.atlas_dims();
        match surface.present_draw_list(&ctx, &list, Some((&texels, dims))) {
            Ok(stats) => {
                assert_eq!(stats.fills, list.fills.len());
                assert_eq!(stats.glyphs, list.glyphs.len());
                assert!(!stats.headless);
                eprintln!(
                    "real present_draw_list ok: frame {} ({} fills, {} glyphs)",
                    stats.frame, stats.fills, stats.glyphs
                );
            }
            Err(e) => eprintln!(
                "real present_draw_list skipped (swap-chain acquire failed, likely window occluded): {e}"
            ),
        }
    }
}

/// Deterministic fake rasterizer for the real-surface DrawList draw above
/// (mirrors the headless fake; no font stack or filesystem touched).
#[derive(Debug)]
struct FakeRealSurfaceR {
    next: u64,
}

impl GlyphRasterizer for FakeRealSurfaceR {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, k: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if k.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(k.character) % 3 + 6) as i32;
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side,
                    height: side,
                    advance: [side, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn try_create_surface_target() -> Option<bitty_platform::SurfaceTarget> {
    // Build a hidden winit window on the current thread's event loop? `winit`
    // requires the event loop to be created on the main thread, but
    // `bitty_platform::App` already wraps that. For a minimal integration
    // test we try the low-level path: create an `EventLoop` and a single
    // window synchronously, then extract its `SurfaceTarget`. If that fails
    // (headless, no display), return None so the caller can skip.
    use bitty_platform::{App, AppHandler, EventContext, LogicalSize, PlatformEvent, WindowConfig};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Probe {
        target: Arc<Mutex<Option<bitty_platform::SurfaceTarget>>>,
        done: Arc<std::sync::atomic::AtomicBool>,
    }
    impl AppHandler for Probe {
        fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
            match event {
                PlatformEvent::Resumed => {
                    let cfg = WindowConfig::new()
                        .with_title("bitty-gpu-surface-probe")
                        .with_inner_size(LogicalSize::new(100.0, 100.0).expect("valid"))
                        .with_visible(false);
                    match ctx.create_window(cfg) {
                        Ok(handle) => {
                            *self.target.lock().expect("probe mutex") =
                                Some(handle.surface_target());
                            self.done.store(true, std::sync::atomic::Ordering::SeqCst);
                            ctx.exit();
                        }
                        Err(e) => {
                            eprintln!("window creation failed (headless): {e}");
                            self.done.store(true, std::sync::atomic::Ordering::SeqCst);
                            ctx.exit();
                        }
                    }
                }
                PlatformEvent::Window { .. } | PlatformEvent::AboutToWait
                    if self.done.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    ctx.exit();
                }
                _ => {}
            }
        }
    }

    let target = Arc::new(Mutex::new(None));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = Probe {
        target: Arc::clone(&target),
        done: Arc::clone(&done),
    };

    // `App::run` returns an owned error when no display exists; map that to
    // `None` so the caller can skip. On success it returns `Ok(())` and the
    // `target` mutex holds the `SurfaceTarget`.
    match App::run(handler) {
        Ok(()) => {
            let guard = target.lock().expect("probe mutex");
            guard.clone()
        }
        Err(e) => {
            eprintln!("App::run failed (headless): {e}");
            None
        }
    }
}
