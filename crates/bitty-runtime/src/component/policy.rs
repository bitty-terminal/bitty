//! Crash accounting and restart backoff (pure, time injected).
//!
//! Core-owned supervision seam (CTX-1177, issue #1997): [`SupervisionPolicy`]
//! adopts the published execution sidecar contract numbers
//! (`SidecarPolicy::core_defaults`) verbatim, so [`CrashTracker`],
//! [`DeadlineStrikes`], and the broker's effective deadline route through
//! one policy instead of re-deriving their own. The small-core graph fence
//! keeps Core from importing the execution crate directly, so this module
//! owns the copy and the parity tests below pin every number and behavior
//! against the contract; the composition root bridges this seam to the
//! execution implementation as follow-up work.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{
    COMPONENT_BACKOFF_INITIAL, COMPONENT_BACKOFF_MAX, COMPONENT_CRASH_LIMIT,
    COMPONENT_CRASH_WINDOW, COMPONENT_DEADLINE_CRASH_THRESHOLD, COMPONENT_HANDSHAKE_TIMEOUT,
    COMPONENT_IDLE_TIMEOUT, COMPONENT_MAX_IN_FLIGHT, COMPONENT_REQUEST_DEADLINE_GRACE,
    COMPONENT_REQUEST_DEFAULT_TIMEOUT, COMPONENT_REQUEST_MAX_TIMEOUT, COMPONENT_SHUTDOWN_GRACE,
};

/// Bounded supervision numbers the component broker enforces.
///
/// Every default is the DIR-030 number, adopted verbatim from the published
/// execution sidecar contract (`SidecarPolicy::core_defaults`); a host with
/// an approved reason tunes one field while keeping the rest. Fields are
/// public for that reason; use sites clamp defensively (`crash_limit.max(1)`,
/// threshold `max(1)`), mirroring the tracker constructors below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionPolicy {
    /// First restart delay after a crash (`COMPONENT_BACKOFF_INITIAL`, 1 s).
    pub backoff_initial: Duration,
    /// Restart delay ceiling (`COMPONENT_BACKOFF_MAX`, 30 s).
    pub backoff_max: Duration,
    /// Crashes inside `crash_window` that latch the sidecar unavailable
    /// (`COMPONENT_CRASH_LIMIT`, 5).
    pub crash_limit: usize,
    /// Sliding window for `crash_limit` (`COMPONENT_CRASH_WINDOW`, 5 min).
    pub crash_window: Duration,
    /// Consecutive Core deadline expiries that count as one crash
    /// (`COMPONENT_DEADLINE_CRASH_THRESHOLD`, 3).
    pub deadline_crash_threshold: u32,
    /// Core deadline base when a request asks for no timeout
    /// (`COMPONENT_REQUEST_DEFAULT_TIMEOUT`, 30 s).
    pub request_default_timeout: Duration,
    /// Ceiling for a requested timeout (`COMPONENT_REQUEST_MAX_TIMEOUT`,
    /// 300 s).
    pub request_max_timeout: Duration,
    /// Grace added on top of the effective timeout before Core expires a
    /// request (`COMPONENT_REQUEST_DEADLINE_GRACE`, 5 s).
    pub deadline_grace: Duration,
    /// Grace between closing stdin and killing the recorded child
    /// (`COMPONENT_SHUTDOWN_GRACE`, 2 s).
    pub shutdown_grace: Duration,
    /// Deadline for the `HelloAck` after spawn (`COMPONENT_HANDSHAKE_TIMEOUT`,
    /// 5 s).
    pub handshake_timeout: Duration,
    /// Idle time with nothing in flight before stdin is closed
    /// (`COMPONENT_IDLE_TIMEOUT`, 60 s).
    pub idle_timeout: Duration,
    /// Requests in flight per sidecar connection
    /// (`COMPONENT_MAX_IN_FLIGHT`, 64).
    pub max_in_flight: usize,
}

