//! Presentation-only overlay session (CTX-1431, `bitty` #2406).
//!
//! A plugin that mounted a block into the `overlay` slot may present it
//! without acquiring keyboard or pointer capture
//! (`bitty.ui.overlay.show` / `hide`). This is the mechanism the help and
//! which-key panel depends on after ADR-0014: Core owns the geometry,
//! stacking, `Esc` routing, and teardown; the plugin owns only the scene.
//!
//! # Semantics (accepted design, stage 2)
//!
//! - **No capture.** A presentation session never claims the focusable
//!   overlay capture, never requires `ui.overlay.focus`, and never feeds the
//!   modal authority. It needs only the v1 `ui.rich` + `ui.overlay` grants
//!   of the mount.
//! - **One surface per window.** The session is admitted through the
//!   accepted [`OverlayManager`] vocabulary as
//!   [`OverlayKind::NonModal`] on [`OverlayTier::Popup`], so it consumes one
//!   non-modal slot under `MAX_OVERLAYS_PER_WINDOW` and can never set
//!   `modal_active`. A second `show` by another plugin fails closed with
//!   [`E_UI_UNAVAILABLE`]; the owner may re-show (same or replacement handle)
//!   and the newest handle wins.
//! - **Hide is idempotent.** A foreign, stale, or repeated hide is a
//!   success-without-effect and never steals another plugin's surface.
//! - **Bounded observations.** `Esc` dismissal queues one
//!   [`DismissedEvent`] per dismissal for the application tick; the buffer is
//!   bounded with drop-oldest, and no plugin runs on the input path.
//! - **Focus-follow (issue #2849).** The surface is bound to the focused view
//!   and re-binds when focus moves; when the view it is bound to closes, the
//!   surface is dropped with one bounded `view-closed` observation instead of
//!   silently re-centering on a survivor. At most one presentation surface
//!   exists per view per generation.
//! - **Teardown is Core-owned.** Suspend, unload, disable, crash, rollback,
//!   and generation change drop the session, so no stale surface survives.

use std::collections::VecDeque;

use bitty_lua::{BridgeError, E_UI_UNAVAILABLE};
use bitty_ui::panel::{MAX_OVERLAYS_PER_WINDOW, OverlayKind, OverlayManager};
use bitty_ui::{OverlayTier, Rect, ViewId};

/// Paint tier of a presentation overlay (accepted v1 design).
pub const PRESENTATION_OVERLAY_TIER: OverlayTier = OverlayTier::Popup;

/// Admission kind of a presentation overlay: never modal.
pub const PRESENTATION_OVERLAY_KIND: OverlayKind = OverlayKind::NonModal;

/// Core-reported reason for a presentation overlay dismissed by `Esc`.
pub const ESCAPE_DISMISS_REASON: &str = "escape";

/// Core-reported reason for a presentation overlay dropped because the view it
/// was bound to closed (issue #2849).
pub const VIEW_CLOSED_DISMISS_REASON: &str = "view-closed";

/// Maximum pending `overlay.dismissed` observations buffered between ticks.
///
/// Mirrors the `overlay.released` bound: an owner that dismisses and
/// re-shows in a tight loop inside one command callback would otherwise grow
/// the buffer without bound (one retained block may be reused for every
/// cycle). Overflow drops the oldest observation and counts it.
pub const PENDING_DISMISSED_MAX: usize = 64;

/// One queued `overlay.dismissed` observation.
///
/// `owner` is the presenting plugin id, `handle` the opaque block handle that
/// was presented, and `reason` the Core-reported cause (`"escape"` today).
/// The payload carries no input bytes, terminal content, or coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DismissedEvent {
    /// Presenting plugin at dismissal time.
    pub owner: String,
    /// Block handle that was presented.
    pub handle: i64,
    /// Core-reported dismissal reason.
    pub reason: String,
}

/// The live presentation session (owner + presented handle + followed view).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PresentationSession {
    plugin_id: String,
    handle: i64,
    /// Focused view the surface is drawn over (issue #2849). Bound on the
    /// first focus observation after `show`, re-bound when focus moves, and
    /// the surface is dropped when this view is no longer live.
    view: Option<ViewId>,
}

