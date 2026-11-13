//! `bitty ctl new-window`: how one CLI request reaches the running instance's
//! `NewWindow` path (bitty#1765, slice 5).
//!
//! # What the channel already carries, and what it does not
//!
//! `bitty ctl` speaks the devtools dispatcher over the instance socket/pipe,
//! and every control verb that needs the main thread (`Runtime`) rides
//! `bitty_ipc::ctl`'s control queue. That queue authorizes a method against the
//! scope table compiled into the `bitty-ipc` pin
//! (`bitty_ipc::ctl::required_scope_for_ctl_method`), so a method the pin does
//! not know is refused with `NotFound` before it is ever enqueued. A window,
//! however, is not `Runtime` state: it is created
//! through [`bitty_platform::EventContext::create_window`], which only the app
//! thread owns while handling an event.
//!
//! This module is therefore the smallest honest seam for the verb: a bounded,
//! single-slot request lane that the servo handler fills and the app thread
//! drains on the next event turn, where it latches through the *same*
//! [`crate::terminal_app::TerminalApp::latch_new_window`] the `new_window`
//! keybinding uses and then lets [`crate::terminal_app::TerminalApp::service_new_window_requests`]
//! create the window. There is no second registry, no second creation path,
//! and no scope bypass: the handler authorizes `view.manage` itself against the
//! live connection grant (the control queue's own default-deny is the model).
//!
//! # Fail-closed rules
//!
//! - At most one request is in flight. A second concurrent caller gets
//!   [`NewWindowReply::Busy`]; nothing is queued, so the lane cannot grow.
//! - A caller that gives up before the app claims the request leaves nothing
//!   behind ([`NewWindowReply::Unserviced`] and an empty lane): no window can
//!   appear after a reported failure.
//! - A caller that gives up *after* the app claimed the request is told the
//!   outcome is unknown ([`NewWindowReply::OutcomeUnknown`]), never that
//!   nothing happened — the same contract the control queue states for an
//!   in-flight apply.
//! - A dropped claim answers `OutcomeUnknown` instead of leaving the caller to
//!   time out on a silent channel.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use crate::window_registry::MAX_WINDOWS;

/// Cross-thread event-loop wakeup hook.
///
/// A request must wake an idle instance (`ControlFlow::Wait`) or the caller
/// waits out its deadline. The app installs the same proxy it already gives
/// `bitty_ipc::ctl::set_control_waker`, so a new-window request costs one
/// extra `wake_pty` and grants nothing.
pub(crate) type LaneWaker = Arc<dyn Fn() + Send + Sync + 'static>;

/// How one `new-window` request ended, as the CLI and the app both see it.
///
/// Every variant carries a stable `(category, code)` pair and message (see
/// [`Self::class`] / [`Self::message`]) so the CLI's exit code and text never
/// depend on where the refusal was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NewWindowReply {
    /// The instance admitted one window through the registry's `NewWindow`
    /// path; `windows` is the registered window count afterwards.
    Admitted {
        /// Registered windows after the request was serviced.
        windows: usize,
    },
    /// The instance already owns its full OS-window allowance.
    AtCapacity {
        /// The enforced ceiling ([`MAX_WINDOWS`]).
        max: usize,
        /// Windows registered when the request arrived.
        live: usize,
    },
    /// The platform refused the creation (no window system, backend refusal).
    PlatformRefused,
    /// Another `new-window` request is in flight on this instance.
    Busy,
    /// The deadline passed before the app claimed the request: nothing was
    /// latched and nothing was created.
    Unserviced,
    /// The deadline passed after the app claimed the request, or the claim was
    /// dropped before it could answer: the effect is unknown.
    OutcomeUnknown,
}

impl NewWindowReply {
    /// Stable `(category, code)` pair (the CLI maps it to an exit code and the
    /// error class of the v1 envelope).
    pub(crate) const fn class(&self) -> (&'static str, &'static str) {
        match self {
            Self::Admitted { .. } => ("", ""),
            Self::AtCapacity { .. } => ("conflict", "WindowAtCapacity"),
            Self::PlatformRefused => ("transport", "WindowRefused"),
            Self::Busy => ("conflict", "ControlBusy"),
            Self::Unserviced | Self::OutcomeUnknown => ("transport", "Unavailable"),
        }
    }

