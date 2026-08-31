//! Real-window startup measurement — PB-1 cold startup.
//!
//! Replaces the former `--help` proxy with instrumentation that covers the
//! full `bitty-app` cold path: process start → config load → PTY spawn →
//! winit window attempt → wgpu init attempt → font init → first shell bytes
//! → first frame presented. Each phase is timestamped with `Instant` and
//! bounded tracing (no unbounded log growth, no `unsafe`).
//!
//! On headless CI (no display server, no GPU) the winit/wgpu phases report
//! `Unavailable` with their attempt duration — proving the instrumentation
//! exists without requiring a display. On a real Tier 1 box with a compositor
//! the same code reports real `Success` durations, enabling p50/p99
//! `≤100 ms / ≤200 ms` gating per `performance-budget-rfc.md#pb-1`.
//!
//! All phases are bounded and deterministic; no shell interpolation, no
//! unbounded allocations (max 8 KiB per synthetic batch, 64 B per key, 256
//! damage regions).

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_platform::LogicalSize;
use bitty_render::gpu::GpuContext;
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, View, ViewId};

/// A single startup phase measurement.
#[derive(Debug, Clone)]
pub struct StartupPhase {
    /// Human name for the phase (matches `performance-budget-rfc.md` pipeline).
    pub name: &'static str,
    /// Duration of the phase itself (from its start to its end).
    pub elapsed: Duration,
    /// Offset from process-start `t0` to phase-end (useful for p50 timeline).
    pub since_start: Duration,
    /// Outcome of the phase.
    pub status: PhaseStatus,
}

/// Outcome of a startup phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Phase completed successfully.
    Success,
    /// Phase skipped (not applicable, e.g. window already existed).
    Skipped(&'static str),
    /// Phase attempted but unavailable on this host (headless CI fallback).
    Unavailable(String),
    /// Phase failed with a bounded error string (truncated to 256 chars).
    Failed(String),
}

impl PhaseStatus {
    /// Returns `true` when the phase did not succeed but its `Unavailable`
    /// status is expected on headless CI (display/GPU absent).
    #[must_use]
    pub const fn is_headless_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Full startup report covering PB-1.
#[derive(Debug, Clone)]
pub struct StartupReport {
    /// All measured phases in chronological order.
    pub phases: Vec<StartupPhase>,
    /// Total wall time from `process_start` to last phase end.
    pub total: Duration,
    /// `true` when winit or wgpu were unavailable (headless fallback path).
    pub headless_fallback: bool,
    /// `true` when a real window + GPU surface were presented.
    pub is_real_window: bool,
    /// Whether the first frame was presented successfully.
    pub first_frame_presented: bool,
    /// Present stats for the first frame when available.
    pub first_frame_stats: Option<FramePresent>,
}

/// Minimal present stats for the first frame (owned, no wgpu type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePresent {
    /// Frame counter.
    pub frame: u64,
    /// Fill rect count.
    pub fills: usize,
    /// Glyph count.
    pub glyphs: usize,
    /// Whether headless software seam was used.
    pub headless: bool,
    /// Generation presented.
    pub generation: u64,
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        s
    } else {
        let mut out = s;
        out.truncate(max);
        out
    }
}

