//! Long-duration soak campaign driver (bitty#2830, presentation plane).
//!
//! The owner's unresponsiveness appeared after long uptime, and the remaining
//! acceptance bullet on bitty#2830 is a long-duration campaign rather than a
//! single compressed fixture. This file is the presentation half of that
//! campaign: one driver whose wall-clock budget comes from
//! `BITTY_SOAK_CAMPAIGN_SECS` (default [`DEFAULT_CAMPAIGN_SECS`], a two-minute
//! nightly form in CI and an hours-long owner form off CI), so the same code
//! path runs minutes or hours without a second harness.
//!
//! Workload per cycle (all against the injected clock, no sleeping):
//!
//! - sustained PTY byte volume: a truecolor TUI repaint, title/query traffic,
//!   and a periodic paste-sized write into the parser;
//! - repeated attach/detach: real shell panes spawned, polled, and closed on
//!   the split layout, with the pane-session set required to empty every cycle;
//! - panel drag/resize: border-drag steps on the two-pane divider plus window
//!   resizes, each committed through the same present path a real gesture uses;
//! - daemon-shaped event traffic on the app side: a subscribed plugin whose
//!   per-subscriber delivery queue is fed and partially drained every cycle.
//!
//! Sampling: the process RSS, its open file descriptors, the queued delivery
//! depth (`pending_deliveries`), worker leases (`workers`), panel leases, the
//! retained event ring (`event_ring`), retained content bytes, the cold queue,
//! and the live pane count are sampled at the start, near the middle, and at
//! the end of the budget (plus an even cadence in between). Every assertion
//! goes through [`Series::check`], so a regression prints the whole sampled
//! series into the CI log; the series is also written to
//! `BITTY_SOAK_EVIDENCE_DIR` when the nightly job sets it.
//!
//! Both tests are `#[ignore]`d: the PR-required `cargo nextest run --workspace`
//! never pays for a multi-minute campaign. Run them explicitly with
//! `just soak-campaign [secs]`.
//!
//! RSS and file-descriptor readings are Linux-only (`/proc`); other targets
//! run the identical workload and keep every queue/lease/ring assertion.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState};
use bitty_plugin_host::{
    CapabilityRequests, Compat as PluginCompat, Event, EventKind, EventPayload, LazyTriggers,
    PluginId, PluginIdentity, PluginManifest,
};
use bitty_runtime::hang_watchdog::HangGauges;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, View, ViewId};
use bitty_ui::SplitAxis;

/// Campaign budget when `BITTY_SOAK_CAMPAIGN_SECS` is unset: the short form
/// used for a local confirmation run.
const DEFAULT_CAMPAIGN_SECS: u64 = 120;

/// Smallest accepted campaign budget (below this the series cannot carry a
/// start/middle/end sample for the default cadence).
const MIN_CAMPAIGN_SECS: u64 = 8;

/// Largest accepted campaign budget (one day), so a typo cannot pin a runner.
const MAX_CAMPAIGN_SECS: u64 = 86_400;

/// Virtual milliseconds advanced per cycle (two simulated seconds of drift
/// per cycle: timers, lease TTLs, and repair wakes all reach their deadlines).
const VIRTUAL_STEP_MS: u64 = 2_000;

/// Cycles between window resizes.
const RESIZE_EVERY: u64 = 24;

/// Cycles between panel border-drag gestures.
const DRAG_EVERY: u64 = 8;

/// Cycles between pane attach/detach cycles (spawn, poll, close).
#[cfg(unix)]
const PANE_EVERY: u64 = 48;

/// Cycles between paste-sized writes (a 64 KiB diff payload).
const PASTE_EVERY: u64 = 32;

/// Cycles between alternate-screen exits (main-screen scrollback growth).
const ALT_EXIT_EVERY: u64 = 96;

/// Cycles between app-side event publishes and partial drains.
const DELIVERY_EVERY: u64 = 4;

/// Cycles of warmup before the first sample, so the start sample reflects the
/// steady state instead of one-off allocator growth.
const WARMUP_CYCLES: u64 = 12;

/// Hard cap on retained terminal content bytes for the whole campaign.
const MAX_CONTENT_BYTES: u64 = 128 * 1024 * 1024;

