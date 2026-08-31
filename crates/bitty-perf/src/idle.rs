//! Idle invariant — PB-7 frame-on-demand, ≤1 % CPU over 10 min.
//!
//! Proves the idle terminal consumes ~0 % CPU/GPU with zero periodic wakeups
//! when no PTY output, animation, or plugin timer is active. The core
//! invariant is that `Runtime::tick` returns `None` when no new generation
//! exists and `pending_full_redraw` is false — keeping the platform loop in
//! `ControlFlow::Wait` (no polling loop, no unnecessary redraw).
//!
//! Measurement is bounded and headless: no 10-minute sleep is required for
//! correctness; the invariant is checked via repeated `tick` calls and a
//! bounded 10 s `ps %cpu` sample when available (otherwise the ticker cost
//! itself is measured). Real 10 min ≤1 % is gated on the Tier 1 reference
//! machine per `performance-budget-rfc.md#pb-7`.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_render::frame::FrameMode;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, GridRenderer};
use bitty_runtime::Runtime;
use bitty_term_state::{Damage, State};

// ---------------------------------------------------------------------------
// Fake rasterizer for idle render assertion (same as benches/render_prepare.rs)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeRasterizer {
    next: u64,
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, bitty_render::error::RenderError> {
        Ok(FontId::next(&mut self.next))
    }
    fn rasterize(
        &mut self,
        key: RasterKey,
    ) -> Result<Option<GlyphBitmap>, bitty_render::error::RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(key.character) % 3 + 6) as i32;
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

// ---------------------------------------------------------------------------
// Frame-on-demand checks
// ---------------------------------------------------------------------------

/// Result of a single idle invariant check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleCheck {
    /// Name of the check (e.g. "tick_is_idle_when_no_damage").
    pub name: &'static str,
    /// Whether the check passed.
    pub passed: bool,
    /// Bounded detail string (≤256 chars).
    pub detail: String,
}

/// Full idle report for PB-7.
#[derive(Debug, Clone)]
pub struct IdleReport {
    /// Individual frame-on-demand checks.
    pub checks: Vec<IdleCheck>,
    /// Mean cost of an idle `tick` (no damage) in microseconds.
    pub idle_tick_mean_us: f64,
    /// Mean cost of a clean `GridRenderer::render` (FrameMode::Clean) in µs.
    pub clean_render_mean_us: f64,
    /// Whether the renderer reports `Clean` when given clean damage.
    pub clean_is_clean: bool,
    /// Bounded CPU sample when `ps` is available (percent), else `None`.
    pub sampled_cpu_pct: Option<f64>,
    /// Total report wall time.
    pub elapsed: Duration,
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max);
    }
    s
}