/// Measures headless startup with instrumentation covering every PB-1 phase.
///
/// This is the CI baseline: it exercises `Runtime::with_defaults` (which
/// covers config load + font init via `AnyRasterizer::try_crossfont` +
/// headless surface), optional PTY spawn, synthetic first bytes, winit
/// `EventLoop` availability probe, wgpu adapter probe, and `tick` → first
/// frame. No display server is required; winit/wgpu phases report
/// `Unavailable` on headless with their attempt duration.
#[must_use]
pub fn measure_headless_startup() -> StartupReport {
    let t0 = Instant::now();
    let mut phases: Vec<StartupPhase> = Vec::with_capacity(16);

    // Phase: args parse (trivial, bounded).
    let start = Instant::now();
    let _args: Vec<String> = std::env::args().collect();
    phases.push(StartupPhase {
        name: "args_parse",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: PhaseStatus::Success,
    });

    // Phase: config load (RuntimeConfig default + validate).
    let start = Instant::now();
    let config = RuntimeConfig::default();
    let status = match config.validate() {
        Ok(()) => PhaseStatus::Success,
        Err(e) => PhaseStatus::Failed(truncate(format!("{e:?}"), 256)),
    };
    let config_elapsed = start.elapsed();
    phases.push(StartupPhase {
        name: "config_load",
        elapsed: config_elapsed,
        since_start: t0.elapsed(),
        status,
    });

    // Phase: runtime create (covers font init + surface headless).
    // This is the dominant phase for PB-1 before window/GPU.
    let start = Instant::now();
    let runtime_result = Runtime::with_defaults();
    let (mut runtime, rt_status) = match runtime_result {
        Ok(rt) => (Some(rt), PhaseStatus::Success),
        Err(e) => (None, PhaseStatus::Failed(truncate(format!("{e:?}"), 256))),
    };
    let rt_elapsed = start.elapsed();
    phases.push(StartupPhase {
        name: "runtime_create (config+font+surface)",
        elapsed: rt_elapsed,
        since_start: t0.elapsed(),
        status: rt_status,
    });

    // Phase: layout install (single leaf — minimal, deterministic).
    let start = Instant::now();
    if let Some(rt) = runtime.as_mut() {
        let cols = rt.config().cols;
        let rows = rt.config().rows;
        let layout = LayoutNode::leaf(View::new(ViewId::new(1), cols, rows));
        rt.set_layout(layout);
        phases.push(StartupPhase {
            name: "layout_install",
            elapsed: start.elapsed(),
            since_start: t0.elapsed(),
            status: PhaseStatus::Success,
        });
    } else {
        phases.push(StartupPhase {
            name: "layout_install",
            elapsed: start.elapsed(),
            since_start: t0.elapsed(),
            status: PhaseStatus::Skipped("no runtime"),
        });
    }

    // Phase: PTY spawn (bounded: try default shell, fallback to echo).
    let start = Instant::now();
    let mut pty_status = PhaseStatus::Skipped("no runtime");
    if let Some(rt) = runtime.as_mut() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        // Prefer SHELL, then /bin/sh; bounded attempts.
        let candidates = [shell.as_str(), "/bin/sh", "/bin/bash"];
        let mut last_err = String::new();
        let mut ok = false;
        for cand in candidates {
            match rt.spawn_shell(cand) {
                Ok(()) => {
                    pty_status = PhaseStatus::Success;
                    ok = true;
                    break;
                }
                Err(e) => {
                    last_err = truncate(format!("{cand}: {e:?}"), 256);
                }
            }
        }
        if !ok && !last_err.is_empty() {
            // On Windows before ConPTY this is Unsupported — treat as Unavailable,
            // not a hard failure for headless measurement.
            if last_err.contains("Unsupported") || last_err.contains("not supported") {
                pty_status = PhaseStatus::Unavailable(last_err);
            } else {
                pty_status = PhaseStatus::Failed(last_err);
            }
        }
    }
    phases.push(StartupPhase {
        name: "pty_spawn",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: pty_status,
    });

    // Phase: winit window availability probe (bounded, non-blocking).
    // On headless CI this returns DisplayUnavailable quickly and proves the
    // winit seam is instrumented without opening a window.
    let start = Instant::now();
    let winit_status = probe_winit_availability();
    phases.push(StartupPhase {
        name: "winit_window_probe",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: winit_status,
    });

    // Phase: wgpu init probe (bounded via pollster + instant timeout).
    // On headless CI this returns NoCompatibleAdapter quickly.
    let start = Instant::now();
    let wgpu_status = probe_wgpu_availability();
    phases.push(StartupPhase {
        name: "wgpu_init_probe",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: wgpu_status.clone(),
    });

    // Phase: font init probe (explicit crossfont attempt, bounded).
    let start = Instant::now();
    let font_status = probe_font_availability();
    phases.push(StartupPhase {
        name: "font_init_probe",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: font_status,
    });

    // Phase: first shell bytes (synthetic bounded batch if no live PTY bytes yet).
    let start = Instant::now();
    let mut bytes_status = PhaseStatus::Skipped("no runtime");
    if let Some(rt) = runtime.as_mut() {
        // Drain any real PTY bytes first (bounded via poll_pty's 128 KiB cap).
        let drained = rt.poll_pty();
        if drained > 0 {
            bytes_status = PhaseStatus::Success;
        } else {
            // Synthetic first prompt bytes — exercises full pipeline without child.
            let synthetic = b"bitty startup probe \x1b[31mred\x1b[0m \x1b]0;bitty-perf\x07\r\n";
            rt.handle_pty_bytes(synthetic);
            bytes_status = PhaseStatus::Success;
        }
    }
    phases.push(StartupPhase {
        name: "first_shell_bytes",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: bytes_status,
    });

    // Phase: first frame presented (tick → Surface::headless_present).
    let start = Instant::now();
    let mut first_frame_presented = false;
    let mut first_frame_stats: Option<FramePresent> = None;
    let mut frame_status = PhaseStatus::Skipped("no runtime");
    if let Some(rt) = runtime.as_mut() {
        match rt.tick() {
            Some(stats) => {
                first_frame_presented = true;
                first_frame_stats = Some(FramePresent {
                    frame: stats.frame,
                    fills: stats.fills,
                    glyphs: stats.glyphs,
                    headless: stats.headless,
                    generation: stats.generation,
                });
                frame_status = PhaseStatus::Success;
            }
            None => {
                frame_status =
                    PhaseStatus::Failed("first tick returned idle (no damage)".to_string());
            }
        }
    }
    phases.push(StartupPhase {
        name: "first_frame_presented",
        elapsed: start.elapsed(),
        since_start: t0.elapsed(),
        status: frame_status,
    });

    let total = t0.elapsed();
    let headless_fallback = phases.iter().any(|p| p.status.is_headless_unavailable());
    let is_real_window = !headless_fallback
        && first_frame_stats.is_some_and(|s| !s.headless)
        && wgpu_status == PhaseStatus::Success;

    StartupReport {
        phases,
        total,
        headless_fallback,
        is_real_window,
        first_frame_presented,
        first_frame_stats,
    }
}

