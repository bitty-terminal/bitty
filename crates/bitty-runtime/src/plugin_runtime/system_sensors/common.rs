//! Shared rate, staleness, and grant plumbing for Phase B sensor providers.
//!
//! Every sensor provider (cpu, memory, battery) is a thin parser over an
//! injected byte source plus the policy enforced here:
//!
//! - **Provider-side ceiling**: [`RateGate`] coalesces over-polling. Bursty
//!   bar ticks share one sample per ceiling window; a failed sample still
//!   advances the gate so a dead source can never drive a polling storm.
//! - **Stale-then-hide**: [`StalePolicy`] serves the last good snapshot with
//!   a growing `stale_age_ms` while the outage is transient, then hides
//!   (`None`) past the horizon. Nothing here fabricates zeros.
//! - **Fail-closed grants**: providers take an explicit `granted: bool`.
//!   An ungranted call returns `None` without touching the source, which is
//!   observationally identical to an absent source.
//!
//! Time is passed in as `now_ms` (milliseconds on a monotonic clock owned by
//! the caller) so every policy above is hermetically testable without
//! wall-clock sleeps. Durations use `u64` saturating/wrapping arithmetic so
//! clock wrap is fail-closed (a wrapped `now` reads as fresh, never as a
//! huge stale age).

/// Provider-side poll ceiling for fast-moving sensors (cpu, memory).
pub const FAST_SENSOR_CEILING_MS: u64 = 2000;

/// Provider-side poll ceiling for slow-moving sensors (battery).
pub const SLOW_SENSOR_CEILING_MS: u64 = 5000;

/// Stale-then-hide horizon expressed as a multiple of the ceiling.
///
/// Three missed windows hide the module: long enough to ride out transient
/// source blips (container `/proc` remounts, suspend/resume), short enough
/// that a dead source never parades stale numbers as live.
pub const STALE_HIDE_WINDOWS: u64 = 3;

/// Coalesces bursty polls onto one sample per ceiling window.
///
/// The gate advances on every due evaluation (success or failure) so a
/// failing source keeps its ceiling instead of busy-retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateGate {
    ceiling_ms: u64,
    last_attempt_ms: Option<u64>,
}

impl RateGate {
    /// Create a gate enforcing `ceiling_ms` between source attempts.
    #[must_use]
    pub fn new(ceiling_ms: u64) -> Self {
        Self {
            ceiling_ms,
            last_attempt_ms: None,
        }
    }

    /// Ceiling in milliseconds.
    #[must_use]
    pub fn ceiling_ms(&self) -> u64 {
        self.ceiling_ms
    }

    /// Whether a fresh source attempt is due at `now_ms`.
    ///
    /// The first poll is always due. Wrapping subtraction keeps a wrapped
    /// clock fail-closed (reads as fresh, never as hugely stale).
    #[must_use]
    pub fn due(&self, now_ms: u64) -> bool {
        self.last_attempt_ms
            .is_none_or(|last| now_ms.wrapping_sub(last) >= self.ceiling_ms)
    }

    /// Record one source attempt at `now_ms` (success or failure).
    pub fn mark_attempt(&mut self, now_ms: u64) {
        self.last_attempt_ms = Some(now_ms);
    }
}

/// Stale-then-hide horizon for one sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalePolicy {
    hide_after_ms: u64,
}

impl StalePolicy {
    /// Create a policy hiding snapshots older than `hide_after_ms`.
    #[must_use]
    pub fn new(hide_after_ms: u64) -> Self {
        Self { hide_after_ms }
    }

    /// Horizon in milliseconds.
    #[must_use]
    pub fn hide_after_ms(&self) -> u64 {
        self.hide_after_ms
    }

    /// Stale age of a snapshot taken at `sampled_ms` as observed at `now_ms`.
    #[must_use]
    pub fn age_ms(sampled_ms: u64, now_ms: u64) -> u64 {
        now_ms.wrapping_sub(sampled_ms)
    }

    /// Whether a snapshot of this age still renders (`true`) or hides.
    ///
    /// The boundary itself still renders; only strictly older snapshots
    /// hide, so a sample taken exactly on the horizon never flickers.
    #[must_use]
    pub fn visible(&self, stale_age_ms: u64) -> bool {
        stale_age_ms <= self.hide_after_ms
    }
}

/// Horizon for a ceiling: [`STALE_HIDE_WINDOWS`] missed windows.
#[must_use]
pub fn horizon_for_ceiling(ceiling_ms: u64) -> u64 {
    ceiling_ms.saturating_mul(STALE_HIDE_WINDOWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_poll_is_always_due() {
        let gate = RateGate::new(FAST_SENSOR_CEILING_MS);
        assert!(gate.due(0));
        assert!(gate.due(u64::MAX));
    }

    #[test]
    fn burst_within_ceiling_coalesces() {
        let mut gate = RateGate::new(2000);
        assert!(gate.due(10_000));
        gate.mark_attempt(10_000);
        assert!(!gate.due(10_000));
        assert!(!gate.due(11_999));
        assert!(gate.due(12_000));
    }

    #[test]
    fn failed_attempts_keep_their_ceiling() {
        let mut gate = RateGate::new(2000);
        gate.mark_attempt(0);
        assert!(!gate.due(1999));
        assert!(gate.due(2000));
    }

    #[test]
    fn stale_boundary_renders_past_it_hides() {
        let policy = StalePolicy::new(6000);
        assert!(policy.visible(0));
        assert!(policy.visible(6000));
        assert!(!policy.visible(6001));
    }

    #[test]
    fn horizon_is_three_windows() {
        assert_eq!(horizon_for_ceiling(2000), 6000);
        assert_eq!(horizon_for_ceiling(5000), 15000);
    }
}
