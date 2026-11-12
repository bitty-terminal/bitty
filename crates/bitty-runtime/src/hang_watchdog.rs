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
//! - [`crate::Runtime::poll_pty`] publishes the bounded PTY writer FIFO
//!   backlog (primary plus panes) as the `pending=pty` evidence.
//! - [`crate::plugin_runtime::PluginRuntime::dispatch_command`] publishes the
//!   one command currently executing in a plugin VM (the active-command
//!   guard), so a hung Lua callback is visible in the dump.
//!
//! The values are gauges, not event logs: a dump reads the latest published
//! sample. That makes the fields stale by at most one tick, which is the
//! correct trade for a diagnostic that must never block or allocate on the
//! hot path. The completed-tick ring is the deliberate exception: it retains
//! the last [`TICK_RING_CAPACITY`] `(sequence, duration)` records so the
//! monitor can report every over-bound tick, not only the latest one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering, fence};

/// Number of completed tick records retained for the hang watchdog.
///
/// The monitor drains the ring every poll (100 ms at the default bound), so
/// 256 slots cover a high-rate burst of ordinary frames after a slow tick.
/// Records overwritten before a drain are surfaced as a [`TickGap`] instead of
/// being dropped silently.
pub const TICK_RING_CAPACITY: usize = 256;

/// One completed tick retained in the bounded ring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickRecord {
    /// Monotonic tick sequence (1-based).
    pub seq: u64,
    /// Measured duration of that tick, in nanoseconds.
    pub duration_ns: u64,
}

/// A sequence span containing records overwritten before the monitor drained
/// them.
///
/// The span names the *missing* sequences, not the retained ones, so the next
/// report can state that evidence existed but was lost to ring overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickGap {
    /// First lost sequence number (inclusive).
    pub from: u64,
    /// Last lost sequence number (inclusive).
    pub to: u64,
}

impl TickGap {
    /// Length of the span.
    ///
    /// A span merged from two gaps includes any retained sequences between
    /// them, so this is an upper bound on the lost entries.
    #[must_use]
    pub const fn missed(self) -> u64 {
        self.to.saturating_sub(self.from).saturating_add(1)
    }

    /// Widest span covering both gaps.
    pub(crate) const fn merged(self, other: Self) -> Self {
        Self {
            from: if self.from < other.from {
                self.from
            } else {
                other.from
            },
            to: if self.to > other.to {
                self.to
            } else {
                other.to
            },
        }
    }
}

/// Bounded, allocation-free drain of completed ticks since a sequence.
///
/// Records are oldest first. A record is returned only when its slot's
/// seqlock stamp still matches the expected sequence *before and after* the
/// duration read, so a slot overwritten by a newer tick is rejected and named
/// as a gap instead of pairing a stale sequence with a newer duration.
#[derive(Clone, Debug)]
pub struct TickRingDrain {
    records: [TickRecord; TICK_RING_CAPACITY],
    len: usize,
    last_seq: u64,
    gap: Option<TickGap>,
}

impl Default for TickRingDrain {
    fn default() -> Self {
        Self {
            records: [TickRecord::default(); TICK_RING_CAPACITY],
            len: 0,
            last_seq: 0,
            gap: None,
        }
    }
}

impl TickRingDrain {
    /// Retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> &[TickRecord] {
        &self.records[..self.len]
    }

    /// Highest sequence the ring could publish at drain time.
    ///
    /// Every sequence up to this value was either returned, named by
    /// [`Self::gap`], or is the caller's `last_seen` watermark.
    #[must_use]
    pub const fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Span containing sequences overwritten before this drain, when any.
    #[must_use]
    pub const fn gap(&self) -> Option<TickGap> {
        self.gap
    }

    fn push(&mut self, record: TickRecord) {
        if self.len < TICK_RING_CAPACITY {
            self.records[self.len] = record;
            self.len += 1;
        }
    }

    fn note_gap(&mut self, seq: u64) {
        let gap = TickGap { from: seq, to: seq };
        self.gap = Some(match self.gap {
            Some(existing) => existing.merged(gap),
            None => gap,
        });
    }
}

