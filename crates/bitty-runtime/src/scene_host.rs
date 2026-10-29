#![forbid(unsafe_code)]
//! Core-owned scene host for the `terminal` UI slot (CTX-1641 / bitty#2785).
//!
//! The accepted `ui.mount` slot set reserves `terminal` for "content attached
//! to a terminal view" (Plugin API v1 Lua Surface RFC). This module is the
//! production owner of that slot's scene path:
//!
//! - the composition root constructs [`SceneHost`] over the same
//!   [`PanelRuntime`] that owns panel identity, mounts one Core-owned `Rich`
//!   panel on the leaf the host is told to present on, and installs the host
//!   into [`Runtime`](crate::Runtime) so the present loop paints the attached
//!   `Scene` (CTX-1630);
//! - `bitty.ui.mount("terminal", …)` and `bitty.ui.update(…)` route their
//!   validated [`UiRichNode`] here; the node becomes one admitted
//!   [`RichBlock`](bitty_rich::scene::RichBlock) through
//!   [`scene_bridge::mount_rich_node`](crate::scene_bridge::mount_rich_node)
//!   and attaches with [`PanelRuntime::attach_panel_scene`], so `SCN-1..5`
//!   run at the same admission boundary as every other rich producer;
//! - suspend, dispose, and reload release the generation's scene and panel
//!   through [`SceneHost::release_owner`], so no stale scene survives a
//!   plugin reload.
//!
//! # Authority
//!
//! The plugin selects no view, panel, workspace, or tier: Core resolves the
//! placement target (the app publishes the focused leaf id), creates the
//! scene-host panel, and owns the attach. The plugin only supplies a bounded
//! declarative tree and receives the generation-owned handle the registry
//! minted. Nothing here decodes an image, allocates a renderer resource, or
//! executes a callback.
//!
//! # Image nodes
//!
//! [`UiRichNode::Image`] maps to `SceneNode::Image` exactly like the other
//! kinds, but the present loop only *counts* the placement reference
//! ([`SceneFrameReport::images`](crate::SceneFrameReport)). Forwarding an
//! image node to the admitted image plane is a separate slice (OQ-051 image
//! forwarding); this host neither decodes nor blits.
//!
//! # One owner
//!
//! The `terminal` slot has one scene-host panel at a time. A second plugin
//! generation that mounts while another generation's scene is attached fails
//! closed with `E_UI_UNAVAILABLE` instead of silently replacing it (the
//! accepted conflict rule: a later contribution never displaces an earlier
//! one). The owner releases the slot when its scene detaches or its
//! generation ends.
//!
//! # Target binding
//!
//! Core republishes the focused leaf into the shared target cell every tick,
//! but the host reads that cell only when a mount runs
//! ([`SceneHost::attach`]): `ui.update` and the present loop keep the binding
//! the last mount resolved. A focus change therefore moves the scene on the
//! owning generation's next mount, not on the tick that moved focus; nothing
//! here polls the cell, so there is no per-tick re-bind and no timer.
//!
//! # Budgets
//!
//! `SCN-1..3` are enforced by the reader and [`Scene::insert`], `SCN-4` by the
//! one scene's block bytes, and `SCN-5` is trivially satisfied: one panel slot
//! per window carries exactly one block per owning generation. A remount
//! replaces the previous block instead of accumulating one per call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bitty_lua::UiRichNode;
use bitty_lua::host::{BridgeError, E_UI_UNAVAILABLE};
use bitty_rich::scene::{BlockAnchor, BlockId, Scene, ScrollBehavior};
use bitty_ui::ViewId;
use bitty_ui::panel::PanelType;

use crate::registry::{PanelError, PanelHandle, PanelRuntime};
use crate::scene_bridge::{RichMount, SceneBridgeError, mount_rich_node};

/// Error code for a scene branch that cannot serve the mount.
///
/// Reuses the accepted v1 `runtime`-class code: an unhosted or occupied
/// surface is exactly what `E_UI_UNAVAILABLE` reports, and the scene path
/// adds no plugin-visible vocabulary.
pub const E_SCENE_UNAVAILABLE: &str = E_UI_UNAVAILABLE;

/// Error code for a rich tree the accepted scene admission refuses.
///
/// The tree already passed the `ui.rich` reader; this code reports the
/// `SCN-1..5` refusal at the same `validation` class the reader uses, so a
/// plugin sees one component error surface rather than a new code.
pub const E_SCENE_REFUSED: &str = "E_UI_COMPONENT_INVALID";

