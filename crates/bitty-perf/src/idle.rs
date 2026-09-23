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

/// Verdict for the PB-7 CPU-budget sample.
///
/// Absence of a sample is not evidence of compliance: an unavailable sample is
/// [`Unmeasured`](Self::Unmeasured), never `Met` (CTX-0484).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuBudgetVerdict {
    /// Sample present and at or below `PB7_IDLE_CPU_PCT`.
    Met,
    /// Sample present and above `PB7_IDLE_CPU_PCT`.
    Exceeded,
    /// No usable sample (e.g. `ps` unavailable); the budget is unverified.
    Unmeasured,
}

impl IdleReport {
    /// Returns `true` when all frame-on-demand checks passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed) && self.clean_is_clean
    }

    /// PB-7 CPU verdict for the bounded sample (fail-closed when absent).
    #[must_use]
    pub fn cpu_budget_verdict(&self) -> CpuBudgetVerdict {
        match self.sampled_cpu_pct {
            Some(pct) if pct <= super::PB7_IDLE_CPU_PCT as f64 => CpuBudgetVerdict::Met,
            Some(_) => CpuBudgetVerdict::Exceeded,
            None => CpuBudgetVerdict::Unmeasured,
        }
    }

    /// Returns `true` only when a CPU sample exists and is within the PB-7
    /// budget. An unmeasured budget is reported as not met, never as passing.
    #[must_use]
    pub fn meets_cpu_budget(&self) -> bool {
        matches!(self.cpu_budget_verdict(), CpuBudgetVerdict::Met)
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
                .unwrap_or_else(|| "unmeasured (no sample)".to_string()),
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
        // Wait loop burns no CPU beyond damage check. The CPU sample is a
        // soft, bounded proxy; an absent sample is reported UNMEASURED
        // rather than passing vacuously (CTX-0484).
        let cpu_verdict = match self.cpu_budget_verdict() {
            CpuBudgetVerdict::Met => "PASS",
            CpuBudgetVerdict::Exceeded => "ABOVE_BUDGET",
            CpuBudgetVerdict::Unmeasured => "UNMEASURED",
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

// ---------------------------------------------------------------------------
// PB-7 bounded idle-CPU/wakeup evidence (CTX-0636, PERF-08)
// ---------------------------------------------------------------------------

/// Schema version of the committed `pb-idle.json` artifact.
pub const IDLE_BASELINE_SCHEMA_VERSION: u32 = 1;
/// Committed PB-7 evidence artifact, relative to the repository root.
pub const IDLE_BASELINE_REL_PATH: &str = "crates/bitty-perf/baselines/pb-idle.json";
/// Hidden bench argument: re-exec the bench binary as the idle subject.
///
/// The child constructs a default `Runtime`, proves it is idle
/// (`tick == None`), then blocks like `ControlFlow::Wait` for the window.
/// The parent samples the child's CPU and wakeup counters across the window.
pub const IDLE_CHILD_ARG: &str = "--idle-child";
/// Environment knob for the extended idle window in seconds.
pub const IDLE_WINDOW_ENV: &str = "BITTY_PERF_IDLE_SECS";
/// Default extended idle window in seconds.
///
/// A full-budget window: the accepted budget averages over 10 minutes, so
/// the maximum covers the whole 600 s acceptance window in one parked
/// sample (CTX-0699 runs the real 10-minute soak on Tier 1 / this host).
/// A parked child shows its steady-state rate within a minute; the window
/// stays configurable up to the maximum.
pub const DEFAULT_IDLE_WINDOW_SECS: u64 = 60;
/// Maximum extended idle window in seconds: 600 s, exactly the PB-7
/// 10-minute acceptance window (raised from 300 in CTX-0699 so the soak
/// needs no stitching of sub-windows).
pub const MAX_IDLE_WINDOW_SECS: u64 = 600;

/// Idle-CPU/wakeup evidence for one parked-`Runtime` window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleCpuEvidence {
    /// Idle window actually observed, in seconds.
    pub window_secs: u64,
    /// Mean CPU over the window as percent of one core (`None` when unmeasured).
    pub avg_cpu_pct: Option<f64>,
    /// Delta of `utime + stime` across the window, in clock ticks.
    pub cpu_ticks: Option<u64>,
    /// `CLK_TCK` used for the tick conversion.
    pub clock_tick_hz: Option<u64>,
    /// Delta of voluntary + involuntary context switches across the window.
    ///
    /// Spawn/exit edge effects account for a small single-digit count; a
    /// parked child shows no steady-state periodic wakeups.
    pub wakeups: Option<u64>,
    /// Voluntary context-switch delta, when readable.
    pub voluntary_ctxt: Option<u64>,
    /// Involuntary context-switch delta, when readable.
    pub involuntary_ctxt: Option<u64>,
    /// Why the measurement is unavailable (`None` when measured).
    pub reason: Option<String>,
}

impl IdleCpuEvidence {
    /// `true` when no CPU sample was collected.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.avg_cpu_pct.is_none()
    }

    /// `true` only when a CPU sample exists and is within the PB-7 budget.
    /// An unmeasured window is reported as not met, never as passing.
    #[must_use]
    pub fn meets_budget(&self) -> bool {
        self.avg_cpu_pct
            .is_some_and(|pct| pct <= super::PB7_IDLE_CPU_PCT as f64)
    }

    /// One-line human summary for bench output and evidence docs.
    #[must_use]
    pub fn format_summary(&self) -> String {
        match (self.avg_cpu_pct, self.wakeups) {
            (Some(pct), Some(w)) => format!(
                "idle-cpu — {} window {}s avg_cpu {:.3}% (ticks {} @ {} Hz) wakeups {} (vol {} invol {}) vs PB-7 budget {}% avg over 10 min, zero periodic wakeups when idle",
                if self.meets_budget() {
                    "PASS"
                } else {
                    "ABOVE_BUDGET"
                },
                self.window_secs,
                pct,
                self.cpu_ticks.unwrap_or(0),
                self.clock_tick_hz.unwrap_or(0),
                w,
                self.voluntary_ctxt.unwrap_or(0),
                self.involuntary_ctxt.unwrap_or(0),
                super::PB7_IDLE_CPU_PCT,
            ),
            _ => format!(
                "idle-cpu — UNMEASURED window {}s ({})",
                self.window_secs,
                self.reason.as_deref().unwrap_or("no sample")
            ),
        }
    }
}

