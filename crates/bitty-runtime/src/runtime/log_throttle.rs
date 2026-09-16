//! Bounded, rate-limited stderr diagnostics for hostile-input hot paths (CTX-0473).
//!
//! Hot paths — OSC 52 / kitty rejects fed straight from PTY bytes, and
//! spawn-failure notices — can be driven at child-output rate by an
//! untrusted or malicious program. A raw `eprintln!` per event lets that
//! child flood the owner's terminal (and any captured stderr) without bound.
//!
//! A [`LogThrottle`] admits at most `burst` diagnostics per fixed `window`
//! and counts the rest so the suppression stays visible: the next admitted
//! message reports how many identical diagnostics were dropped, then resets
//! the count. Deterministic via the caller-supplied [`Instant`], so tests
//! never depend on wall-clock timing.

use std::time::{Duration, Instant};

/// Maximum diagnostics one site may emit per [`LOG_THROTTLE_WINDOW`].
pub(super) const LOG_THROTTLE_BURST: u32 = 4;

/// Fixed window over which [`LOG_THROTTLE_BURST`] diagnostics are admitted.
pub(super) const LOG_THROTTLE_WINDOW: Duration = Duration::from_secs(1);

/// Per-site rate limiter for hot-path stderr diagnostics.
#[derive(Debug, Clone)]
pub(super) struct LogThrottle {
    window: Duration,
    burst: u32,
    window_start: Option<Instant>,
    admitted: u32,
    suppressed: u64,
}

impl LogThrottle {
    /// New throttle admitting `burst` messages per `window`.
    pub(super) const fn new(window: Duration, burst: u32) -> Self {
        Self {
            window,
            burst,
            window_start: None,
            admitted: 0,
            suppressed: 0,
        }
    }

    /// Whether the caller may emit a diagnostic at `now`.
    ///
    /// The first `burst` calls in each window are admitted; later calls are
    /// counted in [`suppressed`](Self::suppressed). A backwards `now` can
    /// never re-open a window early (`saturating_duration_since`).
    pub(super) fn admit(&mut self, now: Instant) -> bool {
        let window_expired = match self.window_start {
            None => true,
            Some(start) => now.saturating_duration_since(start) >= self.window,
        };
        if window_expired {
            self.window_start = Some(now);
            self.admitted = 0;
        }
        if self.admitted < self.burst {
            self.admitted += 1;
            true
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            false
        }
    }

    /// Admit a diagnostic at `now`, returning how many were suppressed since
    /// the last admitted message (`Some` = emit now, `None` = drop).
    ///
    /// The returned count is consumed, so a caller can append
    /// `" (N similar suppressed)"` to the emitted line exactly once.
    pub(super) fn admit_now(&mut self) -> Option<u64> {
        if self.admit(Instant::now()) {
            Some(self.take_suppressed())
        } else {
            None
        }
    }

    /// Suppressed diagnostics not yet reported by an admitted message.
    pub(super) fn suppressed(&self) -> u64 {
        self.suppressed
    }

    /// Take and reset the suppressed count (reported by the next emit).
    pub(super) fn take_suppressed(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed)
    }
}

/// Suffix for an admitted diagnostic that follows `suppressed` dropped ones.
///
/// Empty when nothing was suppressed, so callers can append it
/// unconditionally.
pub(super) fn suppressed_suffix(suppressed: u64) -> String {
    if suppressed == 0 {
        String::new()
    } else {
        format!(" ({suppressed} similar suppressed)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttles_burst_then_counts_suppressed() {
        let mut log = LogThrottle::new(Duration::from_secs(1), 2);
        let t0 = Instant::now();
        assert!(log.admit(t0));
        assert!(log.admit(t0));
        assert!(!log.admit(t0));
        assert!(!log.admit(t0));
        assert_eq!(log.suppressed(), 2, "post-burst drops must be counted");
        assert_eq!(log.take_suppressed(), 2);
        assert_eq!(log.suppressed(), 0, "take resets the counter");
    }

    /// Deterministic `admit_now` for tests (no wall clock).
    fn admit_at(log: &mut LogThrottle, now: Instant) -> Option<u64> {
        log.admit(now).then(|| log.take_suppressed())
    }

    #[test]
    fn new_window_readmits_and_reports_suppressed() {
        let mut log = LogThrottle::new(Duration::from_secs(1), 1);
        let t0 = Instant::now();
        assert_eq!(admit_at(&mut log, t0), Some(0));
        assert_eq!(admit_at(&mut log, t0), None);
        assert_eq!(admit_at(&mut log, t0), None);
        // First message of the next window carries the suppression count.
        assert_eq!(admit_at(&mut log, t0 + Duration::from_secs(1)), Some(2));
        assert_eq!(admit_at(&mut log, t0 + Duration::from_secs(1)), None);
    }

    #[test]
    fn zero_burst_never_admits() {
        let mut log = LogThrottle::new(Duration::from_secs(1), 0);
        assert!(!log.admit(Instant::now()));
        assert_eq!(log.suppressed(), 1);
    }

    #[test]
    fn suppressed_suffix_only_when_nonzero() {
        assert_eq!(suppressed_suffix(0), "");
        assert_eq!(suppressed_suffix(3), " (3 similar suppressed)");
    }
}
