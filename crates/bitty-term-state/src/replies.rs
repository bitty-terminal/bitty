//! Bounded device-status reply queue (RFC invariant 7 "Reply bounds").
//!
//! Replies synthesized from `RequestDeviceStatus` (and any `Reply` actions
//! delivered through the stream) are queued here and returned to the
//! caller — terminal state performs no I/O. Total pending bytes are capped
//! at [`REPLY_CAP_BYTES`]; exceeding the cap drops the new reply whole and
//! sets an overflow flag rather than blocking (bounded reverse channel per
//! the security baseline).

use std::collections::VecDeque;

/// Hard cap on total pending reply bytes per terminal (RFC invariant 7).
pub const REPLY_CAP_BYTES: usize = 4096;

/// The bounded reply queue.
#[derive(Debug, Clone, Default)]
pub struct Replies {
    pending: VecDeque<Box<[u8]>>,
    total_bytes: usize,
    overflowed: bool,
}

impl Replies {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one reply; drops it whole and raises the overflow flag when
    /// it would push the queue past [`REPLY_CAP_BYTES`] (RFC invariant 7:
    /// "exceeding the cap drops new replies and sets a flag").
    pub fn queue(&mut self, bytes: Box<[u8]>) {
        if self.total_bytes + bytes.len() > REPLY_CAP_BYTES {
            self.overflowed = true;
            return;
        }
        self.total_bytes += bytes.len();
        self.pending.push_back(bytes);
    }

    /// Total queued reply bytes; always within [`REPLY_CAP_BYTES`].
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Whether any reply was dropped since the last drain.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Takes every pending reply in arrival order; the overflow flag resets
    /// with the drain so callers observe drop events per consumption cycle.
    pub fn drain(&mut self) -> Vec<Box<[u8]>> {
        self.overflowed = false;
        self.total_bytes = 0;
        self.pending.drain(..).collect()
    }

    /// Empties the queue without returning anything (`FullReset` path).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.total_bytes = 0;
        self.overflowed = false;
    }

    /// Non-consuming read of pending replies in arrival order (used by the
    /// canonical state hash; callers should prefer [`Replies::drain`]).
    pub(crate) fn pending_slices(&self) -> Vec<&[u8]> {
        self.pending.iter().map(|reply| reply.as_ref()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(len: usize) -> Box<[u8]> {
        vec![0x1b; len].into_boxed_slice()
    }

    #[test]
    fn queue_within_cap_is_fifo() {
        let mut q = Replies::new();
        q.queue(reply(4));
        q.queue(reply(2));
        assert_eq!(q.total_bytes(), 6);
        let drained = q.drain();
        assert_eq!(drained[0].len(), 4);
        assert_eq!(drained[1].len(), 2);
        assert!(!q.overflowed());
        assert_eq!(q.total_bytes(), 0);
    }

    #[test]
    fn exceeding_cap_drops_whole_reply_and_flags() {
        let mut q = Replies::new();
        q.queue(reply(REPLY_CAP_BYTES - 2));
        q.queue(reply(3));
        assert!(q.overflowed());
        // The dropped reply never enters the queue.
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].len(), REPLY_CAP_BYTES - 2);
        assert!(!q.overflowed(), "flag clears with its drain");
    }

    #[test]
    fn exact_cap_boundary_is_accepted() {
        let mut q = Replies::new();
        q.queue(reply(REPLY_CAP_BYTES));
        assert!(!q.overflowed());
        assert_eq!(q.total_bytes(), REPLY_CAP_BYTES);
    }

    #[test]
    fn clear_resets_everything() {
        let mut q = Replies::new();
        q.queue(reply(10));
        q.clear();
        assert!(q.drain().is_empty());
        assert!(!q.overflowed());
    }
}