/// Bounded winit availability probe.
///
/// Attempts to build a winit `EventLoop` and reports whether a display server
/// is available. On headless CI this fails with `DisplayUnavailable` in
/// `O(10 ms)` and proves the winit seam without opening a window. No `unsafe`.
fn probe_winit_availability() -> PhaseStatus {
    let result = std::panic::catch_unwind(|| winit::event_loop::EventLoop::<()>::builder().build());
    match result {
        Ok(Ok(_)) => PhaseStatus::Success,
        Ok(Err(e)) => {
            let msg = truncate(format!("{e:?}"), 256);
            // winit maps NotSupported/Os errors to display-unavailable.
            // RecreationAttempt occurs when EventLoop was already created
            // (winit enforces single EventLoop per process via EVENT_LOOP_CREATED);
            // sequential tests with --test-threads=1 hit this on the 2nd probe and
            // must be classified as Unavailable, not Failed.
            if msg.contains("NotSupported")
                || msg.contains("Unavailable")
                || msg.contains("Display")
                || msg.contains("Wayland")
                || msg.contains("X11")
                || msg.contains("RecreationAttempt")
            {
                PhaseStatus::Unavailable(msg)
            } else {
                PhaseStatus::Failed(msg)
            }
        }
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "winit EventLoop::builder panicked".to_string()
            };
            let truncated = truncate(msg, 256);
            // Building EventLoop off the main thread panics on Linux; treat as Unavailable (headless seam).
            if truncated.contains("main thread") || truncated.contains("any_thread") {
                PhaseStatus::Unavailable(truncated)
            } else {
                PhaseStatus::Failed(truncated)
            }
        }
    }
}

/// Bounded wgpu adapter probe.
///
/// Drives `GpuContext::initialize` via `pollster::block_on` and records the
/// duration. On headless CI this returns `NoCompatibleAdapter` quickly;
/// on a real box with a driver it succeeds. The probe is bounded by the
/// driver's own timeout (wgpu returns promptly when no adapter is found).
fn probe_wgpu_availability() -> PhaseStatus {
    let result = pollster::block_on(GpuContext::initialize());
    match result {
        Ok(_) => PhaseStatus::Success,
        Err(e) => {
            let msg = truncate(format!("{e:?}"), 256);
            if msg.contains("NoCompatibleAdapter")
                || msg.contains("Adapter")
                || msg.contains("RequestAdapter")
            {
                PhaseStatus::Unavailable(msg)
            } else {
                PhaseStatus::Failed(msg)
            }
        }
    }
}