/// One ring slot.
///
/// `stamp` is a seqlock word: `(seq << 1) | writing`. The producer stores the
/// odd "writing" stamp with `Release`, fences, stores `duration_ns`, and
/// stores the even sequence stamp with `Release`; the fences are what order
/// the duration on weak-memory targets. A reader that races a rewrite sees
/// either an odd stamp or a changed stamp and rejects the pair. The logical
/// sequence is `stamp >> 1`.
#[derive(Debug, Default)]
struct TickRingSlot {
    stamp: AtomicU64,
    duration_ns: AtomicU64,
}

#[cfg(feature = "hang-gauge-test-support")]
thread_local! {
    /// One-shot block consumed by the next `Runtime::tick` on this thread.
    static TEST_TICK_BLOCK: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Test-support: blocks the next `Runtime::tick` on this thread inside the
/// measured region, after `tick_started` is taken.
///
/// The live freeze is a compositor fence wait inside `runtime.tick()`; this
/// seam reproduces its effect on the measured duration without a display.
/// Test builds only.
#[cfg(feature = "hang-gauge-test-support")]
pub fn set_tick_block_for_test(block: std::time::Duration) {
    TEST_TICK_BLOCK.with(|slot| slot.set(Some(block)));
}

/// Test-support: consumes the one-shot tick block for this thread.
#[cfg(feature = "hang-gauge-test-support")]
pub(crate) fn take_tick_block_for_test() -> Option<std::time::Duration> {
    TEST_TICK_BLOCK.with(std::cell::Cell::take)
}

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
    /// Bytes accepted by the bounded PTY writer FIFOs and not yet written
    /// (primary plus every pane). Rendered as `pending=pty`.
    pub pending_input_bytes: u64,
    /// Bytes dropped from the headless input observation buffer.
    ///
    /// Production runs write to the queued `PtyWriter`; this counter is only
    /// nonzero in headless test harnesses that own no writer.
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
    /// Bytes of PTY replies queued and not yet answered (bitty#2830).
    ///
    /// A child query (DSR/DA1/XTWINOPS/OSC) that the surface never answers
    /// leaves its reply here; the watchdog names it as `pending=replies`.
    pub pending_replies: u64,
    /// Whether the surface-visibility gate is closed while retained work is
    /// owed (`1`/`0`, bitty#2830 hypothesis 2).
    ///
    /// This is the occlusion/reveal stall shape: the loop keeps parsing output
    /// but never presents, so the phase heartbeat alone calls it healthy.
    pub hidden_damage: u64,
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
input_commands={} input_commands_dropped={} pending_exits={} pending_replies={} \
hidden_damage={}",
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
            self.pending_replies,
            self.hidden_damage,
        )
    }
}

#[derive(Default)]
struct GaugeWaker(std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>);

impl std::fmt::Debug for GaugeWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let armed = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        f.debug_tuple("GaugeWaker").field(&armed).finish()
    }
}

/// Atomic gauges shared by the runtime owner thread and the watchdog monitor.
///
/// `Relaxed` ordering is deliberate for the display gauges: each field is an
/// independent monotonic sample and a dump never needs a coherent cross-field
/// snapshot. A torn reading would only misattribute one queue depth by one
/// tick, while `Acquire`/`Release` would add fences to the per-frame path for
/// no diagnostic gain.
///
/// The completed-tick ring is the exception: `finish_tick` publishes a slot
/// with `Release` before the counter that announces the sequence, so the
/// monitor never pairs a sequence with another tick's duration. See
/// [`Self::drain_ticks_since`].
#[derive(Debug)]
pub struct HangGauges {
    tick_count: AtomicU64,
    last_tick_ns: AtomicU64,
    max_tick_ns: AtomicU64,
    tick_ring: [TickRingSlot; TICK_RING_CAPACITY],
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
    pending_replies: AtomicU64,
    hidden_damage: AtomicU64,
    waker: GaugeWaker,
}

