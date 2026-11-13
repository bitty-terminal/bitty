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
//!   active window falls back to a surviving row rather than to `None`.
//!
//! # Honest scope
//!
//! Registering a row is not the same as presenting it. Only the primary
//! window carries a renderer surface in this slice
//! ([`SurfaceState::Attached`]); a second row is registered
//! [`SurfaceState::Unattached`] with its attachment point retained, because
//! per-window renderer/swapchain work is explicitly out of this slice.

#![forbid(unsafe_code)]

use std::fmt::{self, Write as _};

use bitty_platform::{SurfaceTarget, WindowHandle, WindowId};

/// Upper bound on simultaneously registered windows.
///
/// Mirrors `bitty_runtime::MAX_WORKSPACES_PER_WINDOW`: one window per
/// workspace is the accepted ceiling, and a bounded registry is what makes an
/// unbounded `new_window` request impossible.
pub(crate) const MAX_WINDOWS: usize = 16;

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

/// Identity-keyed map of the windows this process owns.
///
/// Insertion order is preserved so lifecycle logs and the primary window are
/// deterministic. Rows are never renumbered: closing one window removes its
/// row and leaves every other identity untouched.
pub(crate) struct WindowRegistry<H> {
    rows: Vec<WindowRow<H>>,
    active: Option<WindowId>,
}

impl<H> Default for WindowRegistry<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H> WindowRegistry<H> {
    /// Empty registry: no window is registered and none is active.
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            active: None,
        }
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
}

/// Identity-bound operations: these derive the row key from the payload.
impl<H: WindowIdentity> WindowRegistry<H> {
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
    /// attachment point is retained, but no renderer surface owns it yet
    /// (per-window renderer work is out of this slice).
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
    /// dangling identity, never to `None` while a window remains).
    ///
    /// Window-close routing is not wired to this call yet (a single-window
    /// process exits on close), so it is exercised by the registry tests that
    /// pin "closing one window leaves the other intact".
    // Wired by the close-routing slice; the semantics are pinned by tests now.
    #[allow(dead_code)]
    pub(crate) fn remove(&mut self, id: WindowId) -> Option<WindowRow<H>> {
        let index = self.rows.iter().position(|row| row.id == id)?;
        let row = self.rows.remove(index);
        if self.active == Some(id) {
            self.active = self.rows.first().map(|row| row.id);
        }
        Some(row)
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
        if self.active.is_none() {
            self.active = Some(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