/// Bit 32 of a UI handle: set only by [`SceneHost`].
///
/// The v1 block registry mints `(epoch << 33) | counter` and never sets this
/// bit, so `ui.update` can route a handle to the v2 rich reader or the v1
/// reader from the handle value alone -- including a released handle, which
/// must still report its documented `Ok(false)` instead of a parse error.
pub const UI_HANDLE_SCENE_BIT: i64 = 1 << 32;

/// Epoch field position shared by both handle minters.
const UI_HANDLE_EPOCH_SHIFT: u32 = 33;

/// Epoch field width shared by both handle minters.
///
/// The epoch occupies bits 33..=62, so bit 32 stays free for
/// [`UI_HANDLE_SCENE_BIT`] and bit 63 (the sign bit) stays clear: every
/// composed handle is positive. An epoch above `2^30 - 1` is masked instead of
/// shifting into the sign bit; the epoch is the activation generation counter,
/// so that bound needs `2^30` activations of one plugin.
const UI_HANDLE_EPOCH_BITS: u32 = 30;

/// Mask selecting the epoch bits of a UI handle.
const UI_HANDLE_EPOCH_MASK: i64 = (1 << UI_HANDLE_EPOCH_BITS) - 1;

/// Whether `handle` is encoded as a terminal-slot scene handle.
#[must_use]
pub const fn is_scene_handle(handle: i64) -> bool {
    handle > 0 && (handle & UI_HANDLE_SCENE_BIT) != 0
}

/// The terminal-attached block's anchor inside the accepted scene model.
///
/// The accepted `BlockAnchor` has no leaf variant: a terminal-attached block
/// is positioned by the paint path from the leaf content rect, and the anchor
/// is attribution only. The grid range starts at the leaf origin so a future
/// anchor consumer cannot resolve the block to a scrollback line it does not
/// own.
const SCENE_HOST_ANCHOR: BlockAnchor = BlockAnchor::Grid {
    start_row: 0,
    start_col: 0,
    end_row: 0,
    end_col: 0,
};

/// Scroll behaviour of a terminal-attached block: a transient overlay that
/// never affects layout or scrollback.
const SCENE_HOST_SCROLL: ScrollBehavior = ScrollBehavior::Overlay;

/// Next scene handle for one activation generation.
fn scene_handle(epoch: u32, counter: u32) -> i64 {
    let epoch = i64::from(epoch) & UI_HANDLE_EPOCH_MASK;
    UI_HANDLE_SCENE_BIT | (epoch << UI_HANDLE_EPOCH_SHIFT) | i64::from(counter)
}

/// One attached terminal-slot scene (OQ-051, bitty#2785).
struct AttachedScene {
    /// Handle returned to Lua (`ui.mount`); `ui.update` addresses the same
    /// handle.
    handle: i64,
    /// Admitted block identity inside the panel's scene.
    block: BlockId,
    /// Owning plugin id (attribution and release).
    owner: String,
    /// Owning activation generation (attribution and release).
    epoch: u32,
}

/// The Core-owned panel that hosts the `terminal` slot scene.
struct HostPanel {
    handle: PanelHandle,
    view: ViewId,
    owner: String,
    epoch: u32,
}

#[derive(Default)]
struct State {
    panel: Option<HostPanel>,
    scenes: Vec<AttachedScene>,
    /// Monotonic handle counter; never reused, so a released handle cannot
    /// alias a later scene.
    next_counter: u32,
}

/// Production owner of the `terminal` slot scene path (bitty#2785).
///
/// Cloneable by `Rc`; the plugin services and the composition root share one
/// instance so a reload observed by the plugin runtime releases exactly the
/// scenes that generation mounted.
pub struct SceneHost {
    panels: Rc<RefCell<PanelRuntime>>,
    target: Rc<Cell<Option<ViewId>>>,
    state: RefCell<State>,
}

impl std::fmt::Debug for SceneHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneHost")
            .field("target", &self.target.get())
            .field("scenes", &self.state.borrow().scenes.len())
            .finish_non_exhaustive()
    }
}