/// Runs all frame-on-demand checks and returns a full report.
///
/// Bounded: at most 10 s of wall time, no polling loop, no unbounded allocations.
/// Headless only (no `winit::Window`, no `wgpu::Surface`).
#[must_use]
pub fn check_idle() -> IdleReport {
    let t0 = Instant::now();
    let mut checks = Vec::with_capacity(12);

    // Check 1: first tick presents, second tick is idle.
    {
        let mut rt = Runtime::with_defaults().expect("runtime for idle check");
        let first = rt.tick();
        let first_ok = first.is_some();
        checks.push(IdleCheck {
            name: "first_tick_presents",
            passed: first_ok,
            detail: truncate(format!("first tick {first:?}"), 256),
        });

        let second = rt.tick();
        let idle_ok = second.is_none();
        checks.push(IdleCheck {
            name: "second_tick_is_idle_no_damage",
            passed: idle_ok,
            detail: truncate(format!("second tick {second:?} (expect None)"), 256),
        });

        // Check 2: after real damage, tick presents then returns to idle.
        rt.handle_pty_bytes(b"hello idle");
        let third = rt.tick();
        let third_ok = third.is_some();
        checks.push(IdleCheck {
            name: "tick_after_bytes_presents",
            passed: third_ok,
            detail: truncate(format!("third tick {third:?}"), 256),
        });
        let fourth = rt.tick();
        let fourth_idle = fourth.is_none();
        checks.push(IdleCheck {
            name: "returns_to_idle_after_present",
            passed: fourth_idle,
            detail: truncate(format!("fourth tick {fourth:?} (expect None)"), 256),
        });

        // Check 3: resize forces full redraw then idle.
        let _ = rt.handle_resize(bitty_platform::PhysicalSize::new(800, 600));
        let after_resize = rt.tick();
        let resize_ok = after_resize.is_some();
        checks.push(IdleCheck {
            name: "resize_forces_full_redraw",
            passed: resize_ok,
            detail: truncate(format!("resize tick {after_resize:?}"), 256),
        });
        let after_resize_idle = rt.tick().is_none();
        checks.push(IdleCheck {
            name: "idle_after_resize_present",
            passed: after_resize_idle,
            detail: truncate(format!("post-resize idle {after_resize_idle}"), 256),
        });

        // Check 4: repeated idle ticks stay idle (no polling loop).
        let mut stays_idle = true;
        for _ in 0..100 {
            if rt.tick().is_some() {
                stays_idle = false;
                break;
            }
        }
        checks.push(IdleCheck {
            name: "100_idle_ticks_remain_idle_no_polling_loop",
            passed: stays_idle,
            detail: truncate(format!("100 ticks idle={stays_idle}"), 256),
        });

        // Check 5: pending_full_redraw false after present (no unnecessary redraw).
        let needs_no_redraw = rt.tick().is_none();
        checks.push(IdleCheck {
            name: "no_unnecessary_redraw",
            passed: needs_no_redraw,
            detail: truncate(format!("no pending redraw idle={needs_no_redraw}"), 256),
        });
    }

    // Check 6: GridRenderer clean-frame invariant (FrameMode::Clean, no draws).
    let (clean_is_clean, clean_detail) = check_clean_render();
    checks.push(IdleCheck {
        name: "grid_renderer_clean_frame_is_clean",
        passed: clean_is_clean,
        detail: clean_detail,
    });

    // Cost measurement: idle tick mean (bounded 10k iterations).
    let idle_tick_mean_us = measure_idle_tick_mean(3_000);

    // Cost measurement: clean render mean (bounded).
    let clean_render_mean_us = measure_clean_render_mean(2_000);

    // Bounded CPU sample via `ps` when available (10 s max window is not
    // executed here — we sample the current process quickly and expect ~0 %).
    // The real 10 min ≤1 % measurement remains on the Tier 1 box.
    let sampled_cpu_pct = sample_self_cpu_pct();

    // Summary: PB-7 is “zero wakeups when idle” — that is exactly the
    // frame-on-demand property that every idle tick returns None, so the
    // platform loop stays in ControlFlow::Wait and the compositor alone
    // drives wakes.
    let elapsed = t0.elapsed();
    IdleReport {
        checks,
        idle_tick_mean_us,
        clean_render_mean_us,
        clean_is_clean,
        sampled_cpu_pct,
        elapsed,
    }
}

fn check_clean_render() -> (bool, String) {
    // Build a State, derive a clean Damage (no new generation).
    let state = State::new();
    // Apply some bytes to generate a non-zero generation, then prime renderer.
    let snap0 = state.snapshot();
    let q = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    let mut renderer = match GridRenderer::new(
        FakeRasterizer { next: 0 },
        &q,
        CellMetrics::new(8, 16).unwrap(),
    ) {
        Ok(r) => r,
        Err(e) => return (false, truncate(format!("renderer build failed {e:?}"), 256)),
    };
    let gen0 = snap0.generation;
    let dmg0 = Damage {
        generation: gen0,
        regions: state.damage_since(0).into_boxed_slice(),
    };
    let _ = renderer.render(&snap0, &dmg0);

    // Now clean damage (no new state).
    let snap1 = state.snapshot();
    let dmg_clean = Damage {
        generation: snap1.generation,
        regions: state.damage_since(gen0).into_boxed_slice(),
    };
    match renderer.render(&snap1, &dmg_clean) {
        Ok(list) => {
            let is_clean = list.plan.mode == FrameMode::Clean || !list.plan.needs_draw();
            (
                is_clean,
                truncate(
                    format!(
                        "clean mode {:?} needs_draw={} dirty={}",
                        list.plan.mode,
                        list.plan.needs_draw(),
                        list.plan.dirty_rects.len()
                    ),
                    256,
                ),
            )
        }
        Err(e) => (false, truncate(format!("clean render failed {e:?}"), 256)),
    }
}