impl SupervisionPolicy {
    /// DIR-030 numbers, matching the sidecar contract defaults exactly.
    #[must_use]
    pub const fn core_defaults() -> Self {
        Self {
            backoff_initial: COMPONENT_BACKOFF_INITIAL,
            backoff_max: COMPONENT_BACKOFF_MAX,
            crash_limit: COMPONENT_CRASH_LIMIT,
            crash_window: COMPONENT_CRASH_WINDOW,
            deadline_crash_threshold: COMPONENT_DEADLINE_CRASH_THRESHOLD,
            request_default_timeout: COMPONENT_REQUEST_DEFAULT_TIMEOUT,
            request_max_timeout: COMPONENT_REQUEST_MAX_TIMEOUT,
            deadline_grace: COMPONENT_REQUEST_DEADLINE_GRACE,
            shutdown_grace: COMPONENT_SHUTDOWN_GRACE,
            handshake_timeout: COMPONENT_HANDSHAKE_TIMEOUT,
            idle_timeout: COMPONENT_IDLE_TIMEOUT,
            max_in_flight: COMPONENT_MAX_IN_FLIGHT,
        }
    }

    /// Core deadline for one request: the requested timeout, or the default
    /// when none was requested (`0`), clamped to the maximum.
    #[must_use]
    pub fn effective_deadline(&self, requested_timeout_ms: u32) -> Duration {
        let base = if requested_timeout_ms == 0 {
            self.request_default_timeout
        } else {
            Duration::from_millis(u64::from(requested_timeout_ms))
        };
        base.min(self.request_max_timeout)
    }

    /// Delay after the `n`-th crash inside the window (`n >= 1`): the
    /// initial delay doubling per crash, capped at the maximum.
    #[must_use]
    pub fn backoff_for(&self, n: usize) -> Duration {
        let shift = u32::try_from(n.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(16);
        self.backoff_initial
            .checked_mul(1u32 << shift)
            .unwrap_or(self.backoff_max)
            .min(self.backoff_max)
    }
}

/// Whether a component may be spawned now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnGate {
    /// Spawning is allowed.
    Ready,
    /// A crash backoff is running; retry after the given delay.
    Backoff {
        /// Remaining delay before the next spawn attempt is allowed.
        retry_after: Duration,
    },
    /// Too many crashes inside the window: unavailable until the next Bitty
    /// start (a fresh [`CrashTracker`]).
    Unavailable,
}

/// Per-component crash history.
///
/// Restart delay is [`COMPONENT_BACKOFF_INITIAL`] doubling per crash inside
/// [`COMPONENT_CRASH_WINDOW`], capped at [`COMPONENT_BACKOFF_MAX`];
/// [`COMPONENT_CRASH_LIMIT`] crashes inside the window latch the component
/// unavailable. The history holds at most the limit, so memory is bounded.
#[derive(Debug, Clone)]
pub struct CrashTracker {
    crashes: VecDeque<Instant>,
    next_spawn_at: Option<Instant>,
    unavailable: bool,
    policy: SupervisionPolicy,
}

impl Default for CrashTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CrashTracker {
    /// Tracker with the DIR-030 policy constants.
    #[must_use]
    pub fn new() -> Self {
        Self::with_supervision_policy(SupervisionPolicy::core_defaults())
    }

    /// Tracker with an explicit policy (tests).
    #[must_use]
    pub fn with_policy(limit: usize, window: Duration, initial: Duration, max: Duration) -> Self {
        let mut policy = SupervisionPolicy::core_defaults();
        policy.crash_limit = limit.max(1);
        policy.crash_window = window;
        policy.backoff_initial = initial;
        policy.backoff_max = max;
        Self::with_supervision_policy(policy)
    }

    /// Tracker under an explicit supervision policy (sidecar seam).
    #[must_use]
    pub fn with_supervision_policy(policy: SupervisionPolicy) -> Self {
        Self {
            crashes: VecDeque::with_capacity(policy.crash_limit.max(1)),
            next_spawn_at: None,
            unavailable: false,
            policy,
        }
    }