/// Runtime-shared single presentation-overlay session per window.
///
/// Owned by [`PluginRuntime`](super::PluginRuntime) and shared with every
/// generation's [`PluginServices`](super::services::PluginServices): `show`
/// admits, `hide` clears, `Esc` dismisses, and lifecycle teardown revokes.
#[derive(Debug)]
pub struct PresentationOverlay {
    session: Option<PresentationSession>,
    /// Accepted overlay manager used only to admit one non-modal slot and to
    /// keep `modal_active` false by construction.
    admissions: OverlayManager,
    /// Id of the admitted entry while `session` is live.
    admission_id: Option<u64>,
    /// Monotonic admission generation (diagnostics; never a capability).
    next_generation: u64,
    /// Pending `overlay.dismissed` observations for the application tick.
    pending_dismissed: VecDeque<DismissedEvent>,
    /// Observations dropped by buffer overflow since creation.
    dismissed_dropped: u64,
}

impl Default for PresentationOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationOverlay {
    /// An empty session (no surface, no queued observations).
    #[must_use]
    pub fn new() -> Self {
        Self {
            session: None,
            admissions: OverlayManager::new(),
            admission_id: None,
            next_generation: 0,
            pending_dismissed: VecDeque::new(),
            dismissed_dropped: 0,
        }
    }

