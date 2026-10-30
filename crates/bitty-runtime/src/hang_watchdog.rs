//! Lock-free runtime gauges for the process hang watchdog (bitty#2830).
//!
//! The desktop composition root runs a monitor thread ([`crate::Runtime`] does
//! not own it; the `bitty-terminal` binary does) that must be able to describe
//! a wedged main thread without taking any runtime lock. A monitor that touched
//! `Runtime` state would deadlock against the very stall it is trying to
//! report, so every field here is a plain atomic written on the runtime owner
//! thread and read with `Relaxed` ordering on the monitor thread.
//!
//! Publication points:
//!
//! - [`Runtime::tick_at`](crate::Runtime::tick_at) brackets one frame
//!   (tick count, last/max duration, queue depths, retained content bytes,
//!   mounted panels and chrome bands).
//! - [`crate::Runtime::poll_pty`] publishes the bounded input buffer depth.
//! - [`crate::plugin_runtime::PluginRuntime::dispatch_command`] publishes the
//!   one command currently executing in a plugin VM (the active-command
//!   guard), so a hung Lua callback is visible in the dump.
//!
//! The values are gauges, not event logs: a dump reads the latest published
//! sample. That makes the fields stale by at most one tick, which is the
//! correct trade for a diagnostic that must never block or allocate on the
//! hot path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Point-in-time copy of [`HangGauges`], rendered by the process watchdog.
///
/// Every field is a plain integer so the report renderer stays allocation
/// bounded and platform independent. Counts saturate at `u64::MAX` rather
/// than wrapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HangGaugeSnapshot {
    /// Completed `Runtime::tick_at` calls since construction.
    pub tick_count: u64,
    /// Duration of the most recent completed tick, in nanoseconds.
    pub last_tick_ns: u64,
    /// Longest completed tick since construction, in nanoseconds.
    pub max_tick_ns: u64,
    /// Bytes buffered for the PTY writer (`pending_input`).
    pub pending_input_bytes: u64,
    /// Bytes dropped from the bounded PTY writer buffer.
    pub pending_input_dropped: u64,
    /// Queued cold-path events.
    pub cold_queue_len: u64,
    /// Cold-path events dropped at capacity.
    pub cold_queue_dropped: u64,
    /// Queued plugin event-pipeline events across every subscriber.
    ///
    /// Rendered as both `plugin_events` and `pending_deliveries`: this is the
    /// app-side delivery queue the monitor can inspect.
    pub plugin_events_queued: u64,
    /// Queued plugin event-pipeline payload bytes.
    pub plugin_events_bytes: u64,
    /// Queued host observations in the plugin side ring.
    ///
    /// Rendered as both `plugin_side` and `event_ring`.
    pub plugin_side_queued: u64,
    /// Host observations dropped from the side ring.
    pub plugin_side_dropped: u64,
    /// Plugin commands parked at the RC-1 wall budget.
    pub plugin_parked_commands: u64,
    /// Plugin commands currently executing (0 or 1 per single-threaded VM).
    pub plugin_active_commands: u64,
    /// Live plugin VMs tracked by the plugin runtime (the app-side workers).
    pub plugin_workers: u64,
    /// Component broker events waiting for the embedder.
    pub component_events: u64,
    /// Component broker events dropped at capacity.
    pub component_events_dropped: u64,
    /// Live split-pane sessions.
    pub pane_count: u64,
    /// Live layout leaves.
    pub leaf_count: u64,
    /// Estimated retained terminal content bytes (grids plus scrollback).
    pub content_bytes: u64,
    /// Mounted panels in the scene registry (the app-side leases).
    pub panel_leases: u64,
    /// Mounted chrome bands (the app-side bindings).
    pub band_bindings: u64,
    /// Queued input-path plugin commands awaiting the cold phase.
    pub input_commands: u64,
    /// Input-path commands dropped at capacity.
    pub input_commands_dropped: u64,
    /// Shell-exit edges awaiting the cold phase.
    pub pending_exits: u64,
}

impl HangGaugeSnapshot {
    /// Render the gauges as one stable, greppable report line.
    ///
    /// Kept here (rather than in the binary) so the field set has one owner:
    /// a new gauge must be added to [`HangGauges`] and this line together,
    /// and the headless watchdog regression asserts every key below.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "  runtime: tick_count={} last_tick_ms={} max_tick_ms={} pending_input_bytes={} \
pending_input_dropped={} cold_queue={} cold_dropped={} plugin_events={} \
plugin_event_bytes={} plugin_side={} plugin_side_dropped={} event_ring={} \
pending_deliveries={} plugin_parked={} plugin_active={} workers={} component_events={} \
component_events_dropped={} panes={} leaves={} content_bytes={} leases={} bindings={} \
input_commands={} input_commands_dropped={} pending_exits={}",
            self.tick_count,
            self.last_tick_ns / 1_000_000,
            self.max_tick_ns / 1_000_000,
            self.pending_input_bytes,
            self.pending_input_dropped,
            self.cold_queue_len,
            self.cold_queue_dropped,
            self.plugin_events_queued,
            self.plugin_events_bytes,
            self.plugin_side_queued,
            self.plugin_side_dropped,
            self.plugin_side_queued,
            self.plugin_events_queued,
            self.plugin_parked_commands,
            self.plugin_active_commands,
            self.plugin_workers,
            self.component_events,
            self.component_events_dropped,
            self.pane_count,
            self.leaf_count,
            self.content_bytes,
            self.panel_leases,
            self.band_bindings,
            self.input_commands,
            self.input_commands_dropped,
            self.pending_exits,
        )
    }
}