/// Hard cap on RSS growth between the first and last sample (allocator noise
/// included).
#[cfg(target_os = "linux")]
const MAX_RSS_GROWTH_KIB: u64 = 96 * 1024;

/// Hard cap on file-descriptor growth between the first and last sample.
#[cfg(target_os = "linux")]
const MAX_FD_GROWTH: i64 = 16;

/// Serializes the campaign tests: RSS and fd counts are process-wide.
static PROCESS_METRICS_LOCK: Mutex<()> = Mutex::new(());

/// Suffix counter keeping evidence files distinct inside one process.
static EVIDENCE_SEQ: AtomicU64 = AtomicU64::new(0);

const CSI: &str = "\x1b[";
const OSC: &str = "\x1b]";

// ---------------------------------------------------------------------------
// Campaign budget and sampling cadence
// ---------------------------------------------------------------------------

fn campaign_budget() -> Duration {
    let secs = std::env::var("BITTY_SOAK_CAMPAIGN_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CAMPAIGN_SECS)
        .clamp(MIN_CAMPAIGN_SECS, MAX_CAMPAIGN_SECS);
    Duration::from_secs(secs)
}

/// One sample per eighth of the budget, clamped so a short run still samples
/// and an hours-long run does not emit a line per second.
fn sample_cadence(budget: Duration) -> Duration {
    (budget / 8).clamp(Duration::from_millis(500), Duration::from_secs(60))
}

// ---------------------------------------------------------------------------
// Sampled series and bounded-growth assertions
// ---------------------------------------------------------------------------

/// Bound values one campaign asserts, rendered with the series.
#[derive(Clone, Copy)]
struct Bounds {
    rss_growth_kib: u64,
    fd_growth: i64,
    pending_deliveries: u64,
    event_ring: u64,
    content_bytes: u64,
}

/// One point-in-time campaign sample.
///
/// The column set is shared by both campaigns of this driver so a CI failure
/// reads identically; a plane that does not use a column samples `0` for it.
#[derive(Clone)]
struct Sample {
    label: String,
    elapsed_ms: u128,
    simulated_ms: u64,
    cycles: u64,
    rss_kib: u64,
    fds: i64,
    pending_deliveries: u64,
    event_ring: u64,
    worker_leases: u64,
    panel_leases: u64,
    content_bytes: u64,
    accounted_bytes: u64,
    cold_queue: u64,
    panes: u64,
    max_tick_ms: u64,
}

/// The campaign samples plus the bounds they are checked against.
struct Series {
    name: &'static str,
    budget: Duration,
    bounds: Bounds,
    samples: Vec<Sample>,
}

impl Series {
    fn new(name: &'static str, budget: Duration, bounds: Bounds) -> Self {
        Self {
            name,
            budget,
            bounds,
            samples: Vec::new(),
        }
    }

    fn push(&mut self, sample: Sample) {
        self.samples.push(sample);
    }

    /// Fill the unlabeled intermediate samples and name the one closest to the
    /// half-way point `middle`, so every series names start/middle/end even
    /// when the cadence drifts on a loaded runner.
    fn finalize_labels(&mut self) {
        let last = self.samples.len().saturating_sub(1);
        for (index, sample) in self.samples.iter_mut().enumerate() {
            if sample.label.is_empty() {
                sample.label = format!("s{index:02}");
            }
        }
        if self.samples.iter().any(|sample| sample.label == "middle") {
            return;
        }
        let half = self.budget.as_millis() / 2;
        let mut best: Option<(u128, usize)> = None;
        for (index, sample) in self.samples.iter().enumerate() {
            if index == 0 || index == last {
                continue;
            }
            let distance = sample.elapsed_ms.abs_diff(half);
            if best.is_none_or(|(current, _)| distance < current) {
                best = Some((distance, index));
            }
        }
        if let Some((_, index)) = best {
            self.samples[index].label = "middle".to_string();
        }
    }

    fn first(&self) -> Option<&Sample> {
        self.samples.first()
    }

    fn last(&self) -> Option<&Sample> {
        self.samples.last()
    }

    fn rss_growth_kib(&self) -> u64 {
        match (self.first(), self.last()) {
            (Some(first), Some(last)) => last.rss_kib.saturating_sub(first.rss_kib),
            _ => 0,
        }
    }

    fn fd_growth(&self) -> i64 {
        match (self.first(), self.last()) {
            (Some(first), Some(last)) => last.fds.saturating_sub(first.fds),
            _ => 0,
        }
    }

    fn peak_pending_deliveries(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| sample.pending_deliveries)
            .max()
            .unwrap_or(0)
    }

    /// Render the whole sampled series; this is the diagnostic a CI failure
    /// must carry, so the next tick can diagnose from the log alone.
    fn render(&self) -> String {
        let bounds = self.bounds;
        let mut out = format!(
            "\nsoak campaign `{}` failed; budget={:?} samples={} bounds: \
             rss_growth<={}KiB fd_growth<={} pending_deliveries<={} event_ring<={} \
             content_bytes<={}KiB\n  {:<8} {:>10} {:>12} {:>8} {:>9} {:>4} {:>10} {:>7} \
             {:>7} {:>7} {:>10} {:>10} {:>7} {:>4} {:>6}",
            self.name,
            self.budget,
            self.samples.len(),
            bounds.rss_growth_kib,
            bounds.fd_growth,
            bounds.pending_deliveries,
            bounds.event_ring,
            bounds.content_bytes / 1024,
            "label",
            "elapsed_ms",
            "simulated_ms",
            "cycles",
            "rss_kib",
            "fds",
            "pending_deliv",
            "event_ring",
            "workers",
            "panels",
            "content_kib",
            "accounted_kib",
            "cold_q",
            "panes",
            "max_tick_ms",
        );
        for sample in &self.samples {
            out.push_str(&format!(
                "\n  {:<8} {:>10} {:>12} {:>8} {:>9} {:>4} {:>10} {:>7} {:>7} {:>7} {:>10} {:>10} {:>7} {:>4} {:>6}",
                sample.label,
                sample.elapsed_ms,
                sample.simulated_ms,
                sample.cycles,
                sample.rss_kib,
                sample.fds,
                sample.pending_deliveries,
                sample.event_ring,
                sample.worker_leases,
                sample.panel_leases,
                sample.content_bytes / 1024,
                sample.accounted_bytes / 1024,
                sample.cold_queue,
                sample.panes,
                sample.max_tick_ms,
            ));
        }
        out
    }

    /// Assert a campaign invariant, printing the sampled series on failure.
    fn check(&self, condition: bool, message: &str) {
        if !condition {
            panic!("{message}{}", self.render());
        }
    }

    /// Append this series to `BITTY_SOAK_EVIDENCE_DIR` when the nightly job
    /// sets it, and always echo it so `--nocapture` logs carry the evidence.
    fn publish(&self, verdict: &str) {
        let rendered = format!(
            "campaign: {}\nbudget: {:?}\nsamples: {}\nverdict: {}\nseries:{}\n",
            self.name,
            self.budget,
            self.samples.len(),
            verdict,
            self.render().replace(" failed;", " recorded;"),
        );
        println!("{rendered}");
        let Ok(dir) = std::env::var("BITTY_SOAK_EVIDENCE_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!(
                "soak campaign: cannot create evidence dir {}",
                dir.display()
            );
            return;
        }
        let seq = EVIDENCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{seq}.txt", self.name));
        if let Err(error) = std::fs::write(&path, rendered) {
            eprintln!(
                "soak campaign: cannot write evidence {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("VmRSS:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn rss_kib() -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn fd_count() -> i64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as i64)
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn fd_count() -> i64 {
    0
}

// ---------------------------------------------------------------------------
// Workload fixtures
// ---------------------------------------------------------------------------

/// One truecolor TUI repaint, close to what a full-screen application emits.
fn frame_payload(cols: usize, rows: usize, frame: u64) -> String {
    let mut out = String::with_capacity(rows * 96);
    out.push_str(CSI);
    out.push_str("?2026h");
    for row in 0..rows.min(24) {
        out.push_str(&format!("{CSI}{};1H", row + 1));
        out.push_str(&format!(
            "{CSI}48;2;{};{};{}m",
            (frame * 7 + row as u64 * 11) % 256,
            (row * 5) % 256,
            (frame * 3 + row as u64 * 17) % 256
        ));
        out.push_str(&format!(
            "{CSI}38;2;{};{};{}m",
            (row * 13) % 256,
            (frame * 9 + 40) % 256,
            (row * 23 + 80) % 256
        ));
        let width = cols.min(80);
        out.push(' ');
        for col in 0..width {
            let pick = (frame as usize + row * 31 + col * 7) % 23;
            out.push(char::from(b'a' + (pick % 26) as u8));
        }
        out.push_str(CSI);
        out.push('K');
    }
    out.push_str(CSI);
    out.push_str("?2026l");
    out
}

/// Paste-sized single write (a bounded diff payload).
fn paste_payload() -> String {
    let mut out = String::with_capacity(32 * 1024);
    out.push_str(CSI);
    out.push_str("200~");
    for line in 0..400 {
        out.push_str(CSI);
        out.push_str("2K");
        out.push_str(&format!("diff line {line:04} + added\n"));
    }
    out.push_str(CSI);
    out.push_str("201~");
    out
}

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        scrollback: 4_000,
        ..RuntimeConfig::default()
    })
    .expect("deterministic runtime must build")
}

