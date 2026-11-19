#![forbid(unsafe_code)]
//! Panel GUI content presentation: the Core-owned content-kind swap
//! (bitty#2388, CTX-1895).
//!
//! Contract source: `specifications/panel-gui-content-presentation-candidate.md`
//! in `bitty-terminal-docs` (draft candidate, decision-ready), sections
//! "Content is a Core-owned kind transition", "PTY preservation semantics",
//! "Focus and input ownership", and "Typed failure modes".
//!
//! A Panel is a container. Its placement, its `PanelId`, and its layout
//! attachment are panel/user facts and never move here. What this module
//! changes is the panel's **content kind**: a terminal-backed panel
//! (`ViewContent::Terminal`) presents a bounded GUI content layer
//! (`ViewContent::Rich`) instead, and release restores the recorded terminal
//! content byte-for-byte. The panel's **presentation mode** is the other half
//! of the same statement (CTX-1902): the layer may sit in the panel's own slot
//! or float over the workspace, and release returns it to the mode it
//! displaced.
//!
//! ```text
//! Terminal(tid) --present--> Gui(surface)     displaced = Terminal(tid)
//! Gui(surface)  --release--> Terminal(tid)    displaced record consumed
//! tiled         --set mode-> floating         record.presentation = floating
//! floating      --release--> tiled            prior_presentation restored
//! ```
//!
//! # What the swap does not touch
//!
//! The displaced terminal's PTY file descriptor, child process, parser, grid,
//! scrollback, cursor, modes, replies, and damage stay owned and live exactly
//! as before: nothing here detaches, suspends, resizes, signals, writes, or
//! kills the terminal content owner. The plugin is never given a handle to
//! any of it, and no plugin-visible output or key buffer is created.
//!
//! # Live effects (each one gated, none observable without a layer)
//!
//! | Effect | Seam |
//! |---|---|
//! | Content kind flips, layer paints in the content rect | [`PanelRuntime::present_panel_content`](crate::registry::PanelRuntime::present_panel_content) attaches the scene to the panel's existing scene slot, which the shipped present loop already composes (`runtime::scene_present`) |
//! | Geometry frozen while hidden | `Runtime::sync_pane_geometry_to` (`runtime::panes`), `Runtime::sync_primary_geometry` (`runtime::layout_focus`), and `Runtime::reflow_content_to_chrome` (`runtime::resize`) skip the `LogicalRect -> PTY` step for a presented view and count the freeze; `Runtime::resize_primary_pty` skips the winsize ioctl itself, so a pixel-only change (font zoom, DPI) cannot deliver `SIGWINCH` either |
//! | Exactly one restore resize, then a full repaint | [`Runtime::release_panel_content`] measures the current content rect (and the owner's own winsize, pixel axes included), issues the existing per-leaf resize path once when either no longer matches the frozen rect, and arms `pending_full_redraw` |
//! | No key reaches the PTY while presented | both keyboard entry points (`Runtime::handle_key_event_at`, `Runtime::handle_key_event_ref`) swallow the key for a presented focused view and count it |
//! | No byte reaches the PTY while presented | the two byte paths to a shell drop everything for a presented view: `Runtime::push_input_bytes` (paste, IME commit, programmatic submit) and `Runtime::route_input_bytes_to` (pointer reports, focus reports). Both count what they dropped, and callers that report an outcome read `input_owner_content_frozen()` so a drop is never labelled a headless buffer |
//! | Layer released when its generation ends | the plugin runtime's generation port calls `PanelRuntime::release_content_layer_at_generation_end` before it presents the placeholder, so a dead generation's layer never outlives it (`runtime::panel_content` "Generation edges") |
//! | Mode chosen and changed on the layer | [`Runtime::set_panel_content_presentation`] validates the token, then stamps the leaf through [`bitty_ui::apply_presentation_command`] (the one transition gate), then records the mode on the layer; a redundant request is a true no-op |
//! | Mode given back on release | [`Runtime::release_panel_content`] restores the mode recorded at present time when the leaf still carries the mode the layer last requested, and leaves the leaf alone when somebody else moved it |
//! | Mode given back at a generation edge | the edge's release cannot stamp (the plugin runtime owns no layout), so it carries the typed [`RestoredPanelContent`] to [`Runtime::restore_released_panel_presentation`], which applies the same two guards before the next frame |
//! | Types refusals, nothing changes | `PanelError::{ContentBusy, ContentUnavailable, ContentNotPresented, ContentTokenMismatch, ContentPresentationUnsupported}` |
//!
//! Every gate is keyed on "does the panel mounted at this view present a
//! content layer?", which is false on every runtime without one. The
//! no-layer path is therefore byte-identical to before this module existed.
//!
//! # Generation edges
//!
//! A presented layer cannot outlive the generation that presented it
//! (bitty#3253). The [`PanelContentToken`] that
//! [`Runtime::release_panel_content`] requires belongs to the presenting
//! caller, and a generation edge — reload, disable, crash, unload — removes
//! that caller. Handing the release to somebody else was rejected: the
//! successor has no token, would have to be given one (which is the same
//! authority under another name), and would still be painting a dead
//! generation's scene until it decided to act.
//!
//! So the edge releases the layer itself, through the panel host's binding
//! rather than a token: `PanelRuntime::release_content_layer_at_generation_end`
//! is fenced on the `(owner, controller generation)` pair the container
//! currently binds, and the generation port calls it *before*
//! `present_placeholder`, because that call is the unbind edge and clears the
//! binding the fence reads. The container comes back terminal-backed with no
//! record and no scene, the placeholder is then presented over a layer-free
//! container, and a successor can present a fresh layer on the same panel
//! instead of being refused with `PanelError::ContentBusy` until the container
//! is retired. A successor that already rebound the container is refused
//! (`PanelError::ControllerConflict`) and inherits nothing, and the three token
//! refusals keep their meaning for live callers.
//!
//! Nothing here issues a restore resize for that path: this module's freeze is
//! a predicate over the record, so with the record gone the ordinary per-leaf
//! sync resumes on the next layout pass, and the edge either closes the leaf or
//! rebinds it to a successor.
//!
//! The **presentation** half of the restore does not fall out of that predicate,
//! so the edge carries it: the port hands the typed
//! [`RestoredPanelContent`] to the app-owned queue it already uses for the leaves
//! it cannot reach (the same seam as its bind-edge observation), and the drain
//! calls [`Runtime::restore_released_panel_presentation`] before the frame is
//! presented. A floated layer therefore comes back to the mode it displaced
//! instead of leaving a placeholder in a float nobody asked for. A refused
//! release (`PanelError::ControllerConflict`) still tears nothing down and
//! queues nothing.
//!
//! # Not in this slice
//!
//! The Lua surface (`bitty.ui.panel.present/update/poll/release`), the
//! `ui.panel` capability, the owner-polled input queue and its
//! `panel.content.*` events, placement-hint application, band exclusion,
//! plugin key bindings, nested-owner refusal (`E_UI_PANEL_NESTED`), and
//! suppressing the displaced leaf's grid paint pass are follow-ups recorded in
//! the pull request body. Nothing here stubs them: there is no queue to drain,
//! no capability to grant, and no owner identity to nest against yet.
//!
//! The mode verb (CTX-1902) is Core-owned and token-fenced, and it is the mode
//! *change* half only. A plugin-facing path is not part of it: `bitty.panel.
//! set_presentation` stays `E_NOT_IMPLEMENTED` because no plugin-facing
//! present hands a plugin a [`PanelContentToken`] to fence a change with, and a
//! verb that can only ever answer "no layer" would be a stub wearing the
//! feature's name. Two of the four modes are answerable (`Tiled`, `Floating`);
//! `Fullscreen` (app-level zoom) and `Scratchpad` (the scratchpad store) are
//! refused with [`PanelError::ContentPresentationUnsupported`] until the
//! mechanism that can honor them owns the change, and a bottom band has no
//! placement at all yet.

