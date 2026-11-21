//! `bitty ctl panel present|release`: how the Core-owned debug pair reaches
//! the running instance's runtime (bitty#2388, CTX-1913).
//!
//! # Why the pair does not ride the shared control queue
//!
//! Every control verb that needs the main-thread `Runtime` rides
//! `bitty_ipc::ctl`'s control queue, and that queue authorizes a method
//! against the scope table compiled into the `bitty-ipc` pin
//! (`bitty_ipc::ctl::required_scope_for_ctl_method`), so a method the pin
//! does not know is refused with `NotFound` before it is ever applied — the
//! same constraint [`super::tab`] and [`super::new_window`] document. The
//! panel-content pair is new in this slice and the pin predates it, so the
//! pair travels through the same smallest honest seam: a bounded,
//! single-slot request lane the servo handler fills and the app thread
//! drains on its next event turn, where it runs the `apply_control` arms —
//! the one dispatch that calls
//! [`bitty_runtime::Runtime::present_panel_content`] and
//! [`bitty_runtime::Runtime::release_panel_content`].
//!
//! There is no second mutation path and no scope bypass: the handler
//! authorizes `view.manage` itself against the live connection grant (the
//! control queue's own default-deny is the model), the params are validated
//! at the handler before the lane is offered anything, and the app answers
//! with the verb's typed refusal when the runtime refuses.
//!
//! # Fail-closed rules
//!
//! - At most one request is in flight. A second concurrent caller gets
//!   [`PanelContentReply::Busy`]; nothing is queued, so the lane cannot
//!   grow.
//! - A caller that gives up before the app claims the request leaves
//!   nothing behind ([`PanelContentReply::Unserviced`]): no layer is
//!   presented or released after a reported failure.
//! - A caller that gives up *after* the app claimed the request is told the
//!   outcome is unknown ([`PanelContentReply::OutcomeUnknown`]), never that
//!   nothing happened.
//! - A dropped claim answers `OutcomeUnknown` instead of leaving the caller
//!   to time out on a silent channel.
//! - A refused verb is a typed [`PanelContentReply::Refused`] carrying the
//!   `apply_control` error triple, never a silent success.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Cross-thread event-loop wakeup hook, same shape as the tab lane's.
///
/// A request must wake an idle instance (`ControlFlow::Wait`) or the caller
/// waits out its deadline. The app installs the same proxy it already gives
/// `bitty_ipc::ctl::set_control_waker` and the tab lane, so a request costs
/// one extra `wake_pty` and grants nothing.
pub(crate) type PanelContentLaneWaker = Arc<dyn Fn() + Send + Sync + 'static>;

/// How one panel-content request ended, as the CLI and the app both see it.
///
/// Every variant carries a stable `(category, code)` pair and message (see
/// [`Self::class`] / [`Self::message`]) so the CLI's exit code and text never
/// depend on where the refusal was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PanelContentReply {
    /// The app thread ran the verb through `apply_control`; `result` is the
    /// receipt JSON (the present receipt names the release token).
    Applied {
        /// Receipt JSON (stable field set, see `apply_control`).
        result: String,
    },
    /// The verb was refused with a typed `apply_control` error.
    Refused {
        /// Stable error class (`usage`, ...).
        category: &'static str,
        /// Stable code (`Conflict`, `NotFound`, ...).
        code: &'static str,
        /// Human message naming the refusal.
        message: String,
    },
    /// Another panel-content request is in flight on this instance.
    Busy,
    /// The deadline passed before the app claimed the request: nothing ran.
    Unserviced,
    /// The deadline passed after the app claimed the request, or the claim
    /// was dropped before it could answer: the effect is unknown.
    OutcomeUnknown,
}

impl PanelContentReply {
    /// Stable `(category, code)` pair (the CLI maps it to an exit code and
    /// the error class of the v1 envelope).
    pub(crate) const fn class(&self) -> (&'static str, &'static str) {
        match self {
            Self::Applied { .. } => ("", ""),
            Self::Refused { category, code, .. } => (category, code),
            Self::Busy => ("conflict", "ControlBusy"),
            Self::Unserviced | Self::OutcomeUnknown => ("transport", "Unavailable"),
        }
    }

    /// Stable client-facing message. Fixed text per variant; the variables
    /// are the receipt or refusal the app produced, so a script can match
    /// on the string.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Applied { result } => result.clone(),
            Self::Refused { message, .. } => message.clone(),
            Self::Busy => String::from(
                "another panel-content request is already in flight on this instance; retry",
            ),
            Self::Unserviced => String::from(
                "the running instance did not service the panel-content request \
                 before the deadline; no layer changed",
            ),
            Self::OutcomeUnknown => String::from(
                "panel-content outcome unknown: the instance claimed the request at \
                 the deadline; read `bitty ctl panel list` before retrying",
            ),
        }
    }

    /// The receipt of an applied request, if this reply carries one.
    #[must_use]
    pub(crate) fn applied(&self) -> Option<&str> {
        match self {
            Self::Applied { result } => Some(result.as_str()),
            _ => None,
        }
    }
}

