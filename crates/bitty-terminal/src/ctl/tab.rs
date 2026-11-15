//! `bitty ctl tab …`: how one CLI request reaches the running instance's Core
//! tab verbs (CTX-1836, bitty#3169).
//!
//! # Why the tab verbs do not ride the shared control queue
//!
//! `bitty ctl` speaks the devtools dispatcher over the instance socket/pipe,
//! and every control verb that needs the main-thread `Runtime` rides
//! `bitty_ipc::ctl`'s control queue. That queue authorizes a method against the
//! scope table compiled into the `bitty-ipc` pin
//! (`bitty_ipc::ctl::required_scope_for_ctl_method`), so a method the pin does
//! not know is refused with `NotFound` before it is ever enqueued — see
//! [`super::new_window`]'s module docs for the same constraint. The tab verbs
//! are new in this slice and the pin predates them, so they use the same
//! smallest honest seam: a bounded, single-slot request lane the servo handler
//! fills and the app thread drains on its next event turn, where it runs
//! [`bitty_runtime::Runtime::tab_apply`] — the one validated commit path the
//! `core.tab:*` verbs already enforce.
//!
//! There is no second mutation path, no second registry, and no scope bypass:
//! the handler authorizes `view.manage` itself against the live connection
//! grant (the control queue's own default-deny is the model), and the app
//! answers with the verb's typed refusal when the transition is refused.
//!
//! # Fail-closed rules
//!
//! - At most one request is in flight. A second concurrent caller gets
//!   [`TabReply::Busy`]; nothing is queued, so the lane cannot grow.
//! - A caller that gives up before the app claims the request leaves nothing
//!   behind ([`TabReply::Unserviced`]): no tab verb can run after a reported
//!   failure.
//! - A caller that gives up *after* the app claimed the request is told the
//!   outcome is unknown ([`TabReply::OutcomeUnknown`]), never that nothing
//!   happened.
//! - A dropped claim answers `OutcomeUnknown` instead of leaving the caller to
//!   time out on a silent channel.
//! - A refused verb is a typed [`TabReply::Refused`] carrying the
//!   [`bitty_ui::TabError`] class, never a silent success.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use bitty_runtime::TabRequest;
use bitty_ui::TabError;

/// Cross-thread event-loop wakeup hook.
///
/// A request must wake an idle instance (`ControlFlow::Wait`) or the caller
/// waits out its deadline. The app installs the same proxy it already gives
/// `bitty_ipc::ctl::set_control_waker` and [`super::new_window`], so a tab
/// request costs one extra `wake_pty` and grants nothing.
pub(crate) type TabLaneWaker = Arc<dyn Fn() + Send + Sync + 'static>;

/// How one tab request ended, as the CLI and the app both see it.
///
/// Every variant carries a stable `(category, code)` pair and message (see
/// [`Self::class`] / [`Self::message`]) so the CLI's exit code and text never
/// depend on where the refusal was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TabReply {
    /// The instance ran the verb through the validated commit path; `result`
    /// is the [`bitty_runtime::TabOutcome`] receipt as JSON.
    Applied {
        /// Receipt JSON (stable field set, see `TabOutcome::to_json`).
        result: String,
    },
    /// The transition was refused with a typed [`TabError`]. `category`,
    /// `code`, and `message` are the refusal as the CLI reports it.
    Refused {
        /// Stable error class (`usage`, `conflict`, ...).
        category: &'static str,
        /// Stable code (`Conflict`, `NotFound`, ...).
        code: &'static str,
        /// Human message naming the refusal.
        message: String,
    },
    /// Another tab request is in flight on this instance.
    Busy,
    /// The deadline passed before the app claimed the request: nothing ran.
    Unserviced,
    /// The deadline passed after the app claimed the request, or the claim was
    /// dropped before it could answer: the effect is unknown.
    OutcomeUnknown,
}

