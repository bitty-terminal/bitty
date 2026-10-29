//! Wire delivery state: the bounded per-session ring and the bounded per-lease
//! outbound queues the service loop drains onto negotiated connections
//! (`D-10`, `D-14`; bitty#2792).
//!
//! The stream in [`super::stream`] owns a registry of its own and is the
//! in-memory reference for the event policy. This bus is the same policy applied
//! to the daemon's served surface: the service layer already owns the session
//! and lease truth (`SessionHost`), so it only needs the bounded delivery state
//! and never a second registry.
//!
//! # Bounds
//!
//! | Bound | Source |
//! | --- | --- |
//! | Retained ring, events and bytes | `max_event_backlog_events`, `max_event_backlog_bytes` |
//! | One lease queue, events and bytes | `max_lease_queue_events`, `max_lease_queue_bytes` |
//!
//! A control frame is never shed. When a lease queue cannot accept one the bus
//! reports the lease as closed with `TerminalEventLost`; the service detaches it
//! and the transport closes the connection. The client reconciles with
//! `session.status`, which keeps serving the terminal record until retention.
//!
//! The ring keeps whole events only, so a replay never starts with a partial
//! event; a gap is reported through `backlog_truncated` plus
//! `earliest_revision`.

use std::collections::{BTreeMap, BTreeSet};

use crate::registry::{AttachId, DaemonInstanceId, MonotonicMs, SessionId};

use super::backlog::Backlog;
use super::event::{Event, EventDetail, EventKind, TerminalMarker};
use super::lease::{Admission, FrameClass, LeaseClose, LeaseCloseReason, LeaseQueue};
use super::limits::EventLimits;

/// The pinned terminal event (`D-10`): retained until every recipient observed
/// it or the record is retired.
#[derive(Debug)]
struct TerminalPin {
    event: Event,
    pending: BTreeSet<AttachId>,
}

/// One session's delivery state.
#[derive(Debug)]
struct SessionEvents {
    revision: u64,
    terminalized: bool,
    backlog: Backlog,
    leases: BTreeMap<AttachId, LeaseQueue>,
    terminal: Option<TerminalPin>,
}

impl SessionEvents {
    fn new(limits: EventLimits) -> Self {
        Self {
            revision: 0,
            terminalized: false,
            backlog: Backlog::new(limits),
            leases: BTreeMap::new(),
            terminal: None,
        }
    }
}

/// The result of admitting one canonical event to every attached lease.
#[derive(Debug, Default)]
pub(crate) struct Emission {
    /// Leases that accepted the frame.
    pub(crate) recipients: Vec<AttachId>,
    /// Leases the queue could not serve; each is closed with
    /// `TerminalEventLost` by the caller.
    pub(crate) closed: Vec<LeaseClose>,
}

/// The replay window for one attach (`D-10`).
#[derive(Debug, Default)]
pub(crate) struct ReplaySlice {
    /// Retained events in revision order, routed to the requesting lease.
    pub(crate) events: Vec<Event>,
    /// Whether events before [`Self::earliest_revision`] were dropped.
    pub(crate) backlog_truncated: bool,
    /// The oldest revision this session can still serve.
    pub(crate) earliest_revision: Option<u64>,
    /// The pinned terminal event when this lease has not observed it yet and it
    /// is not still queued.
    pub(crate) terminal_event: Option<Event>,
}

/// The bounded delivery state for one daemon instance.
#[derive(Debug)]
pub(crate) struct EventBus {
    instance_id: DaemonInstanceId,
    limits: EventLimits,
    sessions: BTreeMap<SessionId, SessionEvents>,
}

impl EventBus {
    /// An empty bus for one daemon instance.
    pub(crate) fn new(instance_id: DaemonInstanceId, limits: EventLimits) -> Self {
        Self {
            instance_id,
            limits,
            sessions: BTreeMap::new(),
        }
    }

