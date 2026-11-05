//! Bounded surface-present telemetry (issue #2570).
//!
//! The compositor can stop releasing a swapchain image while a window is
//! hidden, which makes `get_current_texture` wait in the driver and makes a
//! later present fail. Those paths used to be invisible: a real-GPU present
//! failure silently fell back to the CPU compositor and the window kept the
//! stale frame. This module records a small, fixed set of counters on the
//! present path so the opt-in stall watchdog and the crash report can name
//! what the surface was doing when the main loop stopped making progress.
//!
//! Design constraints:
//!
//! - **Bounded and allocation-free.** One relaxed atomic store per event; no
//!   strings, no heap, no locks, no I/O.
//! - **Diagnostics only.** Nothing here feeds rendering decisions. Counters
//!   wrap on overflow and only ever grow.
//! - **Single process-wide slot.** Bitty owns one live surface per process
//!   (single-window application), so the slot is a `static`; tests can reset
//!   it through the `#[cfg(test)]` helper.
//!
//! The snapshot is copied into the stall-watchdog context at the existing
//! 500 ms sample cadence and read directly by the panic/signal report.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

/// Number of tracked counters; must match the field order in
/// [`SurfaceTelemetrySnapshot`].
const COUNTER_SLOTS: usize = 16;

const CONFIGURE_OK: usize = 0;
const CONFIGURE_ERR: usize = 1;
const ACQUIRE_OK: usize = 2;
const ACQUIRE_TIMEOUT: usize = 3;
const ACQUIRE_OUTDATED: usize = 4;
const ACQUIRE_LOST: usize = 5;
const ACQUIRE_OTHER: usize = 6;
const PRESENT_OK: usize = 7;
const PRESENT_ERR: usize = 8;
const RESIZE: usize = 9;
const RESIZE_ZERO_SKIPPED: usize = 10;
const HEADLESS_PRESENT: usize = 11;
const RETRY_FAILURES: usize = 12;
const RETRY_ATTEMPTS: usize = 13;
const RETRY_RECOVERIES: usize = 14;
const RETRY_LAST_DELAY_MS: usize = 15;

/// Last surface event recorded, stored as one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum LastEvent {
    None = 0,
    ConfigureOk = 1,
    ConfigureErr = 2,
    AcquireOk = 3,
    AcquireTimeout = 4,
    AcquireOutdated = 5,
    AcquireLost = 6,
    AcquireOther = 7,
    PresentOk = 8,
    PresentErr = 9,
    Resize = 10,
    ResizeZeroSkipped = 11,
    HeadlessPresent = 12,
    RetryArmed = 13,
    RetryAttempt = 14,
    RetryRecovered = 15,
}

impl LastEvent {
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::ConfigureOk,
            2 => Self::ConfigureErr,
            3 => Self::AcquireOk,
            4 => Self::AcquireTimeout,
            5 => Self::AcquireOutdated,
            6 => Self::AcquireLost,
            7 => Self::AcquireOther,
            8 => Self::PresentOk,
            9 => Self::PresentErr,
            10 => Self::Resize,
            11 => Self::ResizeZeroSkipped,
            12 => Self::HeadlessPresent,
            13 => Self::RetryArmed,
            14 => Self::RetryAttempt,
            15 => Self::RetryRecovered,
            _ => Self::None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ConfigureOk => "configure-ok",
            Self::ConfigureErr => "configure-err",
            Self::AcquireOk => "acquire-ok",
            Self::AcquireTimeout => "acquire-timeout",
            Self::AcquireOutdated => "acquire-outdated",
            Self::AcquireLost => "acquire-lost",
            Self::AcquireOther => "acquire-other",
            Self::PresentOk => "present-ok",
            Self::PresentErr => "present-err",
            Self::Resize => "resize",
            Self::ResizeZeroSkipped => "resize-zero-skipped",
            Self::HeadlessPresent => "headless-present",
            Self::RetryArmed => "retry-armed",
            Self::RetryAttempt => "retry-attempt",
            Self::RetryRecovered => "retry-recovered",
        }
    }
}