use bitty_rich::scene::Scene;
use bitty_ui::Point;
use bitty_ui::PresentationMode;
use bitty_ui::ViewId;
use bitty_ui::geometry::Rect;

use super::Runtime;
use crate::registry::{
    PanelContentToken, PanelError, PresentedPanelContent, RestoredPanelContent, ViewContent,
};

impl Runtime {
    /// Presents a GUI content layer on the terminal-backed panel of `view`.
    ///
    /// The layer is a bounded, Core-admitted [`Scene`] that the shipped
    /// present loop composes clipped to this leaf's content rect
    /// (`runtime::scene_present`). Presenting it changes the panel's content
    /// kind and writes the displaced-terminal record; it does not create a
    /// surface, a window, a renderer handle, a second present queue, or any
    /// plugin-visible data path.
    ///
    /// The terminal content owner stays live and keeps parsing. Its geometry is
    /// frozen at the content rect it has right now: from this call until the
    /// matching [`Runtime::release_panel_content`], no per-leaf resize enters
    /// the `LogicalRect -> PTY` path, so `TIOCGWINSZ` keeps returning the frozen
    /// size and the child receives no `SIGWINCH`.
    ///
    /// # Errors
    ///
    /// [`PanelError::ContentUnavailable`] when no panel host is installed, no
    /// panel is mounted at `view`, the panel is not terminal-backed (no pane
    /// session and not the primary owner), or the view has no present frame;
    /// [`PanelError::ContentBusy`] when the panel already presents a layer. On
    /// every refusal nothing changes: no record, no scene, no freeze.
    pub fn present_panel_content(
        &mut self,
        view: ViewId,
        scene: Scene,
    ) -> Result<PresentedPanelContent, PanelError> {
        let Some(host) = self.scene_host.clone() else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "no panel host installed",
            });
        };
        let Some(displaced) = self.terminal_content_kind(view) else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "no live terminal content owner",
            });
        };
        let Some(frozen) = self.content_rect_for(view) else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "view has no present frame",
            });
        };
        let scroll_offset = self
            .layout
            .find_leaf(view)
            .map_or(0, bitty_ui::View::scroll_offset);
        // The mode the panel presents in is part of what a layer records: it is
        // the mode release gives back, and the mode a plugin-facing change
        // moves (bitty#2388, CTX-1902). Read from the leaf so the record starts
        // in agreement with the tree.
        let Some(presentation) = self.view_presentation(view) else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "view holds no presentation mode",
            });
        };
        let presented = match host.try_borrow_mut() {
            Ok(mut host) => host.present_panel_content(
                view,
                displaced,
                frozen,
                scroll_offset,
                presentation,
                scene,
            ),
            // The host is shared with the plugin services. A reentrant plugin
            // call holds the borrow; refuse rather than panic, and change
            // nothing (the same fail-closed rule the present loop uses).
            Err(_) => Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "panel host is borrowed",
            }),
        };
        let presented = presented?;
        // The layer replaces what the leaf painted a moment ago: drop the
        // retained leaf primitives and repaint so no stale grid pixels survive
        // the first presented frame.
        self.presented_leaf_frames.remove(&view);
        self.pending_full_redraw = true;
        Ok(presented)
    }

    /// Releases the content layer presented on the panel of `view` and
    /// restores the terminal content it displaced.
    ///
    /// The restore is exact: the panel keeps its `PanelId`, its attachment, its
    /// placement, and the same terminal content kind and content id the
    /// present recorded. Geometry is restored through the existing per-leaf
    /// resize path when the leaf's current content rect differs from the
    /// frozen one, and also when the owner's own `TIOCGWINSZ` (pixel axes
    /// included) no longer matches that rect — a fixed-size leaf can come back
    /// to the same `cols`/`rows` with stale pixels after a font-zoom/DPI change.
    /// The sync skips a winsize that already matches, so a release issues
    /// **exactly one** resize when one is owed and none otherwise. Either way
    /// the next frame is a full repaint, so no partial grid or stale layer
    /// pixels survive.
    ///
    /// # Errors
    ///
    /// [`PanelError::ContentUnavailable`] when no panel host is installed or no
    /// panel is mounted at `view`; [`PanelError::ContentNotPresented`] when the
    /// panel presents no layer; [`PanelError::ContentTokenMismatch`] when
    /// `token` is not that panel's live layer. A refusal restores nothing and
    /// leaves the live layer presented.
    ///
    /// A layer whose generation ended is released by the generation edge
    /// instead (bitty#3253), so its token then lands in `ContentNotPresented`
    /// like any other already-released layer — never in a restore of a
    /// terminal another generation now owns.
    pub fn release_panel_content(
        &mut self,
        view: ViewId,
        token: PanelContentToken,
    ) -> Result<RestoredPanelContent, PanelError> {
        let Some(host) = self.scene_host.clone() else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "no panel host installed",
            });
        };
        let restored = match host.try_borrow_mut() {
            Ok(mut host) => host.release_panel_content(view, token),
            Err(_) => Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "panel host is borrowed",
            }),
        };
        let restored = restored?;
        // A plugin-presented layer's handle dies with the record it names
        // (bitty#2388, CTX-1909): one prune point for every release path, so no
        // `bitty.ui.panel` handle can survive into a panel somebody else owns.
        self.forget_ui_panel_layer(restored.token);
        // The presentation half of "exact restore" runs before the geometry
        // half: the mode decides the content rect the restore resize measures,
        // so the panel must already be back in its own mode when the rect is
        // read.
        self.restore_panel_presentation(&restored);
        self.restore_panel_content_geometry(&restored);
        Ok(restored)
    }

    /// Returns the panel to the mode a released layer displaced, on a release
    /// path that owns no present frame (bitty#2388, CTX-1902).
    ///
    /// [`Runtime::release_panel_content`] restores the mode itself, because it
    /// owns the layout it stamps. The other production release path does not:
    /// the generation edge releases the layer inside the plugin runtime
    /// (`PanelRuntime::release_content_layer_at_generation_end`), which owns the
    /// panel host but no layout, so it **carries** the typed
    /// [`RestoredPanelContent`] to this side instead of discarding it. This is
    /// where that record is applied, through the same two guards the token
    /// release applies: a layer that never moved the mode restores nothing, and
    /// a leaf somebody else moved since is left where it is.
    ///
    /// Geometry is deliberately not touched: the caller owns no present frame,
    /// the freeze is a predicate over the record (already gone), and the edge
    /// either closes the leaf or rebinds it to a successor — the ordinary
    /// per-leaf sync resumes on its own.
    ///
    /// Returns `true` when the leaf was moved back to
    /// [`RestoredPanelContent::prior_presentation`] and `false` for every no-op,
    /// which includes the ordinary disposal case where the edge closed the leaf
    /// before this ran.
    #[must_use]
    pub fn restore_released_panel_presentation(&mut self, restored: &RestoredPanelContent) -> bool {
        // The generation edge released this layer, so any `bitty.ui.panel`
        // handle that named it is dead here — before the caller can present
        // anything over the returned terminal (bitty#2388, CTX-1909).
        self.forget_ui_panel_layer(restored.token);
        let before = self.view_presentation(restored.view);
        self.restore_panel_presentation(restored);
        if self.view_presentation(restored.view) == before {
            return false;
        }
        // The leaf moved, so the retained primitives describe a frame that no
        // longer exists: the first frame in the returned mode is a full paint.
        self.presented_leaf_frames.remove(&restored.view);
        self.pending_full_redraw = true;
        true
    }

    /// Presents the layer of the panel at `view` in `mode` (bitty#2388,
    /// CTX-1902).
    ///
    /// This is the presentation-mode half of "a GUI plugin presents in place of
    /// its panel": the panel that presents a content layer may choose where the
    /// surface sits — in its own slot ([`PresentationMode::Tiled`], the mode
    /// present used) or as a bounded float over the workspace
    /// ([`PresentationMode::Floating`]) — without giving up its `PanelId`, its
    /// attachment, its content layer, or its displaced terminal.
    ///
    /// The change is one atomic step with one authority:
    ///
    /// 1. **Validate.** The layer must exist for `view`'s panel
    ///    ([`PanelError::ContentNotPresented`]), `token` must be that layer
    ///    ([`PanelError::ContentTokenMismatch`]), and `mode` must be a mode a
    ///    presented panel can take ([`PanelError::ContentPresentationUnsupported`]).
    ///    Every one of those refusals happens before anything moves.
    /// 2. **Stamp.** The leaf is stamped through the workspace command registry
    ///    ([`bitty_ui::apply_presentation_command`]) — the same single
    ///    transition gate every other mode change uses, never a direct write to
    ///    the layout node.
    /// 3. **Record.** Only then does the layer record the new mode
    ///    ([`crate::registry::PanelRuntime::commit_presented_content_presentation`]),
    ///    so a refused stamp leaves the record untouched and no state is ever
    ///    reported as changed when it did not change.
    ///
    /// Re-presenting the mode the leaf already has is a true no-op: no stamp,
    /// no record write, no repaint armed. That is what keeps a mode the user
    /// moved out of band from being rewritten by a redundant request, and it is
    /// why release can tell "the layer moved this leaf" from "somebody else
    /// did".
    ///
    /// # What does not move
    ///
    /// The displaced terminal keeps its frozen geometry: the mode change is a
    /// layout fact, and the freeze (`Runtime::content_layer_presented`) is a
    /// predicate over the layer record, so no `LogicalRect -> PTY` step runs
    /// and no `SIGWINCH` is delivered for the new frame. The content kind stays
    /// `Rich(token)`, the token stays valid, the scene slot stays attached, and
    /// focus is untouched: this verb moves *where* the panel paints, never
    /// *what* it presents or who owns input.
    ///
    /// # Errors
    ///
    /// [`PanelError::ContentUnavailable`] when no panel host is installed, no
    /// panel is mounted at `view`, the host is borrowed, or `view` is not a
    /// leaf of the workspace layout (a detached float's placement belongs to
    /// the detach-restore path, not to the layer); [`PanelError::ContentNotPresented`]
    /// when the panel presents no layer; [`PanelError::ContentTokenMismatch`]
    /// when `token` is not that layer; [`PanelError::ContentPresentationUnsupported`]
    /// for a mode this runtime cannot give an in-tree panel. A refusal changes
    /// nothing: no stamp, no record write, no focus, no resize.
    pub fn set_panel_content_presentation(
        &mut self,
        view: ViewId,
        token: PanelContentToken,
        mode: bitty_ui::PresentationMode,
    ) -> Result<PresentedPanelContent, PanelError> {
        let Some(host) = self.scene_host.clone() else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "no panel host installed",
            });
        };
        // Step 1 — validate. Every refusal below leaves the leaf, the record,
        // the focus, and the PTY exactly as they were.
        let (panel, layer) = match host.try_borrow() {
            Ok(host) => {
                let panel = host.content_panel_for_view(view)?;
                let Some(layer) = host.presented_content(panel).cloned() else {
                    return Err(PanelError::ContentNotPresented { panel_id: panel });
                };
                (panel, layer)
            }
            Err(_) => {
                return Err(PanelError::ContentUnavailable {
                    view_id: view,
                    reason: "panel host is borrowed",
                });
            }
        };
        if layer.token != token {
            return Err(PanelError::ContentTokenMismatch {
                panel_id: panel,
                token: token.get(),
            });
        }
        if !presented_panel_mode_supported(mode) {
            return Err(PanelError::ContentPresentationUnsupported {
                view_id: view,
                mode: mode.as_str(),
            });
        }
        let Some(current) = self.view_presentation(view) else {
            return Err(PanelError::ContentUnavailable {
                view_id: view,
                reason: "view holds no presentation mode",
            });
        };
        if current == mode {
            // Nothing to stamp and nothing to record: a redundant request must
            // not claim the leaf's mode as the layer's own.
            return Ok(layer);
        }
        // Step 2 — stamp through the one transition gate.
        bitty_ui::apply_presentation_command(
            &mut self.layout,
            bitty_ui::presentation_command_for_mode(mode),
            view,
        )
        .map_err(|error| match error {
            bitty_ui::PresentationCommandError::LeafNotFound(_) => PanelError::ContentUnavailable {
                view_id: view,
                reason: "view is not a workspace layout leaf",
            },
            bitty_ui::PresentationCommandError::Rejected { .. } => {
                PanelError::ContentPresentationUnsupported {
                    view_id: view,
                    mode: mode.as_str(),
                }
            }
            bitty_ui::PresentationCommandError::UnknownCommand(_) => {
                PanelError::ContentUnavailable {
                    view_id: view,
                    reason: "presentation command is not a workspace command",
                }
            }
        })?;
        // Step 3 — record. The stamp above is the only fallible half, so this
        // cannot refuse on any path that reached it.
        let committed = match host.try_borrow_mut() {
            Ok(mut host) => host.commit_presented_content_presentation(view, mode),
            Err(_) => None,
        };
        let Some(record) = committed else {
            // Unreachable: the record was validated a few statements ago and
            // nothing re-entered. Report the refusal honestly and put the leaf
            // back where the caller found it rather than leave a half state.
            let _ = bitty_ui::apply_presentation_command(
                &mut self.layout,
                bitty_ui::presentation_command_for_mode(current),
                view,
            );
            return Err(PanelError::ContentNotPresented { panel_id: panel });
        };
        // The layer's content rect may have moved: drop the retained leaf
        // primitives so the first frame in the new mode is a full paint rather
        // than a diff against a frame that no longer exists.
        self.presented_leaf_frames.remove(&view);
        self.pending_full_redraw = true;
        Ok(record)
    }

    /// The content layer presented on the panel mounted at `view`, if any.
    ///
    /// Read-only projection of the Core-owned record; it carries no terminal
    /// content, no PTY, and no key bytes.
    #[must_use]
    pub fn panel_content_layer(&self, view: ViewId) -> Option<PresentedPanelContent> {
        let host = self.scene_host.as_ref()?.try_borrow().ok()?;
        host.presented_content_for_view(view).cloned()
    }

    /// The content kind this view's panel presents right now.
    ///
    /// `Some(ViewContent::Rich(_))` while a layer is presented,
    /// `Some(ViewContent::Terminal(_))` for a terminal-backed panel with no
    /// layer, and `None` for a view that hosts no panel or no content owner.
    /// This is the kind the input and geometry gates read; it never becomes
    /// panel or view identity.
    #[must_use]
    pub fn panel_content_kind(&self, view: ViewId) -> Option<ViewContent> {
        if let Some(layer) = self.panel_content_layer(view) {
            return Some(layer.presented);
        }
        self.terminal_content_kind(view)
    }

    /// Whether the panel mounted at `view` presents a GUI content layer.
    ///
    /// The single predicate the geometry and input gates read. A runtime with
    /// no presented layer (every runtime before bitty#2388) answers `false`
    /// for every view, so the gated paths stay byte-identical.
    ///
    /// A borrow conflict on the shared panel host answers `false`: the gates
    /// then take their pre-existing path rather than freezing a visible
    /// terminal, which is the direction that cannot hide a live resize.
    #[must_use]
    pub fn content_layer_presented(&self, view: ViewId) -> bool {
        let Some(host) = self.scene_host.as_ref() else {
            return false;
        };
        match host.try_borrow() {
            Ok(host) => host.presented_content_for_view(view).is_some(),
            Err(_) => false,
        }
    }

    /// Number of geometry steps skipped because a panel presented a content
    /// layer (bitty#2388 "no resize while hidden").
    ///
    /// Counts the gate firing — the per-leaf sync, the layout install, the
    /// window reflow, and the primary winsize ioctl are each a step — which is
    /// the observable half of "the resize never entered the
    /// `LogicalRect -> PTY` path"; the frozen `TIOCGWINSZ` in the same test is
    /// the other half.
    #[must_use]
    pub const fn content_geometry_freezes(&self) -> u64 {
        self.content_geometry_freezes
    }

    /// Number of restore resizes the PTY's own `TIOCGWINSZ` actually
    /// reflected, as issued by [`Runtime::release_panel_content`].
    ///
    /// At most one per release, and only when the leaf's current content rect
    /// differs from the rect frozen at present time **and** the winning
    /// winsize moved. The counter is measured on the kernel side of the
    /// restore (read the winsize before and after the sync, count only a real
    /// change), so it agrees with the assertion that bites: the restored
    /// `TIOCGWINSZ` equals the current content rect. A release that measured a
    /// changed rect but left the winsize where it was counts zero.
    #[must_use]
    pub const fn content_restore_resizes(&self) -> u64 {
        self.content_restore_resizes
    }

    /// Number of keys swallowed because the focused panel presented content.
    ///
    /// A swallowed key is never encoded and never reaches the PTY; it is
    /// counted and dropped (the owner-polled queue is a later slice, so there
    /// is deliberately no queue to leak keys into).
    #[must_use]
    pub const fn content_input_swallowed(&self) -> u64 {
        self.content_input_swallowed
    }

    /// Bytes dropped because the focused panel presented content.
    ///
    /// Covers every non-key input path that shares the one
    /// [`Runtime::push_input_bytes`] router: paste, IME commit, and a
    /// programmatic submit. Dropped bytes are counted, never buffered, so the
    /// transport stays empty and the hidden shell receives nothing.
    #[must_use]
    pub const fn content_input_dropped_bytes(&self) -> u64 {
        self.content_input_dropped_bytes
    }

    /// Whether the primary owner's content layer freezes its geometry.
    ///
    /// `false` when there is no primary owner or the owner presents no layer,
    /// which is every runtime that never called
    /// [`Runtime::present_panel_content`]. Read by the window-reflow gate
    /// (`runtime::resize`), the layout-install gate
    /// (`runtime::layout_focus::sync_primary_geometry`), and the pane sync.
    pub(super) fn primary_content_frozen(&self) -> bool {
        self.primary_view
            .is_some_and(|view| self.content_layer_presented(view))
    }

    /// Whether the focused panel presents a content layer, i.e. owns the keys.
    ///
    /// Read once per keyboard event, before the encoder. No focus, no layer, or
    /// no host all answer `false`, which keeps the encoder path untouched.
    pub(super) fn focused_content_layer_presents(&self) -> bool {
        self.focused_view()
            .is_some_and(|view| self.content_layer_presented(view))
    }

    /// Whether the view that owns routed input bytes presents a content layer.
    ///
    /// The target is resolved exactly like `Runtime::route_input_bytes`: the
    /// focused leaf, else the primary owner. This is the gate for every input
    /// path that does not go through the keyboard encoder — paste, IME commit,
    /// and a programmatic write all reach the PTY through
    /// [`Runtime::push_input_bytes`], so gating there keeps pasted or composed
    /// text out of a displaced shell while its panel presents a layer
    /// (bitty#3250 R2).
    ///
    /// Public because `push_input_bytes` answers `false` for two different
    /// outcomes — "only buffered headless" and "dropped by this gate" — and the
    /// callers that report an outcome to a user (`terminal.submit`, the
    /// `terminal send` control verb, the plugin submit drain) have to tell them
    /// apart instead of calling a drop "buffered" (bitty#3250 review).
    #[must_use]
    pub fn input_owner_content_frozen(&self) -> bool {
        self.focus
            .focused()
            .or(self.primary_view)
            .is_some_and(|view| self.content_layer_presented(view))
    }

    /// The `TIOCGWINSZ` winsize of `view`'s terminal content owner, if any.
    ///
    /// Read on both sides of a restore sync so "one restore resize" is
    /// measured on the kernel side of the call instead of being inferred from
    /// the intent to resize. The pixel axes are part of the tuple: a
    /// font-zoom/DPI change at a fixed cell rect moves only those, and that is
    /// exactly the restore the freeze deferred (bitty#3250 R3).
    fn content_owner_winsize(&self, view: ViewId) -> Option<(u16, u16, u16, u16)> {
        if self.pane_sessions.contains_key(&view) {
            self.pane_pty_pixel_size(&view)
        } else {
            self.pty_pixel_size()
        }
    }

    /// The terminal content owner of `view`, as a content kind.
    ///
    /// The live runtime keys pane terminal sessions by `ViewId` and keeps one
    /// runtime-global primary PTY for the primary owner
    /// ([`Runtime::paints_primary_grid`]), so `Terminal(raw)` carries that live
    /// session key. The `TerminalId`/`ViewId` encoding transition belongs to the
    /// Panel Placement Decision and is unchanged here: this value is recorded so
    /// release can restore the exact kind it displaced, never to allocate an
    /// identity.
    fn terminal_content_kind(&self, view: ViewId) -> Option<ViewContent> {
        if self.pane_sessions.contains_key(&view) || self.paints_primary_grid(&view) {
            return Some(ViewContent::Terminal(view.0));
        }
        None
    }

    /// The content rectangle (cells) the leaf for `view` has right now.
    ///
    /// Derived from the same decorated present frame the geometry sync uses,
    /// and honoring a durable fixed-size constraint exactly like
    /// [`Runtime::sync_pane_geometry_to`], so the frozen rect and the restore
    /// target are measured the same way.
    fn content_rect_for(&self, view: ViewId) -> Option<Rect> {
        let frame = self
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == view)?;
        let (cols, rows) = match self.fixed_size_for_view(view) {
            Some(size) => (size.width.max(1), size.height.max(1)),
            None => (frame.cols.max(1), frame.rows.max(1)),
        };
        let origin = self
            .layout
            .find_leaf(view)
            .map_or(Point::new(0, 0), bitty_ui::View::origin);
        Some(Rect::new(origin.x, origin.y, cols, rows))
    }

    /// Returns the panel to the presentation mode the layer moved it out of.
    ///
    /// The layer is a *presentation* of the panel, so the mode it moved is the
    /// mode it gives back — through the same workspace command registry the
    /// change went through (`bitty_ui::apply_presentation_command`), never by
    /// writing the leaf. Two guards keep the restore from overreaching:
    ///
    /// - A layer that never moved the mode restores nothing (`presentation ==
    ///   prior_presentation`), so an ordinary present/release of a tiled panel
    ///   is byte-identical to the slice that had no modes: not one stamp
    ///   reaches the transition gate.
    /// - A leaf whose mode no longer equals the mode this layer last requested
    ///   was moved by somebody else (a detach, a keybinding, a restored
    ///   snapshot). That intent outranks the layer's, so the leaf is left where
    ///   it is and only the content is restored.
    ///
    /// Both guards share one leaf state, so only the second one is
    /// *behaviourally* discriminating, and it is discriminated alone:
    /// `release_leaves_a_third_party_mode_alone_after_the_layer_floated`
    /// (`tests/panel_content_presentation_modes.rs`, row 8) presents `Tiled`,
    /// floats through the verb so the record's mode differs from the prior mode,
    /// stamps `Fullscreen` out of band, and releases — with this guard deleted
    /// the prior `Tiled` is stamped over that `Fullscreen`. Guard 1 is kept
    /// because it is the only thing that keeps the ordinary case stamp-free;
    /// no row can see its removal, since guard 2 refuses the same leaves for the
    /// same reason.
    ///
    /// This runs on both production release paths: the token release
    /// ([`Runtime::release_panel_content`]) and the generation edge, which
    /// carries its [`RestoredPanelContent`] to
    /// [`Runtime::restore_released_panel_presentation`] because the plugin
    /// runtime owns no layout.
    ///
    /// The stamp cannot refuse for a supported mode on a live leaf; a layer
    /// whose leaf left the layout (closed, discarded, moved to another store)
    /// has no mode to return to and release must not fail over presentation
    /// after the content is already back.
    fn restore_panel_presentation(&mut self, restored: &RestoredPanelContent) {
        if restored.presentation == restored.prior_presentation {
            return;
        }
        if self.view_presentation(restored.view) != Some(restored.presentation) {
            return;
        }
        let _ = bitty_ui::apply_presentation_command(
            &mut self.layout,
            bitty_ui::presentation_command_for_mode(restored.prior_presentation),
            restored.view,
        );
    }

    /// Applies the release-side geometry contract: one resize when the
    /// current content rect differs from the frozen one, then one full
    /// repaint.
    ///
    /// The resize is the existing per-leaf path (the same
    /// `pane_sessions`/`sync_primary_geometry` code the layout sync uses), never
    /// a second resize implementation: the freeze is lifted first, so letting
    /// the sync observe the new frame is what produces the single resize. A
    /// release whose rect did not change issues nothing and only repaints.
    fn restore_panel_content_geometry(&mut self, restored: &RestoredPanelContent) {
        let target = self.content_rect_for(restored.view);
        let owes_cell_resize = target.is_some_and(|rect| {
            rect.width != restored.frozen.width || rect.height != restored.frozen.height
        });
        // The cell rect is not the whole contract: a durable fixed-size leaf can
        // come back to the same `cols`/`rows` while a font-zoom/DPI change moved
        // the pixel axes, and the frame with the layer up deferred exactly that
        // ioctl. The existing sync already skips a winsize that matches, so ask
        // it whenever the owner's pixel axes are stale at the target cell size.
        let owes_winsize_sync = match (target, self.content_owner_winsize(restored.view)) {
            // Only the deferred pixel-only resize belongs here: the owner must
            // already agree with the target on the cell axes. A winsize that
            // disagrees on `cols`/`rows` is the ordinary case (a leaf whose
            // spawn size never matched its frame), and release deliberately
            // leaves it to the next reflow instead of forcing a grid reflow
            // nobody asked for.
            (Some(rect), Some((cols, rows, width, height))) => {
                let (target_width, target_height) = self.pty_pixel_span(rect.width, rect.height);
                cols == rect.width
                    && rows == rect.height
                    && (width, height) != (target_width, target_height)
            }
            _ => false,
        };
        if owes_cell_resize || owes_winsize_sync {
            // Measured on the kernel side of the sync: the counter reports the
            // `TIOCGWINSZ` movement the restore actually produced (cells or
            // pixels), not the intent to produce one (bitty#3250 R10).
            let before = self.content_owner_winsize(restored.view);
            if self.pane_sessions.contains_key(&restored.view) {
                self.sync_pane_geometry();
            } else if self.paints_primary_grid(&restored.view) {
                self.sync_primary_geometry();
            }
            if self.content_owner_winsize(restored.view) != before {
                self.content_restore_resizes = self.content_restore_resizes.saturating_add(1);
            }
        }
        // The restored grid and the dropped layer share the content rect: one
        // full repaint is the only way to prove no layer pixel and no partial
        // grid survives the swap back.
        self.presented_leaf_frames.remove(&restored.view);
        self.pending_full_redraw = true;
    }
}

/// Modes a presented panel can actually take on this tree (bitty#2388,
/// CTX-1902).
///
/// `Tiled` and `Floating` are the two placements the present loop can give an
/// in-tree leaf today: `Tiled` fills the solver slot, and `Floating` re-places
/// the leaf at the configured float frame with the elevated `Float` tier
/// (`PresentationMode::overlay_tier`, `float_frame`). Both are real placements,
/// so both are answerable.
///
/// The other two are refused rather than accepted and ignored, because a mode
/// that stamps and then paints exactly like the mode it replaced is a lie about
/// layout. `Fullscreen` is the app-level zoom (`zoom_backup` backup/restore in
/// the host application), not a leaf stamp: stamping it changes no frame. And
/// `Scratchpad` is the scratchpad store's hide/slot behavior plus the float
/// frame, so a bare stamp would produce a float that never hides — also not
/// what the caller asked for. Both stay refusals until the mechanism that can
/// honor them owns the mode change.
fn presented_panel_mode_supported(mode: PresentationMode) -> bool {
    matches!(mode, PresentationMode::Tiled | PresentationMode::Floating)
}
