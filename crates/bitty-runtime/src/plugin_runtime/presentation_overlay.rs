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
//! - **Teardown is Core-owned.** Suspend, unload, disable, crash, rollback,
//!   and generation change drop the session, so no stale surface survives.

use std::collections::VecDeque;

use bitty_lua::{BridgeError, E_UI_UNAVAILABLE};
use bitty_ui::panel::{MAX_OVERLAYS_PER_WINDOW, OverlayKind, OverlayManager};
use bitty_ui::{OverlayTier, Rect};

/// Paint tier of a presentation overlay (accepted v1 design).
pub const PRESENTATION_OVERLAY_TIER: OverlayTier = OverlayTier::Popup;

/// Admission kind of a presentation overlay: never modal.
pub const PRESENTATION_OVERLAY_KIND: OverlayKind = OverlayKind::NonModal;

/// Core-reported reason for a presentation overlay dismissed by `Esc`.
pub const ESCAPE_DISMISS_REASON: &str = "escape";

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

/// The live presentation session (owner + presented handle).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PresentationSession {
    plugin_id: String,
    handle: i64,
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
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let event = DismissedEvent {
            owner: session.plugin_id.clone(),
            handle: session.handle,
            reason: ESCAPE_DISMISS_REASON.to_string(),
        };
        if self.pending_dismissed.len() >= PENDING_DISMISSED_MAX {
            self.pending_dismissed.pop_front();
            self.dismissed_dropped = self.dismissed_dropped.saturating_add(1);
        }
        self.pending_dismissed.push_back(event);
        self.clear();
        true
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
}