impl TabReply {
    /// Stable `(category, code)` pair (the CLI maps it to an exit code and the
    /// error class of the v1 envelope).
    pub(crate) const fn class(&self) -> (&'static str, &'static str) {
        match self {
            Self::Applied { .. } => ("", ""),
            Self::Refused { category, code, .. } => (category, code),
            Self::Busy => ("conflict", "ControlBusy"),
            Self::Unserviced | Self::OutcomeUnknown => ("transport", "Unavailable"),
        }
    }

    /// Stable client-facing message. Fixed text per variant: the variables are
    /// the refusal the host produced, so a script can match on the string.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Applied { result } => result.clone(),
            Self::Refused { message, .. } => message.clone(),
            Self::Busy => {
                String::from("another tab request is already in flight on this instance; retry")
            }
            Self::Unserviced => String::from(
                "the running instance did not service the tab request before the deadline; \
                 no tab changed",
            ),
            Self::OutcomeUnknown => String::from(
                "tab outcome unknown: the instance claimed the request at the deadline; \
                 read `bitty ctl tab` state before retrying",
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

/// Classify one typed [`TabError`] into the reply the CLI renders.
///
/// The mapping is one-way and total: refusals that mean "the caller named
/// something that is not there" are `NotFound`, refusals that mean "the
/// current state forbids this" are `Conflict`, and a focus that was not
/// re-homed is `Conflict` too (the state changed under the request). Never a
/// silent success.
#[must_use]
pub(crate) fn refusal(error: &TabError) -> TabReply {
    let (category, code) = match error {
        TabError::UnknownTab { .. } | TabError::GenerationGone { .. } => ("usage", "NotFound"),
        TabError::AlreadyOpen { .. }
        | TabError::AlreadyActive { .. }
        | TabError::SingleTab { .. }
        | TabError::LastTab { .. }
        | TabError::FocusNotRehomed { .. } => ("usage", "Conflict"),
        TabError::EmptyStack
        | TabError::ActiveOutOfRange { .. }
        | TabError::DuplicatePanel { .. }
        | TabError::DuplicateView { .. }
        | TabError::TooManyTabs { .. }
        | TabError::Structure(_)
        | TabError::Proposal(_) => ("usage", "InvalidParams"),
    };
    TabReply::Refused {
        category,
        code,
        message: format!("tab verb refused: {error}"),
    }
}

/// The tab verb a wire method names.
///
/// `None` for any method that is not one of [`super::request::TAB_METHODS`], so
/// the handler can answer a typed refusal instead of guessing.
#[must_use]
pub(crate) fn verb_for_method(method: &str) -> Option<TabWireVerb> {
    if method == super::request::METHOD_TAB_OPEN {
        Some(TabWireVerb::Open)
    } else if method == super::request::METHOD_TAB_SWITCH {
        Some(TabWireVerb::Switch)
    } else if method == super::request::METHOD_TAB_CLOSE {
        Some(TabWireVerb::Close)
    } else {
        None
    }
}

/// One tab verb as the wire names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabWireVerb {
    /// `bitty.debug/tabOpen` → `core.tab:open`.
    Open,
    /// `bitty.debug/tabSwitch` → `core.tab:switch`.
    Switch,
    /// `bitty.debug/tabClose` → `core.tab:close`.
    Close,
}

impl TabWireVerb {
    /// The Core verb this method runs, for diagnostics.
    #[must_use]
    pub(crate) const fn core_verb(self) -> &'static str {
        match self {
            Self::Open => bitty_ui::TAB_CMD_OPEN,
            Self::Switch => bitty_ui::TAB_CMD_SWITCH,
            Self::Close => bitty_ui::TAB_CMD_CLOSE,
        }
    }

    /// The runtime request for one validated `p:N` panel id.
    #[must_use]
    pub(crate) fn request(self, panel: u64) -> TabRequest {
        let panel = bitty_ui::PanelId::new(panel);
        match self {
            Self::Open => TabRequest::Open(panel),
            Self::Switch => TabRequest::Switch(panel),
            Self::Close => TabRequest::Close(panel),
        }
    }
}