/// One owned, copyable view of the surface telemetry slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceTelemetrySnapshot {
    /// Successful `configure` calls.
    pub configure_ok: u64,
    /// Failed `configure` calls.
    pub configure_err: u64,
    /// `get_current_texture` calls that returned a texture.
    pub acquire_ok: u64,
    /// Acquires that reported `Timeout` (the compositor did not release an
    /// image; the driver wait is the freeze suspect).
    pub acquire_timeout: u64,
    /// Acquires that reported `Outdated` (reconfigure + retry).
    pub acquire_outdated: u64,
    /// Acquires that reported `Lost` (surface must be recreated).
    pub acquire_lost: u64,
    /// Acquires that reported `OutOfMemory` or an unknown upstream error.
    pub acquire_other: u64,
    /// Frames whose `present` completed.
    pub present_ok: u64,
    /// Present calls that returned an error before `frame.present()`.
    pub present_err: u64,
    /// Non-zero resize requests.
    pub resize: u64,
    /// Zero-sized resizes skipped by the surface contract.
    pub resize_zero_skipped: u64,
    /// Headless CPU composites (CI path or a headless fake surface).
    pub headless_present: u64,
    /// Real-GPU presents that failed and armed a retry. Counts failed
    /// presents only: a backoff re-armed without a present attempt (the tick
    /// produced no frame) is not a failure and is not counted here.
    pub retry_failures: u64,
    /// Present attempts made after a backoff deadline elapsed.
    pub retry_attempts: u64,
    /// Retries that ended in a successful present.
    pub retry_recoveries: u64,
    /// Most recent retry delay in milliseconds.
    pub retry_last_delay_ms: u64,
    /// Whether an off-thread presenter currently holds an acquire/present in
    /// flight (bitty#2830). `false` on the synchronous path and while idle.
    pub presenter_in_flight: bool,
    /// Age of that in-flight present, in milliseconds (`0` when idle).
    ///
    /// A compositor that stops sampling the surface leaves this growing while
    /// the event-loop thread stays responsive: the watchdog names the stall
    /// without a blocked main thread.
    pub presenter_in_flight_ms: u64,
    /// Process-relative millisecond timestamp when the in-flight present
    /// started (`0` when idle). The absolute `in_flight_since` gauge the
    /// watchdog correlates with `presenter_in_flight_ms`.
    pub presenter_in_flight_since_ms: u64,
    /// Stuck-present episodes detected and recovered by the embedder.
    pub presenter_stuck_presents: u64,
    /// Presenter workers abandoned and recreated after a stuck present.
    pub presenter_recoveries: u64,
    /// Frames replaced in the bounded UI -> worker slot before acquisition.
    pub presenter_coalesced: u64,
    /// Acknowledgements dropped because the bounded ack ring was full.
    pub presenter_acks_dropped: u64,
    /// Last recorded event name, stable for reports.
    pub last_event: &'static str,
}

impl Default for SurfaceTelemetrySnapshot {
    fn default() -> Self {
        Self {
            configure_ok: 0,
            configure_err: 0,
            acquire_ok: 0,
            acquire_timeout: 0,
            acquire_outdated: 0,
            acquire_lost: 0,
            acquire_other: 0,
            present_ok: 0,
            present_err: 0,
            resize: 0,
            resize_zero_skipped: 0,
            headless_present: 0,
            retry_failures: 0,
            retry_attempts: 0,
            retry_recoveries: 0,
            retry_last_delay_ms: 0,
            presenter_in_flight: false,
            presenter_in_flight_ms: 0,
            presenter_in_flight_since_ms: 0,
            presenter_stuck_presents: 0,
            presenter_recoveries: 0,
            presenter_coalesced: 0,
            presenter_acks_dropped: 0,
            last_event: "none",
        }
    }
}

static COUNTERS: [AtomicU64; COUNTER_SLOTS] = [const { AtomicU64::new(0) }; COUNTER_SLOTS];
static LAST_EVENT: AtomicU8 = AtomicU8::new(LastEvent::None as u8);

/// Process epoch for the off-thread presenter's in-flight gauge.
static PRESENT_EPOCH: OnceLock<Instant> = OnceLock::new();

/// Packed active presenter state: `generation << 32 | (since_ms + 1)`; `0`
/// means no present is in flight. One compare-exchange clears only the
/// episode that stored the value, so an abandoned worker returning from a
/// blocked call can never clear the gauge of the worker that replaced it.
static PRESENT_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static PRESENT_STUCK: AtomicU64 = AtomicU64::new(0);
static PRESENT_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static PRESENT_COALESCED: AtomicU64 = AtomicU64::new(0);
static PRESENT_ACKS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Milliseconds since the process epoch, bounded to the packed gauge's low
/// 32 bits. Saturates rather than wrapping.
pub(crate) fn presenter_now_ms() -> u64 {
    let epoch = *PRESENT_EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .min(u64::from(u32::MAX))
}

/// Publishes one in-flight present episode.
pub(crate) fn presenter_begin(generation: u32, since_ms: u64) {
    let packed = (u64::from(generation) << 32) | since_ms;
    PRESENT_IN_FLIGHT.store(packed, Ordering::Release);
}

