#![forbid(unsafe_code)]
//! The shared-runtime adapter for the panel content backend (bitty#2388,
//! CTX-1909).
//!
//! [`PanelContentBackend`] is the synchronous seam between one plugin
//! generation's `bitty.ui.panel.*` verbs and the runtime that owns the layout,
//! the panel host, and the displaced-terminal record. This module is the one
//! in-tree implementation: [`RuntimePanelContentBackend`] over a shared
//! [`Runtime`] handle.
//!
//! # Why a shared handle
//!
//! The verbs are synchronous and typed: `present` must either hand back a live
//! handle or refuse with the record's code, because a deferred answer would
//! report success for a presentation that may never happen. Only the runtime
//! that owns the focused leaf and its present frame can answer that, and the
//! plugin call happens deep inside the VM. The composition root therefore hands
//! the backend the same `Rc<RefCell<Runtime>>` shape the other live-runtime
//! seams use (`PanelPinHandler`, `ConfigCommitWindow`), and a reentrant call —
//! one that arrives while the runtime is already borrowed — fails closed with
//! [`E_UI_UNAVAILABLE`] instead of panicking.
//!
//! # Nothing here is authority
//!
//! The adapter forwards and reports. It resolves no panel, grants nothing,
//! records nothing, and mutates nothing on its own: the owner/generation fence,
//! the one-layer rules, and every refusal live in
//! [`Runtime::ui_panel_present`](crate::runtime::Runtime::ui_panel_present) and
//! its siblings, which are also the only place the Core swap is reached from.
//! A host that installs no backend (or a generation without `ui.panel`) never
//! reaches this type at all.

use std::rc::Rc;

use bitty_lua::{BridgeError, E_UI_UNAVAILABLE, UiPanelSpec};
use bitty_plugin_host::manifest::PluginId;

use super::services::PanelContentBackend;
use crate::runtime::Runtime;

/// [`PanelContentBackend`] over a shared [`Runtime`].
///
/// Constructed by the composition root before activation and installed with
/// [`PluginRuntime::set_panel_content_backend`](super::PluginRuntime::set_panel_content_backend);
/// tests install the same type over a headless runtime to drive the production
/// chain from Lua source down to the Core swap.
pub struct RuntimePanelContentBackend {
    runtime: Rc<std::cell::RefCell<Runtime>>,
}

impl RuntimePanelContentBackend {
    /// Wrap the shared runtime handle.
    #[must_use]
    pub fn new(runtime: Rc<std::cell::RefCell<Runtime>>) -> Self {
        Self { runtime }
    }

    /// Borrow the runtime mutably, or refuse like every other unhosted surface.
    ///
    /// A borrow conflict means the runtime is already mutating — a reentrant
    /// plugin call from inside a runtime callback. Refusing is the fail-closed
    /// direction: nothing is presented, nothing is restored, and the caller sees
    /// the same code it would see with no backend at all.
    fn borrow_mut(&self) -> Result<std::cell::RefMut<'_, Runtime>, BridgeError> {
        self.runtime.try_borrow_mut().map_err(|_| {
            BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "the runtime is borrowed by an in-flight call",
            )
        })
    }
}

impl PanelContentBackend for RuntimePanelContentBackend {
    fn present(
        &self,
        owner: &PluginId,
        generation: u32,
        spec: &UiPanelSpec,
        node: &bitty_lua::UiRichNode,
    ) -> Result<i64, BridgeError> {
        self.borrow_mut()?
            .ui_panel_present(owner, generation, spec, node)
    }

    fn update(
        &self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        node: &bitty_lua::UiRichNode,
    ) -> Result<bool, BridgeError> {
        self.borrow_mut()?
            .ui_panel_update(owner, generation, handle, node)
    }

    fn release(
        &self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        reason: &str,
    ) -> Result<bool, BridgeError> {
        self.borrow_mut()?
            .ui_panel_release(owner, generation, handle, reason)
    }

    fn poll(
        &self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        max: usize,
    ) -> Result<bitty_lua::PanelPoll, BridgeError> {
        self.borrow_mut()?
            .ui_panel_poll(owner, generation, handle, max)
    }

    fn release_owner(&self, owner: &PluginId, generation: u32, reason: &str) -> usize {
        match self.borrow_mut() {
            Ok(mut runtime) => runtime.ui_panel_release_owner(owner, generation, reason),
            // The generation edge runs while the runtime may be mid-call. The
            // Core edge releases the record by binding anyway, and the entry is
            // pruned when that release reaches the runtime, so nothing is
            // stranded: reporting zero here is honest, not a lost release.
            Err(_) => 0,
        }
    }

    fn drain_events(&self) -> Vec<crate::runtime::ui_panel::PanelContentEvent> {
        match self.borrow_mut() {
            Ok(mut runtime) => runtime.drain_panel_content_events(),
            // A borrowed runtime has nothing to hand over right now; the
            // observations stay queued (bounded) for the next cold tick instead
            // of being dropped.
            Err(_) => Vec::new(),
        }
    }
}