impl SceneHost {
    /// Builds the one scene host over the shared panel runtime.
    ///
    /// `target` is the placement slot Core publishes every tick (the focused
    /// leaf id). Only [`Self::attach`] reads it, so a focus change moves the
    /// panel on this generation's next mount, not on the tick that moved
    /// focus. The plugin never writes it: a mount with no live target fails
    /// closed.
    #[must_use]
    pub fn new(panels: Rc<RefCell<PanelRuntime>>, target: Rc<Cell<Option<ViewId>>>) -> Self {
        Self {
            panels,
            target,
            state: RefCell::new(State::default()),
        }
    }

    /// The shared panel host the present loop reads.
    #[must_use]
    pub fn panels(&self) -> &Rc<RefCell<PanelRuntime>> {
        &self.panels
    }

    /// Attaches one validated rich tree to the terminal-slot scene panel.
    ///
    /// Creates the Core-owned `Rich` panel on first use, binds it to the
    /// published target leaf, admits the tree through
    /// [`scene_bridge::mount_rich_node`](crate::scene_bridge::mount_rich_node),
    /// and attaches the resulting `Scene`. Returns the generation-owned
    /// handle `ui.update` addresses. Re-attaching from the same generation
    /// replaces the previous scene and returns a fresh handle; the previous
    /// handle goes stale.
    ///
    /// # Errors
    ///
    /// `E_UI_UNAVAILABLE` when no target leaf is published, the slot is held
    /// by another plugin generation, or the panel cannot be created or bound;
    /// `E_UI_COMPONENT_INVALID` when the accepted scene admission refuses the
    /// tree (`SCN-1..5`). A refusal leaves the previous scene attached.
    pub fn attach(&self, owner: &str, epoch: u32, node: &UiRichNode) -> Result<i64, BridgeError> {
        let mut state = self.state.borrow_mut();
        // Mint first: the handle is the block identity inside the scene, and
        // a refused tree must not disturb the attached scene or the panel.
        if state.next_counter == u32::MAX {
            return Err(unavailable(
                "terminal slot scene handle space is exhausted for this process",
            ));
        }
        let handle = scene_handle(epoch, state.next_counter);
        state.next_counter += 1;
        let block = BlockId(handle.unsigned_abs());
        let scene = build_scene(block, owner, epoch, node)?;

        let view = self.target.get().ok_or_else(|| {
            unavailable("no focused terminal view is published for the terminal slot")
        })?;
        self.ensure_panel(&mut state, owner, epoch, view)?;
        let panel = state
            .panel
            .as_ref()
            .map(|panel| panel.handle)
            .ok_or_else(|| unavailable("terminal slot scene panel is unavailable"))?;
        self.panels
            .borrow_mut()
            .attach_panel_scene(panel, scene)
            .map_err(|error| panel_error("attach the terminal slot scene", error))?;
        // Replace this generation's previous scene only after the new one is
        // attached: one terminal-slot scene per generation keeps the panel
        // slot and the handle space bounded, and an admission refusal keeps
        // the previous scene live.
        state
            .scenes
            .retain(|attached| !(attached.owner == owner && attached.epoch == epoch));
        state.scenes.push(AttachedScene {
            handle,
            block,
            owner: owner.to_string(),
            epoch,
        });
        Ok(handle)
    }

    /// Replaces one attached scene from a validated rich tree.
    ///
    /// Scoped to the owning plugin generation. Returns `Ok(false)` for a
    /// handle this host no longer owns (stale after a release or a remount)
    /// or one owned by another generation, so a forged handle can never
    /// replace another plugin's scene. The previous scene stays attached
    /// when admission refuses the new tree.
    ///
    /// # Errors
    ///
    /// `E_UI_COMPONENT_INVALID` when the accepted scene admission refuses the
    /// tree (`SCN-1..5`); `E_UI_UNAVAILABLE` when the host panel disappeared
    /// (unreachable while the handle is live).
    pub fn update(
        &self,
        owner: &str,
        epoch: u32,
        handle: i64,
        node: &UiRichNode,
    ) -> Result<bool, BridgeError> {
        let state = self.state.borrow();
        let Some(attached) = state.scenes.iter().find(|attached| {
            attached.handle == handle && attached.owner == owner && attached.epoch == epoch
        }) else {
            return Ok(false);
        };
        let scene = build_scene(attached.block, &attached.owner, attached.epoch, node)?;
        let panel = state
            .panel
            .as_ref()
            .ok_or_else(|| unavailable("terminal slot scene panel is unavailable"))?;
        self.panels
            .borrow_mut()
            .attach_panel_scene(panel.handle, scene)
            .map_err(|error| panel_error("update the terminal slot scene", error))?;
        Ok(true)
    }