    /// Enforced policy.
    #[must_use]
    pub fn policy(&self) -> SupervisionPolicy {
        self.policy
    }

    /// Record a crash at `now` and return the resulting gate.
    pub fn record_crash(&mut self, now: Instant) -> SpawnGate {
        self.prune(now);
        let limit = self.policy.crash_limit.max(1);
        if self.crashes.len() == limit {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        if self.crashes.len() >= limit {
            self.unavailable = true;
            self.next_spawn_at = None;
            return SpawnGate::Unavailable;
        }
        let delay = self.policy.backoff_for(self.crashes.len());
        self.next_spawn_at = now.checked_add(delay);
        SpawnGate::Backoff { retry_after: delay }
    }

    /// Gate for a spawn attempt at `now`.
    #[must_use]
    pub fn gate(&self, now: Instant) -> SpawnGate {
        if self.unavailable {
            return SpawnGate::Unavailable;
        }
        match self.next_spawn_at {
            Some(at) if now < at => SpawnGate::Backoff {
                retry_after: at.duration_since(now),
            },
            _ => SpawnGate::Ready,
        }
    }

    /// Crashes currently counted inside the window ending at `now`.
    #[must_use]
    pub fn recent_crashes(&self, now: Instant) -> usize {
        self.crashes
            .iter()
            .filter(|at| now.saturating_duration_since(**at) < self.policy.crash_window)
            .count()
    }

    /// Whether the tracker latched unavailable.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.unavailable
    }

