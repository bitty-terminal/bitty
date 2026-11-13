//! Per-window registry for the composition root (bitty#1765, slice 1).
//!
//! The application created exactly one OS window before this module: a single
//! `WindowState::handle` plus a single renderer surface. Multiple windows need
//! one identity-keyed row per window, so every window the app owns is
//! registered here under its [`WindowId`] and every later interaction (a
//! command, an OS event, a close) addresses the window by that identity
//! instead of by a field.
//!
//! # What a row holds
//!
//! Exactly the per-window state the app already tracked for its single
//! window: the live platform handle and the renderer attachment point
//! ([`SurfaceTarget`]) derived from it. [`WindowRegistry`] is generic over the
//! handle payload so the row semantics are pinned headlessly with a
//! constructible payload; production instantiates
//! `WindowRegistry<WindowHandle>` ([`WindowIdentity`] is implemented for
//! [`WindowHandle`]).
//!
//! # Fail-closed rules
//!
//! - Registration is bounded ([`MAX_WINDOWS`]); an over-capacity or duplicate
//!   request returns [`WindowRegistryError`] and never mutates a row.
//! - [`WindowRegistry::admit`] takes a whole window-creation *outcome*: a
//!   platform refusal leaves the registry byte-identical instead of removing a
//!   half-inserted row.
//! - Removing a window never renumbers or re-creates the survivors, and the
//!   active and focused windows fall back to a surviving row rather than to a
//!   dangling identity.
//!
//! # Focus and close (bitty#1765, slice 3)
//!
//! Attribution and focus are deliberately two states, not one. [`WindowRegistry::active`]
//! is *event* attribution — the window the last handled event named — while
//! [`WindowRegistry::focused`] is the window the compositor reports as focused, moved
//! only by `Focused(true)`/`Focused(false)` edges. A key press, a resize or an
//! occlusion edge therefore never claims focus for a background window.
//!
//! Focus is the OS's to decide: the registry never focuses a window because it
//! was created (there is no platform seam to ask for focus), it only keeps the
//! answer consistent — a new row takes focus when nothing was focused, and
//! closing the focused window hands focus to a survivor instead of leaving a
//! hole. [`WindowRegistry::close_decision`] then states what one close event means for
//! the instance: [`CloseDecision::WindowOnly`] while another window survives,
//! [`CloseDecision::LastWindow`] for the last one.
//!
//! # Honest scope
//!
//! Registering a row is not the same as presenting it. A newly admitted
//! secondary window is registered [`SurfaceState::Unattached`] and the app
//! flips it to [`SurfaceState::Attached`] only after the runtime reports that
//! it created *and* configured a wgpu surface for that window
//! (`TerminalApp::attach_secondary_surface`). A row that stays `Unattached` is
//! therefore an accurate statement — the platform refused the surface, or the
//! process has no owned GPU context to create one with — and never a window the
//! app believes it is painting.
//!
//! # Close, teardown, and drop accounting (bitty#1765, slice 4)
//!
//! Slice 3 decided what a close *means*; slice 4 applies it. [`WindowRegistry::close`]
//! removes the row and hands it back, so the caller releases that window's
//! presentation surface before the last handle drops and the OS window goes
//! away — the ordering the surface lifetime contract requires. It also names the
//! survivor that inherits the primary role, which is how a close of the first
//! window destroys only that window instead of exiting the instance.
//!
//! [`WindowRegistry::drain`] is the instance-teardown twin: every row in
//! registration order, returned to the caller for release-then-drop, so a
//! shutdown cannot leave a swap chain to process exit and cannot drop one
//! twice.
//!
//! Two monotonic counters ([`WindowRegistry::admitted`],
//! [`WindowRegistry::removed`]) make the result checkable:
//! `admitted - removed == len()` holds after any sequence of creates, refusals,
//! closes and teardowns, so repeated create/close cycles cannot leak a row
//! unnoticed.

#![forbid(unsafe_code)]

use std::fmt::{self, Write as _};

use bitty_platform::{SurfaceTarget, WindowEventKind, WindowHandle, WindowId};

/// Upper bound on simultaneously registered windows.
///
/// The platform's own OS-window ceiling
/// ([`bitty_platform::MAX_OS_WINDOWS`]): the registry and the renderer's
/// per-window surface fleet bound themselves from one constant, so they can
/// never disagree about how many windows exist. One window per workspace is
/// the accepted ceiling, and a bounded registry is what makes an unbounded
/// `new_window` request impossible.
pub(crate) const MAX_WINDOWS: usize = bitty_platform::MAX_OS_WINDOWS;

/// Renderer attachment state of one registered window's surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceState {
    /// The shared renderer owns this window's surface and presents it.
    Attached,
    /// The attachment point exists, but no renderer surface owns it yet.
    Unattached,
}

impl SurfaceState {
    /// Stable lowercase label for lifecycle logs.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Unattached => "unattached",
        }
    }
}

/// Identity of a registered window payload.
///
/// The registry never derives identity from row order: the payload itself
/// names the window. Production rows carry a [`WindowHandle`]; tests use a
/// constructible payload so the row semantics stay headless.
pub(crate) trait WindowIdentity {
    /// The window identity this payload was created for.
    fn window_id(&self) -> WindowId;
}

impl WindowIdentity for WindowHandle {
    fn window_id(&self) -> WindowId {
        self.id()
    }
}