impl Default for HangGauges {
    fn default() -> Self {
        Self {
            tick_count: AtomicU64::new(0),
            last_tick_ns: AtomicU64::new(0),
            max_tick_ns: AtomicU64::new(0),
            tick_ring: std::array::from_fn(|_| TickRingSlot::default()),
            pending_input_bytes: AtomicU64::new(0),
            pending_input_dropped: AtomicU64::new(0),
            cold_queue_len: AtomicU64::new(0),
            cold_queue_dropped: AtomicU64::new(0),
            plugin_events_queued: AtomicU64::new(0),
            plugin_events_bytes: AtomicU64::new(0),
            plugin_side_queued: AtomicU64::new(0),
            plugin_side_dropped: AtomicU64::new(0),
            plugin_parked_commands: AtomicU64::new(0),
            plugin_active_commands: AtomicU64::new(0),
            plugin_workers: AtomicU64::new(0),
            component_events: AtomicU64::new(0),
            component_events_dropped: AtomicU64::new(0),
            pane_count: AtomicU64::new(0),
            leaf_count: AtomicU64::new(0),
            content_bytes: AtomicU64::new(0),
            panel_leases: AtomicU64::new(0),
            band_bindings: AtomicU64::new(0),
            input_commands: AtomicU64::new(0),
            input_commands_dropped: AtomicU64::new(0),
            pending_exits: AtomicU64::new(0),
            pending_replies: AtomicU64::new(0),
            hidden_damage: AtomicU64::new(0),
            waker: GaugeWaker::default(),
        }
    }
}