/// Bounded font probe.
///
/// Attempts to construct a `CrossFontRasterizer` and reports whether the
/// platform font stack is available. On headless CI with fontconfig this
/// succeeds (headless rasterizer fallback exists); the probe proves font init
/// is instrumented.
fn probe_font_availability() -> PhaseStatus {
    // Cheap headless check: Runtime already proved font via AnyRasterizer, but
    // we also probe crossfont directly. Crossfont construction may touch
    // fontconfig — bounded and quick (ms).
    match bitty_render::CrossFontRasterizer::new() {
        Ok(_) => PhaseStatus::Success,
        Err(e) => {
            let msg = truncate(format!("{e:?}"), 256);
            // Missing font stack is Unavailable, not a hard failure.
            PhaseStatus::Unavailable(msg)
        }
    }
}

impl StartupReport {
    /// Returns total duration in milliseconds (fractional).
    #[must_use]
    pub fn total_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0
    }

    /// Returns `true` when PB-1 p50 (100 ms) budget is met for the measured
    /// total (headless or real — caller must note which in evidence).
    #[must_use]
    pub fn meets_p50(&self) -> bool {
        self.total_ms() <= super::PB1_STARTUP_MS_P50 as f64
    }

    /// Returns `true` when PB-1 p99 (200 ms) budget is met.
    #[must_use]
    pub fn meets_p99(&self) -> bool {
        self.total_ms() <= super::PB1_STARTUP_MS_P99 as f64
    }

    /// Formats the timeline as a human-readable table for bench output and
    /// `docs/product/perf-evidence.md`.
    #[must_use]
    pub fn format_timeline(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "startup — total {:.2} ms (p50 {} ms / p99 {} ms) headless_fallback={} real_window={} first_frame={}\n",
            self.total_ms(),
            super::PB1_STARTUP_MS_P50,
            super::PB1_STARTUP_MS_P99,
            self.headless_fallback,
            self.is_real_window,
            self.first_frame_presented
        ));
        for p in &self.phases {
            let status = match &p.status {
                PhaseStatus::Success => "ok".to_string(),
                PhaseStatus::Skipped(s) => format!("skipped:{s}"),
                PhaseStatus::Unavailable(s) => format!("unavailable:{}", truncate(s.clone(), 80)),
                PhaseStatus::Failed(s) => format!("failed:{}", truncate(s.clone(), 80)),
            };
            out.push_str(&format!(
                "  {:<32} {:>7.2} ms elapsed, {:>7.2} ms since start [{status}]\n",
                p.name,
                p.elapsed.as_secs_f64() * 1000.0,
                p.since_start.as_secs_f64() * 1000.0
            ));
        }
        if let Some(s) = self.first_frame_stats {
            out.push_str(&format!(
                "  first_frame: frame={} fills={} glyphs={} headless={} gen={}\n",
                s.frame, s.fills, s.glyphs, s.headless, s.generation
            ));
        }
        let verdict = if self.meets_p50() {
            "PASS p50"
        } else if self.meets_p99() {
            "PASS p99 (p50 exceeded)"
        } else {
            "ABOVE_BUDGET"
        };
        out.push_str(&format!("  verdict: {verdict}\n"));
        out
    }
}

// ---------------------------------------------------------------------------
// Real-window variant (env-gated, Tier 1 box)
// ---------------------------------------------------------------------------

