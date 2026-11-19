#![forbid(unsafe_code)]
//! Plugin-facing panel content presentation (`bitty.ui.panel.*`, bitty#2388,
//! CTX-1909).
//!
//! Contract source: `specifications/panel-gui-content-presentation-candidate.md`
//! in `bitty-terminal-docs` (draft candidate, decision-ready), sections
//! "Recommended API shape (candidate v2)", "Per-placement and limits", and
//! "Focus, close, and fail-closed paths".
//!
//! The Core swap this module exposes already landed (CTX-1895):
//! [`Runtime::present_panel_content`](Runtime::present_panel_content) changes a
//! terminal-backed panel's content kind, freezes the displaced terminal's
//! geometry, and [`Runtime::release_panel_content`](Runtime::release_panel_content)
//! restores it. This module adds the *plugin* half the record also asks for:
//! a host-derived handle, the owner identity the Core record deliberately does
//! not carry, the recorded placement hint, and the typed refusals the record
//! names.
//!
//! # What this layer owns, and what it does not
//!
//! | Concern | Owner |
//! |---|---|
//! | Which panel the caller presents into | **Core**, resolved from focus. The plugin supplies no `PanelId`, `ViewId`, coordinate, size, or surface handle |
//! | The content swap, the displaced record, the geometry freeze, the input gate | Core (CTX-1895), untouched here |
//! | The handle a plugin holds, its `PluginId`/generation, the placement hint | this module ([`UiPanelLayerRecord`]) |
//! | Whether a placement is *applied* | nobody: the hint is **recorded, not applied** in v1 (record "hint only") |
//!
//! # Typed refusals
//!
//! Every refusal happens before any mutation and uses the record's spelling:
//!
//! | Case | Code |
//! |---|---|
//! | the focused panel already presents another owner's layer | [`E_UI_PANEL_BUSY`] |
//! | this generation already presents | [`E_UI_PANEL_NESTED`] |
//! | no focused panel, no panel there, or not terminal-backed | [`E_UI_PANEL_UNAVAILABLE`] |
//! | no panel host installed (headless/no-surface host) | [`E_UI_UNAVAILABLE`] |
//! | a stale, foreign, or already-released handle | [`E_UI_NOT_OWNER`] |
//! | an unusable scene | `E_UI_COMPONENT_INVALID` |
//!
//! # Handle lifetime
//!
//! A handle dies with its layer. The Core record is released by exactly three
//! paths — the owner's token release, the generation edge, and panel disposal —
//! and this module's entry is pruned on the first two
//! ([`Runtime::release_panel_content`] and
//! [`Runtime::restore_released_panel_presentation`]), so a handle can never
//! outlive what it names. A handle whose Core record is gone answers
//! [`E_UI_NOT_OWNER`] (and drops its entry) instead of restoring a terminal
//! somebody else now owns.

use bitty_lua::{
    BridgeError, E_UI_NOT_OWNER, E_UI_PANEL_BUSY, E_UI_PANEL_NESTED, E_UI_PANEL_UNAVAILABLE,
    E_UI_UNAVAILABLE, UiPanelSpec,
};
use bitty_plugin_host::manifest::PluginId;
use bitty_rich::scene::{BlockId, Scene};
use bitty_ui::ViewId;
use bitty_ui::panel::PanelId;

use super::Runtime;
use crate::registry::{PanelContentToken, PanelError, ViewContent};

/// One layer presented through `bitty.ui.panel.present`.
///
/// The row exists for the three facts the Core record does not hold: who
/// presented (owner + activation generation), the handle the caller was given,
/// and the placement hint the caller asked for. Everything about the swap
/// itself lives in [`crate::registry::PresentedPanelContent`], which stays the
/// single restore authority — this row never restores anything on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPanelLayerRecord {
    /// Host-derived handle the caller holds. Never supplied by the plugin, never
    /// reused after release.
    pub handle: i64,
    /// Plugin that presented the layer.
    pub owner: PluginId,
    /// Activation generation that presented it. One generation presents at most
    /// one layer (`E_UI_PANEL_NESTED` otherwise).
    pub generation: u32,
    /// Panel container the layer was presented on, resolved from focus at
    /// present time.
    pub panel: PanelId,
    /// Attachment view of that panel.
    pub view: ViewId,
    /// The Core record's opaque token: the only value that can release the
    /// layer, held here so the caller never sees it.
    pub token: PanelContentToken,
    /// The validated descriptor exactly as the caller sent it, recorded and
    /// **not applied** (`placement` is a hint in v1).
    pub spec: UiPanelSpec,
}

