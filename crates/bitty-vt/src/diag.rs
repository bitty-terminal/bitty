//! Rate-limited diagnostics for hostile parser input (CTX-0470).
//!
//! Every rejection path is triggered by untrusted PTY bytes, so a single
//! hostile stream can produce one rejection per byte. Unbounded `eprintln!`
//! turns stderr into a denial-of-service amplifier (synchronous I/O plus
//! log volume), so rejection warnings are bounded: the first
//! [`REJECT_LOG_BURST`] observations log in full, after which one in every
//! [`REJECT_LOG_INTERVAL`] observations logs with its running count.
//!
//! The log is per parser/assembler instance, not global, so behavior stays
//! deterministic and testable; the bound is what matters, not the shared
//! counter.

/// Number of rejection warnings printed in full before sampling starts.
pub(crate) const REJECT_LOG_BURST: u64 = 8;

/// One in this many further rejections is reported after the burst.
pub(crate) const REJECT_LOG_INTERVAL: u64 = 1024;

/// Per-instance rejection counter with burst-plus-sampling policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RejectLog {
    seen: u64,
}

impl RejectLog {
    /// Counts one rejection. Returns the 1-based occurrence count when the
    /// warning should be emitted, `None` when it is sampled out.
    pub(crate) fn record(&mut self) -> Option<u64> {
        self.seen = self.seen.saturating_add(1);
        if self.seen <= REJECT_LOG_BURST || self.seen % REJECT_LOG_INTERVAL == 0 {
            Some(self.seen)
        } else {
            None
        }
    }

    /// Total rejections observed by this instance.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn seen(&self) -> u64 {
        self.seen
    }
}

/// Emits one rate-limited rejection warning to stderr.
///
/// `message` is the stable diagnostic text; `occurrence` is the value
/// returned by [`RejectLog::record`]. On the first instances the line is
/// unchanged from the pre-ratelimit output; sampled instances append their
/// running count so operators can see the suppressed volume.
pub(crate) fn warn_rejection(occurrence: u64, message: &str) {
    if occurrence > REJECT_LOG_BURST {
        eprintln!("{message} (rejection #{occurrence})");
    } else {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_sampled() {
        let mut log = RejectLog::default();
        for n in 1..=REJECT_LOG_BURST {
            assert_eq!(log.record(), Some(n));
        }
        // Sampled out up to (but not including) the next interval boundary.
        let sampled_span = REJECT_LOG_INTERVAL - REJECT_LOG_BURST - 1;
        for _ in 0..sampled_span {
            assert_eq!(log.record(), None);
        }
        assert_eq!(log.record(), Some(REJECT_LOG_INTERVAL));
        assert_eq!(log.record(), None);
        assert_eq!(log.seen(), REJECT_LOG_INTERVAL + 1);
    }

    #[test]
    fn interval_boundaries_are_reported() {
        let mut log = RejectLog {
            seen: REJECT_LOG_INTERVAL * 3 - 1,
        };
        assert_eq!(log.record(), Some(REJECT_LOG_INTERVAL * 3));
    }

    #[test]
    fn counter_saturates() {
        let mut log = RejectLog { seen: u64::MAX };
        assert_eq!(log.record(), None);
        assert_eq!(log.seen(), u64::MAX);
    }
}