/// One request the app thread has taken off the lane.
///
/// Dropping it without [`Self::complete`] still answers the caller
/// ([`TabReply::OutcomeUnknown`]), so a panic or an early return on the app
/// thread never leaves a CLI blocked on a silent channel.
pub(crate) struct TabClaim {
    seq: u64,
    verb: TabWireVerb,
    panel: u64,
    reply: Option<SyncSender<TabReply>>,
}

impl TabClaim {
    /// The verb the waiting caller asked for.
    #[must_use]
    pub(crate) fn verb(&self) -> TabWireVerb {
        self.verb
    }

    /// The validated panel id the waiting caller named.
    #[must_use]
    pub(crate) fn panel(&self) -> u64 {
        self.panel
    }

    /// Answers the waiting `bitty ctl` call and releases the lane slot.
    pub(crate) fn complete(mut self, reply: TabReply) {
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

impl Drop for TabClaim {
    fn drop(&mut self) {
        if self.reply.is_none() {
            return;
        }
        let reply = self.reply.take();
        self.release();
        if let Some(reply_tx) = reply {
            let _ = reply_tx.send(TabReply::OutcomeUnknown);
        }
    }
}

/// One in-flight request.
struct Slot {
    seq: u64,
    verb: TabWireVerb,
    panel: u64,
    /// Answers the caller exactly once (`None` once the app thread claimed
    /// it: from then on the outcome is the app's to report).
    reply: Option<SyncSender<TabReply>>,
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

/// Whether a request is waiting (one relaxed-ish load, no lock).
static PENDING: AtomicBool = AtomicBool::new(false);

/// Waker the app installs so an idle instance services the lane promptly.
static WAKER: OnceLock<TabLaneWaker> = OnceLock::new();

/// Installs the event-loop waker (idempotent; a second install is ignored).
pub(crate) fn set_waker(waker: TabLaneWaker) {
    let _ = WAKER.set(waker);
}

/// Whether one request is waiting to be claimed.
#[must_use]
pub(crate) fn is_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// Submits one request and blocks for its reply, bounded by `timeout`.
///
/// # Errors
///
/// Never panics: a full lane answers [`TabReply::Busy`], a deadline answers
/// [`TabReply::Unserviced`] or [`TabReply::OutcomeUnknown`].
pub(crate) fn request(verb: TabWireVerb, panel: u64, timeout: Duration) -> TabReply {
    let (tx, rx) = sync_channel::<TabReply>(1);
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = lock_lane();
        if guard.slot.is_some() {
            return TabReply::Busy;
        }
        guard.slot = Some(Slot {
            seq,
            verb,
            panel,
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
            // Withdraw the slot if it is still ours: the app never claimed it.
            let mut guard = lock_lane();
            let ours = guard.slot.as_ref().is_some_and(|slot| slot.seq == seq);
            if ours {
                guard.slot = None;
                PENDING.store(false, Ordering::Release);
                TabReply::Unserviced
            } else {
                TabReply::OutcomeUnknown
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => TabReply::OutcomeUnknown,
    }
}

/// Takes the waiting request, if any.
///
/// The caller owns the claim and must [`TabClaim::complete`] it (dropping it
/// answers `OutcomeUnknown`).
pub(crate) fn claim() -> Option<TabClaim> {
    let mut guard = lock_lane();
    let slot = guard.slot.as_mut()?;
    let seq = slot.seq;
    let verb = slot.verb;
    let panel = slot.panel;
    let reply = slot.reply.take();
    Some(TabClaim {
        seq,
        verb,
        panel,
        reply,
    })
}

/// Serial guard for the process-global tab lane in tests.
///
/// Mirrors `ctl::test_control_queue_lock`: the lane is process-wide while each
/// test owns its own app, so one test's parked request must never be claimed
/// (or answered) by another test's service turn.
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
    lock_lane().slot = None;
    PENDING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test that drives the lane holds the one serial guard and starts
    /// from an empty lane.
    fn serial_lane() -> MutexGuard<'static, ()> {
        let guard = test_lane_lock();
        reset_lane();
        guard
    }

    #[test]
    fn methods_map_to_their_core_verbs() {
        for (method, verb, core) in [
            (
                super::super::request::METHOD_TAB_OPEN,
                TabWireVerb::Open,
                "core.tab:open",
            ),
            (
                super::super::request::METHOD_TAB_SWITCH,
                TabWireVerb::Switch,
                "core.tab:switch",
            ),
            (
                super::super::request::METHOD_TAB_CLOSE,
                TabWireVerb::Close,
                "core.tab:close",
            ),
        ] {
            assert_eq!(verb_for_method(method), Some(verb));
            assert_eq!(verb.core_verb(), core);
            assert!(bitty_ui::TAB_CMDS.contains(&core));
        }
        assert_eq!(verb_for_method("bitty.debug/tabReorder"), None);
        assert_eq!(verb_for_method("bitty.debug/focusView"), None);
    }

    #[test]
    fn a_request_is_claimed_once_and_answered_exactly_once() {
        let _lane = serial_lane();
        let handle = std::thread::spawn(|| request(TabWireVerb::Switch, 7, Duration::from_secs(5)));
        // Wait for the caller to park its slot.
        let mut spins = 0;
        while !is_pending() && spins < 10_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert!(is_pending(), "the caller parked a slot");
        let claimed = claim().expect("the request is claimable");
        assert_eq!(claimed.verb(), TabWireVerb::Switch);
        assert_eq!(claimed.panel(), 7);
        assert_eq!(
            claimed.verb().request(claimed.panel()).verb(),
            bitty_ui::TabVerb::Switch
        );
        claimed.complete(TabReply::Applied {
            result: String::from("{\"verb\":\"switch\"}"),
        });
        let reply = handle.join().expect("caller thread");
        assert_eq!(
            reply.applied(),
            Some("{\"verb\":\"switch\"}"),
            "the receipt reaches the caller"
        );
        assert!(!is_pending(), "the lane is empty again");
        assert!(claim().is_none());
    }

    #[test]
    fn a_second_request_while_one_is_in_flight_is_busy() {
        let _lane = serial_lane();
        let busy = std::thread::spawn(|| {
            // Park a slot that nobody claims until the deadline.
            request(TabWireVerb::Close, 2, Duration::from_millis(200))
        });
        let mut spins = 0;
        while !is_pending() && spins < 10_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert!(is_pending());
        let second = request(TabWireVerb::Open, 3, Duration::from_millis(50));
        assert_eq!(second, TabReply::Busy);
        // The first caller times out with nothing applied.
        assert_eq!(busy.join().expect("first caller"), TabReply::Unserviced);
        assert!(!is_pending(), "the withdrawn slot is gone");
    }

    #[test]
    fn a_dropped_claim_answers_outcome_unknown() {
        let _lane = serial_lane();
        let handle = std::thread::spawn(|| request(TabWireVerb::Open, 4, Duration::from_secs(5)));
        let mut spins = 0;
        while !is_pending() && spins < 10_000 {
            std::thread::yield_now();
            spins += 1;
        }
        let claim = claim().expect("claimable");
        drop(claim);
        assert_eq!(
            handle.join().expect("caller"),
            TabReply::OutcomeUnknown,
            "a dropped claim never reports success or a silent nothing"
        );
    }

    #[test]
    fn refusals_map_to_typed_classes() {
        let cases = [
            (
                TabError::UnknownTab {
                    panel: bitty_ui::PanelId::new(9),
                },
                "usage",
                "NotFound",
            ),
            (
                TabError::LastTab {
                    panel: bitty_ui::PanelId::new(1),
                },
                "usage",
                "Conflict",
            ),
            (
                TabError::FocusNotRehomed {
                    expected: bitty_ui::PanelId::new(1),
                    actual: None,
                },
                "usage",
                "Conflict",
            ),
            (TabError::EmptyStack, "usage", "InvalidParams"),
        ];
        for (error, category, code) in cases {
            let reply = refusal(&error);
            assert_eq!(reply.class(), (category, code), "{error}");
            assert!(
                reply.message().contains(&error.to_string()),
                "the refusal message names the typed error: {}",
                reply.message()
            );
            assert!(reply.applied().is_none(), "a refusal is never a receipt");
        }
    }
}