fn two_pane_layout() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding, 6px decoration outer inset, 9x19 cells).
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 6.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 6.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn mouse(button: MouseButton, state: PressState) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, state)
}

fn minimal_plugin_manifest(id: &str, events: Vec<&str>) -> PluginManifest {
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("valid plugin id"),
            name: "Soak campaign fixture".to_string(),
            version: "0.1.0".to_string(),
            description: "campaign delivery-queue subscriber".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: PluginCompat {
            bitty: Some(">=0.5,<1.0".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: CapabilityRequests::default(),
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: events.into_iter().map(str::to_string).collect(),
            claims: Vec::new(),
        },
        settings: Vec::new(),
        next_steps: Vec::new(),
        raw_bytes_len: 512,
    }
}

/// Capture one presentation-plane sample.
#[allow(clippy::too_many_arguments)]
fn capture(label: &str, elapsed: Duration, simulated_ms: u64, cycles: u64, rt: &Runtime) -> Sample {
    let gauges = rt.hang_gauges().snapshot();
    Sample {
        label: label.to_string(),
        elapsed_ms: elapsed.as_millis(),
        simulated_ms,
        cycles,
        rss_kib: rss_kib(),
        fds: fd_count(),
        // `plugin_events_queued` is the delivery queue the watchdog renders as
        // `pending_deliveries`.
        pending_deliveries: gauges.plugin_events_queued,
        event_ring: gauges.plugin_side_queued,
        worker_leases: gauges.plugin_workers,
        panel_leases: gauges.panel_leases,
        content_bytes: gauges.content_bytes,
        accounted_bytes: 0,
        cold_queue: u64::try_from(rt.cold_queue_len()).unwrap_or(u64::MAX),
        panes: gauges.pane_count,
        max_tick_ms: gauges.max_tick_ns / 1_000_000,
    }
}