    /// Detaches one scene and releases the panel when this host owned it.
    ///
    /// Scoped to the owning plugin generation: a foreign or stale handle is a
    /// no-op, so one plugin can never release another generation's scene.
    /// Idempotent; returns whether a scene was removed.
    pub fn detach(&self, owner: &str, epoch: u32, handle: i64) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(position) = state.scenes.iter().position(|attached| {
            attached.handle == handle && attached.owner == owner && attached.epoch == epoch
        }) else {
            return false;
        };
        let attached = state.scenes.remove(position);
        if !state
            .scenes
            .iter()
            .any(|other| other.owner == attached.owner && other.epoch == attached.epoch)
        {
            self.release_panel(&mut state, &attached.owner, attached.epoch);
        }
        true
    }

    /// Releases every scene one plugin generation owns (suspend/dispose).
    ///
    /// Called from `clear_ui_blocks`, so a suspended, disposed, crashed, or
    /// reloaded generation leaves no scene, no placement bind, and no panel
    /// behind. Returns the number of scenes removed.
    pub fn release_owner(&self, owner: &str, epoch: u32) -> usize {
        let mut state = self.state.borrow_mut();
        let before = state.scenes.len();
        state
            .scenes
            .retain(|attached| !(attached.owner == owner && attached.epoch == epoch));
        let removed = before - state.scenes.len();
        self.release_panel(&mut state, owner, epoch);
        removed
    }

    /// Number of live terminal-slot scenes (diagnostics).
    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.state.borrow().scenes.len()
    }

    /// The view hosting `handle`, if this host still owns it (diagnostics).
    #[must_use]
    pub fn view_of(&self, handle: i64) -> Option<ViewId> {
        let state = self.state.borrow();
        let attached = state
            .scenes
            .iter()
            .find(|attached| attached.handle == handle)?;
        let panel = state.panel.as_ref()?;
        (panel.owner == attached.owner && panel.epoch == attached.epoch).then_some(panel.view)
    }

    /// Creates or re-binds the Core-owned scene panel for `owner`/`epoch`.
    fn ensure_panel(
        &self,
        state: &mut State,
        owner: &str,
        epoch: u32,
        view: ViewId,
    ) -> Result<(), BridgeError> {
        if let Some(panel) = state.panel.as_ref() {
            if panel.owner != owner || panel.epoch != epoch {
                // Never displace another generation's attached scene: the
                // accepted conflict rule is explicit release, not replacement.
                return Err(unavailable(
                    "the terminal slot scene is held by another plugin generation",
                ));
            }
            if panel.view == view {
                return Ok(());
            }
            // The published placement target moved: re-bind the same panel so
            // the scene composes with the leaf the plugin now targets.
            let handle = panel.handle;
            let previous_view = panel.view;
            let mut panels = self.panels.borrow_mut();
            panels
                .unmount_panel(handle.id, handle.generation)
                .map_err(|error| panel_error("release the previous scene leaf", error))?;
            if let Err(error) = panels.mount_panel(handle.id, handle.generation, view) {
                // No half-bound panel may outlive the failure: restore the
                // previous binding when possible, otherwise retire the panel
                // and drop its recorded scenes so neither `attach` nor
                // `view_of` can observe a disposed binding.
                if panels
                    .mount_panel(handle.id, handle.generation, previous_view)
                    .is_err()
                {
                    let _ = panels.dispose_panel(handle.id, handle.generation);
                    state.panel = None;
                    state
                        .scenes
                        .retain(|attached| !(attached.owner == owner && attached.epoch == epoch));
                }
                return Err(panel_error("bind the terminal slot scene leaf", error));
            }
            state.panel = Some(HostPanel {
                handle: panel.handle,
                view,
                owner: panel.owner.clone(),
                epoch: panel.epoch,
            });
            return Ok(());
        }
        let mut panels = self.panels.borrow_mut();
        let handle = panels
            .create_panel(PanelType::Rich, None)
            .map_err(|error| panel_error("create the terminal slot scene panel", error))?;
        if let Err(error) = panels.mount_panel(handle.id, handle.generation, view) {
            // A create that never bound must not consume a registry record:
            // a retrying plugin would otherwise exhaust the shared panel
            // budget (`bitty.panel.create` uses the same host).
            let _ = panels.dispose_panel(handle.id, handle.generation);
            return Err(panel_error("bind the terminal slot scene leaf", error));
        }
        state.panel = Some(HostPanel {
            handle,
            view,
            owner: owner.to_string(),
            epoch,
        });
        Ok(())
    }

    /// Detaches and disposes the scene panel when `owner`/`epoch` holds it.
    fn release_panel(&self, state: &mut State, owner: &str, epoch: u32) {
        let Some(panel) = state
            .panel
            .as_ref()
            .filter(|panel| panel.owner == owner && panel.epoch == epoch)
        else {
            return;
        };
        let handle = panel.handle;
        state.panel = None;
        // `dispose_panel` also clears the attached scene and the placement
        // bind, so no scene or slot survives the generation.
        let _ = self
            .panels
            .borrow_mut()
            .dispose_panel(handle.id, handle.generation);
    }
}

