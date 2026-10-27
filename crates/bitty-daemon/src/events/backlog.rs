//! The bounded per-session event backlog (`D-10`, `D-14`).
//!
//! The backlog is an append-only ring in `state_revision` order. It keeps whole
//! events only: when a cap is exceeded the oldest event is evicted whole, and a
//! gap is reported through `backlog_truncated` plus `earliest_revision` rather
//! than by serving a partial event. There is no unbounded replay.
//!
//! The ring is freed at the terminal transition (`D-18`: "the event ring ... is
//! freed when the session becomes terminal"); the terminal event survives in
//! the stream's terminal slot, not here.

use std::collections::VecDeque;

use super::event::Event;
use super::limits::EventLimits;

/// The retained ring for one session.
#[derive(Debug)]
pub(crate) struct Backlog {
    events: VecDeque<Event>,
    bytes: usize,
    max_events: usize,
    max_bytes: usize,
}

impl Backlog {
    /// An empty ring with the enforced caps.
    pub(crate) fn new(limits: EventLimits) -> Self {
        Self {
            events: VecDeque::new(),
            bytes: 0,
            max_events: limits.max_event_backlog_events(),
            max_bytes: limits.max_event_backlog_bytes(),
        }
    }

    /// Append one whole event and evict the oldest entries until it fits.
    ///
    /// Callers append in revision order; the stream asserts that before calling
    /// so a gap can never enter the ring.
    pub(crate) fn append(&mut self, event: Event) {
        self.bytes += event.accounted_bytes();
        self.events.push_back(event);
        self.trim();
    }

    fn trim(&mut self) {
        while self.events.len() > self.max_events || self.bytes > self.max_bytes {
            let Some(evicted) = self.events.pop_front() else {
                self.bytes = 0;
                return;
            };
            self.bytes -= evicted.accounted_bytes();
        }
    }

    /// Every retained event strictly after `after_revision`, in revision order.
    pub(crate) fn after(&self, after_revision: u64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|event| event.revision > after_revision)
            .cloned()
            .collect()
    }

    /// The oldest retained revision, or `None` when the ring holds nothing.
    pub(crate) fn earliest(&self) -> Option<u64> {
        self.events.front().map(|event| event.revision)
    }

    /// Retained events.
    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    /// Retained event bytes.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Release the ring at the terminal transition (`D-18`).
    pub(crate) fn free(&mut self) {
        self.events = VecDeque::new();
        self.bytes = 0;
    }

    /// Whether the ring retains an event with this revision and kind.
    #[cfg(test)]
    pub(crate) fn contains(&self, revision: u64, kind: super::event::EventKind) -> bool {
        self.events
            .iter()
            .any(|event| event.revision == revision && event.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use crate::registry::{
        AttachId, CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs,
        SessionId, TerminalReason,
    };

    use super::*;
    use crate::events::event::{EventKind, TerminalMarker};

    fn event(revision: u64) -> Event {
        Event {
            instance_id: DaemonInstanceId::parse("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
                .expect("canonical"),
            session_id: SessionId::parse("1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e").expect("canonical"),
            attach_id: None,
            revision,
            kind: EventKind::SessionState,
            terminal: None,
            detail: crate::events::event::EventDetail::None,
            emitted_at: MonotonicMs::from_millis(revision),
        }
    }

    fn limits(events: usize, bytes: usize) -> EventLimits {
        EventLimits::new(events, bytes, 16, 4096, 8, 15_000, 8).expect("valid test limits")
    }

    fn terminal_event(revision: u64) -> Event {
        let mut event = event(revision);
        event.kind = EventKind::SessionTerminal;
        event.terminal = Some(TerminalMarker {
            state: LifecycleState::Exited,
            reason: TerminalReason::WorkerExit,
            exit: ExitInformation::ExitCode(0),
            cleanup: CleanupClass::Swept,
        });
        event
    }

    #[test]
    fn the_ring_keeps_whole_events_at_both_caps() {
        let one = event(1).accounted_bytes();
        assert_eq!(one, terminal_event(1).accounted_bytes());

        // cap - 1, cap, cap + 1 events at a three-event cap.
        let mut backlog = Backlog::new(limits(3, 3 * one));
        for revision in 1..=2 {
            backlog.append(event(revision));
        }
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog.bytes(), 2 * one);
        assert_eq!(backlog.earliest(), Some(1));

        backlog.append(event(3));
        assert_eq!(backlog.len(), 3);
        assert_eq!(backlog.earliest(), Some(1));
        assert_eq!(backlog.after(0).len(), 3);

        backlog.append(event(4));
        assert_eq!(backlog.len(), 3);
        assert_eq!(backlog.bytes(), 3 * one);
        assert_eq!(backlog.earliest(), Some(2));
        assert_eq!(
            backlog
                .after(0)
                .iter()
                .map(|event| event.revision)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(backlog.after(3).len(), 1);
        assert_eq!(backlog.after(4), Vec::new());
    }

    #[test]
    fn the_byte_cap_evicts_whole_events() {
        let one = event(1).accounted_bytes();
        let mut backlog = Backlog::new(limits(16, 2 * one));
        for revision in 1..=3 {
            backlog.append(event(revision));
        }
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog.bytes(), 2 * one);
        assert_eq!(backlog.earliest(), Some(2));
        assert!(
            backlog
                .after(0)
                .iter()
                .all(|event| event.accounted_bytes() == one)
        );
    }

    #[test]
    fn free_releases_the_ring_and_keeps_no_history() {
        let mut backlog = Backlog::new(limits(4, 4096));
        backlog.append(terminal_event(7));
        assert_eq!(backlog.len(), 1);
        backlog.free();
        assert_eq!(backlog.len(), 0);
        assert_eq!(backlog.bytes(), 0);
        assert_eq!(backlog.earliest(), None);
        assert_eq!(backlog.after(0), Vec::new());
    }

    #[test]
    fn a_materialized_copy_carries_the_routing_lease() {
        let lease = AttachId::parse("2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d").expect("canonical");
        let mut backlog = Backlog::new(limits(4, 4096));
        backlog.append(event(1));
        assert!(backlog.contains(1, EventKind::SessionState));
        assert!(!backlog.contains(2, EventKind::SessionState));
        let routed = backlog.after(0).remove(0).routed_to(lease);
        assert_eq!(routed.attach_id, Some(lease));
        assert_eq!(routed.revision, 1);
    }
}