// ---------------------------------------------------------------------------
// Campaign 1: presentation, PTY, attach/detach, panel drag, delivery queue
// ---------------------------------------------------------------------------

#[test]
#[ignore = "long-duration soak campaign; run via `just soak-campaign`"]
fn presentation_plane_campaign_stays_bounded() {
    let _serial = PROCESS_METRICS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let budget = campaign_budget();
    let cadence = sample_cadence(budget);

    let mut rt = make_runtime();
    let pipeline_capacity = rt.plugin_pipeline_capacity();
    let bounds = Bounds {
        #[cfg(target_os = "linux")]
        rss_growth_kib: MAX_RSS_GROWTH_KIB,
        #[cfg(not(target_os = "linux"))]
        rss_growth_kib: 0,
        #[cfg(target_os = "linux")]
        fd_growth: MAX_FD_GROWTH,
        #[cfg(not(target_os = "linux"))]
        fd_growth: 0,
        pending_deliveries: u64::try_from(pipeline_capacity).unwrap_or(u64::MAX),
        event_ring: u64::try_from(rt.plugin_side_capacity()).unwrap_or(u64::MAX),
        content_bytes: MAX_CONTENT_BYTES,
    };
    let mut series = Series::new("runtime-presentation-plane", budget, bounds);

    // A subscribed plugin keeps a live per-subscriber delivery queue: the
    // campaign feeds it and drains part of it, so `pending_deliveries` is a
    // real depth rather than a permanently empty counter.
    let manifest = minimal_plugin_manifest(
        "soak.campaign",
        vec!["terminal.title-changed", "terminal.bell"],
    );
    rt.register_plugin(manifest)
        .expect("register campaign plugin");
    let subscriber = PluginId::new("soak.campaign").expect("plugin id");
    rt.activate_plugin(&subscriber)
        .expect("activate campaign plugin");
    rt.subscribe_plugin_event(&subscriber, EventKind::TerminalBell)
        .expect("subscribe bells");

    rt.set_layout(two_pane_layout());
    let start = Instant::now();
    let clock = Instant::now();
    let mut now = clock;
    rt.tick_at(now).expect("baseline frame presents");

    let mut cycles = 0u64;
    let mut presented = 0u64;
    let mut drag_steps = 0u64;
    #[cfg(unix)]
    let mut pane_cycles = 0u64;
    let mut published = 0u64;
    let mut drained = 0u64;
    let mut replies = 0u64;
    let mut simulated_ms = 0u64;
    let mut next_sample = start + cadence;

    // Warmup: fill scrollback and settle the delivery queue before the start
    // sample, so growth is measured from a steady state.
    for warmup in 0..WARMUP_CYCLES {
        now += Duration::from_millis(VIRTUAL_STEP_MS);
        simulated_ms += VIRTUAL_STEP_MS;
        rt.handle_pty_bytes(frame_payload(80, 24, warmup).as_bytes());
        rt.publish_plugin_event(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            warmup,
        ));
        let _ = rt.tick_at(now);
        let _ = rt.drain_cold_events();
        let _ = rt.take_replies();
        let _ = rt.drain_plugin_observations_bounded(16);
    }
    series.push(capture("start", start.elapsed(), simulated_ms, cycles, &rt));

    while start.elapsed() < budget {
        now += Duration::from_millis(VIRTUAL_STEP_MS);
        simulated_ms += VIRTUAL_STEP_MS;
        cycles += 1;

        // Sustained PTY byte volume: a repaint plus title/query traffic.
        let mut payload = frame_payload(80, 24, cycles);
        if cycles % 5 == 0 {
            payload.push_str(OSC);
            payload.push_str(&format!("0;campaign frame {cycles}\x07"));
            payload.push_str(CSI);
            payload.push_str("6n");
            payload.push_str(CSI);
            payload.push_str("18t");
        }
        rt.handle_pty_bytes(payload.as_bytes());
        if cycles % PASTE_EVERY == 0 {
            rt.handle_pty_bytes(paste_payload().as_bytes());
        }

        // App-side event traffic into the bounded per-subscriber queue.
        if cycles % DELIVERY_EVERY == 0 {
            for _ in 0..2 {
                published += 1;
                rt.publish_plugin_event(Event::new(
                    EventKind::TerminalBell,
                    EventPayload::Empty,
                    published,
                ));
            }
            if cycles % (DELIVERY_EVERY * 4) == 0 {
                if let Ok(batch) =
                    rt.drain_plugin_events(&subscriber, &EventKind::TerminalBell, 2, 8 * 1024)
                {
                    drained += u64::try_from(batch.len()).unwrap_or(0);
                }
            }
        }

        // Panel drag/resize: grab the divider, step it, release, re-commit.
        if cycles % DRAG_EVERY == 0 {
            let grab = cell_pixels(39, 12);
            rt.handle_cursor_moved(grab);
            rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Pressed));
            if rt.border_drag_active() {
                drag_steps += 1;
                let offset = f64::from(1 + (cycles % 6) as i32);
                let position = CursorPosition {
                    x: grab.x + offset * 9.0,
                    y: grab.y,
                };
                let _ = rt.update_border_drag_at(position, now);
            }
            rt.handle_mouse_input(mouse(MouseButton::Left, PressState::Released));
        }
        if cycles % RESIZE_EVERY == 0 {
            let width = if (cycles / RESIZE_EVERY) % 2 == 0 {
                1_280
            } else {
                1_440
            };
            let _ = rt.handle_resize(PhysicalSize::new(width, 900));
        }

        // Repeated attach/detach: a real shell pane is spawned, polled, and
        // closed; the pane-session set must empty on every cycle.
        #[cfg(unix)]
        if cycles % PANE_EVERY == 0 {
            pane_cycles += 1;
            rt.set_layout(two_pane_layout());
            // pty-gate-exempt: inside `#[cfg(unix)]`; /bin/sh never spawns on Windows.
            let _ = rt.spawn_shell_for_view(
                ViewId::new(2),
                "/bin/sh",
                &["-c", "printf campaign-attach"],
                40,
                10,
            );
            let _ = rt.poll_pty();
            let _ = rt.tick_at(now + Duration::from_millis(3));
            let closed = rt.close_pane_session(&ViewId::new(2));
            series.check(
                closed,
                &format!("cycle {cycles}: the attached pane must close"),
            );
            series.check(
                rt.pane_session_ids().is_empty(),
                &format!("cycle {cycles}: attach/detach churn left a pane session"),
            );
        }

        // Alternate-screen churn grows the main-screen scrollback, the
        // retention surface a long session really accumulates.
        if cycles % ALT_EXIT_EVERY == 0 {
            rt.handle_pty_bytes(b"\x1b[?1049l\x1b[H");
            rt.handle_pty_bytes(format!("main screen tick {cycles}\r\n").as_bytes());
            let _ = rt.tick_at(now + Duration::from_millis(1));
            rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[H");
        }

        if rt.tick_at(now).is_some() {
            presented += 1;
        }
        // The application answers every child query and drains the cold and
        // side rings on each pass; a reply or observation left queued is a
        // backlog, never a present gap.
        replies += u64::try_from(rt.take_replies().len()).unwrap_or(0);
        let _ = rt.drain_cold_events();
        if cycles % 2 == 0 {
            let _ = rt.drain_plugin_observations_bounded(32);
        }

        if Instant::now() >= next_sample && start.elapsed() < budget {
            series.push(capture("", start.elapsed(), simulated_ms, cycles, &rt));
            next_sample += cadence;
        }
    }

    // Quiesce: one leaf, no pane, no pending reply, a visible surface.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    #[cfg(unix)]
    let _ = rt.close_pane_session(&ViewId::new(2));
    let _ = rt.take_replies();
    let _ = rt.drain_cold_events();
    let _ = rt.drain_plugin_observations_bounded(usize::MAX);
    let _ = rt.tick_at(now + Duration::from_millis(10));
    series.push(capture("end", start.elapsed(), simulated_ms, cycles, &rt));
    series.finalize_labels();

    let gauges = rt.hang_gauges().snapshot();
    series.check(
        series.samples.len() >= 3,
        "the campaign must sample at least start, middle, and end",
    );
    series.check(
        series.samples.iter().any(|sample| sample.label == "middle"),
        "the campaign series must name a middle sample",
    );
    series.check(
        cycles > 0 && presented > cycles / 2,
        &format!("most cycles must present (presented {presented} of {cycles})"),
    );
    series.check(
        gauges.tick_count >= cycles,
        &format!("every cycle must be counted, got {}", gauges.tick_count),
    );
    series.check(
        rt.pane_session_ids().is_empty(),
        "the campaign must not leave pane sessions",
    );
    series.check(
        gauges.hidden_damage == 0,
        "the campaign must end with a visible surface",
    );
    series.check(
        gauges.pending_replies == 0,
        &format!(
            "every query reply must be answered, {} left",
            gauges.pending_replies
        ),
    );
    series.check(
        gauges.plugin_parked_commands == 0 && gauges.plugin_active_commands == 0,
        "no plugin command may be parked or active at the end",
    );
    series.check(
        replies > 0,
        "the workload must exercise terminal query replies",
    );
    series.check(
        drag_steps > 0,
        "the campaign must execute panel drag gestures",
    );
    #[cfg(unix)]
    series.check(
        pane_cycles > 0,
        "the campaign must execute attach/detach cycles",
    );
    series.check(
        published > drained,
        "the delivery queue must carry traffic beyond what it drains",
    );

    // Per-sample bounds: no sample may sit outside a compiled cap.
    let pipeline_cap = bounds.pending_deliveries;
    let side_cap = bounds.event_ring;
    series.check(
        series.peak_pending_deliveries() <= pipeline_cap,
        &format!(
            "peak pending_deliveries {} exceeds the per-subscriber cap {pipeline_cap}",
            series.peak_pending_deliveries()
        ),
    );
    for sample in &series.samples {
        series.check(
            sample.pending_deliveries <= pipeline_cap,
            &format!(
                "sample {}: pending_deliveries {} exceeds the per-subscriber cap {pipeline_cap}",
                sample.label, sample.pending_deliveries
            ),
        );
        series.check(
            sample.event_ring <= side_cap,
            &format!(
                "sample {}: event_ring {} exceeds the side-queue cap {side_cap}",
                sample.label, sample.event_ring
            ),
        );
        series.check(
            sample.cold_queue <= u64::try_from(rt.cold_queue_capacity()).unwrap_or(u64::MAX),
            &format!(
                "sample {}: cold queue {} exceeds its cap",
                sample.label, sample.cold_queue
            ),
        );
        series.check(
            sample.content_bytes <= MAX_CONTENT_BYTES,
            &format!(
                "sample {}: retained content {} bytes exceeds the campaign budget",
                sample.label, sample.content_bytes
            ),
        );
    }

    // Bounded growth across the whole budget.
    series.check(
        series.rss_growth_kib() <= bounds.rss_growth_kib,
        &format!(
            "RSS grew {} KiB across the campaign (cap {} KiB)",
            series.rss_growth_kib(),
            bounds.rss_growth_kib
        ),
    );
    series.check(
        series.fd_growth() <= bounds.fd_growth,
        &format!(
            "open fds grew {} across the campaign (cap {})",
            series.fd_growth(),
            bounds.fd_growth
        ),
    );

    series.publish("bounded");
}