/// Builds one admitted terminal-slot scene from a validated rich tree.
fn build_scene(
    block: BlockId,
    owner: &str,
    epoch: u32,
    node: &UiRichNode,
) -> Result<Scene, BridgeError> {
    let mount = RichMount {
        id: block,
        anchor: SCENE_HOST_ANCHOR,
        scroll: SCENE_HOST_SCROLL,
        owner: owner_tag(owner),
        generation: u64::from(epoch),
        created_at: 0,
    };
    // Admit through the same `Scene::insert` boundary as every Core producer.
    let mut scene = Scene::new();
    mount_rich_node(&mut scene, &mount, node).map_err(|error| scene_refused(&error))?;
    if scene.get(block).is_none() {
        // `mount_rich_node` inserts through the same admission boundary, so a
        // missing block is an internal invariant break, not a plugin error.
        return Err(unavailable("scene admission produced no block"));
    }
    Ok(scene)
}

/// Stable, non-authoritative attribution tag for a plugin id.
///
/// `SceneBlock.owner` is a `u64` diagnostic field; the accepted model never
/// resolves it back to authority. FNV-1a keeps the mapping deterministic
/// across processes without a registry lookup.
fn owner_tag(owner: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    owner.as_bytes().iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

fn unavailable(reason: impl Into<String>) -> BridgeError {
    BridgeError::new("runtime", E_SCENE_UNAVAILABLE, reason)
}

fn scene_refused(error: &SceneBridgeError) -> BridgeError {
    BridgeError::new("validation", E_SCENE_REFUSED, error.to_string())
}

/// Maps a panel-host refusal onto the bounded Lua error surface.
///
/// The reason names the operation and the typed panel error only; no view,
/// path, or other plugin's state is exposed.
fn panel_error(operation: &str, error: PanelError) -> BridgeError {
    let detail = match error {
        PanelError::TooManyPanels { max, .. } => format!("panel registry is full ({max} panels)"),
        PanelError::StaleHandle { .. } => "the scene panel handle is stale".to_string(),
        PanelError::AlreadyMounted { .. } | PanelError::PanelAlreadyMounted { .. } => {
            "the target leaf already hosts another panel".to_string()
        }
        PanelError::InvalidState { .. } => {
            "the scene panel is not in a mountable state".to_string()
        }
        other => other.to_string(),
    };
    unavailable(format!("cannot {operation}: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::PanelRegistryConfig;
    use bitty_rich::scene::SceneNode;

    fn text(value: &str) -> UiRichNode {
        UiRichNode::Text {
            text: value.to_string(),
            bold: false,
        }
    }

    /// Reads the scene's one block text back through the live panel slot.
    fn scene_text(panels: &Rc<RefCell<PanelRuntime>>, view: ViewId, block: i64) -> String {
        let panel = panels
            .borrow()
            .placement_panel_of(view)
            .expect("placement resolves");
        let scene = panels
            .borrow()
            .panel_scene(panel)
            .cloned()
            .expect("scene retained");
        let block = scene
            .get(BlockId(block.unsigned_abs()))
            .expect("block identity");
        match block.content() {
            SceneNode::Text(span) => span.text.clone(),
            other => panic!("unexpected scene content {other:?}"),
        }
    }

    /// `(scene host, shared panel store, published placement target)`.
    type Fixture = (
        SceneHost,
        Rc<RefCell<PanelRuntime>>,
        Rc<Cell<Option<ViewId>>>,
    );

    fn host() -> Fixture {
        let panels = Rc::new(RefCell::new(
            PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host"),
        ));
        let target = Rc::new(Cell::new(Some(ViewId::new(7))));
        (
            SceneHost::new(Rc::clone(&panels), Rc::clone(&target)),
            panels,
            target,
        )
    }

    /// PR #2799 review F3: the scene handle shares the v1 30-bit epoch field
    /// at shift 33, so the sign bit and the scene discriminator keep their
    /// meaning even at the largest representable epoch.
    #[test]
    fn scene_handles_stay_positive_at_the_epoch_bound() {
        for epoch in [1, 1 << 29, u32::MAX] {
            let handle = scene_handle(epoch, 1);
            assert!(handle > 0, "epoch {epoch} must keep the sign bit clear");
            assert!(is_scene_handle(handle), "epoch {epoch} keeps the scene bit");
            let expected_epoch = i64::from(epoch) & UI_HANDLE_EPOCH_MASK;
            assert_eq!(
                handle,
                UI_HANDLE_SCENE_BIT | (expected_epoch << UI_HANDLE_EPOCH_SHIFT) | 1
            );
        }
    }

    #[test]
    fn attach_binds_a_rich_panel_and_admits_the_scene() {
        let (host, panels, _target) = host();
        let handle = host
            .attach("example.plugin", 3, &text("hello"))
            .expect("scene attaches");
        assert!(is_scene_handle(handle));
        let panel = panels
            .borrow()
            .placement_panel_of(ViewId::new(7))
            .expect("placement resolves");
        assert_eq!(panel.get(), 1);
        assert_eq!(panels.borrow().panel_scene_len(), 1);
        assert_eq!(host.scene_count(), 1);
        assert_eq!(host.view_of(handle), Some(ViewId::new(7)));
        let scene = panels
            .borrow()
            .panel_scene(panel)
            .cloned()
            .expect("scene retained");
        assert_eq!(scene.len(), 1);
        let block = scene
            .get(BlockId(handle.unsigned_abs()))
            .expect("block identity");
        assert_eq!(block.generation(), 3);
        assert_eq!(block.owner(), owner_tag("example.plugin"));
        assert_eq!(block.scroll(), ScrollBehavior::Overlay);
        assert_eq!(block.text_bytes(), 5);
    }

    #[test]
    fn a_failed_first_bind_disposes_the_new_panel() {
        let (host, panels, _target) = host();
        // Occupy the target leaf so the scene host's first bind fails.
        let other = panels
            .borrow_mut()
            .create_panel(PanelType::Helper, None)
            .expect("other panel");
        panels
            .borrow_mut()
            .mount_panel(other.id, other.generation, ViewId::new(7))
            .expect("other mounts");
        let error = host
            .attach("example.plugin", 1, &text("x"))
            .expect_err("target leaf is occupied");
        assert_eq!(error.code, E_SCENE_UNAVAILABLE);
        assert_eq!(
            panels.borrow().panel_count(),
            1,
            "the failed bind must not leak a registry record"
        );
        // Once the leaf is free, a retry succeeds on the same host.
        panels
            .borrow_mut()
            .unmount_panel(other.id, other.generation)
            .expect("unmount");
        host.attach("example.plugin", 1, &text("x"))
            .expect("retry binds");
        assert_eq!(panels.borrow().panel_count(), 2);
    }

    #[test]
    fn attach_without_a_target_fails_closed_and_creates_nothing() {
        let (host, panels, target) = host();
        target.set(None);
        let error = host
            .attach("example.plugin", 1, &text("x"))
            .expect_err("no target");
        assert_eq!(error.code, E_SCENE_UNAVAILABLE);
        assert_eq!(panels.borrow().panel_count(), 0);
        assert_eq!(panels.borrow().panel_scene_len(), 0);
    }

    #[test]
    fn a_second_generation_cannot_displace_an_attached_scene() {
        let (host, panels, _target) = host();
        let first = host
            .attach("first.plugin", 1, &text("first"))
            .expect("first attaches");
        let error = host
            .attach("second.plugin", 1, &text("second"))
            .expect_err("slot is held");
        assert_eq!(error.code, E_SCENE_UNAVAILABLE);
        assert!(
            panels.borrow().placement_panel_of(ViewId::new(7)).is_some(),
            "placement resolves"
        );
        assert_eq!(scene_text(&panels, ViewId::new(7), first), "first");
        // The owner can still replace its own scene.
        host.attach("first.plugin", 1, &text("first again"))
            .expect("same owner replaces");
        assert_eq!(host.scene_count(), 1);
        assert!(
            !host
                .update("first.plugin", 1, first, &text("stale"))
                .expect("stale is not an error")
        );
        // A foreign generation cannot touch the live scene, even with a
        // correctly shaped handle.
        assert!(
            !host
                .update("second.plugin", 1, first, &text("stolen"))
                .expect("foreign is not an error")
        );
        assert!(!host.detach("second.plugin", 1, first));
        let panel = panels
            .borrow()
            .placement_panel_of(ViewId::new(7))
            .expect("placement resolves");
        let scene = panels
            .borrow()
            .panel_scene(panel)
            .cloned()
            .expect("scene retained");
        match scene.iter().next().expect("one block").content() {
            SceneNode::Text(span) => assert_eq!(span.text, "first again"),
            other => panic!("unexpected scene content {other:?}"),
        }
    }

    #[test]
    fn admission_refusal_keeps_the_previous_scene() {
        let (host, panels, _target) = host();
        let handle = host
            .attach("example.plugin", 1, &text("kept"))
            .expect("attaches");
        let refused = UiRichNode::Table {
            rows: (0..2048).map(|_| Vec::new()).collect(),
        };
        let error = host
            .update("example.plugin", 1, handle, &refused)
            .expect_err("over SCN-1 is refused");
        assert_eq!(error.code, E_SCENE_REFUSED);
        assert!(
            panels.borrow().placement_panel_of(ViewId::new(7)).is_some(),
            "placement resolves"
        );
        assert_eq!(scene_text(&panels, ViewId::new(7), handle), "kept");
    }

    #[test]
    fn release_owner_detaches_the_scene_and_retires_the_panel() {
        let (host, panels, _target) = host();
        let handle = host
            .attach("example.plugin", 4, &text("live"))
            .expect("attaches");
        assert_eq!(host.release_owner("example.plugin", 4), 1);
        assert_eq!(panels.borrow().panel_scene_len(), 0);
        assert_eq!(panels.borrow().panel_count(), 0);
        assert_eq!(panels.borrow().placement_len(), 0);
        assert_eq!(host.scene_count(), 0);
        // A released handle is stale, never aliased onto a new scene.
        assert!(
            !host
                .update("example.plugin", 4, handle, &text("stale"))
                .expect("stale")
        );
        // The slot is free for the next generation.
        host.attach("example.plugin", 5, &text("next"))
            .expect("re-attaches");
        assert_eq!(host.release_owner("example.plugin", 4), 0);
        assert_eq!(host.scene_count(), 1);
    }

    #[test]
    fn attach_rebinds_the_panel_when_the_published_target_moves() {
        let (host, panels, target) = host();
        host.attach("example.plugin", 1, &text("moved"))
            .expect("attaches");
        target.set(Some(ViewId::new(8)));
        host.attach("example.plugin", 1, &text("moved again"))
            .expect("re-attaches on the new leaf");
        assert_eq!(panels.borrow().placement_panel_of(ViewId::new(7)), None);
        assert_eq!(
            panels.borrow().placement_panel_of(ViewId::new(8)),
            Some(bitty_ui::panel::PanelId::new(1))
        );
        assert_eq!(panels.borrow().panel_count(), 1);
    }

    #[test]
    fn detach_releases_the_slot_and_is_idempotent() {
        let (host, panels, _target) = host();
        let handle = host
            .attach("example.plugin", 1, &text("x"))
            .expect("attaches");
        assert!(!host.detach("other.plugin", 1, handle));
        assert!(host.detach("example.plugin", 1, handle));
        assert!(!host.detach("example.plugin", 1, handle));
        assert_eq!(panels.borrow().panel_scene_len(), 0);
        assert_eq!(panels.borrow().panel_count(), 0);
        host.attach("other.plugin", 1, &text("y"))
            .expect("slot is free");
        assert_eq!(host.scene_count(), 1);
    }
}
