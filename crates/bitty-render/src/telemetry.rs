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

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// Number of tracked counters; must match the field order in
/// [`SurfaceTelemetrySnapshot`].
const COUNTER_SLOTS: usize = 12;

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
            last_event: "none",
        }
    }
}

static COUNTERS: [AtomicU64; COUNTER_SLOTS] = [const { AtomicU64::new(0) }; COUNTER_SLOTS];
static LAST_EVENT: AtomicU8 = AtomicU8::new(LastEvent::None as u8);

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

/// Copy the process-wide surface telemetry slot.
#[must_use]
pub fn snapshot() -> SurfaceTelemetrySnapshot {
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
        assert!(!after.last_event.is_empty());
    }

    #[test]
    fn every_event_has_a_stable_name() {
        for raw in 0u8..=12 {
            let event = LastEvent::from_u8(raw);
            assert_eq!(event as u8, raw);
            assert!(!event.as_str().is_empty());
        }
        assert_eq!(LastEvent::from_u8(u8::MAX), LastEvent::None);
    }
}
