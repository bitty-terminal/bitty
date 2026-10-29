//! One lease's bounded outbound queue and the control-frame reservation
//! (contract `D-10`, `D-14`).
//!
//! A lease queue holds whole events in emission order. Control frames are never
//! shed: if the queue cannot accept one, the stream closes that lease with
//! `TerminalEventLost` instead of dropping the frame. A future shed-able
//! observability class is admitted only into the share the control reservation
//! leaves free, so data cannot starve a terminal, kill, abort, or lease-loss
//! frame.

use std::collections::VecDeque;

use crate::registry::{AttachId, MonotonicMs};

use super::event::Event;
use super::limits::EventLimits;

/// Frame priority class (`D-10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameClass {
    /// A control frame. Never silently shed; admission failure closes the lease.
    Control,
    /// A future observability frame. Never emitted in v0.2.0.
    ///
    /// Reserved for the class `D-10` requires to be shed newest-first, counted,
    /// and attributed; admitted only into the queue share the reservation
    /// leaves free.
    #[cfg_attr(not(test), allow(dead_code))]
    Sheddable,
}

/// The outcome of one admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// The frame was queued.
    Accepted,
    /// The frame was dropped newest-first (shed-able class only).
    Shed,
    /// The frame did not fit and must be treated as lost (control class only).
    Lost,
}

/// Why a lease queue was closed (`D-10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseCloseReason {
    /// The bounded queue could not accept a control frame, so the lease is
    /// closed with `TerminalEventLost` and the client reconciles with status.
    TerminalEventLost,
}

impl LeaseCloseReason {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalEventLost => "terminal_event_lost",
        }
    }
}

/// One closed lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseClose {
    /// The closed lease.
    pub attach_id: AttachId,
    /// The typed close reason.
    pub reason: LeaseCloseReason,
}

/// One attached lease's stream state: its last-seen stamp, its outbound queue,
/// and whether it observed the terminal event.
#[derive(Debug)]
pub(crate) struct Lease {
    pub(crate) last_seen: MonotonicMs,
    pub(crate) queue: LeaseQueue,
    pub(crate) terminal_observed: bool,
}

impl Lease {
    /// A freshly attached lease with an empty queue.
    pub(crate) fn new(now: MonotonicMs) -> Self {
        Self {
            last_seen: now,
            queue: LeaseQueue::new(),
            terminal_observed: false,
        }
    }
}

/// One lease's outbound queue.
#[derive(Debug)]
pub(crate) struct LeaseQueue {
    events: VecDeque<Event>,
    bytes: usize,
    shed: usize,
}

impl LeaseQueue {
    /// An empty queue.
    pub(crate) fn new() -> Self {
        Self {
            events: VecDeque::new(),
            bytes: 0,
            shed: 0,
        }
    }

    /// Whether one control frame of `bytes` fits without shedding.
    pub(crate) fn can_admit_control(&self, limits: &EventLimits, bytes: usize) -> bool {
        self.fits(
            limits.max_lease_queue_events(),
            limits.max_lease_queue_bytes(),
            bytes,
        )
    }

    fn fits(&self, max_events: usize, max_bytes: usize, bytes: usize) -> bool {
        self.events.len() < max_events && self.bytes.saturating_add(bytes) <= max_bytes
    }

    /// Admit one frame according to its class.
    pub(crate) fn admit(
        &mut self,
        event: Event,
        class: FrameClass,
        limits: &EventLimits,
    ) -> Admission {
        let bytes = event.accounted_bytes();
        let (max_events, max_bytes) = match class {
            FrameClass::Control => (
                limits.max_lease_queue_events(),
                limits.max_lease_queue_bytes(),
            ),
            FrameClass::Sheddable => (
                limits.observability_queue_events(),
                limits.observability_queue_bytes(),
            ),
        };
        if !self.fits(max_events, max_bytes, bytes) {
            return match class {
                FrameClass::Control => Admission::Lost,
                FrameClass::Sheddable => {
                    // Shed newest-first: the arriving frame is the newest one.
                    self.shed += 1;
                    Admission::Shed
                }
            };
        }
        self.bytes += bytes;
        self.events.push_back(event);
        Admission::Accepted
    }

    /// Hand every queued event to the transport and release the queue.
    pub(crate) fn drain(&mut self) -> Vec<Event> {
        self.bytes = 0;
        self.events.drain(..).collect()
    }

    /// Queued events.
    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the queue holds nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Queued event bytes.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Frames shed since the queue was created (`D-10` requires the count).
    #[cfg(test)]
    pub(crate) fn shed(&self) -> usize {
        self.shed
    }