    /// Admit or replace the presentation surface for (`plugin_id`, `handle`).
    ///
    /// The caller (host services) has already validated the capability gate
    /// and that `handle` is this generation's `overlay`-slot block. Returns
    /// `true` on success.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with code [`E_UI_UNAVAILABLE`] (class `runtime`) when
    /// another plugin already holds the window's presentation surface, or
    /// when the accepted overlay budget refuses the admission.
    pub fn show(&mut self, plugin_id: &str, handle: i64) -> Result<bool, BridgeError> {
        if let Some(session) = self.session.as_mut() {
            if session.plugin_id != plugin_id {
                return Err(occupied_error());
            }
            // Owner re-show: replace the presented handle in place. The
            // admission slot is already owned, so the budget is unchanged.
            session.handle = handle;
            return Ok(true);
        }
        let generation = self.next_generation.saturating_add(1);
        let admission = self
            .admissions
            .create_overlay(
                PRESENTATION_OVERLAY_KIND,
                Rect::new(0, 0, 0, 0),
                "",
                None,
                generation,
            )
            .map_err(|error| {
                BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("presentation overlay admission refused: {error}"),
                )
            })?;
        self.next_generation = generation;
        self.admission_id = Some(admission);
        self.session = Some(PresentationSession {
            plugin_id: plugin_id.to_string(),
            handle,
            // Bound by the next `observe_focus`; the admission itself carries
            // no view, so a host without a layout can still present.
            view: None,
        });
        Ok(true)
    }

    /// Remove the presentation surface owned by (`plugin_id`, `handle`).
    ///
    /// Idempotent: returns `true` only when it actually removed the matching
    /// live surface. A foreign, stale, or repeated hide returns `false` and
    /// never errors, so cleanup cannot wedge.
    pub fn hide(&mut self, plugin_id: &str, handle: i64) -> bool {
        let owned = self
            .session
            .as_ref()
            .is_some_and(|session| session.plugin_id == plugin_id && session.handle == handle);
        if owned {
            self.clear();
        }
        owned
    }

    /// Drop the presentation surface if `plugin_id` owns it (suspend,
    /// disable, unload, crash, rollback, generation change).
    ///
    /// Returns whether a surface was removed. Lifecycle teardown is not a
    /// dismissal observation: the owner is going away, so no event is queued.
    pub fn revoke_plugin(&mut self, plugin_id: &str) -> bool {
        let owned = self
            .session
            .as_ref()
            .is_some_and(|session| session.plugin_id == plugin_id);
        if owned {
            self.clear();
        }
        owned
    }

    /// Dismiss the live presentation surface because Core routed `Esc`.
    ///
    /// Hides the surface and queues exactly one `overlay.dismissed`
    /// observation with the `escape` reason. Returns whether a surface was
    /// dismissed (a second press is a no-op and queues nothing).
    pub fn dismiss_escape(&mut self) -> bool {
        self.dismiss_with_reason(ESCAPE_DISMISS_REASON).is_some()
    }

    /// Focus-follow observation, called once per tick before present
    /// (issue #2849).
    ///
    /// `focused` is the window's currently focused view and `is_live` answers
    /// whether a view still exists in the layout. Semantics:
    ///
    /// - An unbound session binds to `focused` (the surface the user just
    ///   opened follows the view they opened it on).
    /// - A live bound view re-binds when focus moves: the panel tracks the
    ///   focused view and is never dismissed by a plain focus switch.
    /// - A bound view that is no longer live drops the surface with one
    ///   `view-closed` observation instead of silently re-centering on a
    ///   survivor, so a closed view never leaves a panel addressing it.
    ///
    /// Returns the queued observation, if the surface was dropped. Bounded
    /// like every other dismissal: one event, drop-oldest buffer.
    pub fn observe_focus(
        &mut self,
        focused: Option<ViewId>,
        is_live: impl Fn(ViewId) -> bool,
    ) -> Option<DismissedEvent> {
        let bound = self.session.as_ref()?.view;
        match bound {
            None => {
                if let Some(view) = focused {
                    if let Some(session) = self.session.as_mut() {
                        session.view = Some(view);
                    }
                }
                None
            }
            Some(view) if !is_live(view) => self.dismiss_with_reason(VIEW_CLOSED_DISMISS_REASON),
            Some(view) => {
                if let Some(next) = focused {
                    if next != view {
                        if let Some(session) = self.session.as_mut() {
                            session.view = Some(next);
                        }
                    }
                }
                None
            }
        }
    }

    /// Drop the live surface and queue one observation with `reason`.
    ///
    /// Internal funnel shared by the `Esc` and `view-closed` causes, so both
    /// stay one bounded event with the same drop-oldest buffer.
    fn dismiss_with_reason(&mut self, reason: &str) -> Option<DismissedEvent> {
        let session = self.session.as_ref()?;
        let event = DismissedEvent {
            owner: session.plugin_id.clone(),
            handle: session.handle,
            reason: reason.to_string(),
        };
        if self.pending_dismissed.len() >= PENDING_DISMISSED_MAX {
            self.pending_dismissed.pop_front();
            self.dismissed_dropped = self.dismissed_dropped.saturating_add(1);
        }
        self.pending_dismissed.push_back(event.clone());
        self.clear();
        Some(event)
    }

    /// Drain pending `overlay.dismissed` observations in order.
    pub fn drain_dismissed(&mut self) -> Vec<DismissedEvent> {
        self.pending_dismissed.drain(..).collect()
    }

    /// Live session as `(owner plugin, handle)`, if any.
    #[must_use]
    pub fn session(&self) -> Option<(&str, i64)> {
        self.session
            .as_ref()
            .map(|session| (session.plugin_id.as_str(), session.handle))
    }

    /// View the live surface follows, if it has been bound yet (#2849).
    #[must_use]
    pub fn session_view(&self) -> Option<ViewId> {
        self.session.as_ref().and_then(|session| session.view)
    }

    /// Whether a presentation surface is live.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// Admission kind (always [`OverlayKind::NonModal`]).
    #[must_use]
    pub fn kind(&self) -> OverlayKind {
        PRESENTATION_OVERLAY_KIND
    }

    /// Paint tier (always [`OverlayTier::Popup`]).
    #[must_use]
    pub fn tier(&self) -> OverlayTier {
        PRESENTATION_OVERLAY_TIER
    }

    /// Whether the accepted overlay manager reports a modal (always false;
    /// proves the presentation path never becomes the modal authority).
    #[must_use]
    pub fn modal_active(&self) -> bool {
        self.admissions.modal_active()
    }

    /// Admitted non-modal slots consumed through the accepted budget
    /// (diagnostics and the one-slot invariant).
    #[must_use]
    pub fn admitted_slots(&self) -> usize {
        self.admissions.len()
    }

    /// Remaining non-modal slots under `MAX_OVERLAYS_PER_WINDOW`.
    #[must_use]
    pub fn remaining_non_modal_slots(&self) -> usize {
        MAX_OVERLAYS_PER_WINDOW.saturating_sub(self.admissions.len())
    }

    /// Number of pending `overlay.dismissed` observations.
    #[must_use]
    pub fn pending_dismissed_len(&self) -> usize {
        self.pending_dismissed.len()
    }

    /// Number of observations dropped by buffer overflow since creation.
    #[must_use]
    pub fn dismissed_dropped(&self) -> u64 {
        self.dismissed_dropped
    }

    /// Drop the live session and its admitted slot.
    fn clear(&mut self) {
        if let Some(id) = self.admission_id.take() {
            let _ = self.admissions.dismiss(id);
        }
        self.session = None;
    }
}