/// Atomic gauges shared by the runtime owner thread and the watchdog monitor.
///
/// `Relaxed` ordering is deliberate: each field is an independent monotonic
/// sample and a dump never needs a coherent cross-field snapshot. A torn
/// reading would only misattribute one queue depth by one tick, while
/// `Acquire`/`Release` would add fences to the per-frame path for no
/// diagnostic gain.
#[derive(Debug, Default)]
pub struct HangGauges {
    tick_count: AtomicU64,
    last_tick_ns: AtomicU64,
    max_tick_ns: AtomicU64,
    pending_input_bytes: AtomicU64,
    pending_input_dropped: AtomicU64,
    cold_queue_len: AtomicU64,
    cold_queue_dropped: AtomicU64,
    plugin_events_queued: AtomicU64,
    plugin_events_bytes: AtomicU64,
    plugin_side_queued: AtomicU64,
    plugin_side_dropped: AtomicU64,
    plugin_parked_commands: AtomicU64,
    plugin_active_commands: AtomicU64,
    plugin_workers: AtomicU64,
    component_events: AtomicU64,
    component_events_dropped: AtomicU64,
    pane_count: AtomicU64,
    leaf_count: AtomicU64,
    content_bytes: AtomicU64,
    panel_leases: AtomicU64,
    band_bindings: AtomicU64,
    input_commands: AtomicU64,
    input_commands_dropped: AtomicU64,
    pending_exits: AtomicU64,
}

impl HangGauges {
    /// Creates a zeroed gauge set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Copies every gauge for one bounded report.
    #[must_use]
    pub fn snapshot(&self) -> HangGaugeSnapshot {
        HangGaugeSnapshot {
            tick_count: self.tick_count.load(Ordering::Relaxed),
            last_tick_ns: self.last_tick_ns.load(Ordering::Relaxed),
            max_tick_ns: self.max_tick_ns.load(Ordering::Relaxed),
            pending_input_bytes: self.pending_input_bytes.load(Ordering::Relaxed),
            pending_input_dropped: self.pending_input_dropped.load(Ordering::Relaxed),
            cold_queue_len: self.cold_queue_len.load(Ordering::Relaxed),
            cold_queue_dropped: self.cold_queue_dropped.load(Ordering::Relaxed),
            plugin_events_queued: self.plugin_events_queued.load(Ordering::Relaxed),
            plugin_events_bytes: self.plugin_events_bytes.load(Ordering::Relaxed),
            plugin_side_queued: self.plugin_side_queued.load(Ordering::Relaxed),
            plugin_side_dropped: self.plugin_side_dropped.load(Ordering::Relaxed),
            plugin_parked_commands: self.plugin_parked_commands.load(Ordering::Relaxed),
            plugin_active_commands: self.plugin_active_commands.load(Ordering::Relaxed),
            plugin_workers: self.plugin_workers.load(Ordering::Relaxed),
            component_events: self.component_events.load(Ordering::Relaxed),
            component_events_dropped: self.component_events_dropped.load(Ordering::Relaxed),
            pane_count: self.pane_count.load(Ordering::Relaxed),
            leaf_count: self.leaf_count.load(Ordering::Relaxed),
            content_bytes: self.content_bytes.load(Ordering::Relaxed),
            panel_leases: self.panel_leases.load(Ordering::Relaxed),
            band_bindings: self.band_bindings.load(Ordering::Relaxed),
            input_commands: self.input_commands.load(Ordering::Relaxed),
            input_commands_dropped: self.input_commands_dropped.load(Ordering::Relaxed),
            pending_exits: self.pending_exits.load(Ordering::Relaxed),
        }
    }

    /// Records one completed tick.
    pub(crate) fn finish_tick(&self, elapsed_ns: u64) {
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        self.last_tick_ns.store(elapsed_ns, Ordering::Relaxed);
        self.max_tick_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
    }

    pub(crate) fn set_pending_input(&self, bytes: usize, dropped: u64) {
        self.pending_input_bytes
            .store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.pending_input_dropped.store(dropped, Ordering::Relaxed);
    }