/// One registered window: identity, live handle, and surface attachment point.
#[derive(Debug)]
pub(crate) struct WindowRow<H> {
    /// Stable identity; also the registry key.
    pub(crate) id: WindowId,
    /// Live platform handle for this window.
    pub(crate) handle: H,
    /// Renderer attachment point derived from the handle.
    ///
    /// `None` only for a payload that has no attachable window (a headless
    /// test payload), never for a production handle.
    pub(crate) surface: Option<SurfaceTarget>,
    /// Whether the shared renderer currently presents this window.
    pub(crate) state: SurfaceState,
}

/// Typed refusal from the registry itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowRegistryError {
    /// A row for this identity is already registered.
    Duplicate(WindowId),
    /// The registry already holds [`MAX_WINDOWS`] rows.
    OverCapacity {
        /// The enforced ceiling.
        max: usize,
    },
}

impl fmt::Display for WindowRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "window {} is already registered", id.get()),
            Self::OverCapacity { max } => write!(f, "window registry is at capacity ({max})"),
        }
    }
}

/// Why one window-creation request did not become a registry row.
#[derive(Debug)]
pub(crate) enum AdmitError<E> {
    /// The platform refused the request (no window system, refused by the
    /// backend, ...). No row was created and no existing row changed.
    Refused(E),
    /// The platform produced a window the registry refused to track.
    Registry(WindowRegistryError),
}

impl<E: fmt::Display> fmt::Display for AdmitError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "window creation refused: {error}"),
            Self::Registry(error) => write!(f, "{error}"),
        }
    }
}

/// How a platform event's window identity relates to the registered set.
///
/// The app still owns one renderer, one focus tree and one PTY per process, so
/// an event that belongs to a *secondary* window must not reach the primary's
/// lifecycle or chrome handling until per-window routing exists. Deciding that
/// here — instead of inside the event handler — keeps the rule headless-testable
/// with a constructible payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventTarget {
    /// The event names the primary (first registered) window.
    Primary,
    /// The event names a registered secondary window.
    Secondary,
    /// Nothing is registered, or the identity is unknown: the app keeps its
    /// single-window behaviour and the event is handled as before.
    Untracked,
}

/// Whether a window event is a focus edge, and whether it gained or lost focus
/// (bitty#1765, slice 3).
///
/// Only `Focused` events move [`WindowRegistry::focused`]: every other kind
/// names the window that produced it (a key press, a resize, an occlusion
/// edge, a close request) and updates event attribution instead. Returning
/// `None` for those keeps the rule in one place, so the event handler applies a
/// decision instead of re-deriving which events count as focus.
pub(crate) fn focus_edge(kind: &WindowEventKind) -> Option<bool> {
    match kind {
        WindowEventKind::Focused(focused) => Some(*focused),
        _ => None,
    }
}

/// What one window-close event means for the instance (bitty#1765, slice 3).
///
/// The registry answers this instead of the event handler, so the rule is
/// pinned headlessly: a close for a window that is not the last one must never
/// reach the instance exit path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseDecision {
    /// The identity is not registered: the close keeps its existing
    /// single-window path (a headless run, or a window this process never
    /// admitted).
    Unknown,
    /// Another registered window survives, so only this window closes.
    WindowOnly,
    /// This is the last registered window: the instance exits as today.
    LastWindow,
}

/// One applied close: the destroyed row, what the close meant, and the window
/// that inherits the primary role (bitty#1765, slice 4).
///
/// The registry hands the row back instead of dropping it, so the caller
/// releases that window's presentation surface *before* the handle drops and
/// the OS window goes away — the ordering the surface lifetime contract
/// requires. `promoted` names the survivor only when the closed window was the
/// first registered one, because that survivor is what
/// [`WindowRegistry::target`] will classify as
/// [`EventTarget::Primary`] from the next event on.
#[derive(Debug)]
pub(crate) struct WindowClose<H> {
    /// The removed row; dropping it releases the last handle to that window.
    pub(crate) row: WindowRow<H>,
    /// What the close meant for the instance.
    pub(crate) decision: CloseDecision,
    /// The survivor that becomes the primary window, when this close retired
    /// the primary one.
    pub(crate) promoted: Option<WindowId>,
}

/// Identity-keyed map of the windows this process owns.
///
/// Insertion order is preserved so lifecycle logs and the primary window are
/// deterministic. Rows are never renumbered: closing one window removes its
/// row and leaves every other identity untouched.
pub(crate) struct WindowRegistry<H> {
    rows: Vec<WindowRow<H>>,
    active: Option<WindowId>,
    focused: Option<WindowId>,
    /// Rows that entered the registry since it was created (bitty#1765,
    /// slice 4).
    ///
    /// Paired with [`Self::removed`] so a lifecycle log can state
    /// `admitted - removed == len()`, which is what makes "a create/close cycle
    /// leaked nothing" checkable instead of assumed.
    admitted: u64,
    /// Rows that left the registry since it was created.
    removed: u64,
}