/// The wire method a panel-content request names.
///
/// `None` for any method that is not one of
/// [`super::request::PANEL_CONTENT_METHODS`], so the handler can answer a
/// typed refusal instead of guessing.
#[must_use]
pub(crate) fn method_for_request(method: &str) -> Option<&'static str> {
    if method == super::request::METHOD_PANEL_CONTENT_PRESENT {
        Some(super::request::METHOD_PANEL_CONTENT_PRESENT)
    } else if method == super::request::METHOD_PANEL_CONTENT_RELEASE {
        Some(super::request::METHOD_PANEL_CONTENT_RELEASE)
    } else {
        None
    }
}

/// One request the app thread has taken off the lane.
///
/// Dropping it without [`Self::complete`] still answers the caller
/// ([`PanelContentReply::OutcomeUnknown`]), so a panic or an early return on
/// the app thread never leaves a CLI blocked on a silent channel.
pub(crate) struct PanelContentClaim {
    seq: u64,
    method: &'static str,
    params: String,
    reply: Option<SyncSender<PanelContentReply>>,
}

impl PanelContentClaim {
    /// The wire method the waiting caller asked for.
    #[must_use]
    pub(crate) fn method(&self) -> &'static str {
        self.method
    }

    /// The validated params JSON the waiting caller sent.
    #[must_use]
    pub(crate) fn params(&self) -> &str {
        &self.params
    }

    /// Answers the waiting `bitty ctl` call and releases the lane slot.
    pub(crate) fn complete(mut self, reply: PanelContentReply) {
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

impl Drop for PanelContentClaim {
    fn drop(&mut self) {
        if self.reply.is_none() {
            return;
        }
        let reply = self.reply.take();
        self.release();
        if let Some(reply_tx) = reply {
            let _ = reply_tx.send(PanelContentReply::OutcomeUnknown);
        }
    }
}

/// One in-flight request.
struct Slot {
    seq: u64,
    method: &'static str,
    params: String,
    /// Whether the app thread has taken this request ([`claim`]).
    ///
    /// A claimed slot stays in place until the claim is
    /// [`PanelContentClaim::complete`]d or dropped, so the deadline path needs
    /// this flag (not the slot's presence) to tell "still waiting" from
    /// "already running": only an unclaimed slot can be withdrawn as
    /// [`PanelContentReply::Unserviced`].
    claimed: bool,
    /// Answers the caller exactly once (`None` once the app thread claimed
    /// it: from then on the outcome is the app's to report).
    reply: Option<SyncSender<PanelContentReply>>,
}

/// The lane: at most one slot, guarded by a mutex (never held across a send).
#[derive(Default)]
struct Lane {
    slot: Option<Slot>,
}

fn lane() -> &'static Mutex<Lane> {
    static LANE: OnceLock<Mutex<Lane>> = OnceLock::new();
    LANE.get_or_init(|| Mutex::new(Lane::default()))
}

fn lock_lane() -> MutexGuard<'static, Lane> {
    lane().lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Sequence counter for claim identity.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Whether a request is waiting (one atomic load, no lock).
static PENDING: AtomicBool = AtomicBool::new(false);

/// Waker the app installs so an idle instance services the lane promptly.
static WAKER: OnceLock<PanelContentLaneWaker> = OnceLock::new();

/// Installs the event-loop waker (idempotent; a second install is ignored).
pub(crate) fn set_waker(waker: PanelContentLaneWaker) {
    let _ = WAKER.set(waker);
}

/// Whether one request is waiting to be claimed.
#[must_use]
pub(crate) fn is_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// Submits one request and blocks for its reply, bounded by `timeout`.
///
/// The handler has already validated the params document, so the lane only
/// carries the already-checked bytes; a refused document never parks a slot.
///
/// # Errors
///
/// Never panics: a full lane answers [`PanelContentReply::Busy`], a deadline
/// answers [`PanelContentReply::Unserviced`] or
/// [`PanelContentReply::OutcomeUnknown`].
pub(crate) fn request(
    method: &'static str,
    params: String,
    timeout: Duration,
) -> PanelContentReply {
    let (tx, rx) = sync_channel::<PanelContentReply>(1);
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = lock_lane();
        if guard.slot.is_some() {
            return PanelContentReply::Busy;
        }
        guard.slot = Some(Slot {
            seq,
            method,
            params,
            claimed: false,
            reply: Some(tx),
        });
        PENDING.store(true, Ordering::Release);
    }
    if let Some(waker) = WAKER.get() {
        waker();
    }
    match rx.recv_timeout(timeout) {
        Ok(reply) => reply,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // The app may have answered in the same instant the deadline
            // fired; a reply already in the channel is the real answer.
            if let Ok(reply) = rx.try_recv() {
                return reply;
            }
            let mut guard = lock_lane();
            // Only a slot this caller still owns *and* the app has not taken
            // can be withdrawn. A claimed slot means the verb already reached
            // the apply path, so "no layer changed" would be a guess.
            let claimed = match guard.slot.as_ref() {
                Some(slot) if slot.seq == seq => slot.claimed,
                _ => return PanelContentReply::OutcomeUnknown,
            };
            if claimed {
                // The claim owns the outcome now; it answers the (closed)
                // channel with `OutcomeUnknown` on drop at the latest.
                return PanelContentReply::OutcomeUnknown;
            }
            guard.slot = None;
            PENDING.store(false, Ordering::Release);
            PanelContentReply::Unserviced
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => PanelContentReply::OutcomeUnknown,
    }
}

