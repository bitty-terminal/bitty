//! `Runtime` — Hover-to-activate focus, Alt+drag floating moves, and
//! Mod+drag tiled-panel moves (Hyprland-like).
//!
//! CTX-0260 follow-through of DEC-0034: an opt-in hover moves keyboard
//! focus (`RuntimeConfig::focus_follows_mouse`, default off to preserve
//! click-to-focus) plus Alt+drag to move a floating pane position
//! ([`LayoutNode::Overlay`] bounds move).
//!
//! Issue #1694 (CTX-0966): Mod+Left-drag (Alt by default, the working Mod
//! per OQ-052; Super also accepted for Hyprland muscle memory) on a tiled
//! leaf grabs it for a tiled move via the headless
//! [`DragMoveSession`](bitty_ui::DragMoveSession) primitive: motion tracks
//! a live preview (advisory hovered target), release re-parents with
//! Hyprland-like placement
//! ([`drop_spec_for_point`](bitty_ui::drop_spec_for_point): nearest-edge
//! docking with position-based sizing, so small panels grow by dropping
//! centrally and large ones shrink by dropping near an edge; corner-zone
//! drops resolve through the accepted #1804 balanced policy at `0.5`).
//!
//! Issue #1811 (CTX-1070): the tiled move shows continuous feedback — the
//! grab arms the drag transition on the dragged frame (ghost outline) and
//! every motion re-arms it on the dragged frame plus the hovered drop
//! target (drop-target highlight), all through the existing Core-owned
//! chrome-ring fade (geometry and terminal content are never interpolated).
//! A committed drop arms the move transition on the moved panel so the
//! result settles instead of snapping, and `Esc` cancels mid-drag with the
//! tree untouched (the preview never mutates, so cancel restores the
//! pre-drag layout byte-identically with no re-parent and no residue).
//!
//! CTX-0334 (Hyprland-like mouse-enter activation): when
//! `RuntimeConfig::focus_follows_mouse_delay` is non-zero, pointer entry on
//! a non-focused pane arms a pending candidate ([`HoverPending`]) that
//! [`Runtime::apply_hover_deadline`] commits once the dwell deadline
//! elapses; `0` activates immediately on entry. Any explicit focus change
//! cancels the pending dwell so hover can never override a deliberate
//! choice.
//!
//! CTX-0339 (click-to-focus): a left press routes through
//! [`Runtime::click_focus_at`], which focuses the hit-tested leaf even when
//! `focus_follows_mouse` is off (the default). It shares the present-frames
//! hit-test ([`Runtime::cursor_to_present_cell`]) with hover, selection,
//! and hyperlink resolution plus Shift suppression, so a click, a hover, a
//! selection, and a mouse report agree on which pane owns the pointer.
//!
//! Lane note: pointer chrome only. Selection, capture encoding, scrollbar
//! drags, and workspace switching belong to their owning paths; this module
//! only grabs/moves/releases the Alt-drag, the Mod tiled-drag, and the
//! border-drag, plus the gated hover step.
//! Shift still forces the selection path (the CTX-0181 precedent): no grab
//! starts while Shift is held and hover-focus is suppressed under Shift.

use super::*;
use bitty_platform::CursorPosition;
use bitty_ui::{DragMoveSession, DropSpec, Point as UiPoint, SplitAxis, drop_spec_for_point};
use std::time::Instant;

/// Pending hover activation with a positive dwell delay (CTX-0334).
///
/// Recorded when the pointer enters a non-focused pane while
/// `focus_follows_mouse` is enabled and the configured delay is non-zero;
/// [`Runtime::apply_hover_deadline`] commits focus once the deadline passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HoverPending {
    /// Candidate pane the pointer entered.
    view: ViewId,
    /// Time the pointer entered (or last re-targeted) the candidate.
    since: Instant,
}

/// Active Alt+drag: which floating leaf is grabbed plus the press-time
/// anchor in container cells (via [`Runtime::cursor_to_cell`], so deltas
/// match the overlay-bounds space exactly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltDragState {
    /// Grabbed floating leaf.
    pub leaf: ViewId,
    /// Anchor column (container cells) at grab or last move.
    pub anchor_col: i32,
    /// Anchor row (container cells) at grab or last move.
    pub anchor_row: i32,
}

/// Active Mod+drag tiled-panel move (issue #1694, CTX-0966).
///
/// Wraps the headless [`DragMoveSession`] primitive: press lifts the tiled
/// leaf under the cursor, motion tracks the advisory preview target live,
/// release commits via [`drop_spec_for_point`] + `reparent_leaf` with
/// position-based sizing. Floating (overlay-tier) leaves never start here;
/// the Alt+drag floating path owns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiledDragState {
    /// Headless move session (source + advisory preview target).
    pub session: DragMoveSession,
}