impl<H> Default for WindowRegistry<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H> WindowRegistry<H> {
    /// Empty registry: no window is registered, none is active, none focused.
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            active: None,
            focused: None,
            admitted: 0,
            removed: 0,
        }
    }

    /// Rows that were registered since this registry was created.
    pub(crate) fn admitted(&self) -> u64 {
        self.admitted
    }

    /// Rows that were removed since this registry was created.
    pub(crate) fn removed(&self) -> u64 {
        self.removed
    }

    /// Number of registered windows.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no window is registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether another window may be registered.
    pub(crate) fn has_capacity(&self) -> bool {
        self.rows.len() < MAX_WINDOWS
    }

    /// Whether `id` is registered.
    pub(crate) fn contains(&self, id: WindowId) -> bool {
        self.rows.iter().any(|row| row.id == id)
    }

    /// The row registered for `id`.
    pub(crate) fn get(&self, id: WindowId) -> Option<&WindowRow<H>> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// Every registered identity, in registration order.
    pub(crate) fn ids(&self) -> Vec<WindowId> {
        self.rows.iter().map(|row| row.id).collect()
    }

    /// The window that currently owns OS event attribution, if any.
    pub(crate) fn active(&self) -> Option<WindowId> {
        self.active
    }

    /// Makes an already-registered window active.
    ///
    /// Returns `false` and changes nothing for an unknown identity: an OS
    /// event naming a window this process never registered can never steal
    /// attribution from a registered one.
    pub(crate) fn set_active(&mut self, id: WindowId) -> bool {
        if !self.contains(id) {
            return false;
        }
        self.active = Some(id);
        true
    }

    /// The window the compositor reports focused, if any (bitty#1765, slice 3).
    ///
    /// `None` means no window of this process holds OS focus: nothing was
    /// focused yet, the focused window reported `Focused(false)` (the user
    /// switched to another application), or every window is gone. It never
    /// names an unregistered identity.
    pub(crate) fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// The first registered window: the app's startup window while it lives.
    pub(crate) fn primary(&self) -> Option<WindowId> {
        self.rows.first().map(|row| row.id)
    }

    /// Marks an already-registered window as the focused one.
    ///
    /// Returns `false` and changes nothing for an unknown identity: a focus
    /// edge naming a window this process never registered can never move focus
    /// away from a registered one.
    pub(crate) fn focus(&mut self, id: WindowId) -> bool {
        if !self.contains(id) {
            return false;
        }
        self.focused = Some(id);
        true
    }

    /// Clears focus when `id` is the focused window.
    ///
    /// Returns whether focus was cleared. A blur naming any other window is a
    /// no-op, because focus edges can cross in flight: a platform reports the
    /// new window focused before the old one blurred, so validating the
    /// identity is what stops a stale blur from clearing a newer focus.
    pub(crate) fn blur(&mut self, id: WindowId) -> bool {
        if self.focused != Some(id) {
            return false;
        }
        self.focused = None;
        true
    }

    /// What a close event naming `id` means for the instance (bitty#1765,
    /// slice 3).
    ///
    /// [`CloseDecision::WindowOnly`] means another registered window survives,
    /// so the close must stop at that window. [`CloseDecision::LastWindow`] is
    /// the last row: the instance exits exactly as it did with one window.
    /// [`CloseDecision::Unknown`] keeps the pre-registry behaviour, which is
    /// what a headless run and a foreign identity rely on.
    pub(crate) fn close_decision(&self, id: WindowId) -> CloseDecision {
        if !self.contains(id) {
            return CloseDecision::Unknown;
        }
        if self.rows.len() == 1 {
            CloseDecision::LastWindow
        } else {
            CloseDecision::WindowOnly
        }
    }
}

/// Identity-bound operations: these derive the row key from the payload.
impl<H: WindowIdentity> WindowRegistry<H> {
    /// Classifies a platform event's window identity against the registry.
    ///
    /// `Untracked` is the fail-safe answer: with no registered window (a
    /// headless run) or an identity this process never admitted, the app keeps
    /// its single-window behaviour instead of dropping the event.
    pub(crate) fn target(&self, id: WindowId) -> EventTarget {
        match self.rows.first() {
            Some(row) if row.id == id => EventTarget::Primary,
            Some(_) if self.contains(id) => EventTarget::Secondary,
            _ => EventTarget::Untracked,
        }
    }

    /// Registers the startup window with its already-known surface state.
    ///
    /// Production calls this once, from the `Resumed` creation path, after the
    /// renderer attach has been attempted: the handle exists at that point and
    /// whether the shared renderer presents it is known.
    pub(crate) fn register_primary(
        &mut self,
        handle: H,
        surface: Option<SurfaceTarget>,
        state: SurfaceState,
    ) -> Result<WindowId, WindowRegistryError> {
        let id = handle.window_id();
        self.insert(id, handle, surface, state)?;
        Ok(id)
    }

    /// Registers one window from a whole creation *outcome*.
    ///
    /// Refusal is a value, not a panic: a `Err` outcome leaves the registry
    /// exactly as it was, so a platform without window support keeps the
    /// single-window behaviour and a typed refusal reaches the caller's log.
    ///
    /// A newly admitted window is registered [`SurfaceState::Unattached`]: the
    /// attachment point is retained, and the caller flips the row to
    /// [`SurfaceState::Attached`] once the renderer created and configured a
    /// surface for it ([`Self::mark_attached`]).
    pub(crate) fn admit<E>(
        &mut self,
        outcome: Result<(H, Option<SurfaceTarget>), E>,
    ) -> Result<WindowId, AdmitError<E>> {
        match outcome {
            Err(error) => Err(AdmitError::Refused(error)),
            Ok((handle, surface)) => {
                let id = handle.window_id();
                self.insert(id, handle, surface, SurfaceState::Unattached)
                    .map_err(AdmitError::Registry)?;
                Ok(id)
            }
        }
    }
}

