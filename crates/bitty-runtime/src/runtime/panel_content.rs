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
//! content byte-for-byte.
//!
//! ```text
//! Terminal(tid) --present--> Gui(surface)     displaced = Terminal(tid)
//! Gui(surface)  --release--> Terminal(tid)    displaced record consumed
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
//! | No byte reaches the PTY while presented | the two byte paths to a shell drop everything for a presented view: `Runtime::push_input_bytes` (paste, IME commit, programmatic submit) and `Runtime::route_input_bytes_to` (pointer reports, focus reports). Both count what they dropped |
//! | Types refusals, nothing changes | `PanelError::{ContentBusy, ContentUnavailable, ContentNotPresented, ContentTokenMismatch}` |
//!
//! Every gate is keyed on "does the panel mounted at this view present a
//! content layer?", which is false on every runtime without one. The
//! no-layer path is therefore byte-identical to before this module existed.
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
//! One lifecycle question is also deliberately undecided: a generation edge
//! (reload, disable, crash) that ends while its panel presents a layer. The
//! layer's token belongs to the presenting caller, so neither the successor nor
//! the edge can release it; the detach refuses (bitty#3250 R5), the record dies
//! with the container, and a successor cannot present again on that panel until
//! it is retired. That needs its own decision — release on the edge, move the
//! release authority to the successor, or refuse before the edge changes state
//! — and it must be settled before the Lua verb exposes the path.

use bitty_rich::scene::Scene;
use bitty_ui::Point;
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
        let presented = match host.try_borrow_mut() {
            Ok(mut host) => {
                host.present_panel_content(view, displaced, frozen, scroll_offset, scene)
            }
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
        self.restore_panel_content_geometry(&restored);
        Ok(restored)
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
    pub(super) fn input_owner_content_frozen(&self) -> bool {
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
