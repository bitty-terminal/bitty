#![forbid(unsafe_code)]
//! `SystemMetricsService`: Platform Core-owned sampler for status metrics.
//!
//! Contract source: the draft Status System Specification. The service is
//! the **only** v1 mechanism producing `cpu`, `memory`, and `network`
//! values for status modules. Lua code, including Provider status modules,
//! must never read `/proc`, `/sys`, or platform-specific metric files
//! directly; such reads stay outside the allowed standard-library subset
//! and fail capability checks.
//!
//! Design rules pinned here:
//!
//! - **Single sampler**: `cpu`, `memory`, and `network` share one service
//!   instance; a second sampler is a schema violation, not an option.
//! - **Rate floor**: sampling enforces [`MIN_SAMPLE_INTERVAL_MS`] per
//!   metric regardless of how many modules request it; bursty configuration
//!   cannot drive tighter polling.
//! - **Error isolation**: one failing metric never poisons the others; each
//!   metric keeps independent `Ok(cached) | held` state.
//! - **Last-sample-held**: failures and skipped ticks reuse the cached
//!   snapshot; a metric never sampled renders as missing upstream.
//! - **Testability**: the OS adapter ([`MetricsAdapter`]) and the clock
//!   ([`MonotonicClock`]) are injected, so headless tests assert the floor,
//!   isolation, and hold behavior without hardware or wall-clock time.
//!
//! This module performs no filesystem, network, or PTY access itself and
//! holds no window handle; platform adapters plug in behind the trait.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Minimum sampling interval per metric (1 Hz ceiling).
pub const MIN_SAMPLE_INTERVAL_MS: u64 = 1000;

/// Default sampling interval per metric.
pub const DEFAULT_SAMPLE_INTERVAL_MS: u64 = 2000;

// ---------------------------------------------------------------------------
// Snapshot and traits
// ---------------------------------------------------------------------------

/// Cached metric values shared with every status module.
/// Each field is independent: `None` means never successfully sampled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricsSnapshot {
    /// CPU usage percent, clamped to `[0, 100]`.
    pub cpu_percent: Option<f32>,
    /// Memory usage percent, clamped to `[0, 100]`.
    pub memory_percent: Option<f32>,
    /// Received bytes counter (delta computed by the consumer).
    pub network_rx_bytes: Option<u64>,
    /// Transmitted bytes counter (delta computed by the consumer).
    pub network_tx_bytes: Option<u64>,
}

impl MetricsSnapshot {
    /// Whether no metric has ever been sampled successfully.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cpu_percent.is_none()
            && self.memory_percent.is_none()
            && self.network_rx_bytes.is_none()
            && self.network_tx_bytes.is_none()
    }

    /// One-line cold-path summary for diagnostics (never on a hot path).
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "cpu={} mem={} rx={} tx={}",
            self.cpu_percent
                .map_or_else(|| "—".to_string(), |value| format!("{value:.1}%")),
            self.memory_percent
                .map_or_else(|| "—".to_string(), |value| format!("{value:.1}%")),
            self.network_rx_bytes
                .map_or_else(|| "—".to_string(), |value| value.to_string()),
            self.network_tx_bytes
                .map_or_else(|| "—".to_string(), |value| value.to_string()),
        );
        out
    }
}

/// OS metric source behind the service. Implementations call the platform
/// adapter (never `/proc` from Lua); `None` reports per-metric failure.
pub trait MetricsAdapter {
    /// Samples CPU usage percent. `None` on failure.
    fn sample_cpu(&mut self) -> Option<f32>;
    /// Samples memory usage percent. `None` on failure.
    fn sample_memory(&mut self) -> Option<f32>;
    /// Samples `(rx_bytes, tx_bytes)` counters. `None` on failure.
    fn sample_network(&mut self) -> Option<(u64, u64)>;
}

/// Injected monotonic clock (milliseconds) for cadence control.
pub trait MonotonicClock {
    /// Current time in milliseconds.
    fn now_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Platform Core-owned metrics sampler shared by every status module.
#[derive(Debug)]
pub struct SystemMetricsService {
    interval_ms: u64,
    last_sample_ms: Option<u64>,
    cached: MetricsSnapshot,
}

impl SystemMetricsService {
    /// Creates the service with the default sampling interval.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interval_ms: DEFAULT_SAMPLE_INTERVAL_MS,
            last_sample_ms: None,
            cached: MetricsSnapshot::default(),
        }
    }

    /// Creates the service with a custom interval, clamped up to the floor.
    /// Configuration can never request sub-second polling.
    #[must_use]
    pub fn with_interval(requested_ms: u64) -> Self {
        Self {
            interval_ms: requested_ms.max(MIN_SAMPLE_INTERVAL_MS),
            last_sample_ms: None,
            cached: MetricsSnapshot::default(),
        }
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Last successfully held snapshot (shared by all callers).
    #[must_use]
    pub fn cached(&self) -> &MetricsSnapshot {
        &self.cached
    }

    /// Polls the adapter when the interval floor has elapsed, otherwise
    /// returns the held snapshot. Each metric updates independently: a
    /// `None` sample keeps that metric's previous value.
    pub fn poll(
        &mut self,
        clock: &dyn MonotonicClock,
        adapter: &mut dyn MetricsAdapter,
    ) -> &MetricsSnapshot {
        let now = clock.now_ms();
        let due = self
            .last_sample_ms
            .is_none_or(|last| now.wrapping_sub(last) >= self.interval_ms);
        if due {
            if let Some(cpu) = adapter.sample_cpu().and_then(finite_percent) {
                self.cached.cpu_percent = Some(cpu);
            }
            if let Some(memory) = adapter.sample_memory().and_then(finite_percent) {
                self.cached.memory_percent = Some(memory);
            }
            if let Some((rx, tx)) = adapter.sample_network() {
                self.cached.network_rx_bytes = Some(rx);
                self.cached.network_tx_bytes = Some(tx);
            }
            self.last_sample_ms = Some(now);
        }
        &self.cached
    }
}