    /// Stable client-facing message. Fixed text per variant: the numbers are
    /// the only variables, so a script can match on the string.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Admitted { windows } => {
                format!("requested a new window on this instance ({windows} window(s) registered)")
            }
            Self::AtCapacity { max, live } => format!(
                "the running instance is at its OS-window ceiling ({live}/{max} windows); \
                 close a window and retry"
            ),
            Self::PlatformRefused => String::from(
                "the running instance refused the window (its platform backend has no window \
                 system); nothing was created",
            ),
            Self::Busy => String::from(
                "another new-window request is already in flight on this instance; retry",
            ),
            Self::Unserviced => String::from(
                "the running instance did not service the new-window request before the \
                 deadline; nothing was created",
            ),
            Self::OutcomeUnknown => String::from(
                "new-window outcome unknown: the instance claimed the request at the deadline; \
                 check the instance's windows before retrying",
            ),
        }
    }

    /// Whether the request produced a window.
    ///
    /// Test-only: production code matches the variant, because the admitted
    /// count travels with it.
    #[cfg(test)]
    pub(crate) const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }
}

/// The registry ceiling decision for one request (bitty#1765, slice 4's
/// [`MAX_WINDOWS`] predicate, shared with [`crate::window_registry::WindowRegistry::has_capacity`]).
///
/// Free of app state so the refusal is pinned headlessly: `live` is the
/// registry's current row count.
pub(crate) fn capacity_refusal(live: usize) -> Option<NewWindowReply> {
    if live >= MAX_WINDOWS {
        Some(NewWindowReply::AtCapacity {
            max: MAX_WINDOWS,
            live,
        })
    } else {
        None
    }
}

/// One request the app thread has taken off the lane.
///
/// Dropping it without [`Self::complete`] still answers the caller
/// ([`NewWindowReply::OutcomeUnknown`]), so a panic or an early return on the
/// app thread never leaves a CLI blocked on a silent channel.
pub(crate) struct NewWindowClaim {
    seq: u64,
    reply: Option<SyncSender<NewWindowReply>>,
}

impl NewWindowClaim {
    /// Answers the waiting `bitty ctl` call and releases the lane slot.
    pub(crate) fn complete(mut self, reply: NewWindowReply) {
        self.release();
        if let Some(reply_tx) = self.reply.take() {
            let _ = reply_tx.send(reply);
        }
    }

    /// Clears the lane slot when it still belongs to this claim.
    fn release(&self) {
        let mut lane = lock_lane();
        if lane.slot.as_ref().is_some_and(|slot| slot.seq == self.seq) {
            lane.slot = None;
            PENDING.store(false, Ordering::Release);
        }
    }
}

impl Drop for NewWindowClaim {
    fn drop(&mut self) {
        if self.reply.is_none() {
            return;
        }
        let reply = self.reply.take();
        self.release();
        if let Some(reply_tx) = reply {
            let _ = reply_tx.send(NewWindowReply::OutcomeUnknown);
        }
    }
}

/// One in-flight request.
struct Slot {
    seq: u64,
    /// Set once the app thread took it: from then on the outcome is the app's
    /// to report, never the caller's to assume.
    claimed: bool,
    reply: SyncSender<NewWindowReply>,
}

/// The lane: at most one slot plus the wakeup hook.
struct Lane {
    slot: Option<Slot>,
    waker: Option<LaneWaker>,
}

fn lane() -> &'static Mutex<Lane> {
    static LANE: OnceLock<Mutex<Lane>> = OnceLock::new();
    LANE.get_or_init(|| {
        Mutex::new(Lane {
            slot: None,
            waker: None,
        })
    })
}

fn lock_lane() -> MutexGuard<'static, Lane> {
    lane()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fast-path flag: whether a request is waiting for the app thread.