// ---------------------------------------------------------------------------
// Campaign 2: plugin activate/dispose churn (worker leases)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

const PLUGIN_INIT: &str = r#"
bitty.commands.register({
  id = "noop",
  title = "Noop",
  run = function(_args) return "ok" end,
})
return {}
"#;

fn write_plugin(root: &Path, id: &str) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Soak"
version = "0.1.0"
description = "soak"

[compat]
plugin-api = "^1.0"

[lazy]
commands = ["soak.plugin:noop"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), PLUGIN_INIT).expect("init");
    plugin
}

fn plugin_metrics(gauges: &HangGauges, label: &str, elapsed: Duration, cycles: u64) -> Sample {
    let snapshot = gauges.snapshot();
    Sample {
        label: label.to_string(),
        elapsed_ms: elapsed.as_millis(),
        simulated_ms: 0,
        cycles,
        rss_kib: rss_kib(),
        fds: fd_count(),
        pending_deliveries: snapshot.plugin_events_queued,
        event_ring: snapshot.plugin_side_queued,
        worker_leases: snapshot.plugin_workers,
        panel_leases: snapshot.panel_leases,
        content_bytes: snapshot.content_bytes,
        accounted_bytes: snapshot.plugin_events_bytes,
        cold_queue: snapshot.cold_queue_len,
        panes: snapshot.pane_count,
        max_tick_ms: snapshot.max_tick_ns / 1_000_000,
    }
}