impl Default for SystemMetricsService {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps finite percents clamped to `[0, 100]`; drops NaN/infinite noise.
fn finite_percent(value: f32) -> Option<f32> {
    if value.is_finite() {
        Some(value.clamp(0.0, 100.0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock {
        now: u64,
    }

    impl MonotonicClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    struct FakeAdapter {
        cpu: Option<f32>,
        memory: Option<f32>,
        network: Option<(u64, u64)>,
        calls: usize,
    }

    impl MetricsAdapter for FakeAdapter {
        fn sample_cpu(&mut self) -> Option<f32> {
            self.calls += 1;
            self.cpu
        }

        fn sample_memory(&mut self) -> Option<f32> {
            self.calls += 1;
            self.memory
        }

        fn sample_network(&mut self) -> Option<(u64, u64)> {
            self.calls += 1;
            self.network
        }
    }

    fn healthy() -> FakeAdapter {
        FakeAdapter {
            cpu: Some(12.0),
            memory: Some(34.0),
            network: Some((1000, 2000)),
            calls: 0,
        }
    }

    #[test]
    fn interval_clamps_up_to_floor() {
        assert_eq!(
            SystemMetricsService::with_interval(100).interval_ms(),
            MIN_SAMPLE_INTERVAL_MS
        );
        assert_eq!(
            SystemMetricsService::with_interval(5000).interval_ms(),
            5000
        );
        assert_eq!(
            SystemMetricsService::new().interval_ms(),
            DEFAULT_SAMPLE_INTERVAL_MS
        );
    }

    #[test]
    fn bursty_polls_share_one_sample() {
        let mut service = SystemMetricsService::new();
        let clock = FakeClock { now: 10_000 };
        let mut adapter = healthy();
        service.poll(&clock, &mut adapter);
        assert_eq!(adapter.calls, 3);
        // Second poll inside the floor performs no adapter calls.
        service.poll(&clock, &mut adapter);
        assert_eq!(adapter.calls, 3);
        assert_eq!(service.cached().cpu_percent, Some(12.0));
    }

    #[test]
    fn resamples_after_interval_elapses() {
        let mut service = SystemMetricsService::new();
        let mut adapter = healthy();
        service.poll(&FakeClock { now: 0 }, &mut adapter);
        let mut adapter2 = FakeAdapter {
            cpu: Some(50.0),
            memory: Some(60.0),
            network: Some((3000, 4000)),
            calls: 0,
        };
        service.poll(
            &FakeClock {
                now: DEFAULT_SAMPLE_INTERVAL_MS,
            },
            &mut adapter2,
        );
        assert_eq!(adapter2.calls, 3);
        assert_eq!(service.cached().cpu_percent, Some(50.0));
        assert_eq!(service.cached().network_rx_bytes, Some(3000));
    }

    #[test]
    fn metric_failure_is_isolated_and_held() {
        let mut service = SystemMetricsService::new();
        let mut adapter = healthy();
        service.poll(&FakeClock { now: 0 }, &mut adapter);
        // CPU fails on the next tick; memory and network still advance.
        let mut failing = FakeAdapter {
            cpu: None,
            memory: Some(70.0),
            network: Some((5000, 6000)),
            calls: 0,
        };
        service.poll(
            &FakeClock {
                now: DEFAULT_SAMPLE_INTERVAL_MS,
            },
            &mut failing,
        );
        assert_eq!(service.cached().cpu_percent, Some(12.0));
        assert_eq!(service.cached().memory_percent, Some(70.0));
        assert_eq!(service.cached().network_tx_bytes, Some(6000));
    }

    #[test]
    fn never_sampled_snapshot_is_empty() {
        let service = SystemMetricsService::new();
        assert!(service.cached().is_empty());
        assert!(service.cached().summary().contains('—'));
    }

    #[test]
    fn non_finite_percents_are_dropped() {
        let mut service = SystemMetricsService::new();
        let mut adapter = FakeAdapter {
            cpu: Some(f32::NAN),
            memory: Some(140.0),
            network: None,
            calls: 0,
        };
        service.poll(&FakeClock { now: 0 }, &mut adapter);
        assert_eq!(service.cached().cpu_percent, None);
        assert_eq!(service.cached().memory_percent, Some(100.0));
    }
}