    /// Start tracking a freshly created session at revision `0`.
    pub(crate) fn register(&mut self, session_id: SessionId) {
        self.sessions
            .entry(session_id)
            .or_insert_with(|| SessionEvents::new(self.limits));
    }

    /// Stop tracking a session and report every lease it still held.
    ///
    /// The terminal record is gone, so each lease is closed with the typed
    /// `TerminalEventLost` reason instead of being dropped silently (`D-10`).
    pub(crate) fn remove(&mut self, session_id: SessionId) -> Vec<LeaseClose> {
        self.sessions
            .remove(&session_id)
            .map(|session| {
                session
                    .leases
                    .keys()
                    .map(|attach_id| LeaseClose {
                        attach_id: *attach_id,
                        reason: LeaseCloseReason::TerminalEventLost,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The last emitted revision for one session.
    pub(crate) fn revision(&self, session_id: SessionId) -> Option<u64> {
        self.sessions
            .get(&session_id)
            .map(|session| session.revision)
    }

    /// Register one attached lease with an empty queue.
    ///
    /// Returns `false` when the session has no delivery state. A live session
    /// is registered at create, so a caller must treat that as an internal
    /// invariant failure instead of silently serving a lease that can never
    /// receive a frame.
    pub(crate) fn attach(&mut self, session_id: SessionId, attach_id: AttachId) -> bool {
        match self.sessions.get_mut(&session_id) {
            Some(session) => {
                session.leases.insert(attach_id, LeaseQueue::new());
                true
            }
            None => false,
        }
    }

    /// Release one lease's queue. A queued final frame must be drained first.
    pub(crate) fn detach(&mut self, session_id: SessionId, attach_id: AttachId) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.leases.remove(&attach_id);
            if let Some(pin) = session.terminal.as_mut() {
                pin.pending.remove(&attach_id);
                if pin.pending.is_empty() {
                    session.terminal = None;
                }
            }
        }
    }

    /// Emit the canonical event for `revision`, filling any revision the caller
    /// did not report with a plain `session_state` frame.
    ///
    /// A revision that is not ahead of the last emitted one is refused: an event
    /// is never renumbered, re-emitted, or synthesized after the fact.
    pub(crate) fn emit(
        &mut self,
        session_id: SessionId,
        revision: u64,
        kind: EventKind,
        detail: EventDetail,
        terminal: Option<TerminalMarker>,
        now: MonotonicMs,
    ) -> Result<Emission, ()> {
        let instance_id = self.instance_id;
        let session = self.sessions.get_mut(&session_id).ok_or(())?;
        if revision <= session.revision {
            return Err(());
        }
        let mut frames = Vec::new();
        while session.revision + 1 < revision {
            let next = session.revision + 1;
            session.revision = next;
            let event = Event {
                instance_id,
                session_id,
                attach_id: None,
                revision: next,
                kind: EventKind::SessionState,
                terminal: None,
                detail: EventDetail::None,
                emitted_at: now,
            };
            session.backlog.append(event.clone());
            frames.push(event);
        }
        session.revision = revision;
        let event = Event {
            instance_id,
            session_id,
            attach_id: None,
            revision,
            kind,
            terminal,
            detail,
            emitted_at: now,
        };
        session.backlog.append(event.clone());
        if kind == EventKind::SessionTerminal {
            let pending: BTreeSet<AttachId> = session.leases.keys().copied().collect();
            session.terminalized = true;
            session.terminal = Some(TerminalPin {
                event: event.clone(),
                pending,
            });
            // `D-18`: the ring is freed at the terminal transition; the pinned
            // terminal event is the recovery copy.
            session.backlog.free();
        }
        frames.push(event);

        let mut emission = Emission::default();
        for frame in frames {
            self.admit(session_id, frame, &mut emission)?;
        }
        Ok(emission)
    }

    /// Emit one event at the session's current revision without bumping it.
    ///
    /// Used for the daemon-scoped `daemon_stopping` frame (`D-10`: it is not a
    /// session mutation) and for the target-only `lease_revoked` frame.
    pub(crate) fn emit_scoped(
        &mut self,
        session_id: SessionId,
        kind: EventKind,
        detail: EventDetail,
        now: MonotonicMs,
    ) -> Result<Emission, ()> {
        let revision = self
            .sessions
            .get(&session_id)
            .map(|session| session.revision)
            .ok_or(())?;
        self.emit_to(session_id, None, revision, kind, detail, now)
    }

    /// Emit one event copy to a single lease at `revision`.
    pub(crate) fn emit_to(
        &mut self,
        session_id: SessionId,
        only: Option<AttachId>,
        revision: u64,
        kind: EventKind,
        detail: EventDetail,
        now: MonotonicMs,
    ) -> Result<Emission, ()> {
        let instance_id = self.instance_id;
        let session = self.sessions.get_mut(&session_id).ok_or(())?;
        let event = Event {
            instance_id,
            session_id,
            attach_id: None,
            revision,
            kind,
            terminal: None,
            detail,
            emitted_at: now,
        };
        let mut emission = Emission::default();
        match only {
            Some(attach_id) => {
                let Some(queue) = session.leases.get_mut(&attach_id) else {
                    return Err(());
                };
                match queue.admit(
                    event.routed_to(attach_id),
                    FrameClass::Control,
                    &self.limits,
                ) {
                    Admission::Accepted => emission.recipients.push(attach_id),
                    Admission::Lost | Admission::Shed => emission.closed.push(LeaseClose {
                        attach_id,
                        reason: LeaseCloseReason::TerminalEventLost,
                    }),
                }
            }
            None => {
                self.admit(session_id, event, &mut emission)?;
            }
        }
        Ok(emission)
    }

    /// Admit one canonical frame to every attached lease of `session_id`.
    fn admit(
        &mut self,
        session_id: SessionId,
        event: Event,
        emission: &mut Emission,
    ) -> Result<(), ()> {
        let limits = self.limits;
        let session = self.sessions.get_mut(&session_id).ok_or(())?;
        let attach_ids: Vec<AttachId> = session.leases.keys().copied().collect();
        for attach_id in attach_ids {
            let Some(queue) = session.leases.get_mut(&attach_id) else {
                continue;
            };
            match queue.admit(event.routed_to(attach_id), FrameClass::Control, &limits) {
                Admission::Accepted => emission.recipients.push(attach_id),
                Admission::Lost | Admission::Shed => emission.closed.push(LeaseClose {
                    attach_id,
                    reason: LeaseCloseReason::TerminalEventLost,
                }),
            }
        }
        Ok(())
    }

    /// Hand every queued frame for one lease to the transport, in order.
    pub(crate) fn drain(&mut self, session_id: SessionId, attach_id: AttachId) -> Vec<Event> {
        self.sessions
            .get_mut(&session_id)
            .and_then(|session| session.leases.get_mut(&attach_id))
            .map(LeaseQueue::drain)
            .unwrap_or_default()
    }

    /// Pop the oldest queued frame for one lease.
    ///
    /// The transport drains one frame at a time so a full outbound buffer
    /// leaves the rest of the queue intact instead of dropping a batch.
    pub(crate) fn drain_next(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
    ) -> Option<Event> {
        self.sessions
            .get_mut(&session_id)
            .and_then(|session| session.leases.get_mut(&attach_id))
            .and_then(LeaseQueue::pop_front)
    }

    /// Whether one lease still has a queued frame.
    pub(crate) fn has_pending(&self, session_id: SessionId, attach_id: AttachId) -> bool {
        self.sessions
            .get(&session_id)
            .and_then(|session| session.leases.get(&attach_id))
            .is_some_and(|queue| !queue.is_empty())
    }

    /// Queued frames for one lease.
    #[cfg(test)]
    pub(crate) fn queue_len(&self, session_id: SessionId, attach_id: AttachId) -> usize {
        self.sessions
            .get(&session_id)
            .and_then(|session| session.leases.get(&attach_id))
            .map_or(0, LeaseQueue::len)
    }

    /// Queued bytes for one lease.
    #[cfg(test)]
    pub(crate) fn queue_bytes(&self, session_id: SessionId, attach_id: AttachId) -> usize {
        self.sessions
            .get(&session_id)
            .and_then(|session| session.leases.get(&attach_id))
            .map_or(0, LeaseQueue::bytes)
    }

    /// Retained ring length for one session.
    #[cfg(test)]
    pub(crate) fn backlog_len(&self, session_id: SessionId) -> usize {
        self.sessions
            .get(&session_id)
            .map_or(0, |session| session.backlog.len())
    }

    /// The replay window for a freshly attached lease.
    ///
    /// The ring is served whole: the client deduplicates by
    /// `(session_id, revision, kind)`, so a reconnect never needs a cursor and
    /// the frozen `1.0` attach request carries none. `backlog_truncated` plus
    /// `earliest_revision` state the gap explicitly.
    pub(crate) fn replay(&self, session_id: SessionId, attach_id: AttachId) -> ReplaySlice {
        let Some(session) = self.sessions.get(&session_id) else {
            return ReplaySlice::default();
        };
        let earliest = session.backlog.earliest();
        let backlog_truncated = match earliest {
            Some(front) => front > 1,
            None => session.terminalized && session.revision > 0,
        };
        let earliest_revision = earliest
            .or_else(|| (session.terminalized && session.revision > 0).then_some(session.revision));
        let events = session
            .backlog
            .after(0)
            .into_iter()
            .map(|event| event.routed_to(attach_id))
            .collect();
        let terminal_event = session.terminal.as_ref().and_then(|pin| {
            if !pin.pending.contains(&attach_id) {
                return None;
            }
            let queued = session.leases.get(&attach_id).is_some_and(|queue| {
                queue.contains(pin.event.revision, EventKind::SessionTerminal)
            });
            if queued {
                None
            } else {
                Some(pin.event.routed_to(attach_id))
            }
        });
        ReplaySlice {
            events,
            backlog_truncated,
            earliest_revision,
            terminal_event,
        }
    }

    /// Mark the pinned terminal event observed by one lease (`D-10`).
    ///
    /// The pin is released once every recipient observed it.
    pub(crate) fn ack_terminal(&mut self, session_id: SessionId, attach_id: AttachId) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(pin) = session.terminal.as_mut() else {
            return;
        };
        pin.pending.remove(&attach_id);
        if pin.pending.is_empty() {
            session.terminal = None;
        }
    }

    /// Whether a terminal event is still pinned for one lease.
    #[cfg(test)]
    pub(crate) fn terminal_pending(&self, session_id: SessionId, attach_id: AttachId) -> bool {
        self.sessions
            .get(&session_id)
            .and_then(|session| session.terminal.as_ref())
            .is_some_and(|pin| pin.pending.contains(&attach_id))
    }

    /// Whether the session already emitted its terminal event.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn terminalized(&self, session_id: SessionId) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|session| session.terminalized)
    }

    /// Every live lease of one session.
    pub(crate) fn leases(&self, session_id: SessionId) -> Vec<AttachId> {
        self.sessions
            .get(&session_id)
            .map(|session| session.leases.keys().copied().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AttachId, DaemonInstanceId};

    fn bus(events: usize) -> EventBus {
        let instance = DaemonInstanceId::parse(&"1".repeat(32)).expect("instance");
        let limits =
            EventLimits::new(events, 262_144, events, 65_536, 8, 15_000, 8).expect("limits");
        EventBus::new(instance, limits)
    }

    fn session(bus: &mut EventBus) -> (SessionId, AttachId) {
        let session_id = SessionId::parse(&"2".repeat(32)).expect("session");
        let attach_id = AttachId::parse(&"3".repeat(32)).expect("attach");
        bus.register(session_id);
        assert!(bus.attach(session_id, attach_id));
        (session_id, attach_id)
    }

    #[test]
    fn every_revision_is_emitted_once_and_routed_per_lease() {
        let mut bus = bus(8);
        let (session_id, attach_id) = session(&mut bus);
        let other = AttachId::parse(&"4".repeat(32)).expect("attach");
        assert!(bus.attach(session_id, other));

        bus.emit(
            session_id,
            1,
            EventKind::SessionState,
            EventDetail::None,
            None,
            MonotonicMs::from_millis(1),
        )
        .expect("emit");
        // A caller that skips a revision gets the gap filled, never a jump.
        bus.emit(
            session_id,
            3,
            EventKind::SessionState,
            EventDetail::None,
            None,
            MonotonicMs::from_millis(3),
        )
        .expect("emit");

        let first = bus.drain(session_id, attach_id);
        assert_eq!(
            first.iter().map(|event| event.revision).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(
            first.iter().all(|event| event.attach_id == Some(attach_id)),
            "a copy names the lease it is routed to"
        );
        assert_eq!(bus.queue_len(session_id, other), 3);
        assert!(
            bus.emit(
                session_id,
                3,
                EventKind::SessionState,
                EventDetail::None,
                None,
                MonotonicMs::from_millis(4),
            )
            .is_err(),
            "a revision is never re-emitted"
        );
    }

    #[test]
    fn the_ring_reports_a_gap_and_frees_at_the_terminal_transition() {
        let mut bus = bus(2);
        let (session_id, attach_id) = session(&mut bus);
        for revision in 1..=4 {
            bus.emit(
                session_id,
                revision,
                EventKind::SessionState,
                EventDetail::None,
                None,
                MonotonicMs::from_millis(revision),
            )
            .expect("emit");
        }
        let slice = bus.replay(session_id, attach_id);
        assert_eq!(slice.events.len(), 2);
        assert!(slice.backlog_truncated);
        assert_eq!(slice.earliest_revision, Some(3));

        let marker = TerminalMarker {
            state: crate::registry::LifecycleState::Exited,
            reason: crate::registry::TerminalReason::WorkerExit,
            exit: crate::registry::ExitInformation::ExitCode(0),
            cleanup: crate::registry::CleanupClass::Swept,
        };
        bus.emit(
            session_id,
            5,
            EventKind::SessionTerminal,
            EventDetail::None,
            Some(marker),
            MonotonicMs::from_millis(5),
        )
        .expect("terminal");
        assert_eq!(bus.backlog_len(session_id), 0, "the ring is freed");
        let slice = bus.replay(session_id, attach_id);
        assert!(slice.events.is_empty());
        assert!(slice.backlog_truncated);
        assert_eq!(slice.earliest_revision, Some(5));
        assert_eq!(
            slice.terminal_event.as_ref().map(|event| event.revision),
            Some(5),
            "the pinned terminal event is the recovery copy"
        );

        bus.ack_terminal(session_id, attach_id);
        assert!(!bus.terminal_pending(session_id, attach_id));
        assert!(
            bus.replay(session_id, attach_id).terminal_event.is_none(),
            "an observed terminal event is never replayed"
        );
    }

    #[test]
    fn a_full_control_queue_closes_the_lease_instead_of_shedding() {
        let mut bus = bus(1);
        let (session_id, attach_id) = session(&mut bus);
        for revision in 1..=2 {
            let emission = bus
                .emit(
                    session_id,
                    revision,
                    EventKind::SessionState,
                    EventDetail::None,
                    None,
                    MonotonicMs::from_millis(revision),
                )
                .expect("emit");
            if revision == 2 {
                assert_eq!(
                    emission.closed,
                    [LeaseClose {
                        attach_id,
                        reason: LeaseCloseReason::TerminalEventLost,
                    }]
                );
            }
        }
        assert_eq!(bus.queue_len(session_id, attach_id), 1);
        assert!(bus.queue_bytes(session_id, attach_id) > 0);
    }
}