impl Runtime {
    /// Presents `node` as the focused panel's content and returns the
    /// caller's handle (bitty#2388 `bitty.ui.panel.present`).
    ///
    /// The target is resolved here and nowhere else: the focused leaf, whose
    /// panel must be terminal-backed. The plugin cannot name a panel, a
    /// coordinate, a size, or a surface, and the displaced terminal keeps
    /// running hidden exactly as the Core swap already guarantees.
    ///
    /// `spec` is recorded on the returned row and the placement hint is not
    /// applied: the panel keeps the placement the user gave it, which is the
    /// record's "hint only" rule, not an omission.
    ///
    /// # Errors
    ///
    /// [`E_UI_UNAVAILABLE`] when no panel host is installed or the shared host
    /// is borrowed (reentrant plugin call); [`E_UI_PANEL_NESTED`] when this
    /// generation already presents; [`E_UI_PANEL_BUSY`] when the panel already
    /// presents another owner's layer; [`E_UI_PANEL_UNAVAILABLE`] when there is
    /// no focused panel, the panel is not terminal-backed, or it has no present
    /// frame; `E_UI_COMPONENT_INVALID` when the scene is refused by admission.
    /// Every refusal changes nothing: no record, no hint, no handle, no freeze.
    pub fn ui_panel_present(
        &mut self,
        owner: &PluginId,
        generation: u32,
        spec: &UiPanelSpec,
        node: &bitty_lua::UiRichNode,
    ) -> Result<i64, BridgeError> {
        if self.scene_host.is_none() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        }
        // One layer per generation, checked before the panel so a plugin that
        // re-opens its own panel gets the nesting diagnosis rather than a busy
        // one about itself.
        if self
            .ui_panel_layers
            .values()
            .any(|layer| layer.owner == *owner && layer.generation == generation)
        {
            return Err(BridgeError::new(
                "runtime",
                E_UI_PANEL_NESTED,
                "this plugin generation already presents a panel content layer",
            ));
        }
        let Some(view) = self.focused_view() else {
            return Err(panel_unavailable("no focused panel to present into"));
        };
        let Some(panel) = self.ui_panel_target(view)? else {
            return Err(panel_busy());
        };
        if !matches!(
            self.panel_content_kind(view),
            Some(ViewContent::Terminal(_))
        ) {
            return Err(panel_unavailable(
                "the focused panel is not terminal-backed",
            ));
        }
        let scene = self.ui_panel_scene(panel, owner, generation, node)?;
        let presented = self
            .present_panel_content(view, scene)
            .map_err(|error| match error {
                PanelError::ContentUnavailable { reason, .. } => panel_unavailable(reason),
                PanelError::ContentBusy { .. } => panel_busy(),
                other => BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content presentation refused: {other}"),
                ),
            })?;
        let handle = self.next_ui_panel_handle;
        self.next_ui_panel_handle = self.next_ui_panel_handle.saturating_add(1);
        self.ui_panel_layers.insert(
            handle,
            UiPanelLayerRecord {
                handle,
                owner: owner.clone(),
                generation,
                panel,
                view,
                token: presented.token,
                spec: *spec,
            },
        );
        Ok(handle)
    }

    /// Replaces the content of the layer `handle` names (bitty#2388
    /// `bitty.ui.panel.update`).
    ///
    /// Only the painted scene changes: the panel, its attachment, its
    /// placement, the displaced terminal, and the frozen geometry are exactly
    /// as the present left them, so an update can never be used to move a
    /// panel or to resize a hidden PTY.
    ///
    /// # Errors
    ///
    /// [`E_UI_NOT_OWNER`] for a handle this generation does not hold (stale,
    /// foreign, already released, or whose Core record is gone — the entry is
    /// dropped, so a repeat answers the same way); `E_UI_COMPONENT_INVALID` for
    /// a refused scene; [`E_UI_UNAVAILABLE`] when no panel host is installed.
    /// A refusal keeps the last good content.
    pub fn ui_panel_update(
        &mut self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        node: &bitty_lua::UiRichNode,
    ) -> Result<bool, BridgeError> {
        let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
            return Err(not_owner());
        };
        if layer.owner != *owner || layer.generation != generation {
            return Err(not_owner());
        }
        let Some(host) = self.scene_host.clone() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        };
        let scene = self.ui_panel_scene(layer.panel, owner, generation, node)?;
        let replaced = match host.try_borrow_mut() {
            Ok(mut host) => host.replace_presented_content_scene(layer.view, layer.token, scene),
            Err(_) => Err(PanelError::ContentUnavailable {
                view_id: layer.view,
                reason: "panel host is borrowed",
            }),
        };
        match replaced {
            Ok(()) => {}
            Err(
                PanelError::ContentNotPresented { .. } | PanelError::ContentTokenMismatch { .. },
            ) => {
                // The record left between the lookup and the write: the layer
                // was released (owner path, generation edge, or disposal), so
                // the handle names nothing and must not come back to life.
                self.ui_panel_layers.remove(&handle);
                return Err(not_owner());
            }
            Err(PanelError::ContentUnavailable { reason, .. }) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content update refused: {reason}"),
                ));
            }
            Err(other) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content update refused: {other}"),
                ));
            }
        }
        // The layer's painted content moved: drop the retained leaf primitives
        // so the next frame is a full paint of the new scene rather than a diff
        // against the old one.
        self.presented_leaf_frames.remove(&layer.view);
        self.pending_full_redraw = true;
        Ok(true)
    }

    /// Releases the layer `handle` names and restores the terminal it displaced
    /// (bitty#2388 `bitty.ui.panel.release`).
    ///
    /// The restore is the Core token release, so it keeps every guarantee that
    /// path already has: the recorded content kind, the same panel and
    /// attachment, the prior presentation mode, and exactly one resize when the
    /// leaf's content rect moved while the layer was up.
    ///
    /// `reason` is the caller's validated disposition; it is accepted here for
    /// the release audit row this slice does not add (see the pull request's
    /// residual list) and never changes what is restored.
    ///
    /// # Errors
    ///
    /// [`E_UI_NOT_OWNER`] for a handle this generation does not hold, including
    /// a second release of the same handle (the record is gone and the entry is
    /// dropped); [`E_UI_PANEL_UNAVAILABLE`] when the panel is no longer mounted;
    /// [`E_UI_UNAVAILABLE`] when no panel host is installed. A refusal restores
    /// nothing and leaves a live layer presented.
    pub fn ui_panel_release(
        &mut self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        reason: &str,
    ) -> Result<bool, BridgeError> {
        let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
            return Err(not_owner());
        };
        if layer.owner != *owner || layer.generation != generation {
            return Err(not_owner());
        }
        let _ = reason;
        match self.release_panel_content(layer.view, layer.token) {
            // `release_panel_content` prunes the row itself (one prune point
            // for every release path), so nothing else to drop here.
            Ok(_) => Ok(true),
            Err(
                PanelError::ContentNotPresented { .. } | PanelError::ContentTokenMismatch { .. },
            ) => {
                self.ui_panel_layers.remove(&handle);
                Err(not_owner())
            }
            Err(PanelError::ContentUnavailable { reason, .. }) => {
                self.ui_panel_layers.remove(&handle);
                Err(panel_unavailable(reason))
            }
            Err(other) => Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                format!("panel content release refused: {other}"),
            )),
        }
    }

    /// Releases every layer `generation` of `owner` presented (bitty#2388
    /// "Crash/disable/reload/suspend: teardown-ordered release").
    ///
    /// Called by the generation edge before the placeholder is presented, so a
    /// dead generation's layer never outlives it and the panel comes back to
    /// its terminal instead of staying covered by content nobody owns. A layer
    /// the Core generation edge already released (it releases by binding, not
    /// by token) is simply forgotten: the entry is dropped and no second
    /// restore runs. Returns how many layers this call released.
    pub fn ui_panel_release_owner(&mut self, owner: &PluginId, generation: u32) -> usize {
        let handles: Vec<i64> = self
            .ui_panel_layers
            .values()
            .filter(|layer| layer.owner == *owner && layer.generation == generation)
            .map(|layer| layer.handle)
            .collect();
        let mut released = 0;
        for handle in handles {
            let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
                continue;
            };
            match self.release_panel_content(layer.view, layer.token) {
                Ok(_) => released += 1,
                // Already gone (the Core generation edge owns that path) or the
                // panel is disposed: either way the row must not survive.
                Err(_) => {
                    self.ui_panel_layers.remove(&handle);
                }
            }
        }
        released
    }

    /// The layer `handle` names, if this runtime still holds it.
    ///
    /// Read-only projection for diagnostics and tests: it carries the owner,
    /// the generation, the attachment, and the recorded descriptor, and it
    /// exposes no terminal content, no PTY, and no key bytes.
    #[must_use]
    pub fn ui_panel_layer(&self, handle: i64) -> Option<&UiPanelLayerRecord> {
        self.ui_panel_layers.get(&handle)
    }

    /// Number of plugin-presented layers this runtime holds.
    #[must_use]
    pub fn ui_panel_layer_count(&self) -> usize {
        self.ui_panel_layers.len()
    }

    /// Drops the plugin row of a released Core layer.
    ///
    /// Every Core release path funnels here through
    /// [`Runtime::release_panel_content`], so a handle can never outlive the
    /// record it names (see the module's "Handle lifetime").
    pub(super) fn forget_ui_panel_layer(&mut self, token: PanelContentToken) {
        self.ui_panel_layers.retain(|_, layer| layer.token != token);
    }

    /// Resolves the panel the present would attach to, refusing a panel that
    /// already presents a layer.
    ///
    /// Returns `Ok(None)` when the panel already presents a content layer —
    /// whoever owns it; the one-layer-per-generation rule was checked first, so
    /// a `None` here is always another owner's layer — and the caller reports
    /// `E_UI_PANEL_BUSY` without a second host borrow.
    fn ui_panel_target(&self, view: ViewId) -> Result<Option<PanelId>, BridgeError> {
        let Some(host) = self.scene_host.as_ref() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        };
        let host = match host.try_borrow() {
            Ok(host) => host,
            Err(_) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    "panel host is borrowed",
                ));
            }
        };
        if host.presented_content_for_view(view).is_some() {
            return Ok(None);
        }
        match host.content_panel_for_view(view) {
            Ok(panel) => Ok(Some(panel)),
            Err(_) => Err(panel_unavailable("no panel is mounted at the focused view")),
        }
    }

    /// Builds one admitted scene for a layer from the bridge's validated tree.
    ///
    /// The block id is the panel's: a layer replaces that panel's content, so a
    /// stable id per container keeps the paint slot addressable across updates.
    /// Admission refusals are `E_UI_COMPONENT_INVALID` — the record's code for
    /// a scene the host will not paint — while a missing admission path stays
    /// [`E_UI_UNAVAILABLE`].
    fn ui_panel_scene(
        &self,
        panel: PanelId,
        owner: &PluginId,
        generation: u32,
        node: &bitty_lua::UiRichNode,
    ) -> Result<Scene, BridgeError> {
        crate::scene_host::build_panel_scene(BlockId(panel.get()), owner.as_str(), generation, node)
            .map_err(|error| {
                if error.code == "E_SCENE_REFUSED" {
                    bitty_lua::ui::component_invalid(error.message)
                } else {
                    error
                }
            })
    }
}

/// `E_UI_PANEL_BUSY`: the panel already presents somebody's layer.
fn panel_busy() -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_PANEL_BUSY,
        "the focused panel already presents a content layer",
    )
}

/// `E_UI_PANEL_UNAVAILABLE`: no usable target panel.
fn panel_unavailable(reason: impl Into<String>) -> BridgeError {
    BridgeError::new("runtime", E_UI_PANEL_UNAVAILABLE, reason)
}

/// `E_UI_NOT_OWNER`: the handle is not this generation's live layer.
fn not_owner() -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_NOT_OWNER,
        "handle is not a live panel content layer of this plugin generation",
    )
}