    pub(crate) fn set_cold_queue(&self, len: usize, dropped: u64) {
        self.cold_queue_len
            .store(u64::try_from(len).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.cold_queue_dropped.store(dropped, Ordering::Relaxed);
    }

    pub(crate) fn set_plugin_pipeline(&self, events: usize, bytes: usize) {
        self.plugin_events_queued
            .store(u64::try_from(events).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.plugin_events_bytes
            .store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(crate) fn set_plugin_side(&self, queued: usize, dropped: u64) {
        self.plugin_side_queued
            .store(u64::try_from(queued).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.plugin_side_dropped.store(dropped, Ordering::Relaxed);
    }

    pub(crate) fn set_plugin_parked(&self, parked: usize) {
        self.plugin_parked_commands
            .store(u64::try_from(parked).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(crate) fn set_plugin_workers(&self, workers: usize) {
        self.plugin_workers.store(
            u64::try_from(workers).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn set_component_events(&self, queued: usize, dropped: u64) {
        self.component_events
            .store(u64::try_from(queued).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.component_events_dropped
            .store(dropped, Ordering::Relaxed);
    }

    pub(crate) fn set_layout(&self, panes: usize, leaves: usize, content_bytes: u64) {
        self.pane_count
            .store(u64::try_from(panes).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.leaf_count
            .store(u64::try_from(leaves).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.content_bytes.store(content_bytes, Ordering::Relaxed);
    }

    pub(crate) fn set_panel_bindings(&self, panels: usize, bands: usize) {
        self.panel_leases
            .store(u64::try_from(panels).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.band_bindings
            .store(u64::try_from(bands).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Publishes the app-owned input-command queue depth.
    ///
    /// The queue lives in the composition root, not the runtime, so the app
    /// writes it on its bounded context-sample cadence.
    pub fn set_input_commands(&self, queued: usize, dropped: u64) {
        self.input_commands
            .store(u64::try_from(queued).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.input_commands_dropped
            .store(dropped, Ordering::Relaxed);
    }

    /// Publishes the app-owned pending-exit edge count.
    pub fn set_pending_exits(&self, pending: usize) {
        self.pending_exits.store(
            u64::try_from(pending).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// Publishes "one plugin command is executing" for its own lifetime.
///
/// The guard owns a cloned handle, so it never borrows the plugin runtime and
/// can live across the mutable VM call. Dropping it (normal return, error, or
/// unwind) decrements the gauge, so a completed command cannot leave the dump
/// claiming a stuck invocation.
pub(crate) struct ActiveCommandGuard {
    gauges: Option<Arc<HangGauges>>,
}

impl ActiveCommandGuard {
    pub(crate) fn new(gauges: Option<&Arc<HangGauges>>) -> Self {
        if let Some(gauges) = gauges {
            gauges
                .plugin_active_commands
                .fetch_add(1, Ordering::Relaxed);
        }
        Self {
            gauges: gauges.cloned(),
        }
    }
}

impl Drop for ActiveCommandGuard {
    fn drop(&mut self) {
        if let Some(gauges) = self.gauges.as_ref() {
            // Saturating decrement: a double drop cannot underflow below 0.
            let mut current = gauges.plugin_active_commands.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_sub(1);
                match gauges.plugin_active_commands.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_track_count_last_and_max() {
        let gauges = HangGauges::new();
        gauges.finish_tick(1_000_000);
        gauges.finish_tick(5_000_000);
        gauges.finish_tick(2_000_000);
        let snapshot = gauges.snapshot();
        assert_eq!(snapshot.tick_count, 3);
        assert_eq!(snapshot.last_tick_ns, 2_000_000);
        assert_eq!(snapshot.max_tick_ns, 5_000_000);
    }

    #[test]
    fn active_command_guard_is_balanced() {
        let gauges = Arc::new(HangGauges::new());
        {
            let _guard = ActiveCommandGuard::new(Some(&gauges));
            assert_eq!(gauges.snapshot().plugin_active_commands, 1);
        }
        assert_eq!(gauges.snapshot().plugin_active_commands, 0);
        // A guard without a shared gauge set is a no-op.
        let _guard = ActiveCommandGuard::new(None);
    }

    #[test]
    fn render_names_every_gauge() {
        let snapshot = HangGaugeSnapshot {
            tick_count: 7,
            last_tick_ns: 3_000_000,
            max_tick_ns: 9_000_000,
            plugin_parked_commands: 2,
            plugin_active_commands: 1,
            ..HangGaugeSnapshot::default()
        };
        let rendered = snapshot.render();
        for key in [
            "tick_count=7",
            "last_tick_ms=3",
            "max_tick_ms=9",
            "plugin_parked=2",
            "plugin_active=1",
            "pending_input_bytes=",
            "cold_queue=",
            "plugin_events=",
            "plugin_side=",
            "component_events=",
            "panes=",
            "leaves=",
            "content_bytes=",
            "event_ring=",
            "pending_deliveries=",
            "leases=",
            "bindings=",
            "workers=",
            "input_commands=",
            "pending_exits=",
        ] {
            assert!(rendered.contains(key), "missing {key}: {rendered}");
        }
    }
}