///
/// The app checks this on every event turn, so the check must be one relaxed
/// load and never a lock while no request is in flight.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Monotonic request identity.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Install (or clear with `None`) the event-loop wakeup hook.
///
/// Called once by the app next to `bitty_ipc::ctl::set_control_waker`; tests
/// clear it to stay hermetic.
pub(crate) fn set_waker(waker: Option<LaneWaker>) {
    lock_lane().waker = waker;
}

/// Ask the running instance for one new window and wait for its answer.
///
/// Called from the servo handler thread. `timeout` is the single bound on how
/// long the app may take to claim (and service) the request.
pub(crate) fn request(timeout: Duration) -> NewWindowReply {
    let (reply_tx, reply_rx) = sync_channel::<NewWindowReply>(1);
    let seq = NEXT_SEQ.fetch_add(1, Ordering::SeqCst);
    let waker = {
        let mut lane = lock_lane();
        if lane.slot.is_some() {
            // Bounded: one request at a time, so the lane cannot grow and a
            // second caller learns it right away instead of queueing.
            return NewWindowReply::Busy;
        }
        lane.slot = Some(Slot {
            seq,
            claimed: false,
            reply: reply_tx,
        });
        lane.waker.clone()
    };
    PENDING.store(true, Ordering::Release);
    if let Some(wake) = waker {
        wake();
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(reply) => reply,
        Err(_) => {
            // The app may have answered in the same instant the deadline
            // fired; a reply already in the channel is the real answer.
            if let Ok(reply) = reply_rx.try_recv() {
                return reply;
            }
            let mut lane = lock_lane();
            let still_ours = lane.slot.as_ref().is_some_and(|slot| slot.seq == seq);
            if !still_ours {
                // The app answered and released the slot between the deadline
                // and the re-read; it is nothing this caller may claim.
                return NewWindowReply::OutcomeUnknown;
            }
            let claimed = lane.slot.as_ref().is_some_and(|slot| slot.claimed);
            if claimed {
                // The app owns the outcome now; answering "nothing happened"
                // would be a guess, not a report.
                return NewWindowReply::OutcomeUnknown;
            }
            lane.slot = None;
            PENDING.store(false, Ordering::Release);
            NewWindowReply::Unserviced
        }
    }
}

/// Take the waiting request, if any (app thread, once per event turn).
///
/// Returns `None` while nothing is pending, which is the common case and costs
/// one relaxed load.
pub(crate) fn claim() -> Option<NewWindowClaim> {
    if !PENDING.load(Ordering::Acquire) {
        return None;
    }
    let mut lane = lock_lane();
    let slot = lane.slot.as_mut()?;
    if slot.claimed {
        return None;
    }
    slot.claimed = true;
    Some(NewWindowClaim {
        seq: slot.seq,
        reply: Some(slot.reply.clone()),
    })
}