/// Takes the waiting request, if any.
///
/// The caller owns the claim and must [`PanelContentClaim::complete`] it
/// (dropping it answers `OutcomeUnknown`). A request is claimable once: a
/// second caller gets `None`, so the lane can never run one request twice.
pub(crate) fn claim() -> Option<PanelContentClaim> {
    if !PENDING.load(Ordering::Acquire) {
        return None;
    }
    let mut guard = lock_lane();
    let slot = guard.slot.as_mut()?;
    if slot.claimed {
        return None;
    }
    slot.claimed = true;
    let seq = slot.seq;
    let method = slot.method;
    let params = slot.params.clone();
    let reply = slot.reply.take();
    Some(PanelContentClaim {
        seq,
        method,
        params,
        reply,
    })
}

/// Serial guard for the process-global panel-content lane in tests.
///
/// Mirrors [`super::tab::test_lane_lock`]: the lane is process-wide while
/// each test owns its own app, so one test's parked request must never be
/// claimed (or answered) by another test's service turn.
#[cfg(test)]
pub(crate) fn test_lane_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Clears the lane. Test-only; the caller must hold [`test_lane_lock`].
#[cfg(test)]
pub(crate) fn reset_lane() {
    let mut guard = lock_lane();
    guard.slot = None;
    PENDING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::request::{
        METHOD_PANEL_CONTENT_PRESENT, METHOD_PANEL_CONTENT_RELEASE, panel_content_release_params,
        tab_params,
    };

    /// The lane runs one request end to end: submit, claim, answer, and the
    /// slot is free for the next caller. The apply itself is covered by the
    /// `apply_control` tests; this pins the transport.
    #[test]
    fn lane_answers_one_request_and_frees_the_slot() {
        let _lane = test_lane_lock();
        reset_lane();

        let caller = std::thread::spawn(|| {
            request(
                METHOD_PANEL_CONTENT_PRESENT,
                tab_params("p:1"),
                Duration::from_secs(5),
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let held = loop {
            if let Some(held) = claim() {
                break held;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "an authorized request never reached the lane"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(held.method(), METHOD_PANEL_CONTENT_PRESENT);
        assert_eq!(held.params(), tab_params("p:1"));
        held.complete(PanelContentReply::Applied {
            result: String::from("{\"presented\":true}"),
        });
        assert_eq!(
            caller.join().expect("caller thread"),
            PanelContentReply::Applied {
                result: String::from("{\"presented\":true}")
            }
        );
        assert!(claim().is_none(), "the slot is free after completion");

        // A second request parks again, so the single-slot rule is a live
        // rule, not a leftover of the first round trip.
        let caller = std::thread::spawn(|| {
            request(
                METHOD_PANEL_CONTENT_RELEASE,
                panel_content_release_params("p:1", 7),
                Duration::from_secs(5),
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let held = loop {
            if let Some(held) = claim() {
                break held;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the freed slot must park the next request"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(held.method(), METHOD_PANEL_CONTENT_RELEASE);
        assert_eq!(held.params(), panel_content_release_params("p:1", 7));
        held.complete(PanelContentReply::Refused {
            category: "usage",
            code: "NotFound",
            message: String::from("panel p:1 presents no content layer"),
        });
        let reply = caller.join().expect("caller thread");
        assert_eq!(
            reply.class(),
            ("usage", "NotFound"),
            "the typed refusal keeps its class through the lane: {reply:?}"
        );
        assert_eq!(reply.applied(), None);
    }

    /// A dropped claim answers `OutcomeUnknown`: the caller is never left to
    /// time out on a silent channel, and the lane is free afterwards.
    #[test]
    fn a_dropped_claim_answers_outcome_unknown() {
        let _lane = test_lane_lock();
        reset_lane();

        let caller = std::thread::spawn(|| {
            request(
                METHOD_PANEL_CONTENT_PRESENT,
                tab_params("p:2"),
                Duration::from_secs(5),
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let held = loop {
            if let Some(held) = claim() {
                break held;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the request never reached the lane"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        drop(held);
        assert_eq!(
            caller.join().expect("caller thread"),
            PanelContentReply::OutcomeUnknown
        );
        assert!(claim().is_none(), "a dropped claim releases the slot");
    }

    /// A request nobody claims is withdrawn as `Unserviced` at the deadline:
    /// the caller is told nothing ran, and the slot is free afterwards.
    #[test]
    fn an_unserviced_request_leaves_nothing_behind() {
        let _lane = test_lane_lock();
        reset_lane();

        let reply = request(
            METHOD_PANEL_CONTENT_RELEASE,
            panel_content_release_params("p:3", 9),
            Duration::from_millis(20),
        );
        assert_eq!(reply, PanelContentReply::Unserviced);
        assert!(claim().is_none(), "a withdrawn request leaves no slot");
    }
}