impl HangGauges {
    /// Creates a zeroed gauge set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs or clears the wake callback invoked when a tick completes or
    /// pending work is armed while the watchdog monitor is parked at 0 Hz.
    pub fn set_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
        let mut slot = self
            .waker
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = waker;
    }

    fn notify_waker(&self) {
        let waker = self
            .waker
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(wake) = waker {
            wake();
        }
    }

    /// Copies every display gauge for one bounded report.
    ///
    /// `tick_count`, `last_tick_ns`, and `max_tick_ns` are report-body values
    /// here. Detection of a completed over-bound tick must go through
    /// [`Self::drain_ticks_since`]: the snapshot's counter and last duration
    /// are independent relaxed reads and are not a pair.
    #[must_use]
    pub fn snapshot(&self) -> HangGaugeSnapshot {
        HangGaugeSnapshot {
            tick_count: self.tick_count.load(Ordering::Acquire),
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
            pending_replies: self.pending_replies.load(Ordering::Relaxed),
            hidden_damage: self.hidden_damage.load(Ordering::Relaxed),
        }
    }

    /// Records one completed tick.
    ///
    /// Caller contract: exactly one producer (the runtime owner thread) calls
    /// this; readers never write. The slot uses a seqlock: the odd "writing"
    /// stamp is stored with `Release`, a `Release` fence orders it before the
    /// duration, the even sequence stamp is stored with `Release` (ordering
    /// the duration before it), and only then `tick_count` announces the
    /// sequence with `Release`.
    ///
    /// The fences are load-bearing on weak-memory targets (macOS ARM64 is
    /// shipped): a `Release` store orders *prior* operations, not the
    /// following duration store, so without the first fence the duration
    /// could become visible before the odd stamp. A reader that observes
    /// sequence `S` in the counter therefore observes the completed slot `S`,
    /// and a reader that races a rewrite rejects the pair and surfaces a
    /// [`TickGap`] instead of pairing a sequence with another tick's duration.
    ///
    /// `last_tick_ns`, `max_tick_ns`, and `tick_count` remain display values
    /// for the report body; per-tick decisions use [`Self::drain_ticks_since`].
    pub(crate) fn finish_tick(&self, elapsed_ns: u64) {
        let seq = self.tick_count.load(Ordering::Relaxed).saturating_add(1);
        let slot = &self.tick_ring[(seq % TICK_RING_CAPACITY as u64) as usize];
        let stamp = seq << 1;
        slot.stamp.store(stamp | 1, Ordering::Release);
        // Release store orders prior operations; the fence makes the odd
        // stamp visible before the duration on weak-memory targets.
        fence(Ordering::Release);
        slot.duration_ns.store(elapsed_ns, Ordering::Relaxed);
        // The Release store of the even stamp orders the duration before it.
        slot.stamp.store(stamp, Ordering::Release);
        self.last_tick_ns.store(elapsed_ns, Ordering::Relaxed);
        self.max_tick_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        self.tick_count.store(seq, Ordering::Release);
        self.notify_waker();
    }

    /// Drains completed ticks with a sequence greater than `last_seen`.
    ///
    /// Oldest first, at most [`TICK_RING_CAPACITY`] records and allocation
    /// free. Every sequence between `last_seen + 1` and [`TickRingDrain::last_seq`]
    /// is either returned or covered by [`TickRingDrain::gap`]; the caller
    /// never silently loses a retained tick. A duration is accepted only when
    /// the slot's sequence stamp still matches both before and after the read.
    /// The reader's `Acquire` fence between the duration load and the second
    /// stamp load is load-bearing: an `Acquire` load orders later operations,
    /// not the following plain load, so without the fence a rewrite could slip
    /// between the duration read and the check and pair a newer duration with
    /// the old sequence. A rewritten slot is rejected and surfaced as a gap.
    #[must_use]
    pub fn drain_ticks_since(&self, last_seen: u64) -> TickRingDrain {
        let mut drain = TickRingDrain {
            last_seq: last_seen,
            ..TickRingDrain::default()
        };
        let total = self.tick_count.load(Ordering::Acquire);
        if total <= last_seen {
            return drain;
        }
        let oldest = total
            .saturating_sub(TICK_RING_CAPACITY as u64)
            .saturating_add(1);
        let start = last_seen.saturating_add(1).max(oldest);
        if start > last_seen.saturating_add(1) {
            drain.gap = Some(TickGap {
                from: last_seen.saturating_add(1),
                to: start.saturating_sub(1),
            });
        }
        for seq in start..=total {
            let slot = &self.tick_ring[(seq % TICK_RING_CAPACITY as u64) as usize];
            let stamp = seq << 1;
            let before = slot.stamp.load(Ordering::Acquire);
            if before & 1 == 1 || before != stamp {
                // Overwritten by a newer tick, or the producer is mid-write.
                drain.note_gap(seq);
                continue;
            }
            let duration_ns = slot.duration_ns.load(Ordering::Relaxed);
            // Make the duration load visible before the second stamp check;
            // Acquire alone on `before` does not order the following load.
            fence(Ordering::Acquire);
            let after = slot.stamp.load(Ordering::Acquire);
            if after != before {
                drain.note_gap(seq);
                continue;
            }
            drain.push(TickRecord { seq, duration_ns });
        }
        drain.last_seq = total;
        drain
    }

    /// Test-support: records one completed tick without a live runtime.
    ///
    /// The runtime's own tick path records the same way; this seam lets the
    /// watchdog regressions seed a completed over-bound tick deterministically.
    #[cfg(feature = "hang-gauge-test-support")]
    pub fn record_tick_for_test(&self, duration: std::time::Duration) {
        self.finish_tick(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    pub(crate) fn set_pending_input(&self, bytes: usize, dropped: u64) {
        self.pending_input_bytes
            .store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.pending_input_dropped.store(dropped, Ordering::Relaxed);
        if bytes > 0 {
            self.notify_waker();
        }
    }

    pub(crate) fn set_cold_queue(&self, len: usize, dropped: u64) {
        self.cold_queue_len
            .store(u64::try_from(len).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.cold_queue_dropped.store(dropped, Ordering::Relaxed);
        if len > 0 {
            self.notify_waker();
        }
    }

    pub(crate) fn set_plugin_pipeline(&self, events: usize, bytes: usize) {
        self.plugin_events_queued
            .store(u64::try_from(events).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.plugin_events_bytes
            .store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        if events > 0 {
            self.notify_waker();
        }
    }

    pub(crate) fn set_plugin_side(&self, queued: usize, dropped: u64) {
        self.plugin_side_queued
            .store(u64::try_from(queued).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.plugin_side_dropped.store(dropped, Ordering::Relaxed);
        if queued > 0 {
            self.notify_waker();
        }
    }

    pub(crate) fn set_plugin_parked(&self, parked: usize) {
        self.plugin_parked_commands
            .store(u64::try_from(parked).unwrap_or(u64::MAX), Ordering::Relaxed);
        if parked > 0 {
            self.notify_waker();
        }
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
        if queued > 0 {
            self.notify_waker();
        }
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
        if queued > 0 {
            self.notify_waker();
        }
    }

    /// Publishes the app-owned pending-exit edge count.
    pub fn set_pending_exits(&self, pending: usize) {
        self.pending_exits.store(
            u64::try_from(pending).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if pending > 0 {
            self.notify_waker();
        }
    }

    /// Publishes the bytes of PTY replies queued and not yet answered.
    ///
    /// Written by the runtime owner thread on every tick; public so a
    /// composition root or a diagnostic test can seed the same gauge.
    pub fn set_pending_replies(&self, bytes: usize) {
        self.pending_replies
            .store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        if bytes > 0 {
            self.notify_waker();
        }
    }

    /// Publishes whether a hidden surface retains work (bitty#2830).
    ///
    /// Written by the runtime owner thread on every tick; public so the
    /// watchdog's no-present regression can seed the occlusion shape.
    pub fn set_hidden_damage(&self, retained: bool) {
        self.hidden_damage
            .store(u64::from(retained), Ordering::Relaxed);
        if retained {
            self.notify_waker();
        }
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

    /// Every completed tick is retained for its own report, oldest first, and
    /// a drain from the watermark never repeats one.
    #[test]
    fn completed_ticks_drain_oldest_first_without_a_gap() {
        let gauges = HangGauges::new();
        gauges.finish_tick(1_000);
        gauges.finish_tick(17_490_000_000);
        let drain = gauges.drain_ticks_since(0);
        assert_eq!(
            drain.records(),
            &[
                TickRecord {
                    seq: 1,
                    duration_ns: 1_000,
                },
                TickRecord {
                    seq: 2,
                    duration_ns: 17_490_000_000,
                },
            ]
        );
        assert_eq!(drain.last_seq(), 2);
        assert_eq!(drain.gap(), None);
        let empty = gauges.drain_ticks_since(drain.last_seq());
        assert!(empty.records().is_empty());
        assert_eq!(empty.last_seq(), 2);
        assert_eq!(empty.gap(), None);
    }

    /// A reader that falls behind by more than the ring capacity loses the
    /// oldest records, but the loss is named by a contiguous gap span.
    #[test]
    fn completed_tick_ring_names_the_overwritten_span() {
        let gauges = HangGauges::new();
        let total = (TICK_RING_CAPACITY as u64) + 4;
        for seq in 1..=total {
            gauges.finish_tick(seq * 1_000);
        }
        let drain = gauges.drain_ticks_since(0);
        assert_eq!(drain.gap(), Some(TickGap { from: 1, to: 4 }));
        assert_eq!(drain.gap().expect("gap").missed(), 4);
        let seqs: Vec<u64> = drain.records().iter().map(|record| record.seq).collect();
        assert_eq!(seqs, (5..=total).collect::<Vec<_>>());
        assert_eq!(drain.last_seq(), total);
    }

    /// The ring keeps the `(seq, duration)` pair intact while a producer
    /// advances: no duplicate sequence, no out-of-order record, and every
    /// accepted duration belongs to the sequence it is reported under.
    #[test]
    fn completed_tick_ring_stays_paired_under_a_concurrent_producer() {
        const TICKS: u64 = 5_000;
        let gauges = Arc::new(HangGauges::new());
        let producer = {
            let gauges = Arc::clone(&gauges);
            std::thread::spawn(move || {
                for seq in 1..=TICKS {
                    gauges.finish_tick(seq * 1_000);
                }
            })
        };
        let mut seen = std::collections::HashSet::new();
        let mut last_seq = 0u64;
        while last_seq < TICKS {
            let drain = gauges.drain_ticks_since(last_seq);
            let mut previous = last_seq;
            for record in drain.records() {
                assert!(record.seq > previous, "records must be oldest-first");
                assert_eq!(
                    record.duration_ns,
                    record.seq * 1_000,
                    "a sequence must never pair with another tick's duration"
                );
                assert!(
                    seen.insert(record.seq),
                    "sequence {} drained twice",
                    record.seq
                );
                previous = record.seq;
            }
            last_seq = drain.last_seq().max(last_seq);
            if last_seq < TICKS {
                std::thread::yield_now();
            }
        }
        producer.join().expect("producer thread");
        assert!(!seen.is_empty());
        assert!(seen.len() <= TICKS as usize);
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
            "pending_replies=",
            "hidden_damage=",
        ] {
            assert!(rendered.contains(key), "missing {key}: {rendered}");
        }
    }
}