    /// Delay after the `n`-th crash inside the window (`n >= 1`).
    #[must_use]
    pub fn backoff_for(&self, n: usize) -> Duration {
        self.policy.backoff_for(n)
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.crashes.front() {
            if now.saturating_duration_since(*front) >= self.policy.crash_window {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Consecutive Core deadline expiries on one component (DIR-030 D2).
///
/// [`Self::expire`] counts one expiry and reports when
/// [`COMPONENT_DEADLINE_CRASH_THRESHOLD`] is reached (the caller then takes
/// the crash path); reaching it resets the count. [`Self::reset`] runs when
/// any request on the component ends with a component-produced terminal
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineStrikes {
    count: u32,
    threshold: u32,
}

impl Default for DeadlineStrikes {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadlineStrikes {
    /// Counter with the DIR-030 threshold.
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy(SupervisionPolicy::core_defaults())
    }

    /// Counter under an explicit supervision policy's threshold (sidecar
    /// seam).
    #[must_use]
    pub fn with_policy(policy: SupervisionPolicy) -> Self {
        Self::with_threshold(policy.deadline_crash_threshold)
    }

    /// Counter with an explicit threshold (tests); `0` behaves as `1`.
    #[must_use]
    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            count: 0,
            threshold: threshold.max(1),
        }
    }

    /// Count one expiry; `true` when the threshold is reached (the counter
    /// resets).
    pub fn expire(&mut self) -> bool {
        self.count = self.count.saturating_add(1);
        if self.count >= self.threshold {
            self.count = 0;
            return true;
        }
        false
    }

    /// A component-produced terminal frame ended a request.
    pub fn reset(&mut self) {
        self.count = 0;
    }

    /// Consecutive expiries counted so far.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervision_policy_core_defaults_match_the_sidecar_contract() {
        // CTX-1177 parity: every field adopts the published execution
        // sidecar contract default (`SidecarPolicy::core_defaults`) verbatim.
        // The contract lives outside this repository, so the numbers are
        // pinned here field by field; any contract change must update both.
        let policy = SupervisionPolicy::core_defaults();
        assert_eq!(policy.backoff_initial, Duration::from_secs(1));
        assert_eq!(policy.backoff_max, Duration::from_secs(30));
        assert_eq!(policy.crash_limit, 5);
        assert_eq!(policy.crash_window, Duration::from_secs(5 * 60));
        assert_eq!(policy.deadline_crash_threshold, 3);
        assert_eq!(policy.request_default_timeout, Duration::from_secs(30));
        assert_eq!(policy.request_max_timeout, Duration::from_secs(300));
        assert_eq!(policy.deadline_grace, Duration::from_secs(5));
        assert_eq!(policy.shutdown_grace, Duration::from_secs(2));
        assert_eq!(policy.handshake_timeout, Duration::from_secs(5));
        assert_eq!(policy.idle_timeout, Duration::from_secs(60));
        assert_eq!(policy.max_in_flight, 64);
    }

    #[test]
    fn supervision_policy_agrees_with_the_dir030_constants() {
        // The DIR-030 constants stay the canonical Core source; the policy
        // only routes through them, so drift in either direction fails here.
        let policy = SupervisionPolicy::core_defaults();
        assert_eq!(policy.backoff_initial, COMPONENT_BACKOFF_INITIAL);
        assert_eq!(policy.backoff_max, COMPONENT_BACKOFF_MAX);
        assert_eq!(policy.crash_limit, COMPONENT_CRASH_LIMIT);
        assert_eq!(policy.crash_window, COMPONENT_CRASH_WINDOW);
        assert_eq!(
            policy.deadline_crash_threshold,
            COMPONENT_DEADLINE_CRASH_THRESHOLD
        );
        assert_eq!(
            policy.request_default_timeout,
            COMPONENT_REQUEST_DEFAULT_TIMEOUT
        );
        assert_eq!(policy.request_max_timeout, COMPONENT_REQUEST_MAX_TIMEOUT);
        assert_eq!(policy.deadline_grace, COMPONENT_REQUEST_DEADLINE_GRACE);
        assert_eq!(policy.shutdown_grace, COMPONENT_SHUTDOWN_GRACE);
        assert_eq!(policy.handshake_timeout, COMPONENT_HANDSHAKE_TIMEOUT);
        assert_eq!(policy.idle_timeout, COMPONENT_IDLE_TIMEOUT);
        assert_eq!(policy.max_in_flight, COMPONENT_MAX_IN_FLIGHT);
    }

    #[test]
    fn supervision_effective_deadline_defaults_and_clamps() {
        let policy = SupervisionPolicy::core_defaults();
        assert_eq!(
            policy.effective_deadline(0),
            Duration::from_secs(30),
            "no requested timeout selects the default"
        );
        assert_eq!(
            policy.effective_deadline(1),
            Duration::from_millis(1),
            "an explicit timeout passes through"
        );
        assert_eq!(
            policy.effective_deadline(30_000),
            Duration::from_secs(30),
            "the default value is a fixed point"
        );
        assert_eq!(
            policy.effective_deadline(300_000),
            Duration::from_secs(300),
            "the ceiling value is a fixed point"
        );
        assert_eq!(
            policy.effective_deadline(999_000_000),
            Duration::from_secs(300),
            "larger requests clamp to the ceiling"
        );
        assert_eq!(
            policy.effective_deadline(u32::MAX),
            Duration::from_secs(300),
            "the clamp is total"
        );
    }

    #[test]
    fn supervision_backoff_matches_the_contract_sequence() {
        // Contract sequence: 1, 2, 4, 8, 16, then the 30 s ceiling.
        let policy = SupervisionPolicy::core_defaults();
        let delays: Vec<u64> = (1..=8).map(|n| policy.backoff_for(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(policy.backoff_for(usize::MAX), Duration::from_secs(30));
        let tracker = CrashTracker::new();
        assert_eq!(tracker.policy(), policy);
        let tracker_delays: Vec<u64> = (1..=8).map(|n| tracker.backoff_for(n).as_secs()).collect();
        assert_eq!(tracker_delays, delays);
    }

    #[test]
    fn supervision_tracker_latch_matches_the_contract() {
        // Contract latch: the fifth crash inside five minutes latches
        // unavailable, and only a fresh tracker recovers.
        let t0 = Instant::now();
        let mut tracker = CrashTracker::with_supervision_policy(SupervisionPolicy::core_defaults());
        for i in 0..4u64 {
            let gate = tracker.record_crash(t0 + Duration::from_secs(i * 60));
            assert!(matches!(gate, SpawnGate::Backoff { .. }), "crash {i}");
        }
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(4 * 60 + 59)),
            SpawnGate::Unavailable
        );
        assert!(tracker.is_unavailable());
        assert_eq!(
            tracker.gate(t0 + Duration::from_secs(3600)),
            SpawnGate::Unavailable
        );
    }

    #[test]
    fn supervision_deadline_strikes_match_the_contract() {
        let mut strikes = DeadlineStrikes::with_policy(SupervisionPolicy::core_defaults());
        assert!(!strikes.expire());
        assert!(!strikes.expire());
        strikes.reset();
        assert_eq!(strikes.count(), 0);
        assert!(!strikes.expire());
        assert!(!strikes.expire());
        assert!(strikes.expire(), "third consecutive expiry trips");
        assert_eq!(strikes.count(), 0, "tripping resets the counter");
    }

    #[test]
    fn deadline_strikes_trip_at_threshold_and_reset() {
        let mut strikes = DeadlineStrikes::new();
        assert_eq!(COMPONENT_DEADLINE_CRASH_THRESHOLD, 3);
        assert!(!strikes.expire());
        assert!(!strikes.expire());
        strikes.reset();
        assert_eq!(strikes.count(), 0);
        assert!(!strikes.expire());
        assert!(!strikes.expire());
        assert!(strikes.expire(), "third consecutive expiry trips");
        assert_eq!(strikes.count(), 0, "tripping resets the counter");
        assert!(DeadlineStrikes::with_threshold(0).expire());
    }

    #[test]
    fn backoff_doubles_from_one_second_to_thirty() {
        let tracker = CrashTracker::new();
        let delays: Vec<u64> = (1..=8).map(|n| tracker.backoff_for(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(tracker.backoff_for(usize::MAX), COMPONENT_BACKOFF_MAX);
    }

    #[test]
    fn crash_starts_backoff_and_gate_reopens() {
        let t0 = Instant::now();
        let mut tracker = CrashTracker::new();
        assert_eq!(tracker.gate(t0), SpawnGate::Ready);
        assert_eq!(
            tracker.record_crash(t0),
            SpawnGate::Backoff {
                retry_after: Duration::from_secs(1)
            }
        );
        assert!(matches!(tracker.gate(t0), SpawnGate::Backoff { .. }));
        assert_eq!(tracker.gate(t0 + Duration::from_secs(1)), SpawnGate::Ready);
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(
            tracker.record_crash(t1),
            SpawnGate::Backoff {
                retry_after: Duration::from_secs(2)
            }
        );
        assert_eq!(tracker.recent_crashes(t1), 2);
    }

    #[test]
    fn five_crashes_in_five_minutes_latch_unavailable() {
        let t0 = Instant::now();
        let mut tracker = CrashTracker::new();
        for i in 0..4u64 {
            let gate = tracker.record_crash(t0 + Duration::from_secs(i * 60));
            assert!(matches!(gate, SpawnGate::Backoff { .. }), "crash {i}");
        }
        assert_eq!(
            tracker.record_crash(t0 + Duration::from_secs(4 * 60 + 59)),
            SpawnGate::Unavailable
        );
        assert!(tracker.is_unavailable());
        assert_eq!(
            tracker.gate(t0 + Duration::from_secs(3600)),
            SpawnGate::Unavailable
        );
    }

    #[test]
    fn crashes_outside_the_window_do_not_count() {
        let t0 = Instant::now();
        let mut tracker = CrashTracker::new();
        for i in 0..10u64 {
            // One crash every 80 s: never 5 inside any 300 s window.
            let gate = tracker.record_crash(t0 + Duration::from_secs(i * 80));
            assert!(matches!(gate, SpawnGate::Backoff { .. }), "crash {i}");
        }
        assert!(!tracker.is_unavailable());
        assert!(tracker.recent_crashes(t0 + Duration::from_secs(9 * 80)) <= 4);
    }
}
