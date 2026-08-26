//! Bounded cold-path event queue.
//!
//! The cold-path observes terminal state without ever entering the PTY -> parser
//! -> state -> damage hot path (architecture overview: "Terminal state -> cold-path
//! event -> plugin runtime"). Its queue is strictly bounded so untrusted PTY
//! bytes cannot grow memory without limit (threat T-01). When full, the oldest
//! event is dropped and a counter increments, mirroring the reply-cap policy
//! in terminal state (RFC invariant 7).

use std::collections::VecDeque;

/// An observable event that leaves the hot path toward the plugin runtime.
///
/// No payload allocates unboundedly; every string is already bounded by the
/// producing layer (terminal state replies carry bounded strings/bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdEvent {
    /// Window/icon title changed (`OSC 0`/`OSC 2`).
    TitleChanged(String),
    /// Working directory report changed (`OSC 7`).
    CwdChanged(String),
    /// Semantic prompt/command zone marker (`OSC 133`).
    ZoneMarked(bitty_vt::ZoneKind),
    /// Hyperlink span began or ended (`OSC 8`).
    HyperlinkChanged(Option<String>),
    /// Terminal bell (`BEL`, C0 `0x07`).
    Bell,
    /// Terminal mode toggled (`SM`/`RM`).
    ModeChanged {
        /// Which mode.
        mode: bitty_vt::Mode,
        /// New state.
        enabled: bool,
    },
    /// Damage became available after applying actions; generation counter.
    Damage {
        /// New generation after the batch.
        generation: u64,
    },
    /// An unmapped sequence was observed; inert telemetry.
    UnknownSequence(bitty_vt::SequenceKind),
}

/// Bounded FIFO queue for [`ColdEvent`]s.
///
/// Oldest entries are dropped when at capacity; [`ColdQueue::dropped`] counts
/// how many have been lost since creation or the last [`ColdQueue::clear`].
#[derive(Debug)]
pub struct ColdQueue {
    inner: VecDeque<ColdEvent>,
    capacity: usize,
    dropped: u64,
}

impl ColdQueue {
    /// Creates a queue with `capacity` entries. Capacity must be > 0.
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0` (checked by [`crate::RuntimeConfig`]).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cold queue capacity must be > 0");
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Capacity of this queue.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of queued events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no event is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of events dropped due to overflow since creation or clear.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Enqueues `event`, dropping the oldest entry when at capacity.
    pub fn push(&mut self, event: ColdEvent) {
        if self.inner.len() >= self.capacity {
            self.inner.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        self.inner.push_back(event);
    }

    /// Drains all queued events in FIFO order.
    pub fn drain(&mut self) -> Vec<ColdEvent> {
        self.inner.drain(..).collect()
    }

    /// Drains up to `limit` events.
    pub fn drain_bounded(&mut self, limit: usize) -> Vec<ColdEvent> {
        let take = limit.min(self.inner.len());
        self.inner.drain(..take).collect()
    }

    /// Clears queued events and resets the dropped counter.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.dropped = 0;
    }

    /// Iterates queued events in order without consuming.
    pub fn iter(&self) -> impl Iterator<Item = &ColdEvent> {
        self.inner.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_drops_oldest_when_full() {
        let mut q = ColdQueue::new(2);
        q.push(ColdEvent::Bell);
        q.push(ColdEvent::Bell);
        q.push(ColdEvent::TitleChanged("next".into()));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert!(matches!(drained[0], ColdEvent::Bell));
        assert!(matches!(drained[1], ColdEvent::TitleChanged(_)));
    }

    #[test]
    fn drain_bounded_limits_output() {
        let mut q = ColdQueue::new(8);
        for i in 0..4 {
            q.push(ColdEvent::TitleChanged(format!("{i}")));
        }
        let first = q.drain_bounded(2);
        assert_eq!(first.len(), 2);
        assert_eq!(q.len(), 2);
    }
}