/// Provenance metadata supplied to [`baseline_json`].
#[derive(Debug, Clone, Default)]
pub struct IdleBaselineMeta {
    /// Owning CarryCtx task.
    pub task: String,
    /// Owning GitHub issues.
    pub issues: Vec<u64>,
    /// Capture date (`YYYY-MM-DD`).
    pub captured_at: String,
    /// Git revision the baseline was captured at.
    pub revision: String,
    /// Exact measurement command.
    pub command: String,
    /// Cargo profile used.
    pub profile: String,
}

/// Clamp the extended idle window from `BITTY_PERF_IDLE_SECS`.
#[must_use]
pub fn idle_window_secs() -> u64 {
    let parsed = std::env::var(IDLE_WINDOW_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_IDLE_WINDOW_SECS);
    clamp_window(parsed)
}

/// Clamp a raw idle-window value into `1..=MAX_IDLE_WINDOW_SECS`.
#[must_use]
pub const fn clamp_window(window_secs: u64) -> u64 {
    if window_secs < 1 {
        1
    } else if window_secs > MAX_IDLE_WINDOW_SECS {
        MAX_IDLE_WINDOW_SECS
    } else {
        window_secs
    }
}

/// Idle-subject entry point for the re-execed bench child. Never returns.
///
/// Proves the fresh `Runtime` is idle, parks on a condvar with a deadline
/// (the headless equivalent of the platform `ControlFlow::Wait` loop), then
/// re-verifies no internal timer produced damage while parked. Exits nonzero
/// when the runtime never reaches idle or wakes with pending damage.
pub fn run_idle_child(window_secs: u64) -> ! {
    use std::sync::{Arc, Condvar, Mutex};
    let window_secs = window_secs.clamp(1, MAX_IDLE_WINDOW_SECS);
    let mut rt = Runtime::with_defaults().expect("idle child runtime");
    let _ = rt.tick();
    assert!(
        rt.tick().is_none(),
        "idle child must reach idle before parking"
    );
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let (lock, cvar) = &*pair;
    let deadline = Instant::now() + Duration::from_secs(window_secs);
    let mut guard = lock.lock().expect("idle child condvar lock");
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (g, _) = cvar
            .wait_timeout(guard, remaining)
            .expect("idle child condvar wait");
        guard = g;
    }
    drop(guard);
    if rt.tick().is_some() {
        std::process::exit(3);
    }
    std::process::exit(0);
}