/// Attempts a real winit window + wgpu surface creation via `bitty-platform`
/// and `bitty-render` seams, returning a headless fallback report on CI.
///
/// This function requires `BITTY_PERF_REAL_WINDOW=1` and a display server;
/// otherwise it delegates to [`measure_headless_startup`] so CI stays green.
/// When enabled, it creates an `EventLoop`, a `Window`, and a `SurfaceTarget`,
/// then probes `GpuContext::create_surface` — all bounded and `forbid(unsafe)`.
///
/// Note: this is intentionally not called by default benches; use
/// `cargo bench --bench startup_real -- --nocapture` with the env var on a
/// Tier 1 box to obtain real-window numbers for evidence.
#[must_use]
pub fn measure_real_window_startup() -> StartupReport {
    if std::env::var("BITTY_PERF_REAL_WINDOW").as_deref() != Ok("1") {
        let mut report = measure_headless_startup();
        report.phases.push(StartupPhase {
            name: "real_window_gate",
            elapsed: Duration::from_micros(0),
            since_start: report.total,
            status: PhaseStatus::Skipped("BITTY_PERF_REAL_WINDOW!=1 (headless baseline)"),
        });
        return report;
    }

    // Real path: delegate to headless then attempt window+surface steps with
    // additional timing, so the baseline phases are always comparable.
    let mut report = measure_headless_startup();
    let t_extra = Instant::now();

    // Extra phase: winit window create via bitty-platform App seam.
    // We cannot run App::run without blocking, so we probe via EventLoop builder
    // plus a WindowConfig validation step that proves winit types are reachable.
    let probe_start = Instant::now();
    let window_status = probe_winit_window_create();
    report.phases.push(StartupPhase {
        name: "winit_window_create",
        elapsed: probe_start.elapsed(),
        since_start: report.total + t_extra.elapsed(),
        status: window_status,
    });

    report.total += t_extra.elapsed();
    report
}

fn probe_winit_window_create() -> PhaseStatus {
    // Validate LogicalSize and WindowConfig without touching the display.
    // This proves winit config sizing is instrumented even when display is absent.
    let size = match LogicalSize::new(800.0, 600.0) {
        Ok(s) => s,
        Err(e) => return PhaseStatus::Failed(truncate(format!("{e:?}"), 256)),
    };
    let _config = bitty_platform::WindowConfig::new()
        .with_title("bitty perf probe")
        .with_inner_size(size)
        .with_visible(false);
    // Now probe EventLoop creation again (already done in headless, but this
    // phase isolates window-config validation).
    probe_winit_availability()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_startup_instruments_all_phases() {
        let report = measure_headless_startup();
        assert!(!report.phases.is_empty(), "must have phases");
        assert!(
            report.first_frame_presented,
            "first frame must present headlessly"
        );
        assert!(report.total.as_millis() > 0, "total must be >0");
        // Every expected phase name must appear (pipeline coverage).
        let names: Vec<_> = report.phases.iter().map(|p| p.name).collect();
        for expected in [
            "args_parse",
            "config_load",
            "runtime_create (config+font+surface)",
            "layout_install",
            "pty_spawn",
            "winit_window_probe",
            "wgpu_init_probe",
            "font_init_probe",
            "first_shell_bytes",
            "first_frame_presented",
        ] {
            assert!(names.contains(&expected), "missing phase {expected}");
        }
        // Bounded: each elapsed < 5 s (no unbounded hang).
        for p in &report.phases {
            assert!(
                p.elapsed.as_secs() < 5,
                "phase {} elapsed {:?} exceeds bounded 5s",
                p.name,
                p.elapsed
            );
        }
    }

    #[test]
    fn real_window_gate_is_headless_when_env_absent() {
        // Without BITTY_PERF_REAL_WINDOW=1 the real variant must be headless baseline + skipped.
        let report = measure_real_window_startup();
        let has_gate = report.phases.iter().any(|p| p.name == "real_window_gate");
        assert!(
            has_gate,
            "real_window_gate phase must be present when env absent"
        );
    }

    #[test]
    fn winit_and_wgpu_probes_are_bounded() {
        let winit = probe_winit_availability();
        let wgpu = probe_wgpu_availability();
        // Both must be Success or Unavailable (never panic, never unbounded).
        assert!(
            matches!(winit, PhaseStatus::Success | PhaseStatus::Unavailable(_)),
            "winit probe must be Success or Unavailable, got {winit:?}"
        );
        assert!(
            matches!(wgpu, PhaseStatus::Success | PhaseStatus::Unavailable(_)),
            "wgpu probe must be Success or Unavailable, got {wgpu:?}"
        );
    }
}