    /// Whether an event with this revision and kind is still queued.
    pub(crate) fn contains(&self, revision: u64, kind: super::event::EventKind) -> bool {
        self.events
            .iter()
            .any(|event| event.revision == revision && event.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use crate::registry::{AttachId, DaemonInstanceId, MonotonicMs, SessionId};

    use super::*;
    use crate::events::event::{EventDetail, EventKind};
    use crate::events::limits::LEASE_CONTROL_RESERVATION_BYTES;

    fn event(revision: u64) -> Event {
        Event {
            instance_id: DaemonInstanceId::parse("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
                .expect("canonical"),
            session_id: SessionId::parse("1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e").expect("canonical"),
            attach_id: Some(
                AttachId::parse("2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d").expect("canonical"),
            ),
            revision,
            kind: EventKind::SessionState,
            terminal: None,
            detail: EventDetail::None,
            emitted_at: MonotonicMs::from_millis(revision),
        }
    }

    fn limits(events: usize, bytes: usize) -> EventLimits {
        EventLimits::new(256, 262_144, events, bytes, 8, 15_000, 8).expect("valid test limits")
    }

    /// The control reservation under a data flood: the future shed-able class
    /// can claim at most `queue - reservation`, and the reservation stays free
    /// for control frames.
    #[test]
    fn a_data_flood_never_consumes_the_control_reservation() {
        let limits = limits(64, 65_536);
        let mut queue = LeaseQueue::new();

        for revision in 1..=limits.observability_queue_events() as u64 {
            assert_eq!(
                queue.admit(event(revision), FrameClass::Sheddable, &limits),
                Admission::Accepted,
                "data frame {revision} is inside the unreserved share"
            );
        }
        assert_eq!(queue.len(), 64 - 8);
        assert_eq!(
            queue.admit(event(100), FrameClass::Sheddable, &limits),
            Admission::Shed
        );
        assert_eq!(queue.len(), 64 - 8);
        assert_eq!(queue.shed(), 1);

        // The flood left exactly the reserved control frames free.
        for revision in 101..=100 + 8 {
            assert_eq!(
                queue.admit(event(revision), FrameClass::Control, &limits),
                Admission::Accepted
            );
        }
        assert_eq!(queue.len(), 64);
        assert_eq!(
            queue.admit(event(200), FrameClass::Control, &limits),
            Admission::Lost
        );
        assert_eq!(queue.shed(), 1);
    }

    #[test]
    fn the_byte_reservation_is_independent_of_the_event_reservation() {
        let one = event(1).accounted_bytes();
        let limits = limits(256, LEASE_CONTROL_RESERVATION_BYTES + 4 * one);
        assert_eq!(limits.observability_queue_bytes(), 4 * one);

        let mut queue = LeaseQueue::new();
        for revision in 1..=4u64 {
            assert_eq!(
                queue.admit(event(revision), FrameClass::Sheddable, &limits),
                Admission::Accepted
            );
        }
        assert_eq!(
            queue.admit(event(5), FrameClass::Sheddable, &limits),
            Admission::Shed
        );
        assert_eq!(queue.bytes(), 4 * one);

        // The data flood left the whole byte reservation free for control.
        let mut control: usize = 0;
        while queue.admit(event(100 + control as u64), FrameClass::Control, &limits)
            == Admission::Accepted
        {
            control += 1;
        }
        let used = control * one;
        assert!(used <= LEASE_CONTROL_RESERVATION_BYTES);
        assert!(used + one > LEASE_CONTROL_RESERVATION_BYTES);
        assert_eq!(queue.bytes(), 4 * one + used);
        assert!(queue.bytes() <= limits.max_lease_queue_bytes());
        assert_eq!(queue.shed(), 1);
    }

    #[test]
    fn a_control_frame_is_never_shed_and_a_full_queue_is_reported() {
        let one = event(1).accounted_bytes();
        let limits = limits(2, 8 * one);
        let mut queue = LeaseQueue::new();
        assert_eq!(
            queue.admit(event(1), FrameClass::Control, &limits),
            Admission::Accepted
        );
        assert_eq!(
            queue.admit(event(2), FrameClass::Control, &limits),
            Admission::Accepted
        );
        assert!(!queue.can_admit_control(&limits, one));
        assert_eq!(
            queue.admit(event(3), FrameClass::Control, &limits),
            Admission::Lost
        );
        assert_eq!(queue.len(), 2);
        assert!(queue.contains(1, EventKind::SessionState));
        assert!(queue.contains(2, EventKind::SessionState));
        assert!(!queue.contains(3, EventKind::SessionState));

        let drained = queue.drain();
        assert_eq!(
            drained
                .iter()
                .map(|event| event.revision)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.bytes(), 0);
        assert!(queue.can_admit_control(&limits, one));
    }
}