/// Measure the idle CPU/wakeup of a parked `Runtime` child over
/// [`idle_window_secs`].
///
/// Linux-only (`/proc` counters); every other platform returns `Unavailable`
/// with a reason. The child is killed and reaped on every path.
#[must_use]
pub fn measure_idle_cpu() -> IdleCpuEvidence {
    measure_idle_cpu_with(idle_window_secs())
}

/// Measurement variant with an explicit window in seconds (used by the bench).
#[must_use]
pub fn measure_idle_cpu_with(window_secs: u64) -> IdleCpuEvidence {
    let window_secs = window_secs.clamp(1, MAX_IDLE_WINDOW_SECS);
    let unavailable = |reason: &str| IdleCpuEvidence {
        window_secs,
        reason: Some(reason.to_string()),
        ..IdleCpuEvidence::default()
    };
    if !cfg!(target_os = "linux") {
        return unavailable("non-Linux host: no /proc counters for wakeup evidence");
    }
    let Some(hz) = clock_tick_hz() else {
        return unavailable("CLK_TCK undiscoverable (getconf failed)");
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return unavailable("bench executable path undiscoverable"),
    };
    let child = match std::process::Command::new(exe)
        .arg(IDLE_CHILD_ARG)
        .arg(window_secs.to_string())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return unavailable("idle child spawn failed"),
    };
    let pid = child.id();
    // Settle: let the child prove idle and reach its park before t0.
    std::thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    let Some((ticks0, vol0, invol0)) = read_child_counters(pid) else {
        reap(child);
        return unavailable("t0 /proc counters unreadable (child exited early?)");
    };
    std::thread::sleep(Duration::from_secs(window_secs));
    let wall_secs = t0.elapsed().as_secs_f64().max(1e-9);
    let result = match read_child_counters(pid) {
        Some((ticks1, vol1, invol1)) => {
            let dticks = ticks1.saturating_sub(ticks0);
            let pct = 100.0 * dticks as f64 / (hz as f64 * wall_secs);
            IdleCpuEvidence {
                window_secs,
                avg_cpu_pct: Some(pct),
                cpu_ticks: Some(dticks),
                clock_tick_hz: Some(hz),
                wakeups: Some(vol1.saturating_sub(vol0) + invol1.saturating_sub(invol0)),
                voluntary_ctxt: Some(vol1.saturating_sub(vol0)),
                involuntary_ctxt: Some(invol1.saturating_sub(invol0)),
                reason: None,
            }
        }
        None => unavailable("t1 /proc counters unreadable"),
    };
    reap(child);
    result
}

fn reap(mut child: std::process::Child) {
    // The child exits on its own when its park deadline passes; allow a
    // bounded grace, then kill. Every path reaps via `wait`.
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn clock_tick_hz() -> Option<u64> {
    let out = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|hz| *hz > 0)
}

/// CPU ticks plus wakeup counters for one pid via `/proc`.
fn read_child_counters(pid: u32) -> Option<(u64, u64, u64)> {
    let (utime, stime) =
        parse_stat_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let (vol, invol) =
        parse_status_ctxt(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)?;
    Some((utime + stime, vol, invol))
}