#[test]
#[ignore = "long-duration soak campaign; run via `just soak-campaign`"]
fn plugin_activation_campaign_stays_bounded() {
    let _serial = PROCESS_METRICS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let budget = campaign_budget();
    let cadence = sample_cadence(budget);
    let bounds = Bounds {
        #[cfg(target_os = "linux")]
        rss_growth_kib: MAX_RSS_GROWTH_KIB,
        #[cfg(not(target_os = "linux"))]
        rss_growth_kib: 0,
        #[cfg(target_os = "linux")]
        fd_growth: MAX_FD_GROWTH,
        #[cfg(not(target_os = "linux"))]
        fd_growth: 0,
        pending_deliveries: 0,
        event_ring: 0,
        content_bytes: MAX_CONTENT_BYTES,
    };
    let mut series = Series::new("runtime-plugin-activation", budget, bounds);

    let root = std::env::temp_dir().join(format!("bitty-campaign-soak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let third_party = root.join("plugins");
    write_plugin(&third_party, "soak.plugin");
    let data = root.join("data");
    let gauges = Arc::new(HangGauges::new());
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![third_party],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([(
            "version",
            LuaValue::Integer(1),
        )]))),
    });
    common::install_stub_backend(&mut rt);
    common::install_harness_seams(&mut rt);
    rt.set_hang_gauges(Arc::clone(&gauges));
    let id = PluginId::new("soak.plugin").expect("id");
    let report = rt.discover();
    series.check(
        report.len() == 1 && report[0].1.is_ok(),
        "the campaign fixture plugin must discover",
    );

    let start = Instant::now();
    let mut cycles = 0u64;
    let mut next_sample = start + cadence;
    series.push(plugin_metrics(&gauges, "start", start.elapsed(), cycles));

    while start.elapsed() < budget {
        cycles += 1;
        let report = rt.activate(&id).expect("activate");
        series.check(
            report.state == LifecycleState::Active,
            "activation must reach Active",
        );
        series.check(
            gauges.snapshot().plugin_workers == 1,
            &format!("cycle {cycles}: activation must hold exactly one worker lease"),
        );
        let _ = rt.dispatch_command(&id, "noop", &[]).expect("command");
        rt.dispose(&id).expect("dispose");
        series.check(
            gauges.snapshot().plugin_workers == 0,
            &format!("cycle {cycles}: dispose must release the worker lease"),
        );
        series.check(
            gauges.snapshot().plugin_active_commands == 0
                && gauges.snapshot().plugin_parked_commands == 0,
            &format!("cycle {cycles}: no command may stay active or parked"),
        );
        if Instant::now() >= next_sample && start.elapsed() < budget {
            series.push(plugin_metrics(&gauges, "", start.elapsed(), cycles));
            next_sample += cadence;
        }
    }

    series.push(plugin_metrics(&gauges, "end", start.elapsed(), cycles));
    series.finalize_labels();

    series.check(
        series.samples.len() >= 3,
        "the campaign must sample at least start, middle, and end",
    );
    series.check(
        series.samples.iter().any(|sample| sample.label == "middle"),
        "the campaign series must name a middle sample",
    );
    series.check(cycles > 0, "the campaign must run at least one cycle");
    series.check(
        gauges.snapshot().plugin_workers == 0,
        "no worker lease may survive the campaign",
    );
    for sample in &series.samples {
        series.check(
            sample.worker_leases <= 1,
            &format!(
                "sample {}: worker leases {} exceeds the single-VM bound",
                sample.label, sample.worker_leases
            ),
        );
        series.check(
            sample.pending_deliveries == 0 && sample.event_ring == 0,
            &format!(
                "sample {}: plugin queues must stay empty (deliveries {}, ring {})",
                sample.label, sample.pending_deliveries, sample.event_ring
            ),
        );
        series.check(
            sample.content_bytes <= MAX_CONTENT_BYTES,
            &format!(
                "sample {}: retained content {} bytes exceeds the campaign budget",
                sample.label, sample.content_bytes
            ),
        );
    }
    series.check(
        series.rss_growth_kib() <= bounds.rss_growth_kib,
        &format!(
            "RSS grew {} KiB across the campaign (cap {} KiB)",
            series.rss_growth_kib(),
            bounds.rss_growth_kib
        ),
    );
    series.check(
        series.fd_growth() <= bounds.fd_growth,
        &format!(
            "open fds grew {} across the campaign (cap {})",
            series.fd_growth(),
            bounds.fd_growth
        ),
    );

    let _ = std::fs::remove_dir_all(&root);
    series.publish("bounded");
}