/// Typed "another plugin holds the window's presentation surface" denial.
fn occupied_error() -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_UNAVAILABLE,
        "a presentation overlay is already active in this window",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ui::panel::MAX_OVERLAYS_PER_WINDOW;

    #[test]
    fn show_admits_one_non_modal_popup_surface() {
        // CTX-1431: a presentation session consumes exactly one admitted
        // non-modal slot at the accepted Popup tier and never sets the modal
        // authority.
        let mut overlay = PresentationOverlay::new();
        assert!(!overlay.modal_active());
        assert_eq!(overlay.admitted_slots(), 0);
        overlay.show("acme.help", 7).expect("owner admits");
        assert!(overlay.is_active());
        assert_eq!(overlay.session(), Some(("acme.help", 7)));
        assert_eq!(overlay.kind(), OverlayKind::NonModal);
        assert_eq!(overlay.tier(), OverlayTier::Popup);
        assert!(!overlay.modal_active(), "presentation is never modal");
        assert_eq!(overlay.admitted_slots(), 1, "one non-modal slot is used");
        assert_eq!(
            overlay.remaining_non_modal_slots(),
            MAX_OVERLAYS_PER_WINDOW - 1,
            "the accepted budget accounts for the surface"
        );
    }

    #[test]
    fn second_plugin_fails_closed_while_owner_resbows() {
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        let error = overlay
            .show("acme.other", 9)
            .expect_err("a second plugin must fail closed");
        assert_eq!(error.class, "runtime");
        assert_eq!(error.code, E_UI_UNAVAILABLE);
        assert_eq!(
            overlay.session(),
            Some(("acme.help", 7)),
            "the live owner is unchanged"
        );
        assert_eq!(overlay.admitted_slots(), 1, "no second slot is consumed");

        // Same owner re-show replaces the presented handle in place.
        overlay.show("acme.help", 8).expect("owner may re-show");
        assert_eq!(overlay.session(), Some(("acme.help", 8)));
        assert_eq!(overlay.admitted_slots(), 1, "re-show reuses its slot");
        assert!(!overlay.modal_active());
    }

    #[test]
    fn hide_is_idempotent_and_never_steals() {
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        assert!(!overlay.hide("acme.other", 7), "foreign hide is a no-op");
        assert!(!overlay.hide("acme.help", 8), "stale handle is a no-op");
        assert!(overlay.is_active(), "no-op hides keep the surface");
        assert!(overlay.hide("acme.help", 7), "owner hide removes it");
        assert!(!overlay.is_active());
        assert_eq!(overlay.admitted_slots(), 0, "the slot is released");
        assert!(!overlay.hide("acme.help", 7), "second hide is a no-op");
    }

    #[test]
    fn escape_queues_exactly_one_bounded_observation() {
        let mut overlay = PresentationOverlay::new();
        assert!(!overlay.dismiss_escape(), "nothing to dismiss");
        overlay.show("acme.help", 7).expect("owner admits");
        assert!(overlay.dismiss_escape(), "escape dismisses");
        assert!(!overlay.is_active());
        assert!(!overlay.dismiss_escape(), "second press queues nothing");
        let events = overlay.drain_dismissed();
        assert_eq!(events.len(), 1, "exactly one observation");
        assert_eq!(events[0].owner, "acme.help");
        assert_eq!(events[0].handle, 7);
        assert_eq!(events[0].reason, "escape");
        assert!(overlay.drain_dismissed().is_empty(), "drain is destructive");
    }

    #[test]
    fn dismissal_observations_are_bounded_with_drop_oldest() {
        let mut overlay = PresentationOverlay::new();
        let rounds = PENDING_DISMISSED_MAX + 8;
        for index in 0..rounds {
            overlay.show("acme.help", 7).expect("admit");
            assert!(overlay.dismiss_escape(), "dismiss {index}");
        }
        assert_eq!(overlay.pending_dismissed_len(), PENDING_DISMISSED_MAX);
        assert_eq!(
            overlay.dismissed_dropped(),
            u64::try_from(rounds - PENDING_DISMISSED_MAX).expect("count fits")
        );
        assert_eq!(overlay.drain_dismissed().len(), PENDING_DISMISSED_MAX);
    }

    #[test]
    fn revoke_plugin_drops_only_its_own_surface_and_queues_no_event() {
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        assert!(!overlay.revoke_plugin("acme.other"));
        assert!(overlay.is_active());
        assert!(overlay.revoke_plugin("acme.help"));
        assert!(!overlay.is_active());
        assert_eq!(overlay.admitted_slots(), 0);
        assert!(
            overlay.drain_dismissed().is_empty(),
            "lifecycle teardown is not an Esc dismissal observation"
        );
    }

    #[test]
    fn focus_follow_binds_then_tracks_the_focused_view() {
        // Issue #2849: the surface follows the focused view and is never
        // dismissed by a plain focus switch while the bound view is live.
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        assert_eq!(overlay.session_view(), None, "unbound at admission");
        let a = ViewId::new(1);
        let b = ViewId::new(2);
        assert!(
            overlay.observe_focus(Some(a), |_| true).is_none(),
            "binding is not a dismissal"
        );
        assert_eq!(overlay.session_view(), Some(a));
        assert!(overlay.observe_focus(Some(b), |_| true).is_none());
        assert_eq!(overlay.session_view(), Some(b), "focus switch re-binds");
        assert!(overlay.is_active(), "a focus switch never dismisses");
        assert!(overlay.drain_dismissed().is_empty());
    }

    #[test]
    fn focus_follow_drops_the_surface_when_the_bound_view_closes() {
        // The closed view leaves no surface behind: one bounded `view-closed`
        // observation, no re-centering on a survivor, slot released.
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        let a = ViewId::new(1);
        let b = ViewId::new(2);
        overlay.observe_focus(Some(a), |_| true);
        let event = overlay
            .observe_focus(Some(b), |view| view != a)
            .expect("a closed view dismisses");
        assert_eq!(event.owner, "acme.help");
        assert_eq!(event.handle, 7);
        assert_eq!(event.reason, VIEW_CLOSED_DISMISS_REASON);
        assert!(!overlay.is_active(), "no surface survives the closed view");
        assert_eq!(overlay.admitted_slots(), 0, "the admitted slot is released");
        assert!(
            overlay.observe_focus(Some(b), |_| true).is_none(),
            "a dropped surface observes nothing further"
        );
        let drained = overlay.drain_dismissed();
        assert_eq!(drained.len(), 1, "exactly one observation");
        assert_eq!(drained[0].reason, "view-closed");
    }

    #[test]
    fn focus_follow_with_no_focused_view_keeps_a_live_surface() {
        // A window with no focused view is not a close: the surface waits for
        // the next focused view instead of dismissing itself.
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        let a = ViewId::new(1);
        overlay.observe_focus(Some(a), |_| true);
        assert!(overlay.observe_focus(None, |_| true).is_none());
        assert!(overlay.is_active());
        assert_eq!(overlay.session_view(), Some(a));
    }

    #[test]
    fn one_surface_per_view_per_generation() {
        // Issue #2849 bound: a generation presents at most one surface, and
        // the surface occupies exactly one view at a time.
        let mut overlay = PresentationOverlay::new();
        overlay.show("acme.help", 7).expect("owner admits");
        overlay.observe_focus(Some(ViewId::new(1)), |_| true);
        overlay
            .show("acme.help", 8)
            .expect("re-show reuses the slot");
        assert_eq!(overlay.admitted_slots(), 1);
        assert_eq!(overlay.session_view(), Some(ViewId::new(1)));
        assert_eq!(
            overlay.show("acme.other", 9).map_err(|e| e.code),
            Err(E_UI_UNAVAILABLE),
            "a second generation cannot present over the first"
        );
        assert_eq!(overlay.admitted_slots(), 1);
    }
}