/// Parse `utime`/`stime` (fields 14/15) from `/proc/<pid>/stat` contents.
///
/// The comm field may contain spaces and parentheses, so fields are split
/// after the last `)`.
fn parse_stat_ticks(stat: &str) -> Option<(u64, u64)> {
    let after_comm = stat.rsplit(')').next()?;
    let mut fields = after_comm.split_whitespace();
    // After `pid (comm)`: state is field 3, utime field 14, stime field 15.
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some((utime, stime))
}

/// Parse voluntary/involuntary context switches from `/proc/<pid>/status`.
fn parse_status_ctxt(status: &str) -> Option<(u64, u64)> {
    let mut vol = None;
    let mut invol = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
            vol = rest.trim().parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            invol = rest.trim().parse::<u64>().ok();
        }
    }
    Some((vol?, invol?))
}

fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Serialize a PB-7 report plus provenance into the committed shape.
///
/// Provenance comes from the caller (environment-derived); host context is
/// captured from the environment. No checkout path, username, or hostname is
/// embedded.
#[must_use]
pub fn baseline_json(
    report: &IdleReport,
    cpu: &IdleCpuEvidence,
    meta: &IdleBaselineMeta,
) -> String {
    let host = crate::real_window::HostContext::capture();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {IDLE_BASELINE_SCHEMA_VERSION},\n"
    ));
    out.push_str(&format!("  \"task\": \"{}\",\n", escape_json(&meta.task)));
    let issues: Vec<String> = meta.issues.iter().map(u64::to_string).collect();
    out.push_str(&format!("  \"issues\": [{}],\n", issues.join(", ")));
    out.push_str(&format!(
        "  \"captured_at\": \"{}\",\n",
        escape_json(&meta.captured_at)
    ));
    out.push_str(&format!(
        "  \"revision\": \"{}\",\n",
        escape_json(&meta.revision)
    ));
    out.push_str(&format!(
        "  \"command\": \"{}\",\n",
        escape_json(&meta.command)
    ));
    out.push_str(&format!(
        "  \"profile\": \"{}\",\n",
        escape_json(&meta.profile)
    ));
    out.push_str(
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-7-idle-resource-usage\",\n",
    );
    out.push_str("  \"host_context\": {\n");
    out.push_str(&format!("    \"os\": \"{}\",\n", escape_json(&host.os)));
    out.push_str(&format!("    \"arch\": \"{}\",\n", escape_json(&host.arch)));
    out.push_str(&format!(
        "    \"toolchain\": \"{}\",\n",
        escape_json(&host.toolchain)
    ));
    out.push_str(&format!("    \"cpus\": {},\n", host.cpus));
    match host.total_memory_mb {
        Some(mb) => out.push_str(&format!("    \"total_memory_mb\": {mb}\n")),
        None => out.push_str("    \"total_memory_mb\": null\n"),
    }
    out.push_str("  },\n");
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!("    \"idle_window_secs\": {},\n", cpu.window_secs));
    out.push_str("    \"idle_tick_samples\": 3000,\n");
    out.push_str("    \"clean_render_samples\": 2000\n");
    out.push_str("  },\n");
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name)
        .collect();
    out.push_str("  \"frame_on_demand\": {\n");
    out.push_str(&format!(
        "    \"status\": \"{}\",\n",
        if report.all_passed() { "pass" } else { "fail" }
    ));
    out.push_str(&format!("    \"checks_run\": {},\n", report.checks.len()));
    out.push_str(&format!(
        "    \"checks_failed\": [{}],\n",
        failed
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "    \"idle_tick_mean_us\": {:.3},\n",
        report.idle_tick_mean_us
    ));
    out.push_str(&format!(
        "    \"clean_render_mean_us\": {:.3},\n",
        report.clean_render_mean_us
    ));
    out.push_str(&format!(
        "    \"clean_is_clean\": {}\n",
        report.clean_is_clean
    ));
    out.push_str("  },\n");
    out.push_str("  \"pb7_idle_cpu\": {\n");
    out.push_str(&format!(
        "    \"status\": \"{}\",\n",
        if cpu.is_unavailable() {
            "unavailable"
        } else {
            "measured"
        }
    ));
    out.push_str(&format!("    \"window_secs\": {},\n", cpu.window_secs));
    match cpu.avg_cpu_pct {
        Some(pct) => out.push_str(&format!("    \"avg_cpu_pct\": {pct:.4},\n")),
        None => out.push_str("    \"avg_cpu_pct\": null,\n"),
    }
    match cpu.cpu_ticks {
        Some(t) => out.push_str(&format!("    \"cpu_ticks\": {t},\n")),
        None => out.push_str("    \"cpu_ticks\": null,\n"),
    }
    match cpu.clock_tick_hz {
        Some(hz) => out.push_str(&format!("    \"clock_tick_hz\": {hz},\n")),
        None => out.push_str("    \"clock_tick_hz\": null,\n"),
    }
    match cpu.wakeups {
        Some(w) => out.push_str(&format!("    \"wakeups\": {w},\n")),
        None => out.push_str("    \"wakeups\": null,\n"),
    }
    match cpu.voluntary_ctxt {
        Some(v) => out.push_str(&format!("    \"voluntary_ctxt\": {v},\n")),
        None => out.push_str("    \"voluntary_ctxt\": null,\n"),
    }
    match cpu.involuntary_ctxt {
        Some(v) => out.push_str(&format!("    \"involuntary_ctxt\": {v},\n")),
        None => out.push_str("    \"involuntary_ctxt\": null,\n"),
    }
    out.push_str(&format!(
        "    \"budget_cpu_pct\": {},\n",
        super::PB7_IDLE_CPU_PCT
    ));
    out.push_str(&format!(
        "    \"meets_cpu_budget\": {}\n",
        cpu.meets_budget()
    ));
    out.push_str("  }\n");
    out.push_str("}\n");
    out
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

    #[test]
    fn probe_unmeasured_cpu_sample_is_not_met() {
        // CTX-0484 probe: an unavailable CPU sample (e.g. no `ps`) must never
        // read as "met". Fails before the fix because `is_none_or` vacuously
        // passes when the sample is `None`.
        let report = IdleReport {
            checks: Vec::new(),
            idle_tick_mean_us: 0.0,
            clean_render_mean_us: 0.0,
            clean_is_clean: true,
            sampled_cpu_pct: None,
            elapsed: Duration::ZERO,
        };
        assert!(
            !report.meets_cpu_budget(),
            "a missing CPU sample must not be reported as within budget"
        );
    }

    #[test]
    fn cpu_budget_verdict_is_tri_state_and_fail_closed() {
        let with_sample = |pct: Option<f64>| IdleReport {
            checks: Vec::new(),
            idle_tick_mean_us: 0.0,
            clean_render_mean_us: 0.0,
            clean_is_clean: true,
            sampled_cpu_pct: pct,
            elapsed: Duration::ZERO,
        };

        let within = with_sample(Some(0.5));
        assert_eq!(within.cpu_budget_verdict(), CpuBudgetVerdict::Met);
        assert!(within.meets_cpu_budget());

        let over = with_sample(Some(super::super::PB7_IDLE_CPU_PCT as f64 + 0.1));
        assert_eq!(over.cpu_budget_verdict(), CpuBudgetVerdict::Exceeded);
        assert!(!over.meets_cpu_budget());

        let unmeasured = with_sample(None);
        assert_eq!(
            unmeasured.cpu_budget_verdict(),
            CpuBudgetVerdict::Unmeasured
        );
        assert!(!unmeasured.meets_cpu_budget());
    }

    #[test]
    fn stat_ticks_parse_handles_paren_comm() {
        // pid (comm with spaces and (parens)) state ppid ... utime stime.
        let stat = "12345 (idle_real (test)) S 1 2 3 4 5 6 7 8 9 10 42 7 0 0 0";
        assert_eq!(parse_stat_ticks(stat), Some((42, 7)));
    }

    #[test]
    fn stat_ticks_parse_rejects_truncated() {
        assert_eq!(parse_stat_ticks("1 (x) S"), None);
        assert_eq!(parse_stat_ticks("no parens here"), None);
    }

    #[test]
    fn status_ctxt_parse_reads_both_counters() {
        let status = "Name:\tidle_real\nState:\tS (sleeping)\nvoluntary_ctxt_switches:\t12\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(parse_status_ctxt(status), Some((12, 3)));
    }

    #[test]
    fn status_ctxt_parse_requires_both() {
        assert_eq!(parse_status_ctxt("voluntary_ctxt_switches:\t1\n"), None);
        assert_eq!(parse_status_ctxt(""), None);
    }

    #[test]
    fn idle_cpu_meets_budget_is_fail_closed() {
        let met = IdleCpuEvidence {
            window_secs: 60,
            avg_cpu_pct: Some(0.05),
            ..IdleCpuEvidence::default()
        };
        assert!(!met.is_unavailable());
        assert!(met.meets_budget());

        let over = IdleCpuEvidence {
            avg_cpu_pct: Some(super::super::PB7_IDLE_CPU_PCT as f64 + 0.1),
            ..IdleCpuEvidence::default()
        };
        assert!(!over.meets_budget());

        let unmeasured = IdleCpuEvidence::default();
        assert!(unmeasured.is_unavailable());
        assert!(!unmeasured.meets_budget());
    }

    #[test]
    fn baseline_json_carries_schema_budget_and_numbers() {
        let report = IdleReport {
            checks: vec![IdleCheck {
                name: "second_tick_is_idle_no_damage",
                passed: true,
                detail: "ok".to_string(),
            }],
            idle_tick_mean_us: 0.5,
            clean_render_mean_us: 0.3,
            clean_is_clean: true,
            sampled_cpu_pct: Some(0.0),
            elapsed: Duration::ZERO,
        };
        let cpu = IdleCpuEvidence {
            window_secs: 60,
            avg_cpu_pct: Some(0.02),
            cpu_ticks: Some(1),
            clock_tick_hz: Some(100),
            wakeups: Some(4),
            voluntary_ctxt: Some(3),
            involuntary_ctxt: Some(1),
            reason: None,
        };
        let meta = IdleBaselineMeta {
            task: "CTX-0636".to_string(),
            issues: vec![1062],
            captured_at: "2026-09-22".to_string(),
            revision: "test-revision".to_string(),
            command: "test command".to_string(),
            profile: "test".to_string(),
        };
        let json = baseline_json(&report, &cpu, &meta);
        for needle in [
            "\"schema_version\": 1",
            "\"task\": \"CTX-0636\"",
            "\"issues\": [1062]",
            "pb-7-idle-resource-usage",
            "\"avg_cpu_pct\": 0.0200",
            "\"budget_cpu_pct\": 1",
            "\"meets_cpu_budget\": true",
            "\"checks_failed\": []",
        ] {
            assert!(json.contains(needle), "baseline must contain {needle}");
        }
    }

    #[test]
    fn idle_window_clamp_bounds_the_window() {
        assert_eq!(super::clamp_window(0), 1);
        assert_eq!(super::clamp_window(60), 60);
        assert_eq!(super::clamp_window(600), 600);
        assert_eq!(
            super::clamp_window(601),
            super::MAX_IDLE_WINDOW_SECS,
            "windows above the 10-minute acceptance window clamp to the maximum"
        );
        assert_eq!(super::clamp_window(u64::MAX), super::MAX_IDLE_WINDOW_SECS);
        assert_eq!(
            super::idle_window_secs().clamp(1, super::MAX_IDLE_WINDOW_SECS),
            super::idle_window_secs()
        );
    }
}