/// Serial guard for the process-global lane in tests.
///
/// The lane is process-wide while every test owns its own app, so lane tests
/// and app-level lane tests must not interleave: both hold this one guard
/// around their request/claim window.
#[cfg(test)]
pub(crate) fn test_lane_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a request is waiting for the app thread (test hook only).
///
/// Gated on the recording-context feature because that is the only test that
/// needs to observe the lane from outside: the other tests claim instead.
#[cfg(all(test, feature = "input-vm-test-support"))]
pub(crate) fn is_waiting_for_tests() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// Drop any in-flight request and the waker (test hook only).
#[cfg(test)]
pub(crate) fn clear_for_tests() {
    let mut lane = lock_lane();
    lane.slot = None;
    lane.waker = None;
    drop(lane);
    PENDING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Short deadline: the tests prove the deadline contract, not patience.
    const BRIEF: Duration = Duration::from_millis(50);

    /// Poll the lane until the async request lands, so no assertion races the
    /// caller thread.
    fn take(deadline: Instant) -> NewWindowClaim {
        loop {
            if let Some(held) = claim() {
                return held;
            }
            assert!(
                Instant::now() < deadline,
                "the request never reached the lane"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_claimed_request_is_answered_with_the_app_outcome() {
        let _guard = test_lane_lock();
        clear_for_tests();
        let deadline = Instant::now() + Duration::from_secs(5);
        let caller = std::thread::spawn(move || request(Duration::from_secs(5)));
        let held = take(deadline);
        held.complete(NewWindowReply::Admitted { windows: 2 });
        let reply = caller.join().expect("caller thread");
        assert_eq!(reply, NewWindowReply::Admitted { windows: 2 });
        assert!(reply.is_admitted());
        assert!(
            claim().is_none(),
            "a completed request leaves no slot behind"
        );
    }

    #[test]
    fn a_second_request_while_one_is_in_flight_is_refused_as_busy() {
        let _guard = test_lane_lock();
        clear_for_tests();
        let deadline = Instant::now() + Duration::from_secs(5);
        let caller = std::thread::spawn(move || request(Duration::from_secs(5)));
        let held = take(deadline);
        assert_eq!(
            request(BRIEF),
            NewWindowReply::Busy,
            "the lane never queues a second request"
        );
        assert!(
            claim().is_none(),
            "an already-claimed request is not handed out twice"
        );
        held.complete(NewWindowReply::Admitted { windows: 1 });
        assert_eq!(
            caller.join().expect("caller thread"),
            NewWindowReply::Admitted { windows: 1 }
        );
    }

    #[test]
    fn an_unclaimed_request_times_out_as_unserviced_and_leaves_nothing() {
        let _guard = test_lane_lock();
        clear_for_tests();
        assert_eq!(
            request(BRIEF),
            NewWindowReply::Unserviced,
            "nothing claimed the request, so nothing was created"
        );
        assert!(
            claim().is_none(),
            "a timed-out request is withdrawn, never left for a later turn"
        );
    }

    #[test]
    fn a_claim_dropped_without_an_answer_reports_an_unknown_outcome() {
        let _guard = test_lane_lock();
        clear_for_tests();
        let deadline = Instant::now() + Duration::from_secs(5);
        let caller = std::thread::spawn(move || request(Duration::from_secs(5)));
        let held = take(deadline);
        drop(held);
        assert_eq!(
            caller.join().expect("caller thread"),
            NewWindowReply::OutcomeUnknown,
            "a dropped claim never reads as a failed request"
        );
    }

    #[test]
    fn the_ceiling_refusal_is_the_registry_bound() {
        assert!(
            capacity_refusal(MAX_WINDOWS - 1).is_none(),
            "one free slot is admitted"
        );
        match capacity_refusal(MAX_WINDOWS) {
            Some(NewWindowReply::AtCapacity { max, live }) => {
                assert_eq!((max, live), (MAX_WINDOWS, MAX_WINDOWS));
            }
            other => panic!("the ceiling must refuse with a typed reply: {other:?}"),
        }
        // The message is the caller-visible contract of that refusal.
        let message = NewWindowReply::AtCapacity {
            max: MAX_WINDOWS,
            live: MAX_WINDOWS,
        }
        .message();
        assert!(message.contains("OS-window ceiling"), "{message}");
        assert!(message.contains("close a window and retry"), "{message}");
    }

    #[test]
    fn every_refusal_has_a_stable_class_and_message() {
        let refusals = [
            NewWindowReply::AtCapacity { max: 16, live: 16 },
            NewWindowReply::PlatformRefused,
            NewWindowReply::Busy,
            NewWindowReply::Unserviced,
            NewWindowReply::OutcomeUnknown,
        ];
        for refusal in refusals {
            let (category, code) = refusal.class();
            assert!(!category.is_empty(), "{refusal:?} has no category");
            assert!(!code.is_empty(), "{refusal:?} has no code");
            assert!(!refusal.is_admitted(), "{refusal:?} is not an admission");
            assert!(!refusal.message().is_empty(), "{refusal:?} has no message");
        }
        assert_eq!(
            NewWindowReply::AtCapacity { max: 16, live: 16 }.class(),
            ("conflict", "WindowAtCapacity")
        );
        assert_eq!(
            NewWindowReply::Busy.class(),
            ("conflict", "ControlBusy"),
            "busy is a conflict, never a transport failure"
        );
        assert_eq!(
            NewWindowReply::Admitted { windows: 1 }.class(),
            ("", ""),
            "an admission is not an error"
        );
    }
}