/// Active border-drag resize: which split divider(s) are grabbed plus the
/// press-time anchor in container cells (issue #1348, #1445).
///
/// A plain left press on a split handle grabs the divider; motion adjusts
/// the adjacent split ratio live through the same clamped geometry the
/// keyboard resize path uses. Corner-drag (issue #1445: Hyprland/Niri
/// 4-way model) grabs up to two perpendicular splits simultaneously,
/// resizing all adjacent panels at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorderDragState {
    /// Paths of the grabbed splits (same indexing as
    /// [`LayoutNode::set_split_ratio_at`](bitty_ui::LayoutNode::set_split_ratio_at)).
    /// Single-edge drag has one entry; corner-drag has two (one per axis).
    pub splits: Vec<(Vec<usize>, SplitAxis)>,
    /// Anchor column (container cells) at grab or last move.
    pub anchor_col: i32,
    /// Anchor row (container cells) at grab or last move.
    pub anchor_row: i32,
}

impl Runtime {
    /// Whether an Alt+drag move is currently active.
    #[must_use]
    pub fn alt_drag_active(&self) -> bool {
        self.alt_drag.is_some()
    }

    /// Attempts to grab the floating pane under the last known cursor for
    /// an Alt+drag move.
    ///
    /// Requires Alt held and Shift released (Shift forces selection, per
    /// the CTX-0181 precedent) plus a known cursor over a floating leaf.
    /// The grab also focuses the dragged pane (Hyprland-like). Returns
    /// `true` when the drag started (caller consumes the press and skips
    /// selection); `false` leaves all state untouched so the press falls
    /// through to selection ("without breaking selection").
    pub fn begin_alt_drag(&mut self) -> bool {
        if !self.alt_pressed || self.shift_pressed {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        // Topmost hit-test in paint order: the grab resolves through the
        // present frames (last painted wins), so a visible float owns the
        // cursor over the base leaf it covers — including a mode-floating
        // leaf whose anchored float geometry shares no solver allocation
        // with its slot. The grab still anchors in the primary-global cell
        // space `update_alt_drag` measures its deltas in.
        let Some((leaf, _)) = self.cursor_to_present_cell(cursor) else {
            return false;
        };
        let anchor = self.cursor_to_cell(cursor);
        // Probe: a zero-delta move succeeds only where the layout model
        // permits (an owning structural float exists). A mode-floating leaf
        // has anchored (container-derived) geometry with no stored position
        // to move — free rects are a follow-up — but the grab still owns the
        // gesture (focus moves, no selection starts), so it passes the probe
        // via `leaf_is_floating`. A pinned leaf passes the same way
        // (CTX-1081: detached from the tree yet presented at Float tier);
        // its motion accumulates a present-time re-anchor offset instead.
        // Tiled leaves fail soft here and the Mod
        // tiled-drag path (`begin_tiled_drag`) owns them instead
        // (issue #1694, CTX-0966).
        if !self
            .layout
            .move_overlay_containing(leaf, 0, 0, self.container)
            && !self.leaf_is_floating(leaf)
        {
            return false;
        }
        self.alt_drag = Some(AltDragState {
            leaf,
            anchor_col: i32::from(anchor.col),
            anchor_row: i32::from(anchor.row),
        });
        // Dragging focuses the grabbed pane (no-op present-wise when
        // already focused; `set_focus` dirties only on change).
        self.set_focus(leaf);
        true
    }

    /// Moves the active Alt+drag to `pos`, offsetting the grabbed overlay
    /// by the cell delta since the grab (or last move).
    ///
    /// Returns `true` when a drag was active (caller consumes the motion:
    /// no selection update, no hover-focus, no capture motion encoding).
    /// A zero delta keeps the drag armed with no redraw. When the layout no
    /// longer owns the leaf (closed mid-drag) the drag ends fail-soft and
    /// `false` is returned so the motion falls through to normal handling.
    pub fn update_alt_drag(&mut self, pos: CursorPosition) -> bool {
        self.update_alt_drag_at(pos, Instant::now())
    }

    /// [`Self::update_alt_drag`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the drag transition deterministically).
    pub fn update_alt_drag_at(&mut self, pos: CursorPosition, now: Instant) -> bool {
        let Some(drag) = self.alt_drag else {
            return false;
        };
        let cell = self.cursor_to_cell(pos);
        let dx = i32::from(cell.col) - drag.anchor_col;
        let dy = i32::from(cell.row) - drag.anchor_row;
        if dx == 0 && dy == 0 {
            return true;
        }
        if !self
            .layout
            .move_overlay_containing(drag.leaf, dx, dy, self.container)
        {
            // CTX-1058 (#1844 P2): a mode-floating leaf has anchored
            // (container-derived) geometry with no stored position to move,
            // so motion changes nothing — but the grab still owns the
            // gesture (focus moved, no selection started), so it stays armed
            // and consumes the motion instead of dropping into
            // selection/hover/capture mid-gesture. A leaf that is neither
            // movable nor floating (closed mid-drag) ends fail-soft as
            // before, and the motion falls through to normal handling.
            //
            // CTX-1081: a pinned leaf is anchored the same way, but its
            // bounds are composited per present (not stored in the tree), so
            // the drag accumulates the cell delta in the Runtime-owned
            // re-anchor offset (`pinned_offsets`) instead of standing still:
            // the present path adds it to the anchored bounds, the drop
            // keeps it (the drop point becomes the new anchor, clamped into
            // the container at present time), and unpin drops it. The
            // pinned store itself is never written here.
            if self.pinned.contains(drag.leaf) {
                let entry = self.pinned_offsets.entry(drag.leaf).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(dx);
                entry.1 = entry.1.saturating_add(dy);
                self.pending_full_redraw = true;
                self.trigger_animation(AnimationKind::Drag, Some(drag.leaf), now);
            } else if !self.leaf_is_floating(drag.leaf) {
                self.alt_drag = None;
                return false;
            }
        } else {
            self.pending_full_redraw = true;
            // CTX-0967: a live float move arms the drag transition on the
            // dragged leaf. The layout commits immediately (terminal content is
            // never interpolated); only the leaf's chrome ring fades, and repeat
            // updates restart the bounded transition instead of accumulating.
            self.trigger_animation(AnimationKind::Drag, Some(drag.leaf), now);
        }
        self.alt_drag = Some(AltDragState {
            leaf: drag.leaf,
            anchor_col: i32::from(cell.col),
            anchor_row: i32::from(cell.row),
        });
        true
    }

    /// Ends the active Alt+drag, if any. Returns `true` when one was active
    /// (caller skips the selection-release commit/copy: no selection was
    /// started by the grabbing press).
    ///
    /// CTX-1081: ending the drag keeps a pinned leaf's accumulated
    /// re-anchor offset — the drop point is the new anchor — while a
    /// structural float keeps its moved overlay bounds and a mode float
    /// keeps its anchored frame.
    pub fn end_alt_drag(&mut self) -> bool {
        if self.alt_drag.is_none() {
            return false;
        }
        self.alt_drag = None;
        true
    }

    /// Whether a Mod+drag tiled-panel move is currently active (issue #1694).
    #[must_use]
    pub fn tiled_drag_active(&self) -> bool {
        self.tiled_drag.is_some()
    }

    /// Grabbed tiled leaf for the active Mod+drag, if any (test seam).
    #[must_use]
    pub fn tiled_drag_source(&self) -> Option<ViewId> {
        self.tiled_drag.as_ref().map(|drag| drag.session.source())
    }

    /// Advisory preview target for the active Mod+drag, if any (test seam
    /// and live-preview paint hook): the hovered drop anchor from the last
    /// motion, or `None` over background.
    #[must_use]
    pub fn tiled_drag_preview(&self) -> Option<ViewId> {
        self.tiled_drag
            .as_ref()
            .and_then(|drag| drag.session.preview_target())
    }

    /// Hyprland-like drop placement for the active Mod+drag at the last
    /// known cursor (issue #1811, CTX-1070 test seam and live-preview paint
    /// hook): the [`DropSpec`] a release would commit — target, edge axis,
    /// ratio, and side — or `None` over background, over a floating
    /// overlay, on the source itself, or when no drag is active. Pure query;
    /// the tree is never mutated here.
    #[must_use]
    pub fn tiled_drag_drop_spec(&self) -> Option<DropSpec> {
        let drag = self.tiled_drag.as_ref()?;
        let cursor = self.last_cursor?;
        let point = self.cursor_to_layout_point(cursor)?;
        drop_spec_for_point(
            &self.layout,
            self.container,
            self.gaps(),
            drag.session.source(),
            point,
        )
    }

    /// Whether `id` is a floating (overlay-tier) leaf in the current tree.
    ///
    /// CTX-1058 (#1844 P2): structural [`LayoutNode::Overlay`] tiers are
    /// not the whole story — a leaf stamped `Floating` (or a shown
    /// `Scratchpad`) carries no structural tier but paints at
    /// [`OverlayTier::Float`](bitty_ui::OverlayTier::Float) via the present
    /// override, so it counts as floating here too. Without the mode arm a
    /// mode-floating leaf was invisible to the Alt+drag path (the grab probe
    /// failed soft) yet eligible for the Mod tiled-drag below, which would
    /// re-parent it on release.
    ///
    /// CTX-1081: a pinned leaf is detached from the layout tree (the store
    /// is read-only here — [`PinnedStore::contains`](bitty_ui::PinnedStore::contains)
    /// only), yet the present path composites it at `Float` tier over the
    /// active scene, so it counts as floating too. Without this arm an
    /// Alt+press on a presented pinned frame failed soft in
    /// [`Self::begin_alt_drag`] (no selection started, but no grab either)
    /// while the Mod tiled-drag below could grab the covered base leaf.
    fn leaf_is_floating(&self, id: ViewId) -> bool {
        if self
            .layout
            .leaf_overlay_tiers()
            .into_iter()
            .find(|(leaf, _)| *leaf == id)
            .is_some_and(|(_, tier)| tier.is_some())
        {
            return true;
        }
        if self.pinned.contains(id) {
            return true;
        }
        self.layout
            .find_leaf(id)
            .is_some_and(|leaf| leaf.presentation().overlay_tier().is_some())
    }

    /// Attempts to grab the tiled pane under the last known cursor for a
    /// Mod+drag move (issue #1694, CTX-0966).
    ///
    /// Requires Mod held (Alt, the working Mod per OQ-052, or Super for
    /// Hyprland muscle memory) and Shift released (Shift forces selection
    /// per the CTX-0181 precedent), plus a known cursor over a tiled
    /// (non-overlay) leaf. Floating leaves fail soft here; the Alt+drag
    /// floating path owns them. Single-leaf trees fail soft too, preserving
    /// Alt+block selection where a move would be a self-drop no-op. The
    /// grab focuses the dragged pane (Hyprland-like). Returns `true` when
    /// the drag started (caller consumes the press and skips selection);
    /// `false` leaves all state untouched so the press falls through.
    pub fn begin_tiled_drag(&mut self) -> bool {
        self.begin_tiled_drag_at(Instant::now())
    }

    /// [`Self::begin_tiled_drag`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the drag transition deterministically).
    pub fn begin_tiled_drag_at(&mut self, now: Instant) -> bool {
        if self.shift_pressed || !(self.alt_pressed || self.super_pressed) {
            return false;
        }
        if self.alt_drag.is_some() || self.tiled_drag.is_some() {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        let Some((leaf, _)) = self.cursor_to_leaf_cell(cursor) else {
            return false;
        };
        if self.leaf_is_floating(leaf) {
            return false;
        }
        if self.layout.leaf_count() < 2 {
            return false;
        }
        let Ok(session) = DragMoveSession::start(leaf, true) else {
            return false;
        };
        self.tiled_drag = Some(TiledDragState { session });
        self.set_focus(leaf);
        self.clear_hover_pending();
        // CTX-1070 (issue #1811): the grab arms the drag transition on the
        // dragged frame (ghost outline). The layout is untouched — only the
        // leaf's chrome ring fades — and repeat motion restarts the bounded
        // transition instead of accumulating (see `update_tiled_drag_at`).
        self.trigger_animation(AnimationKind::Drag, Some(leaf), now);
        true
    }

    /// Moves the active Mod+drag to `pos`, tracking the advisory preview
    /// target live (issue #1694).
    ///
    /// Hit-tests `pos` against the current allocations via
    /// [`DragMoveSession::preview`]; the tree is never mutated here. Returns
    /// `true` when a drag was active (caller consumes the motion: no
    /// selection update, no hover-focus, no capture motion encoding). An
    /// unmappable position keeps the drag armed with no preview change.
    /// When the layout no longer owns the source (closed mid-drag) the drag
    /// ends fail-soft and `false` is returned so the motion falls through.
    pub fn update_tiled_drag(&mut self, pos: CursorPosition) -> bool {
        self.update_tiled_drag_at(pos, Instant::now())
    }

    /// [`Self::update_tiled_drag`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the drag transition deterministically).
    pub fn update_tiled_drag_at(&mut self, pos: CursorPosition, now: Instant) -> bool {
        let Some(point) = self.cursor_to_layout_point(pos) else {
            return self.tiled_drag.is_some();
        };
        // Disjoint-field borrow: preview against the current tree while
        // holding the session mutably.
        let (container, gaps) = (self.container, self.gaps());
        let Some(drag) = self.tiled_drag.as_mut() else {
            return false;
        };
        if self.layout.find_leaf(drag.session.source()).is_none() {
            self.tiled_drag = None;
            return false;
        }
        let before = drag.session.preview_target();
        drag.session.preview(&self.layout, container, gaps, point);
        if drag.session.preview_target() != before {
            self.pending_full_redraw = true;
        }
        // CTX-1070 (issue #1811): live preview transitions. The dragged
        // frame keeps its ghost outline while moving and the hovered drop
        // target highlights under the cursor; both ride the existing
        // Core-owned chrome-ring fade (geometry and terminal content are
        // never interpolated). Repeat motion restarts the bounded
        // transitions instead of accumulating, mirroring the Alt+drag path.
        let source = drag.session.source();
        let target = drag.session.preview_target();
        self.trigger_animation(AnimationKind::Drag, Some(source), now);
        if let Some(hovered) = target {
            self.trigger_animation(AnimationKind::Drag, Some(hovered), now);
        }
        true
    }

    /// Ends the active Mod+drag, committing the drop when it lands on
    /// another leaf (issue #1694).
    ///
    /// Computes Hyprland-like placement via [`drop_spec_for_point`]
    /// (nearest-edge docking with position-based sizing) at the last known
    /// cursor and re-parents through [`Runtime::set_layout`] so leaf Views,
    /// pane sessions, and the primary grid reflow exactly like a keyboard
    /// move. Self-drops, background drops, unmappable releases, and
    /// mid-drag closes end the gesture with the tree untouched. Always
    /// returns `true` when a drag was active (caller skips the
    /// selection-release commit/copy: the grabbing press never started a
    /// selection, so there is nothing to commit and stale highlights must
    /// not auto-copy); `false` when no drag was active.
    pub fn end_tiled_drag(&mut self) -> bool {
        self.end_tiled_drag_at(Instant::now())
    }

    /// [`Self::end_tiled_drag`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the drop transition deterministically).
    pub fn end_tiled_drag_at(&mut self, now: Instant) -> bool {
        let Some(drag) = self.tiled_drag.take() else {
            return false;
        };
        let source = drag.session.source();
        let Some(cursor) = self.last_cursor else {
            return true;
        };
        let Some(point) = self.cursor_to_layout_point(cursor) else {
            return true;
        };
        let Some(drop) =
            drop_spec_for_point(&self.layout, self.container, self.gaps(), source, point)
        else {
            return true;
        };
        let mut next = self.layout.clone();
        if !next.reparent_leaf(source, drop.target, drop.axis, drop.ratio, drop.after) {
            return true;
        }
        self.set_layout(next);
        self.set_focus(source);
        self.pending_full_redraw = true;
        // CTX-1070 (issue #1811): a committed drop arms the move transition
        // on the moved panel so the result settles instead of snapping. The
        // tree commits immediately (terminal content is never interpolated);
        // only the moved panel's chrome ring fades, mirroring the keyboard
        // reposition path.
        self.trigger_animation(AnimationKind::Move, Some(source), now);
        true
    }

    /// Cancels the active Mod+drag without committing (cursor left the
    /// window, or `Esc` cancelled mid-drag). The preview never mutates the
    /// tree, so cancel restores the pre-drag layout byte-identically: no
    /// re-parent, no selection, no residue. Arms a repaint when a drag was
    /// active so the ghost outline and drop-target highlight clear on the
    /// next frame. Returns `true` when a drag was active.
    pub fn cancel_tiled_drag(&mut self) -> bool {
        if self.tiled_drag.is_none() {
            return false;
        }
        self.tiled_drag = None;
        self.pending_full_redraw = true;
        true
    }

    /// Maps a physical cursor position to container-cell coordinates for
    /// split-handle and tiled-drop hit-testing (issues #1348, #1710).
    ///
    /// Removes only the window padding inset and converts with live cell
    /// metrics, keeping the point in container coordinates: layout
    /// hit-tests (`hit_test_leaf`, `hit_test_split_handle(s)`,
    /// `drop_spec_for_point`) apply the outer gap inset themselves via
    /// `layout_with_gaps`, so subtracting `gaps_out` here would shift the
    /// drop target with nonzero outer gaps. Unlike the leaf variant there
    /// is no leaf lookup: gap bands and zero-gap boundary lines own no
    /// leaf but may own a split handle.
    /// Returns `None` when the pointer maps outside the container origin
    /// or the cell metrics are degenerate.
    fn cursor_to_layout_point(&self, pos: CursorPosition) -> Option<UiPoint> {
        let live = self.live_cell_metrics();
        let cell_w = live.width as f64;
        let cell_h = live.height as f64;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let pad_px = f64::from(self.window_padding_physical());
        let x = pos.x - pad_px;
        let y = pos.y - pad_px;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w).floor() as i64;
        let row = (y / cell_h).floor() as i64;
        if col < 0 || row < 0 || col > u16::MAX as i64 || row > u16::MAX as i64 {
            return None;
        }
        Some(UiPoint::new(col as u16, row as u16))
    }

    /// Split axis under the pointer, if any (issue #1348 hover affordance).
    ///
    /// Pure query over [`LayoutNode::hit_test_split_handle`]: returns the
    /// divider axis so a future platform cursor-icon API can show a
    /// column/row-resize shape on hover. The platform layer currently
    /// exposes no cursor-icon setter, so no caller sets a shape yet; the
    /// drag itself works without it.
    #[must_use]
    pub fn border_drag_hover_at(&self, pos: CursorPosition) -> Option<SplitAxis> {
        let point = self.cursor_to_layout_point(pos)?;
        let path = self
            .layout
            .hit_test_split_handle(self.container, self.gaps(), point)?;
        self.layout.split_axis_at(&path)
    }

    /// Whether a border-drag resize is currently active.
    #[must_use]
    pub fn border_drag_active(&self) -> bool {
        self.border_drag.is_some()
    }

    /// Grabbed split divider(s) for the active border drag, if any (issue
    /// #1445 corner-drag test seam).
    ///
    /// Returns the divider paths and axes grabbed by
    /// [`Self::begin_border_drag`]: one entry for a single-edge drag, two
    /// perpendicular entries for a corner-drag (at most one per axis, so
    /// up to four adjacent panels). Headless-testable without a window.
    #[must_use]
    pub fn border_drag_splits(&self) -> Option<Vec<(Vec<usize>, SplitAxis)>> {
        self.border_drag.as_ref().map(|drag| drag.splits.clone())
    }

    /// Attempts to grab the split divider(s) under the last known cursor for
    /// a border-drag resize (issue #1348, #1445 corner-drag).
    ///
    /// Requires a plain press: Shift, Alt, and Super released (those force
    /// the selection, Alt+drag, and Mod tiled-drag paths per the
    /// CTX-0181/CTX-0260/CTX-0966 precedents)
    /// plus a known cursor over a split handle
    /// ([`LayoutNode::hit_test_split_handles`]). Callers run this after the
    /// mouse-capture and scrollbar checks, so a mouse-mode app and the
    /// scroll thumb keep the pointer. Returns `true` when the drag started
    /// (caller consumes the press and skips selection); `false` leaves all
    /// state untouched so the press falls through to selection.
    ///
    /// Corner-drag (issue #1445): when the cursor is at the intersection of
    /// perpendicular splits (e.g., 2x2 grid corner), all intersecting splits
    /// are grabbed and resized simultaneously (Hyprland/Niri model).
    pub fn begin_border_drag(&mut self) -> bool {
        if self.alt_pressed || self.super_pressed || self.shift_pressed {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(cursor) else {
            return false;
        };
        let gaps = self.gaps();
        let splits = self
            .layout
            .hit_test_split_handles(self.container, gaps, point);
        if splits.is_empty() {
            return false;
        }
        // Bounded corner grab (issue #1445): at most two perpendicular
        // dividers (one per axis) for up to four panels. The layout hit
        // test already enforces this, but fail closed here too rather
        // than driving an unbounded set.
        if splits.len() > 2 {
            return false;
        }
        {
            let mut seen_h = false;
            let mut seen_v = false;
            for (_, axis) in &splits {
                match axis {
                    SplitAxis::Horizontal => {
                        if seen_h {
                            return false;
                        }
                        seen_h = true;
                    }
                    SplitAxis::Vertical => {
                        if seen_v {
                            return false;
                        }
                        seen_v = true;
                    }
                }
            }
        }
        self.border_drag = Some(BorderDragState {
            splits,
            anchor_col: i32::from(point.x),
            anchor_row: i32::from(point.y),
        });
        // A border owns no leaf, so — unlike click-to-focus — the grab
        // moves no focus. A pending hover dwell is dropped: the pointer is
        // committed to a gesture, not a hover target.
        self.clear_hover_pending();
        true
    }

    /// Moves the active border drag to `pos`, adjusting the grabbed split
    /// ratio(s) live (issue #1348, #1445 corner-drag).
    ///
    /// The cell delta since the grab (or last move) along each split axis
    /// flows through [`LayoutNode::resize_split_by_drag`] — the same
    /// clamped geometry the keyboard resize path uses — installed via
    /// [`Runtime::set_layout`] so leaf Views, pane sessions, and the
    /// primary grid reflow exactly like a keyboard resize (sizes persist
    /// in the tree per layout). Overshoot clamps fail-closed at
    /// `[MIN_RATIO, MAX_RATIO]`; a zero delta keeps the drag armed with no
    /// redraw. When the layout no longer owns the split (pane closed
    /// mid-drag) the drag ends fail-soft and `false` is returned so the
    /// motion falls through to normal handling.
    ///
    /// Corner-drag (issue #1445): when multiple splits are grabbed (up to
    /// two perpendicular splits at a corner), both are resized with their
    /// respective axis deltas, resizing up to four adjacent panels.
    ///
    /// Returns `true` when a drag was active (caller consumes the motion:
    /// no selection update, no hover-focus, no capture motion encoding).
    pub fn update_border_drag(&mut self, pos: CursorPosition) -> bool {
        self.update_border_drag_at(pos, Instant::now())
    }

    /// [`Self::update_border_drag`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the resize transition deterministically).
    pub fn update_border_drag_at(&mut self, pos: CursorPosition, now: Instant) -> bool {
        let Some(drag) = self.border_drag.clone() else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(pos) else {
            // Unmappable motion (outside the container origin) keeps the
            // drag armed but changes nothing.
            return true;
        };
        let delta_col = i32::from(point.x) - drag.anchor_col;
        let delta_row = i32::from(point.y) - drag.anchor_row;
        if delta_col == 0 && delta_row == 0 {
            return true;
        }

        let mut next = self.layout.clone();
        let mut any_changed = false;

        // Apply delta to each grabbed split along its axis.
        for (path, axis) in &drag.splits {
            let (raw_delta, total) = match axis {
                SplitAxis::Horizontal => (delta_col, self.container.width),
                SplitAxis::Vertical => (delta_row, self.container.height),
            };
            if raw_delta == 0 {
                continue;
            }
            // `resize_split_by_drag` narrows to `i16`: clamp the cell delta so
            // a pointer teleport can never wrap the ratio step.
            let delta = raw_delta.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            let before = next.split_ratio_at(path);
            if !next.resize_split_by_drag(path, delta, total) {
                // Split no longer exists (closed mid-drag): end the drag.
                self.border_drag = None;
                return false;
            }
            if next.split_ratio_at(path) != before {
                any_changed = true;
            }
        }

        if any_changed {
            self.set_layout(next);
            // CTX-0967: a live divider move arms the whole-surface resize
            // transition. The ratios commit immediately (terminal content is
            // never interpolated); only Core-owned chrome fades. `None`
            // because one gesture resizes every adjacent panel at once
            // (corner-drag touches up to four); repeat updates restart the
            // bounded transition instead of accumulating.
            self.trigger_animation(AnimationKind::Resize, None, now);
        }
        self.border_drag = Some(BorderDragState {
            splits: drag.splits,
            anchor_col: i32::from(point.x),
            anchor_row: i32::from(point.y),
        });
        true
    }

    /// Ends the active border drag, if any. Returns `true` when one was
    /// active (caller skips the selection-release commit/copy: no selection
    /// was started by the grabbing press).
    pub fn end_border_drag(&mut self) -> bool {
        if self.border_drag.is_none() {
            return false;
        }
        self.border_drag = None;
        true
    }

    /// Click-to-focus (CTX-0339): a left press focuses the pane under the
    /// pointer, independent of the opt-in hover flag (the default keeps
    /// click-to-focus).
    ///
    /// Uses the present-frames hit-test ([`Self::cursor_to_present_cell`],
    /// topmost painted leaf first) — the same order the selection press and
    /// hyperlink paths resolve — so a click, a selection, and a mouse report
    /// agree on which pane owns the pointer, including where a mode-floating
    /// leaf covers a sibling's slot. Gap/padding bands (no frame) keep the
    /// current focus. Shift is the accessibility escape (CTX-0181) and never
    /// steals focus, matching the hover path. Mouse capture and scrollbar
    /// chrome never reach here: the caller consumes those presses first, so
    /// a mouse-tracking app or an active thumb drag keeps the pointer.
    ///
    /// Returns `true` when the pointer landed on a leaf (whether or not it
    /// already held focus), `false` over a gap or under Shift.
    pub(super) fn click_focus_at(&mut self, pos: CursorPosition) -> bool {
        if self.shift_pressed {
            return false;
        }
        let Some((id, _)) = self.cursor_to_present_cell(pos) else {
            if self.focus.focused().is_none() {
                if let Some(primary) = self.primary_view {
                    self.set_focus(primary);
                    return true;
                }
            }
            return false;
        };
        // A click is an explicit focus choice: `set_focus` also drops any
        // pending hover dwell, so hover can never override it.
        self.set_focus(id);
        true
    }

    /// Whether `pos` lies on a split handle (divider) of the active layout.
    ///
    /// Pure twin of the hit test inside [`Self::begin_border_drag`], used by
    /// the capture pre-focus (CTX-0804) so a divider press never moves focus.
    fn pointer_on_split_handle(&self, pos: CursorPosition) -> bool {
        self.cursor_to_layout_point(pos).is_some_and(|point| {
            !self
                .layout
                .hit_test_split_handles(self.container, self.gaps(), point)
                .is_empty()
        })
    }

    /// Capture-aware click-to-focus (CTX-0804, #1477).
    ///
    /// Mouse reports route to the focused pane, and the capture decision
    /// reads the focused pane's modes. Without this step, a left press on
    /// another pane while a mouse-tracking app (vim, htop, yazi) held focus
    /// was reported to that app at clamped coordinates, and focus could
    /// never leave it by clicking. Conversely, a press on a capturing pane
    /// while a plain pane was focused started a selection instead of
    /// reaching the app.
    ///
    /// When either the focused pane or the pane under the pointer tracks
    /// the mouse, a left press on a *different* leaf moves focus first
    /// through [`Self::click_focus_at`]. The caller then re-evaluates
    /// capture against the newly focused pane, so the click reaches the
    /// pane it landed on. Nothing changes when no pane involved tracks the
    /// mouse (the selection path's own click-to-focus stays authoritative),
    /// under Shift, Alt, or Super (selection and drag escapes), over a gap
    /// band, or on a split handle (a divider owns no leaf). The pointer
    /// owner resolves in present paint order (a visible float, including a
    /// mode-floating leaf, wins over the base beneath it), matching the
    /// click the selection path is about to apply.
    pub(super) fn focus_pointer_pane_before_capture(&mut self) {
        if self.shift_pressed || self.alt_pressed || self.super_pressed {
            return;
        }
        let Some(pos) = self.last_cursor else {
            return;
        };
        let Some((hit, _)) = self.cursor_to_present_cell(pos) else {
            return;
        };
        if Some(hit) == self.focus.focused() {
            return;
        }
        let hit_tracks = self.view_tracks_mouse(hit);
        if !hit_tracks && !self.should_capture_mouse() {
            return;
        }
        if self.pointer_on_split_handle(pos) {
            return;
        }
        let _ = self.click_focus_at(pos);
    }

    /// Dwell-delay-aware hover activation (CTX-0334 virtual-clock seam).
    ///
    /// No-op unless [`crate::config::RuntimeConfig::focus_follows_mouse`]
    /// is set (default off preserves click-to-focus) and Shift is released
    /// (Shift forces the selection path). Hover over gap/padding bands
    /// (the present hit-test yields `None` there) keeps focus and clears
    /// any pending dwell. The hover owner resolves in present paint order
    /// (a visible float, including a mode-floating leaf, wins over the base
    /// beneath it), so hover and click-to-focus agree on the target. With
    /// a zero delay focus moves immediately through [`Self::set_focus`]
    /// (which dirties only on change, so steady hover costs no present);
    /// with a positive delay the candidate is recorded and
    /// [`Self::apply_hover_deadline`] commits it once `now` reaches the
    /// deadline. Moving to a different candidate re-arms the dwell clock.
    pub(super) fn hover_focus_at_at(&mut self, pos: CursorPosition, now: Instant) {
        if !self.config.focus_follows_mouse || self.shift_pressed {
            self.hover_pending = None;
            return;
        }
        if self.should_capture_mouse() {
            self.hover_pending = None;
            return;
        }
        if self.pointer_on_split_handle(pos) {
            self.hover_pending = None;
            return;
        }
        let Some((id, _)) = self.cursor_to_present_cell(pos) else {
            // Gap/padding band: no candidate, keep focus, drop any dwell.
            self.hover_pending = None;
            return;
        };
        if self.focus.focused() == Some(id) {
            self.hover_pending = None;
            return;
        }
        if self.config.focus_follows_mouse_delay.is_zero() {
            self.hover_pending = None;
            self.set_focus(id);
            return;
        }
        // Re-arm only when the candidate actually changes; repeated motion
        // inside the same pane must not reset the dwell clock.
        match self.hover_pending {
            Some(HoverPending { view, .. }) if view == id => {}
            _ => {
                self.hover_pending = Some(HoverPending {
                    view: id,
                    since: now,
                });
            }
        }
    }

    /// Commits a pending hover activation whose dwell deadline has elapsed
    /// (CTX-0334).
    ///
    /// Called from `Runtime::tick_at`; the app schedules a wake at
    /// [`Self::hover_activation_deadline`] so focus still lands when the
    /// pointer stops moving. Clearing the pending candidate keeps steady
    /// hover present-neutral after the one focus present.
    pub(super) fn apply_hover_deadline(&mut self, now: Instant) {
        let Some(pending) = self.hover_pending else {
            return;
        };
        if !self.config.focus_follows_mouse || self.shift_pressed {
            self.hover_pending = None;
            return;
        }
        if now.saturating_duration_since(pending.since) < self.config.focus_follows_mouse_delay {
            return;
        }
        self.hover_pending = None;
        self.set_focus(pending.view);
    }

    /// Deadline at which the pending hover activation commits, if any
    /// (CTX-0334).
    ///
    /// The app loop arms a timed wake at this instant
    /// (`EventContext::set_wait_until`) so a stopped pointer still activates
    /// without busy-polling. `None` when no dwell is pending or the feature
    /// is disabled.
    #[must_use]
    pub fn hover_activation_deadline(&self) -> Option<Instant> {
        self.hover_pending
            .map(|pending| pending.since + self.config.focus_follows_mouse_delay)
    }

    /// Drops any pending hover dwell (cursor left the window, a capture
    /// path took over, or focus was applied elsewhere).
    pub(super) fn clear_hover_pending(&mut self) {
        self.hover_pending = None;
    }
}