/// Registry-local bookkeeping: these operations never derive a row key from
/// the payload, so they carry no identity bound.
impl<H> WindowRegistry<H> {
    /// Removes one window, returning its row.
    ///
    /// The survivors keep their identities and order. When the removed window
    /// was active, attribution moves to the first surviving row (never to a
    /// dangling identity, never to `None` while a window remains); when it was
    /// the focused window, focus moves to that same survivor, so a close can
    /// never punch a focus hole.
    ///
    /// Window-close routing uses this for a close request that belongs to a
    /// secondary window: dropping the row releases the last handle, so that OS
    /// window goes away without touching the primary's close gate. The
    /// primary's own close still runs the existing exit path.
    pub(crate) fn remove(&mut self, id: WindowId) -> Option<WindowRow<H>> {
        let index = self.rows.iter().position(|row| row.id == id)?;
        let row = self.rows.remove(index);
        self.removed = self.removed.saturating_add(1);
        if self.active == Some(id) {
            self.active = self.rows.first().map(|row| row.id);
        }
        if self.focused == Some(id) {
            self.focused = self.rows.first().map(|row| row.id);
        }
        Some(row)
    }

    /// Applies one close event: removes that window and reports what the
    /// instance must do next (bitty#1765, slice 4).
    ///
    /// `None` means the identity is not registered — the caller keeps its
    /// pre-registry close path — and nothing changes. Otherwise the row is
    /// removed exactly as [`Self::remove`] does (survivors keep their order,
    /// attribution and focus re-home) and returned to the caller so it can
    /// release that window's surface before the handle drops.
    ///
    /// `promoted` is the survivor that inherits the primary role when the first
    /// registered window closed: [`Self::target`] classifies that survivor as
    /// the primary from the next event on, and the caller promotes its surface
    /// in the same step. Closing a non-first window promotes nothing — the
    /// primary is untouched and keeps painting.
    pub(crate) fn close(&mut self, id: WindowId) -> Option<WindowClose<H>> {
        let decision = self.close_decision(id);
        if decision == CloseDecision::Unknown {
            return None;
        }
        let was_primary = self.rows.first().map(|row| row.id) == Some(id);
        let row = self.remove(id)?;
        let promoted = if was_primary {
            self.rows.first().map(|row| row.id)
        } else {
            None
        };
        Some(WindowClose {
            row,
            decision,
            promoted,
        })
    }

    /// Removes every row in registration order, returning them (bitty#1765,
    /// slice 4).
    ///
    /// Instance teardown calls this so each window's surface is released in
    /// registry order *before* the returned handles drop, which is the order
    /// the surface lifetime contract requires. Handing the rows back (rather
    /// than clearing the vector here) keeps the drop of the OS windows — and
    /// therefore of their swap chains — under the caller's control, so a
    /// teardown can release surfaces first and drop windows second.
    ///
    /// The registry ends empty: no active identity, no focused identity, and
    /// `admitted - removed == 0`.
    pub(crate) fn drain(&mut self) -> Vec<WindowRow<H>> {
        let rows = std::mem::take(&mut self.rows);
        self.removed = self.removed.saturating_add(rows.len() as u64);
        self.active = None;
        self.focused = None;
        rows
    }