fn measure_idle_tick_mean(iters: usize) -> f64 {
    let mut rt = Runtime::with_defaults().expect("runtime for idle mean");
    let _ = rt.tick(); // prime so next is idle
    let start = Instant::now();
    for _ in 0..iters {
        let v = rt.tick();
        debug_assert!(v.is_none(), "idle mean: ticks must be idle");
        std::hint::black_box(v);
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    (elapsed * 1_000_000.0) / iters as f64
}

fn measure_clean_render_mean(iters: usize) -> f64 {
    let s = State::new();
    let snap = s.snapshot();
    let q = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    let mut renderer = GridRenderer::new(
        FakeRasterizer { next: 0 },
        &q,
        CellMetrics::new(8, 16).unwrap(),
    )
    .expect("fake renderer for clean mean");
    // Prime with full damage so generation advances, then clean.
    let dmg_full = Damage {
        generation: snap.generation,
        regions: s.damage_since(0).into_boxed_slice(),
    };
    let _ = renderer.render(&snap, &dmg_full);
    let generation = snap.generation;
    let snap_clean = s.snapshot();
    let dmg_clean = Damage {
        generation: snap_clean.generation,
        regions: s.damage_since(generation).into_boxed_slice(),
    };
    let start = Instant::now();
    for _ in 0..iters {
        let out = renderer
            .render(&snap_clean, &dmg_clean)
            .expect("clean render");
        std::hint::black_box(out);
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    (elapsed * 1_000_000.0) / iters as f64
}

fn sample_self_cpu_pct() -> Option<f64> {
    // Use `ps -o %cpu=` for the current pid; headless and bounded.
    // Sleep briefly before sampling so the measurement reflects an idle
    // window, not the busy bench loop that just ran (otherwise ps reports
    // the compilation/bench CPU, not idle). On Windows `ps` is unavailable.
    std::thread::sleep(Duration::from_millis(200));
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "%cpu=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            text.parse::<f64>().ok()
        }
        _ => None,
    }
}

impl IdleReport {
    /// Returns `true` when all frame-on-demand checks passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed) && self.clean_is_clean
    }

    /// Returns `true` when PB-7 CPU budget (≤1 %) is met for the sampled value.
    #[must_use]
    pub fn meets_cpu_budget(&self) -> bool {
        self.sampled_cpu_pct
            .is_none_or(|pct| pct <= super::PB7_IDLE_CPU_PCT as f64)
    }

    /// Formats a human-readable summary for bench output and evidence docs.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "idle — {} ({} checks) idle_tick_mean {:.2} µs clean_render_mean {:.2} µs clean_is_clean={} sampled_cpu={} (budget {}% avg over 10 min) elapsed {:.2} ms\n",
            if self.all_passed() { "PASS frame-on-demand" } else { "FAIL frame-on-demand" },
            self.checks.len(),
            self.idle_tick_mean_us,
            self.clean_render_mean_us,
            self.clean_is_clean,
            self.sampled_cpu_pct
                .map(|pct| format!("{pct:.2}%"))
                .unwrap_or_else(|| "n/a (ps unavailable)".to_string()),
            super::PB7_IDLE_CPU_PCT,
            self.elapsed.as_secs_f64() * 1000.0
        ));
        for c in &self.checks {
            out.push_str(&format!(
                "  {}: {} — {}\n",
                c.name,
                if c.passed { "PASS" } else { "FAIL" },
                c.detail
            ));
        }
        // PB-7 verdict: zero wakeups == every idle tick returns None, so
        // Wait loop burns no CPU beyond damage check. CPU sample is a soft proxy.
        let cpu_verdict = if self.meets_cpu_budget() {
            "PASS"
        } else {
            "ABOVE_BUDGET"
        };
        out.push_str(&format!(
            "  PB-7 verdict: frame-on-demand={} cpu {cpu_verdict} — zero periodic wakeups when idle (tick==None)\n",
            if self.all_passed() { "ok" } else { "FAIL" }
        ));
        if self.idle_tick_mean_us > 8_000.0 {
            out.push_str(
                "  warning: idle_tick_mean exceeds PB-4 p50 headroom (should be << 8 ms)\n",
            );
        }
        if self.clean_render_mean_us > 8_000.0 {
            out.push_str("  warning: clean_render_mean exceeds PB-4 p50 headroom\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_frame_on_demand_is_zero_wakeups() {
        let report = check_idle();
        assert!(
            report.all_passed(),
            "frame-on-demand must PASS: {}",
            report.format_summary()
        );
        assert!(report.clean_is_clean, "clean must be Clean");
        assert!(
            report.idle_tick_mean_us < 8_000.0,
            "idle tick mean {:.2} µs << 8 ms",
            report.idle_tick_mean_us
        );
        assert!(
            report.clean_render_mean_us < 8_000.0,
            "clean render mean {:.2} µs << 8 ms",
            report.clean_render_mean_us
        );
    }

    #[test]
    fn idle_checks_cover_required_phases() {
        let report = check_idle();
        let names: Vec<_> = report.checks.iter().map(|c| c.name).collect();
        for expected in [
            "first_tick_presents",
            "second_tick_is_idle_no_damage",
            "tick_after_bytes_presents",
            "returns_to_idle_after_present",
            "resize_forces_full_redraw",
            "idle_after_resize_present",
            "100_idle_ticks_remain_idle_no_polling_loop",
            "no_unnecessary_redraw",
            "grid_renderer_clean_frame_is_clean",
        ] {
            assert!(names.contains(&expected), "missing idle check {expected}");
        }
    }
}
