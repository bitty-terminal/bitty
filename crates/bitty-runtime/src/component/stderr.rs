//! Bounded stderr capture ring for component processes.

use std::collections::VecDeque;

/// Fixed-capacity byte ring keeping the newest bytes a component wrote to
/// stderr. Older bytes are dropped first; memory never exceeds the
/// capacity.
#[derive(Debug, Clone)]
pub struct StderrRing {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: u64,
}

impl StderrRing {
    /// Empty ring holding at most `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Append `chunk`, evicting the oldest bytes beyond the capacity.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.capacity == 0 {
            self.dropped = self.dropped.saturating_add(chunk.len() as u64);
            return;
        }
        let tail = if chunk.len() > self.capacity {
            let skip = chunk.len() - self.capacity;
            self.dropped = self.dropped.saturating_add(skip as u64);
            &chunk[skip..]
        } else {
            chunk
        };
        let overflow = (self.bytes.len() + tail.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.dropped = self.dropped.saturating_add(overflow as u64);
        }
        self.bytes.extend(tail.iter().copied());
    }

    /// Current contents, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    /// Bytes currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the ring holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Total bytes evicted or skipped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_newest_bytes_within_capacity() {
        let mut ring = StderrRing::new(4);
        ring.push(b"ab");
        ring.push(b"cdef");
        assert_eq!(ring.snapshot(), b"cdef");
        assert_eq!(ring.dropped(), 2);
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.dropped(), 12);
    }

    #[test]
    fn zero_capacity_holds_nothing() {
        let mut ring = StderrRing::new(0);
        ring.push(b"abc");
        assert!(ring.is_empty());
        assert_eq!(ring.dropped(), 3);
    }

    #[test]
    fn default_bound_is_64_kib() {
        let mut ring = StderrRing::new(super::super::COMPONENT_STDERR_MAX_BYTES);
        ring.push(&vec![b'x'; 200 * 1024]);
        assert_eq!(ring.len(), 64 * 1024);
        assert_eq!(ring.capacity(), 64 * 1024);
    }
}