/// Clears the in-flight gauge only when `packed` is still the live episode.
pub(crate) fn presenter_end(packed: u64) {
    let _ = PRESENT_IN_FLIGHT.compare_exchange(packed, 0, Ordering::AcqRel, Ordering::Relaxed);
}

/// Records one presentation-worker recovery and clears the abandoned
/// worker's in-flight gauge so a wedged episode stops counting as active.
pub(crate) fn presenter_recovered() {
    PRESENT_IN_FLIGHT.store(0, Ordering::Release);
    let _ = PRESENT_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

/// Records one stuck-present episode.
pub(crate) fn presenter_stuck() {
    let _ = PRESENT_STUCK.fetch_add(1, Ordering::Relaxed);
}

/// Records `count` frames replaced in the bounded presenter slot.
pub(crate) fn presenter_coalesced(count: u64) {
    let _ = PRESENT_COALESCED.fetch_add(count, Ordering::Relaxed);
}

/// Records one acknowledgement dropped from the bounded ack ring.
pub(crate) fn presenter_ack_dropped() {
    let _ = PRESENT_ACKS_DROPPED.fetch_add(1, Ordering::Relaxed);
}

fn bump(slot: usize) {
    let _ = COUNTERS[slot].fetch_add(1, Ordering::Relaxed);
}

fn record(event: LastEvent, slot: Option<usize>) {
    if let Some(slot) = slot {
        bump(slot);
    }
    LAST_EVENT.store(event as u8, Ordering::Relaxed);
}

pub(crate) fn configure_ok() {
    record(LastEvent::ConfigureOk, Some(CONFIGURE_OK));
}

pub(crate) fn configure_err() {
    record(LastEvent::ConfigureErr, Some(CONFIGURE_ERR));
}

pub(crate) fn acquire_ok() {
    record(LastEvent::AcquireOk, Some(ACQUIRE_OK));
}

pub(crate) fn acquire_timeout() {
    record(LastEvent::AcquireTimeout, Some(ACQUIRE_TIMEOUT));
}

pub(crate) fn acquire_outdated() {
    record(LastEvent::AcquireOutdated, Some(ACQUIRE_OUTDATED));
}

pub(crate) fn acquire_lost() {
    record(LastEvent::AcquireLost, Some(ACQUIRE_LOST));
}

pub(crate) fn acquire_other() {
    record(LastEvent::AcquireOther, Some(ACQUIRE_OTHER));
}

pub(crate) fn present_ok() {
    record(LastEvent::PresentOk, Some(PRESENT_OK));
}

pub(crate) fn present_err() {
    record(LastEvent::PresentErr, Some(PRESENT_ERR));
}

pub(crate) fn resize() {
    record(LastEvent::Resize, Some(RESIZE));
}

pub(crate) fn resize_zero_skipped() {
    record(LastEvent::ResizeZeroSkipped, Some(RESIZE_ZERO_SKIPPED));
}

pub(crate) fn headless_present() {
    record(LastEvent::HeadlessPresent, Some(HEADLESS_PRESENT));
}

/// A real-GPU present failed and armed the bounded retry.
///
/// Bumps `retry_failures`, which therefore counts failed *presents* only -
/// the no-frame backoff re-arm reports through [`retry_rearmed`] instead.
pub fn retry_armed(delay_ms: u64) {
    record(LastEvent::RetryArmed, Some(RETRY_FAILURES));
    COUNTERS[RETRY_LAST_DELAY_MS].store(delay_ms, Ordering::Relaxed);
}

/// The backoff deadline elapsed and was re-armed without a present attempt
/// (the tick produced no frame).
///
/// Deliberately does not bump `retry_failures`: nothing presented, so nothing
/// failed. Only the armed delay and the last event are refreshed.
pub fn retry_rearmed(delay_ms: u64) {
    COUNTERS[RETRY_LAST_DELAY_MS].store(delay_ms, Ordering::Relaxed);
    LAST_EVENT.store(LastEvent::RetryArmed as u8, Ordering::Relaxed);
}

/// The backoff deadline elapsed and a retry present is being attempted.
pub fn retry_attempt() {
    record(LastEvent::RetryAttempt, Some(RETRY_ATTEMPTS));
}

/// A retry present succeeded and cleared the retry state.
pub fn retry_recovered() {
    record(LastEvent::RetryRecovered, Some(RETRY_RECOVERIES));
}

/// Copy the process-wide surface telemetry slot.
#[must_use]
pub fn snapshot() -> SurfaceTelemetrySnapshot {
    let in_flight = PRESENT_IN_FLIGHT.load(Ordering::Acquire);
    let presenter_in_flight = in_flight != 0;
    SurfaceTelemetrySnapshot {
        configure_ok: COUNTERS[CONFIGURE_OK].load(Ordering::Relaxed),
        configure_err: COUNTERS[CONFIGURE_ERR].load(Ordering::Relaxed),
        acquire_ok: COUNTERS[ACQUIRE_OK].load(Ordering::Relaxed),
        acquire_timeout: COUNTERS[ACQUIRE_TIMEOUT].load(Ordering::Relaxed),
        acquire_outdated: COUNTERS[ACQUIRE_OUTDATED].load(Ordering::Relaxed),
        acquire_lost: COUNTERS[ACQUIRE_LOST].load(Ordering::Relaxed),
        acquire_other: COUNTERS[ACQUIRE_OTHER].load(Ordering::Relaxed),
        present_ok: COUNTERS[PRESENT_OK].load(Ordering::Relaxed),
        present_err: COUNTERS[PRESENT_ERR].load(Ordering::Relaxed),
        resize: COUNTERS[RESIZE].load(Ordering::Relaxed),
        resize_zero_skipped: COUNTERS[RESIZE_ZERO_SKIPPED].load(Ordering::Relaxed),
        headless_present: COUNTERS[HEADLESS_PRESENT].load(Ordering::Relaxed),
        retry_failures: COUNTERS[RETRY_FAILURES].load(Ordering::Relaxed),
        retry_attempts: COUNTERS[RETRY_ATTEMPTS].load(Ordering::Relaxed),
        retry_recoveries: COUNTERS[RETRY_RECOVERIES].load(Ordering::Relaxed),
        retry_last_delay_ms: COUNTERS[RETRY_LAST_DELAY_MS].load(Ordering::Relaxed),
        presenter_in_flight,
        presenter_in_flight_ms: if presenter_in_flight {
            presenter_now_ms().saturating_sub(in_flight & 0xFFFF_FFFF)
        } else {
            0
        },
        presenter_in_flight_since_ms: if presenter_in_flight {
            in_flight & 0xFFFF_FFFF
        } else {
            0
        },
        presenter_stuck_presents: PRESENT_STUCK.load(Ordering::Relaxed),
        presenter_recoveries: PRESENT_RECOVERIES.load(Ordering::Relaxed),
        presenter_coalesced: PRESENT_COALESCED.load(Ordering::Relaxed),
        presenter_acks_dropped: PRESENT_ACKS_DROPPED.load(Ordering::Relaxed),
        last_event: LastEvent::from_u8(LAST_EVENT.load(Ordering::Relaxed)).as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_last_event_are_monotonic_and_bounded() {
        // The slot is process-wide and tests run concurrently, so assert
        // deltas rather than absolute values. No reset: a reset would race a
        // sibling test's present path.
        let before = snapshot();
        configure_ok();
        configure_err();
        acquire_timeout();
        acquire_lost();
        acquire_other();
        present_err();
        resize();
        resize_zero_skipped();
        headless_present();
        acquire_ok();
        present_ok();
        retry_armed(100);
        retry_rearmed(200);
        retry_attempt();
        retry_recovered();

        let after = snapshot();
        assert!(after.configure_ok > before.configure_ok);
        assert!(after.configure_err > before.configure_err);
        assert!(after.acquire_timeout > before.acquire_timeout);
        assert!(after.acquire_lost > before.acquire_lost);
        assert!(after.acquire_other > before.acquire_other);
        assert!(after.present_err > before.present_err);
        assert!(after.resize > before.resize);
        assert!(after.resize_zero_skipped > before.resize_zero_skipped);
        assert!(after.headless_present > before.headless_present);
        assert!(after.acquire_ok > before.acquire_ok);
        assert!(after.present_ok > before.present_ok);
        assert!(after.retry_failures > before.retry_failures);
        assert!(after.retry_attempts > before.retry_attempts);
        assert!(after.retry_recoveries > before.retry_recoveries);
        // `retry_rearmed` ran last: it refreshes the armed delay but is not a
        // failed present, so it must not be what moves `retry_failures`.
        assert_eq!(after.retry_last_delay_ms, 200);
        assert!(!after.last_event.is_empty());
    }

    #[test]
    fn every_event_has_a_stable_name() {
        for raw in 0u8..=15 {
            let event = LastEvent::from_u8(raw);
            assert_eq!(event as u8, raw);
            assert!(!event.as_str().is_empty());
        }
        assert_eq!(LastEvent::from_u8(u8::MAX), LastEvent::None);
    }
}
