//! Crash accounting and restart backoff (pure, time injected).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{
    COMPONENT_BACKOFF_INITIAL, COMPONENT_BACKOFF_MAX, COMPONENT_CRASH_LIMIT, COMPONENT_CRASH_WINDOW,
};

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
    limit: usize,
    window: Duration,
    initial: Duration,
    max: Duration,
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
        Self::with_policy(
            COMPONENT_CRASH_LIMIT,
            COMPONENT_CRASH_WINDOW,
            COMPONENT_BACKOFF_INITIAL,
            COMPONENT_BACKOFF_MAX,
        )
    }

    /// Tracker with an explicit policy (tests).
    #[must_use]
    pub fn with_policy(limit: usize, window: Duration, initial: Duration, max: Duration) -> Self {
        Self {
            crashes: VecDeque::with_capacity(limit),
            next_spawn_at: None,
            unavailable: false,
            limit: limit.max(1),
            window,
            initial,
            max,
        }
    }

    /// Record a crash at `now` and return the resulting gate.
    pub fn record_crash(&mut self, now: Instant) -> SpawnGate {
        self.prune(now);
        if self.crashes.len() == self.limit {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        if self.crashes.len() >= self.limit {
            self.unavailable = true;
            self.next_spawn_at = None;
            return SpawnGate::Unavailable;
        }
        let delay = self.backoff_for(self.crashes.len());
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
            .filter(|at| now.saturating_duration_since(**at) < self.window)
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
        let shift = u32::try_from(n.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(16);
        self.initial
            .checked_mul(1u32 << shift)
            .unwrap_or(self.max)
            .min(self.max)
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.crashes.front() {
            if now.saturating_duration_since(*front) >= self.window {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
