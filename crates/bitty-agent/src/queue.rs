//! Bounded side queue per `ADR-0003` rule 4.

use std::collections::VecDeque;

/// Bounded side queue through which the agent observes terminal/runtime events.
///
/// # Invariant (ADR-0003 rule 4, threat `T-01`)
///
/// The queue is strictly bounded so untrusted input cannot grow memory without
/// limit. Producers never block on a subscriber; when full the oldest entry
/// is dropped and a counter increments. The queue is drained by the agent
/// host, never by the terminal hot path.
///
/// This queue carries only host-mediated, bounded observations (e.g.
/// `AgentObservation`). It never holds GPU objects, window handles, PTY file
/// descriptors, or internal Rust hot-path objects.
///
/// # Headless testability
///
/// Pure `std` with no display-server, GPU, PTY, or network coupling. All
/// behavior is exercised by unit tests that run on the default Linux CI job
/// and on the `windows-latest` job.
#[derive(Debug)]
pub struct SideQueue<T> {
    inner: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> SideQueue<T> {
    /// Create a queue with `capacity` entries. Capacity must be `> 0`.
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "side queue capacity must be > 0");
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no item is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of items dropped due to overflow since creation or last `clear`.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Enqueue `item`, dropping the oldest entry when at capacity.
    pub fn push(&mut self, item: T) {
        if self.inner.len() >= self.capacity {
            self.inner.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        self.inner.push_back(item);
    }

    /// Drain all queued items in FIFO order.
    pub fn drain(&mut self) -> Vec<T> {
        self.inner.drain(..).collect()
    }

    /// Drain up to `limit` items (bounded batch).
    pub fn drain_bounded(&mut self, limit: usize) -> Vec<T> {
        let take = limit.min(self.inner.len());
        self.inner.drain(..take).collect()
    }

    /// Clear queued items and reset the dropped counter.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.dropped = 0;
    }

    /// Iterate queued items in order without consuming.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.inner.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_fifo_drop_oldest() {
        let mut q = SideQueue::new(2);
        q.push(1);
        q.push(2);
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 0);
        q.push(3); // evicts 1
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.drain(), vec![2, 3]);
    }

    #[test]
    fn drain_bounded() {
        let mut q = SideQueue::new(4);
        for i in 0..4 {
            q.push(i);
        }
        assert_eq!(q.drain_bounded(2), vec![0, 1]);
        assert_eq!(q.len(), 2);
        assert_eq!(q.drain(), vec![2, 3]);
    }

    #[test]
    fn clear_resets_dropped() {
        let mut q = SideQueue::new(1);
        q.push(1);
        q.push(2);
        assert_eq!(q.dropped(), 1);
        q.clear();
        assert_eq!(q.dropped(), 0);
        assert!(q.is_empty());
    }

    #[test]
    #[should_panic(expected = "side queue capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ = SideQueue::<u8>::new(0);
    }
}