    /// One-line lifecycle summary of the registered window set.
    ///
    /// `id:surface:state` per row, comma separated, in registration order.
    /// Reads the surface fields so a diagnostic can never describe a window
    /// as attached when no attachment point is registered.
    pub(crate) fn summary(&self) -> String {
        let mut out = String::new();
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let surface = if row.surface.is_some() {
                "surface"
            } else {
                "no-surface"
            };
            let _ = write!(&mut out, "{}:{surface}:{}", row.id.get(), row.state.label());
        }
        out
    }

    /// Records whether the shared renderer presents `window`'s surface
    /// (bitty#1765, slice 2).
    ///
    /// The row state is the app's honest record of what the renderer did with
    /// that window: a window whose surface the runtime created and configured
    /// is [`SurfaceState::Attached`], one whose attach was refused is
    /// [`SurfaceState::Unattached`]. Returns `false` and changes nothing for an
    /// unknown identity, so a stale event can never relabel a live window.
    pub(crate) fn set_surface_state(&mut self, id: WindowId, state: SurfaceState) -> bool {
        match self.rows.iter_mut().find(|row| row.id == id) {
            Some(row) => {
                row.state = state;
                true
            }
            None => false,
        }
    }

    /// Marks `window` as presented by the shared renderer.
    ///
    /// Convenience over [`Self::set_surface_state`] for the attach path.
    pub(crate) fn mark_attached(&mut self, id: WindowId) -> bool {
        self.set_surface_state(id, SurfaceState::Attached)
    }

    /// Shared insert path: capacity and duplicate checks happen before any
    /// mutation, so a refused insert leaves the registry untouched.
    fn insert(
        &mut self,
        id: WindowId,
        handle: H,
        surface: Option<SurfaceTarget>,
        state: SurfaceState,
    ) -> Result<(), WindowRegistryError> {
        if self.contains(id) {
            return Err(WindowRegistryError::Duplicate(id));
        }
        if !self.has_capacity() {
            return Err(WindowRegistryError::OverCapacity { max: MAX_WINDOWS });
        }
        self.rows.push(WindowRow {
            id,
            handle,
            surface,
            state,
        });
        self.admitted = self.admitted.saturating_add(1);
        if self.active.is_none() {
            self.active = Some(id);
        }
        // A window becomes the focused one only when nothing was focused: the
        // registry has no platform seam to ask for focus, so creation never
        // steals it from a window the compositor is actually showing focused.
        // The next real `Focused` edge confirms or moves it, and this fallback
        // is what keeps "registered" and "focusable" from disagreeing while a
        // window exists.
        if self.focused.is_none() {
            self.focused = Some(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Headless stand-in for a platform window handle.
    #[derive(Debug, PartialEq, Eq)]
    struct TestWindow(u64);

    impl WindowIdentity for TestWindow {
        fn window_id(&self) -> WindowId {
            WindowId::from_raw_public(self.0)
        }
    }

    fn window(raw: u64) -> TestWindow {
        TestWindow(raw)
    }

    fn id(raw: u64) -> WindowId {
        WindowId::from_raw_public(raw)
    }

    #[test]
    fn empty_registry_holds_no_window_and_no_active_identity() {
        let registry: WindowRegistry<TestWindow> = WindowRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.active(), None);
        assert!(registry.ids().is_empty());
        assert_eq!(registry.summary(), "");
    }

    #[test]
    fn second_window_registration_takes_its_own_row() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .expect("primary registers");
        let second = registry
            .admit(Ok::<_, ()>((window(2), None)))
            .expect("second window registers");
        assert_eq!((first, second), (id(1), id(2)));
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.ids(), vec![id(1), id(2)]);
        assert_eq!(
            registry.get(second).map(|row| row.state),
            Some(SurfaceState::Unattached),
            "a second window is registered without a renderer owner"
        );
        assert_eq!(
            registry.get(first).map(|row| row.state),
            Some(SurfaceState::Attached)
        );
        assert_eq!(
            registry.summary(),
            "1:no-surface:attached,2:no-surface:unattached"
        );
    }

    #[test]
    fn each_window_is_addressable_independently() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        // Attribution follows the addressed identity, not registration order.
        assert_eq!(
            registry.active(),
            Some(first),
            "the first row starts active"
        );
        assert!(registry.set_active(second));
        assert_eq!(registry.active(), Some(second));
        assert!(
            !registry.set_active(id(99)),
            "an unregistered identity never steals attribution"
        );
        assert_eq!(registry.active(), Some(second));
        // The rows themselves stay distinct payloads.
        assert_eq!(registry.get(first).map(|row| &row.handle), Some(&window(1)));
        assert_eq!(
            registry.get(second).map(|row| &row.handle),
            Some(&window(2))
        );
    }

    #[test]
    fn closing_one_window_leaves_the_other_intact() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert!(registry.set_active(second));
        let closed = registry.remove(second).expect("second row closes");
        assert_eq!(closed.id, second);
        assert_eq!(
            registry.ids(),
            vec![first],
            "the survivor keeps its identity"
        );
        assert!(registry.contains(first));
        assert!(!registry.contains(second));
        assert_eq!(
            registry.active(),
            Some(first),
            "attribution falls back to a surviving window, never to a dangling identity"
        );
        assert_eq!(
            registry.get(first).map(|row| row.state),
            Some(SurfaceState::Attached),
            "the survivor's surface state is untouched by the close"
        );
        // Removing an unknown identity is a no-op, not a corruption.
        assert!(registry.remove(id(99)).is_none());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn closing_the_primary_promotes_the_surviving_window() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert!(registry.remove(first).is_some());
        assert_eq!(registry.ids(), vec![second]);
        assert_eq!(registry.active(), Some(second));
        assert!(registry.remove(second).is_some());
        assert_eq!(
            registry.active(),
            None,
            "an empty registry has no active id"
        );
    }

    #[test]
    fn event_target_classifies_primary_secondary_and_unknown_identities() {
        let mut registry = WindowRegistry::new();
        assert_eq!(
            registry.target(id(1)),
            EventTarget::Untracked,
            "with nothing registered the app keeps single-window handling"
        );
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert_eq!(registry.target(first), EventTarget::Primary);
        assert_eq!(registry.target(second), EventTarget::Secondary);
        assert_eq!(
            registry.target(id(99)),
            EventTarget::Untracked,
            "an identity this process never admitted is never treated as ours"
        );
        assert!(registry.remove(second).is_some());
        assert_eq!(
            registry.target(second),
            EventTarget::Untracked,
            "a closed identity stops being a secondary target"
        );
        assert_eq!(registry.target(first), EventTarget::Primary);
    }

    #[test]
    fn a_refused_backend_leaves_the_registry_unchanged() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let before = registry.summary();
        let refused = registry.admit(Err::<(TestWindow, Option<SurfaceTarget>), _>(
            "no window system",
        ));
        match refused {
            Err(AdmitError::Refused(reason)) => assert_eq!(reason, "no window system"),
            other => panic!("refusal must surface as a typed error: {other:?}"),
        }
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.summary(), before);
        assert_eq!(registry.active(), Some(first));
    }

    #[test]
    fn naming_a_surface_state_flips_only_that_row() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert_eq!(
            registry.get(second).map(|row| row.state),
            Some(SurfaceState::Unattached)
        );
        assert!(
            registry.mark_attached(second),
            "a registered row accepts it"
        );
        assert_eq!(
            registry.get(second).map(|row| row.state),
            Some(SurfaceState::Attached),
            "the renderer now presents the second window"
        );
        assert_eq!(
            registry.get(first).map(|row| row.state),
            Some(SurfaceState::Attached),
            "the primary's own state is untouched"
        );
        assert_eq!(
            registry.summary(),
            "1:no-surface:attached,2:no-surface:attached"
        );
        // A surface the renderer released again demotes the row honestly.
        assert!(registry.set_surface_state(second, SurfaceState::Unattached));
        assert_eq!(
            registry.get(second).map(|row| row.state),
            Some(SurfaceState::Unattached)
        );
        assert!(
            !registry.mark_attached(id(99)),
            "an identity this process never registered is never relabelled"
        );
    }

    #[test]
    fn registration_is_bounded_and_never_renumbers() {
        let mut registry = WindowRegistry::new();
        for raw in 1..=MAX_WINDOWS as u64 {
            registry
                .admit(Ok::<_, ()>((window(raw), None)))
                .expect("registration within the bound");
        }
        assert_eq!(registry.len(), MAX_WINDOWS);
        assert!(!registry.has_capacity());
        match registry.admit(Ok::<_, ()>((window(999), None))) {
            Err(AdmitError::Registry(WindowRegistryError::OverCapacity { max })) => {
                assert_eq!(max, MAX_WINDOWS);
            }
            other => panic!("over-capacity registration must fail closed: {other:?}"),
        }
        assert_eq!(registry.len(), MAX_WINDOWS, "the refused row was not added");
        // A duplicate identity is refused even with capacity left.
        let mut small = WindowRegistry::new();
        small
            .admit(Ok::<_, ()>((window(7), None)))
            .expect("first row");
        match small.admit(Ok::<_, ()>((window(7), None))) {
            Err(AdmitError::Registry(WindowRegistryError::Duplicate(duplicate))) => {
                assert_eq!(duplicate, id(7));
            }
            other => panic!("duplicate identity must fail closed: {other:?}"),
        }
        assert_eq!(small.len(), 1);
    }

    // -----------------------------------------------------------------------
    // bitty#1765 (slice 3): per-window focus and the per-window close decision
    // -----------------------------------------------------------------------
    //
    // These pin the state model the app delegates to: which window holds OS
    // focus, how creation and destruction keep that answer consistent, and
    // what one close event means for the instance. The app layer only forwards
    // `Focused` edges and applies the decision, so the invariants that matter
    // (no focus hole, no dangling identity, never exit while a window survives)
    // are provable here without a display server.

    #[test]
    fn only_focus_events_move_window_focus() {
        // The app forwards every window event through this rule, so an event
        // that merely names a window (a key press in a background window, a
        // resize, an occlusion edge) must never be read as a focus change.
        assert_eq!(
            focus_edge(&WindowEventKind::Focused(true)),
            Some(true),
            "focus gain is the one edge that moves focus"
        );
        assert_eq!(
            focus_edge(&WindowEventKind::Focused(false)),
            Some(false),
            "focus loss is the other"
        );
        for kind in [
            WindowEventKind::Resized(bitty_platform::PhysicalSize::new(80, 24)),
            WindowEventKind::ScaleFactorChanged(bitty_platform::ScaleFactor::new_sanitized(1.5)),
            WindowEventKind::RedrawRequested,
            WindowEventKind::CursorEntered,
            WindowEventKind::CursorLeft,
            WindowEventKind::CloseRequested,
            WindowEventKind::Closed,
            WindowEventKind::Occluded(true),
            WindowEventKind::Occluded(false),
        ] {
            assert_eq!(
                focus_edge(&kind),
                None,
                "{kind:?} names a window, it does not focus one"
            );
        }
    }

    #[test]
    fn a_registered_window_starts_focused_only_when_nothing_was_focused() {
        let mut registry: WindowRegistry<TestWindow> = WindowRegistry::new();
        assert_eq!(
            registry.focused(),
            None,
            "an empty registry has no focused window"
        );

        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .expect("primary registers");
        assert_eq!(
            registry.focused(),
            Some(first),
            "the startup window is the target of focus-sensitive commands"
        );

        let second = registry
            .admit(Ok::<_, ()>((window(2), None)))
            .expect("second window registers");
        assert_eq!(
            registry.focused(),
            Some(first),
            "creation never steals focus from the window the compositor shows focused"
        );
        assert!(registry.focus(second), "a registered window can take focus");
        assert_eq!(registry.focused(), Some(second));
    }

    #[test]
    fn focus_edges_move_focus_and_unknown_identities_are_refused() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();

        assert!(registry.focus(second));
        assert!(registry.blur(second), "the focused window clears its focus");
        assert_eq!(
            registry.focused(),
            None,
            "the user moved to another application"
        );
        assert!(
            !registry.blur(second),
            "a repeated blur is a no-op, not a state change"
        );
        // Fail closed: an identity this process never admitted can neither
        // take nor clear focus.
        assert!(!registry.focus(id(99)));
        assert_eq!(registry.focused(), None);
        assert!(registry.focus(first));
        assert!(!registry.blur(id(99)));
        assert_eq!(
            registry.focused(),
            Some(first),
            "a foreign blur never clears a registered window's focus"
        );
    }

    #[test]
    fn a_stale_blur_from_the_previously_focused_window_cannot_clear_a_newer_focus() {
        // Focus edges cross in flight: a platform reports the new window
        // focused before the old one blurred. Validating the identity is what
        // keeps the late edge from clearing focus.
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();

        assert!(registry.focus(second));
        assert!(
            !registry.blur(first),
            "the old window's late blur is dropped"
        );
        assert_eq!(registry.focused(), Some(second));
        assert_eq!(
            registry.active(),
            Some(first),
            "attribution is a separate state: only focus edges move focus"
        );
    }

    #[test]
    fn closing_the_focused_window_rehomes_focus_to_a_survivor() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert!(registry.mark_attached(second));
        assert!(registry.focus(second));

        let closed = registry.remove(second).expect("the focused window closes");
        assert_eq!(closed.id, second);
        assert_eq!(
            registry.focused(),
            Some(first),
            "focus moves to the surviving window instead of leaving a hole"
        );
        assert_eq!(registry.active(), Some(first));
        assert!(!registry.contains(second), "no dangling identity survives");
        assert_eq!(
            registry.get(first).map(|row| row.state),
            Some(SurfaceState::Attached),
            "the survivor's surface state is untouched by the close"
        );
        assert_eq!(
            registry.close_decision(first),
            CloseDecision::LastWindow,
            "one window remains, so its close is the instance exit again"
        );
    }

    #[test]
    fn closing_an_unfocused_window_leaves_focus_on_its_window() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert!(registry.set_active(second));
        assert!(registry.focus(first));

        assert!(registry.remove(second).is_some());
        assert_eq!(
            registry.focused(),
            Some(first),
            "closing another window never moves focus"
        );
        assert_eq!(
            registry.active(),
            Some(first),
            "attribution falls back to the survivor, never to the closed row"
        );
        assert_eq!(registry.ids(), vec![first]);
    }

    #[test]
    fn closing_the_last_window_leaves_no_focus_and_no_registry_row() {
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        assert!(registry.remove(first).is_some());
        assert_eq!(registry.focused(), None);
        assert_eq!(registry.active(), None);
        assert!(registry.ids().is_empty());
        assert_eq!(registry.summary(), "");
        assert_eq!(
            registry.close_decision(first),
            CloseDecision::Unknown,
            "a closed identity stops deciding anything"
        );
        // The next window is focused again: a fully closed registry has no
        // remembered focus to leak into it.
        let reopened = registry.admit(Ok::<_, ()>((window(3), None))).unwrap();
        assert_eq!(registry.focused(), Some(reopened));
        assert_eq!(registry.close_decision(reopened), CloseDecision::LastWindow);
    }

    #[test]
    fn close_decision_separates_window_only_last_window_and_unknown() {
        let mut registry: WindowRegistry<TestWindow> = WindowRegistry::new();
        assert_eq!(
            registry.close_decision(id(1)),
            CloseDecision::Unknown,
            "with nothing registered the pre-registry close path owns the event"
        );
        assert_eq!(registry.primary(), None);

        let first = registry
            .register_primary(window(1), None, SurfaceState::Attached)
            .unwrap();
        assert_eq!(registry.primary(), Some(first));
        assert_eq!(
            registry.close_decision(first),
            CloseDecision::LastWindow,
            "a single window is the instance: its close exits exactly as before"
        );

        let second = registry.admit(Ok::<_, ()>((window(2), None))).unwrap();
        assert_eq!(
            registry.close_decision(first),
            CloseDecision::WindowOnly,
            "another window survives, so the close must stop at that window"
        );
        assert_eq!(registry.close_decision(second), CloseDecision::WindowOnly);
        assert_eq!(
            registry.close_decision(id(99)),
            CloseDecision::Unknown,
            "a foreign identity is never closed by this registry"
        );

        assert!(registry.remove(first).is_some());
        assert_eq!(
            registry.primary(),
            Some(second),
            "the survivor becomes the first registered window"
        );
        assert_eq!(registry.close_decision(second), CloseDecision::LastWindow);
    }

    // -----------------------------------------------------------------------
    // bitty#1765 (slice 4): close applies destruction, teardown ordering, and
    // drop accounting
    // -----------------------------------------------------------------------
    //
    // The payload here counts its own drops, which is how these tests prove
    // "exactly once" instead of assuming it: one clone per row, no hidden
    // second owner.

    /// Headless payload that reports when it is dropped.
    #[derive(Debug)]
    struct TrackedWindow {
        raw: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for TrackedWindow {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl WindowIdentity for TrackedWindow {
        fn window_id(&self) -> WindowId {
            WindowId::from_raw_public(self.raw)
        }
    }

    fn tracked(raw: u64, drops: &Arc<AtomicUsize>) -> TrackedWindow {
        TrackedWindow {
            raw,
            drops: Arc::clone(drops),
        }
    }

    #[test]
    fn closing_the_first_window_destroys_only_it_and_names_the_survivor() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .expect("primary registers");
        let second = registry
            .admit(Ok::<_, ()>((tracked(2, &drops), None)))
            .expect("second window registers");
        let third = registry
            .admit(Ok::<_, ()>((tracked(3, &drops), None)))
            .expect("third window registers");
        assert_eq!(drops.load(Ordering::SeqCst), 0, "no row was dropped yet");

        let closed = registry.close(first).expect("the primary close applies");

        assert_eq!(closed.decision, CloseDecision::WindowOnly);
        assert_eq!(closed.row.id, first);
        assert_eq!(
            closed.promoted,
            Some(second),
            "the first survivor inherits the primary role"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the closed row is returned, so its handle drops only when the caller lets it"
        );
        drop(closed);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the closed window's handle is dropped exactly once"
        );

        assert_eq!(registry.ids(), vec![second, third]);
        assert_eq!(
            registry.target(second),
            EventTarget::Primary,
            "the survivor is the primary from the next event on"
        );
        assert_eq!(registry.target(third), EventTarget::Secondary);
        assert_eq!(registry.admitted(), 3);
        assert_eq!(registry.removed(), 1);
        assert_eq!(
            registry.admitted() - registry.removed(),
            registry.len() as u64,
            "the counters account for every row"
        );
    }

    #[test]
    fn closing_a_secondary_window_promotes_nothing() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .unwrap();
        let second = registry
            .admit(Ok::<_, ()>((tracked(2, &drops), None)))
            .unwrap();

        let closed = registry.close(second).expect("the secondary close applies");

        assert_eq!(closed.decision, CloseDecision::WindowOnly);
        assert_eq!(
            closed.promoted, None,
            "the primary keeps its role: only the named window is destroyed"
        );
        drop(closed);
        assert_eq!(registry.ids(), vec![first]);
        assert_eq!(registry.target(first), EventTarget::Primary);
        assert_eq!(registry.primary(), Some(first));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closing_the_last_window_reports_the_exit_decision() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        let only = registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .unwrap();

        let closed = registry.close(only).expect("the last close applies");

        assert_eq!(
            closed.decision,
            CloseDecision::LastWindow,
            "the caller keeps its existing instance-exit path"
        );
        assert_eq!(
            closed.promoted, None,
            "nothing survives to inherit the primary role"
        );
        drop(closed);
        assert!(registry.is_empty());
        assert_eq!(registry.primary(), None);
        assert_eq!(registry.focused(), None);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closing_an_unregistered_identity_changes_nothing() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .unwrap();
        let before = registry.summary();

        assert!(
            registry.close(id(99)).is_none(),
            "a foreign identity is never closed by this registry"
        );

        assert_eq!(registry.summary(), before);
        assert_eq!(registry.admitted(), 1);
        assert_eq!(registry.removed(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(registry.target(first), EventTarget::Primary);
    }

    #[test]
    fn teardown_drains_rows_in_registration_order_and_drops_each_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        let first = registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .unwrap();
        let second = registry
            .admit(Ok::<_, ()>((tracked(2, &drops), None)))
            .unwrap();
        let third = registry
            .admit(Ok::<_, ()>((tracked(3, &drops), None)))
            .unwrap();

        let rows = registry.drain();

        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first, second, third],
            "teardown hands the rows back in registry order, so surfaces are released in that order"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "handles are still alive: the caller releases surfaces before dropping windows"
        );
        assert!(registry.is_empty());
        assert_eq!(registry.active(), None);
        assert_eq!(registry.focused(), None);
        assert_eq!(registry.summary(), "");
        assert_eq!(registry.admitted(), 3);
        assert_eq!(registry.removed(), 3);
        assert_eq!(
            registry.admitted() - registry.removed(),
            0,
            "the counters balance after teardown"
        );

        drop(rows);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            3,
            "every window handle is dropped exactly once"
        );
        assert!(registry.drain().is_empty(), "a second teardown is a no-op");
        assert_eq!(registry.removed(), 3);
    }

    #[test]
    fn repeated_create_close_cycles_leak_neither_a_row_nor_a_double_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        for cycle in 0..50u64 {
            let id = registry
                .register_primary(tracked(1_000 + cycle, &drops), None, SurfaceState::Attached)
                .expect("registration within the bound");
            assert_eq!(registry.len(), 1, "cycle {cycle} holds one row");
            let closed = registry.close(id).expect("the row closes");
            assert_eq!(closed.decision, CloseDecision::LastWindow);
            drop(closed);
            assert!(registry.is_empty(), "cycle {cycle} left no row");
            assert!(
                registry.has_capacity(),
                "cycle {cycle} gave its capacity back"
            );
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            50,
            "one drop per created window — a double drop would exceed 50"
        );
        assert_eq!(registry.admitted(), 50);
        assert_eq!(registry.removed(), 50);
    }

    #[test]
    fn a_refused_creation_moves_no_counter() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = WindowRegistry::new();
        registry
            .register_primary(tracked(1, &drops), None, SurfaceState::Attached)
            .unwrap();
        let (admitted, removed) = (registry.admitted(), registry.removed());

        let refused = registry.admit(Err::<(TrackedWindow, Option<SurfaceTarget>), _>(
            "no window system",
        ));

        assert!(matches!(refused, Err(AdmitError::Refused(_))));
        assert_eq!(
            (registry.admitted(), registry.removed()),
            (admitted, removed)
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "a refused creation never produced a window to drop"
        );
        // The duplicate-identity refusal is the other cancel path.
        let duplicate = registry.admit(Ok::<_, ()>((tracked(1, &drops), None)));
        assert!(matches!(duplicate, Err(AdmitError::Registry(_))));
        assert_eq!(
            (registry.admitted(), registry.removed()),
            (admitted, removed)
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the duplicate payload drops once"
        );
    }
}
