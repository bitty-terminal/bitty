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
//! a live preview (advisory hovered target), release commits one
//! [`DropPlan`](bitty_ui::DropPlan) mutation
//! ([`drop_plan_for_point`](bitty_ui::drop_plan_for_point): center targets
//! swap in place, edge targets insert at a balanced split).
//!
//! Issue #2337: the press immediately paints a bounded, present-only ghost
//! of the dragged panel that follows the pointer one-to-one from the grab
//! point, plus the candidate-tile highlight. Solver geometry, leaf grids,
//! and PTYs are untouched until release; the committed layout is the only
//! place PTY sizes change. `Esc`, pointer leave, and window focus loss cancel
//! without residue; a mid-drag layout install (close, zoom, workspace switch)
//! abandons the drop while keeping that install's own layout change, so a
//! stale release can never commit against the new tree. The bounded subset
//! and its Hyprland deviations are documented in the workspace-compositor
//! specification.
//!
//! Issue #1811 (CTX-1070): the tiled move shows continuous feedback — the
//! grab arms the drag transition on the dragged frame and the first hover
//! arms it on the hovered drop target (drop-target highlight), all through
//! the existing Core-owned chrome-ring fade
//! (geometry and terminal content are never interpolated). CTX-1167:
//! motion holds those arms so progress accumulates toward full visibility
//! instead of restarting near 0 on every motion. Gesture-scoped flags
//! hold admitted arms past completion (no re-pulse on long drags); only
//! refused arms retry.
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
use bitty_ui::{
    DragMoveSession, DropPlan, DropSpec, Gaps, LayoutNode, Point as UiPoint, RESIZE_HANDLE_CELLS,
    Rect, ResizeEdge, SplitAxis, detect_resize_edge, drop_plan_for_point, resize_floating_rect,
};
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
///
/// CTX-1150 (issue #1980): pixel-smooth motion. The solver stays
/// cell-quantized; the sub-cell remainder lives here as a present-only
/// physical-pixel translation (`pixel_dx`/`pixel_dy`, bounded to one live
/// cell each way). [`Runtime::present_frames`] adds it to the dragged
/// frame, and release drops it with the grab so the frame snaps to cells.
/// `anchor_px_*` is the last-move cursor position in physical pixels
/// (rounded), so the remainder tracks the pointer without drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltDragState {
    /// Grabbed floating leaf.
    pub leaf: ViewId,
    /// Anchor column (container cells) at grab or last move.
    pub anchor_col: i32,
    /// Anchor row (container cells) at grab or last move.
    pub anchor_row: i32,
    /// Anchor cursor x in physical pixels (rounded) at grab or last move.
    pub anchor_px_x: i32,
    /// Anchor cursor y in physical pixels (rounded) at grab or last move.
    pub anchor_px_y: i32,
    /// Present-only x remainder in physical pixels (bounded, never grid truth).
    pub pixel_dx: i32,
    /// Present-only y remainder in physical pixels (bounded, never grid truth).
    pub pixel_dy: i32,
}

/// Active Alt+edge float resize (CTX-1150).
///
/// An Alt+Left-press on a structural float border grabs that edge via
/// [`detect_resize_edge`] (with a [`RESIZE_HANDLE_CELLS`]-cell hit area);
/// motion resizes the overlay bounds live through [`resize_floating_rect`]
/// (clamped to view minima and the container) installed via
/// [`Runtime::set_layout`], so leaf grids and PTY winsize follow exactly
/// like a keyboard resize. Anchored floats (pinned, detached, mode-floating)
/// fail soft at grab time and stay on the Alt+drag move path; that deferral
/// is documented on [`Runtime::begin_float_resize`]. `None` when no resize
/// is active; cleared on release. Presentation-adjacent but solver-committed
/// per step (each step is a clamped layout install, never interpolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatResizeState {
    /// Grabbed floating leaf (structural overlay only).
    pub leaf: ViewId,
    /// Border the grab holds (edges and corners).
    pub edge: ResizeEdge,
    /// Anchor column (container cells) at grab or last move.
    pub anchor_col: i32,
    /// Anchor row (container cells) at grab or last move.
    pub anchor_row: i32,
}

/// Active Mod+drag tiled-panel move (issue #1694, CTX-0966; issue #2337).
///
/// Wraps the headless [`DragMoveSession`] primitive: press lifts the tiled
/// leaf under the cursor and immediately arms a bounded, present-only ghost
/// plus the candidate-tile highlight; motion moves that ghost with the
/// pointer and tracks the advisory preview target, while the solver tree and
/// PTY geometry stay untouched; release commits exactly one swap (center
/// target) or insert (edge target) through [`Runtime::set_layout`]. Floating
/// (overlay-tier) leaves never start here; the Alt+drag floating path owns
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiledDragState {
    /// Headless move session (source + advisory preview target).
    pub session: DragMoveSession,
    /// Gesture-scoped admission: the source ghost was admitted by the
    /// tracker (grab or a capacity retry). A completed arm holds its final
    /// visible state, so motion never re-triggers it (CodeRabbit 2026:
    /// re-triggering a completed arm re-pulsed the ghost on long drags).
    /// Set only when [`Runtime::trigger_animation`] admits; a refused arm
    /// (instant policy, full capacity) retries on later motion.
    source_armed: bool,
    /// Last hover target admitted this gesture (`None` for no hover yet or
    /// background). The highlight arms only on hover change; a completed
    /// arm holds its final visible state until release. Same retry rule as
    /// the source: refused arms stay unrecorded and retry.
    armed_target: Option<ViewId>,
    /// Pointer position at grab in physical pixels (rounded). Ghost motion
    /// is the pointer delta from here, so the grab point stays under the
    /// cursor exactly like a Hyprland window drag.
    press_px: (i32, i32),
    /// Last pointer position in physical pixels (rounded). The drop plan and
    /// release read this instead of the global cursor, so a stale
    /// `last_cursor` can never hit-test a different tile.
    last_px: (i32, i32),
    /// Present-only ghost translation from the source's committed frame,
    /// bounded so the ghost stays inside the container.
    ghost_dx: i32,
    /// Present-only ghost vertical translation.
    ghost_dy: i32,
    /// Permitted horizontal ghost translation range at grab.
    ghost_min_dx: i32,
    /// Maximum horizontal ghost translation at grab.
    ghost_max_dx: i32,
    /// Permitted vertical ghost translation range at grab.
    ghost_min_dy: i32,
    /// Maximum vertical ghost translation at grab.
    ghost_max_dy: i32,
}

/// Active border-drag resize: which split divider(s) are grabbed plus the
/// press-time anchor in container cells (issue #1348, #1445, #2572).
///
/// A plain left press on a split handle grabs the divider; motion adjusts
/// the adjacent split ratio live through the same clamped geometry the
/// keyboard resize path uses. Corner-drag (issue #1445: Hyprland/Niri
/// 4-way model) grabs up to two perpendicular axes; cross-drag (issue
/// #2572) additionally grabs every collinear segment of both crossed
/// dividers, so a four-pane inner junction moves its whole shared vertical
/// and horizontal lines and all four adjacent panels resize together.
#[derive(Debug, Clone, PartialEq)]
pub struct BorderDragState {
    /// Paths of the grabbed splits (same indexing as
    /// [`LayoutNode::set_split_ratio_at`](bitty_ui::LayoutNode::set_split_ratio_at)).
    /// Single-edge drag has one entry; a cross has the column split plus
    /// every collinear row split (and vice versa for nested crosses).
    pub splits: Vec<(Vec<usize>, SplitAxis)>,
    /// Anchor column (container cells) at the grabbing press.
    pub anchor_col: i32,
    /// Anchor row (container cells) at the grabbing press.
    pub anchor_row: i32,
    /// Layout at the press: drag targets are computed absolutely from
    /// these ratios and allocations, so hitting a clamp never introduces a
    /// dead zone or a single-axis snap on the way back (issue #2572).
    press_layout: LayoutNode,
    /// Container at the press (cells), paired with `press_layout`.
    press_container: Rect,
    /// Gaps at the press (cells), paired with `press_layout`.
    press_gaps: Gaps,
    /// The consumed press still owns its release after external cancellation.
    canceled: bool,
}

/// Rounds a cursor position to physical pixels, fail-closed.
///
/// CTX-1150: the Alt+drag pixel remainder tracks the pointer in whole
/// physical pixels. Non-finite coordinates yield `0` (the motion path
/// treats a non-finite position as no movement and stays armed); huge
/// values saturate instead of wrapping.
fn round_cursor_px(pos: CursorPosition) -> (i32, i32) {
    fn round_one(v: f64) -> i32 {
        if !v.is_finite() {
            return 0;
        }
        let r = v.round();
        if r >= f64::from(i32::MAX) {
            i32::MAX
        } else if r <= f64::from(i32::MIN) {
            i32::MIN
        } else {
            r as i32
        }
    }
    (round_one(pos.x), round_one(pos.y))
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
    /// Requires Alt held (the working Mod per OQ-052; Super is the
    /// compositor's window-drag Mod and must not grab in-app floats per
    /// issue #2140) and Shift released (Shift forces selection, per
    /// the CTX-0181 precedent) plus a known cursor over a floating leaf.
    /// The grab also focuses the dragged pane (Hyprland-like). Returns
    /// `true` when the drag started (caller consumes the press and skips
    /// selection); `false` leaves all state untouched so the press falls
    /// through to selection ("without breaking selection").
    ///
    /// CTX-1150: fails soft while a float resize holds the pointer (edge
    /// wins over interior; see [`Self::begin_float_resize`]).
    /// Whether an Alt+float gesture may grab (issue #2137, narrowed by
    /// issue #2140).
    ///
    /// Mod is Alt only (the working Mod per OQ-052); Shift always forces
    /// selection per the CTX-0181 precedent. Super is deliberately NOT
    /// accepted even though Hyprland muscle memory suggests it: the
    /// compositor owns Super+drag for window move/resize, so an in-app
    /// Super+drag fights the window gesture — the window moves and
    /// re-tiles on release while the float bounces back (#2140). Shared
    /// by the float move and float edge-resize grabs so the two can
    /// never disagree about which modifier owns floats.
    fn float_mod_held(&self) -> bool {
        self.alt_pressed && !self.shift_pressed
    }

    pub fn begin_alt_drag(&mut self) -> bool {
        if !self.float_mod_held() {
            return false;
        }
        if self.float_resize.is_some() {
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
        // a workspace-local detached float passes the same way (CTX-1147);
        // its motion accumulates a present-time re-anchor offset instead.
        // Tiled leaves fail soft here and the Mod
        // tiled-drag path (`begin_tiled_drag_at`) owns them instead
        // (issue #1694, CTX-0966).
        if !self
            .layout
            .move_overlay_containing(leaf, 0, 0, self.container)
            && !self.leaf_is_floating(leaf)
        {
            return false;
        }
        let (anchor_px_x, anchor_px_y) = round_cursor_px(cursor);
        self.alt_drag = Some(AltDragState {
            leaf,
            anchor_col: i32::from(anchor.col),
            anchor_row: i32::from(anchor.row),
            anchor_px_x,
            anchor_px_y,
            pixel_dx: 0,
            pixel_dy: 0,
        });
        // Dragging focuses the grabbed pane (no-op present-wise when
        // already focused; `set_focus` dirties only on change).
        self.set_focus(leaf);
        true
    }

    /// Moves the active Alt+drag to `pos`, offsetting the grabbed overlay
    /// by the cell delta since the grab (or last move).
    ///
    /// CTX-1150 (issue #1980): pixel-smooth motion. The solver stays
    /// cell-quantized; the sub-cell remainder is a present-only
    /// physical-pixel translation on the dragged frame (see
    /// [`Runtime::present_frames`]). A zero cell delta with a pixel remainder
    /// still moves the frame; release snaps to cells by dropping the grab.
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
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return true;
        }
        let cell = self.cursor_to_cell(pos);
        let dx = i32::from(cell.col) - drag.anchor_col;
        let dy = i32::from(cell.row) - drag.anchor_row;
        let live = self.live_cell_metrics();
        let cell_w = i32::try_from(live.width).unwrap_or(i32::MAX);
        let cell_h = i32::try_from(live.height).unwrap_or(i32::MAX);
        let (cur_px_x, cur_px_y) = round_cursor_px(pos);
        let dx_px_raw = cur_px_x.saturating_sub(drag.anchor_px_x);
        let dy_px_raw = cur_px_y.saturating_sub(drag.anchor_px_y);
        let (inc_x, inc_y) = if cell_w <= 0 || cell_h <= 0 {
            (0, 0)
        } else {
            let ix = (i64::from(dx_px_raw) - i64::from(dx) * i64::from(cell_w))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            let iy = (i64::from(dy_px_raw) - i64::from(dy) * i64::from(cell_h))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            (ix, iy)
        };
        let mut next_px_x = drag.pixel_dx.saturating_add(inc_x);
        let mut next_px_y = drag.pixel_dy.saturating_add(inc_y);
        if cell_w > 0 {
            next_px_x = next_px_x.clamp(-cell_w, cell_w);
        } else {
            next_px_x = 0;
        }
        if cell_h > 0 {
            next_px_y = next_px_y.clamp(-cell_h, cell_h);
        } else {
            next_px_y = 0;
        }
        if dx == 0 && dy == 0 && next_px_x == drag.pixel_dx && next_px_y == drag.pixel_dy {
            return true;
        }
        if (dx != 0 || dy != 0)
            && !self
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
            // keeps it (the drop point becomes the new anchor), and unpin
            // drops it. The pinned store itself is never written here.
            //
            // CTX-1147: a workspace-local detached float is anchored the
            // same way; its motion accumulates in `float_offsets` (same
            // clamp discipline, same present composite) and restore drops
            // it. The float store itself is never written here.
            //
            // CodeRabbit 1893: the STORED offset clamps into the permitted
            // range from `pinned_anchor` (the same base + cascade +
            // container values the composite uses) right after the
            // saturating add. Present only clamps the composed origin, so an
            // unclamped store would keep a dead zone when dragging back
            // (drag 30 past an 8-cell margin, drag back 1, the frame sits
            // still) and teleport on a later container grow.
            if self.pinned.contains(drag.leaf) {
                let (cur_dx, cur_dy) = self
                    .pinned_offsets
                    .get(&drag.leaf)
                    .copied()
                    .unwrap_or((0, 0));
                let (next_dx, next_dy) = (cur_dx.saturating_add(dx), cur_dy.saturating_add(dy));
                let (stored_dx, stored_dy) = match self.pinned_anchor(drag.leaf) {
                    Some(anchor) => (
                        next_dx.clamp(anchor.dx_range.0, anchor.dx_range.1),
                        next_dy.clamp(anchor.dy_range.0, anchor.dy_range.1),
                    ),
                    // Not a composited pin (defensive duplicate also living
                    // in the base layout): the offset paints nothing, so
                    // store it verbatim; unpin still drops the entry.
                    None => (next_dx, next_dy),
                };
                self.pinned_offsets
                    .insert(drag.leaf, (stored_dx, stored_dy));
            } else if self.floats.contains(drag.leaf) {
                // CTX-1147: a detached float re-anchors exactly like a pin,
                // against its own `float_anchor` range; restore drops the
                // entry.
                let (cur_dx, cur_dy) = self
                    .float_offsets
                    .get(&drag.leaf)
                    .copied()
                    .unwrap_or((0, 0));
                let (next_dx, next_dy) = (cur_dx.saturating_add(dx), cur_dy.saturating_add(dy));
                let (stored_dx, stored_dy) = match self.float_anchor(drag.leaf) {
                    Some(anchor) => (
                        next_dx.clamp(anchor.dx_range.0, anchor.dx_range.1),
                        next_dy.clamp(anchor.dy_range.0, anchor.dy_range.1),
                    ),
                    // Not a composited float (defensive duplicate also
                    // living in the base layout): the offset paints nothing,
                    // so store it verbatim; restore still drops the entry.
                    None => (next_dx, next_dy),
                };
                self.float_offsets.insert(drag.leaf, (stored_dx, stored_dy));
            } else if !self.leaf_is_floating(drag.leaf) {
                self.alt_drag = None;
                return false;
            }
        }
        // The layout commits immediately (terminal content is never
        // interpolated); only the leaf chrome ring fades, and repeat updates
        // restart the bounded transition instead of accumulating. CTX-1150:
        // sub-cell motion arms the same transition even when the cell model
        // did not move, so the pixel ghost never lags a frame behind.
        self.pending_full_redraw = true;
        self.trigger_animation(AnimationKind::Drag, Some(drag.leaf), now);
        self.alt_drag = Some(AltDragState {
            leaf: drag.leaf,
            anchor_col: i32::from(cell.col),
            anchor_row: i32::from(cell.row),
            anchor_px_x: cur_px_x,
            anchor_px_y: cur_px_y,
            pixel_dx: next_px_x,
            pixel_dy: next_px_y,
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
    ///
    /// CTX-1147: ending the drag keeps a detached float's accumulated
    /// re-anchor offset the same way; restore drops it.
    ///
    /// CTX-1150: ending the drag drops the present-only pixel remainder, so
    /// the frame snaps to cells. Cell state (moved bounds, kept offsets)
    /// persists per the disciplines above; only the sub-cell ghost clears.
    pub fn end_alt_drag(&mut self) -> bool {
        if self.alt_drag.is_none() {
            return false;
        }
        self.alt_drag = None;
        true
    }

    /// Whether an Alt+edge float resize is currently active (CTX-1150).
    #[must_use]
    pub fn float_resize_active(&self) -> bool {
        self.float_resize.is_some()
    }

    /// Grabbed edge for the active float resize, if any (CTX-1150 test seam).
    #[must_use]
    pub fn float_resize_edge(&self) -> Option<ResizeEdge> {
        self.float_resize.map(|resize| resize.edge)
    }

    /// Grabbed leaf for the active float resize, if any (CTX-1150 test seam).
    #[must_use]
    pub fn float_resize_leaf(&self) -> Option<ViewId> {
        self.float_resize.map(|resize| resize.leaf)
    }

    /// Edge under the pointer for a structural float, if any (CTX-1150 hover
    /// affordance query).
    ///
    /// Pure query over [`detect_resize_edge`]: resolves the topmost floating
    /// leaf under `pos` through the present frames, reads its structural
    /// overlay bounds, and reports which border the pointer grabs. Returns
    /// `None` over interiors, background, and anchored floats (pinned,
    /// detached, mode-floating: deferred, see [`Self::begin_float_resize`]).
    /// The platform layer currently exposes no cursor-icon setter, so no
    /// caller sets a shape yet; the drag itself works without it (mirrors
    /// [`Self::border_drag_hover_at`]).
    #[must_use]
    pub fn float_resize_hover_at(&self, pos: CursorPosition) -> Option<ResizeEdge> {
        let (leaf, _) = self.cursor_to_present_cell(pos)?;
        let point = self.cursor_to_layout_point(pos)?;
        let bounds = self.layout.overlay_bounds_containing(leaf)?;
        detect_resize_edge(bounds, point, RESIZE_HANDLE_CELLS)
    }

    /// Attempts to grab a structural float border for an Alt+edge resize
    /// (CTX-1150).
    ///
    /// Requires Alt held (the Alt+drag move family owns
    /// floats; Super is the compositor's window-drag Mod and must not
    /// grab in-app floats per issue #2140. Shift forces selection per
    /// the CTX-0181 precedent), no active
    /// drag of any kind, a known cursor over a structural float border
    /// ([`detect_resize_edge`] with a [`RESIZE_HANDLE_CELLS`]-cell hit
    /// area). The grab focuses the resized pane. Returns `true` when the
    /// resize started (caller consumes the press and skips selection);
    /// `false` leaves all state untouched so the press falls through to the
    /// Alt+drag move (interior) or selection.
    ///
    /// Anchored floats (pinned, detached, mode-floating) fail soft here on
    /// purpose: their present size is container-derived with no free rect to
    /// resize, so the interior move path keeps them. Free-rect resizing for
    /// those kinds stays a follow-up; this keeps the solver untouched and
    /// the gesture fail-closed.
    pub fn begin_float_resize(&mut self) -> bool {
        if !self.float_mod_held() {
            return false;
        }
        if self.alt_drag.is_some()
            || self.tiled_drag.is_some()
            || self.border_drag.is_some()
            || self.float_resize.is_some()
        {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        let Some((leaf, _)) = self.cursor_to_present_cell(cursor) else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(cursor) else {
            return false;
        };
        let Some(bounds) = self.layout.overlay_bounds_containing(leaf) else {
            return false;
        };
        let Some(edge) = detect_resize_edge(bounds, point, RESIZE_HANDLE_CELLS) else {
            return false;
        };
        self.float_resize = Some(FloatResizeState {
            leaf,
            edge,
            anchor_col: i32::from(point.x),
            anchor_row: i32::from(point.y),
        });
        self.set_focus(leaf);
        self.clear_hover_pending();
        true
    }

    /// Moves the active float resize to `pos`, resizing the grabbed overlay
    /// bounds live (CTX-1150).
    ///
    /// The cell delta since the grab (or last move) flows through
    /// [`resize_floating_rect`] — the same clamped geometry the unit tests
    /// pin (view minima, container clip) — installed via
    /// [`Runtime::set_layout`] so leaf Views, pane sessions, and the primary
    /// grid reflow exactly like a keyboard resize (live grid and PTY follow;
    /// no deferral). Overshoot clamps fail-closed; a zero delta keeps the
    /// resize armed with no redraw. When the layout no longer owns the leaf
    /// (closed mid-drag) the resize ends fail-soft and `false` is returned.
    ///
    /// Returns `true` when a resize was active (caller consumes the motion).
    pub fn update_float_resize(&mut self, pos: CursorPosition) -> bool {
        self.update_float_resize_at(pos, Instant::now())
    }

    /// [`Self::update_float_resize`] with an explicit wall clock (CTX-0967
    /// virtual-clock seam: tests arm the resize transition deterministically).
    pub fn update_float_resize_at(&mut self, pos: CursorPosition, now: Instant) -> bool {
        let Some(resize) = self.float_resize else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(pos) else {
            return true;
        };
        let dx_raw = i32::from(point.x) - resize.anchor_col;
        let dy_raw = i32::from(point.y) - resize.anchor_row;
        if dx_raw == 0 && dy_raw == 0 {
            return true;
        }
        let Some(current) = self.layout.overlay_bounds_containing(resize.leaf) else {
            self.float_resize = None;
            return false;
        };
        // `resize_floating_rect` narrows to `i16`: clamp the cell delta so
        // a pointer teleport can never wrap the resize step.
        let dx = dx_raw.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let dy = dy_raw.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let next = resize_floating_rect(current, resize.edge, dx, dy, self.container);
        if next != current {
            let mut layout = self.layout.clone();
            if !layout.set_overlay_bounds_containing(resize.leaf, next) {
                self.float_resize = None;
                return false;
            }
            self.set_layout(layout);
            self.trigger_animation(AnimationKind::Resize, Some(resize.leaf), now);
        }
        self.float_resize = Some(FloatResizeState {
            leaf: resize.leaf,
            edge: resize.edge,
            anchor_col: i32::from(point.x),
            anchor_row: i32::from(point.y),
        });
        true
    }

    /// Ends the active float resize, if any. Returns `true` when one was
    /// active (caller skips the selection-release commit/copy: no selection
    /// was started by the grabbing press).
    pub fn end_float_resize(&mut self) -> bool {
        if self.float_resize.is_none() {
            return false;
        }
        self.float_resize = None;
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

    /// Hyprland-style bounded drop plan for the active Mod+drag at the last
    /// motion position (issue #2337 test seam and live-preview hook): the
    /// [`DropPlan`] a release would commit — a center swap or an edge insert
    /// — or `None` over background, over a floating overlay, on the source
    /// itself, or when no drag is active. The drag's own recorded pointer is
    /// used (never the global `last_cursor`), so the plan cannot go stale.
    /// Pure query; the tree is never mutated here.
    #[must_use]
    pub fn tiled_drag_drop_plan(&self) -> Option<DropPlan> {
        let drag = self.tiled_drag.as_ref()?;
        let cursor = CursorPosition {
            x: f64::from(drag.last_px.0),
            y: f64::from(drag.last_px.1),
        };
        let point = self.cursor_to_layout_point(cursor)?;
        drop_plan_for_point(
            &self.layout,
            self.container,
            self.gaps(),
            drag.session.source(),
            point,
        )
    }

    /// Insert-only compatibility view of [`Self::tiled_drag_drop_plan`]:
    /// returns the edge [`DropSpec`] when the release would insert, and
    /// `None` for a center swap or no plan.
    #[must_use]
    pub fn tiled_drag_drop_spec(&self) -> Option<DropSpec> {
        match self.tiled_drag_drop_plan()? {
            DropPlan::Insert(spec) => Some(spec),
            DropPlan::Swap { .. } => None,
        }
    }

    /// Present-only ghost translation from the source's committed frame, in
    /// physical pixels, bounded inside the container (issue #2337). `None`
    /// when no drag is active.
    #[must_use]
    pub fn tiled_drag_ghost_offset(&self) -> Option<(i32, i32)> {
        self.tiled_drag
            .as_ref()
            .map(|drag| (drag.ghost_dx, drag.ghost_dy))
    }

    /// The translated paint frame of the dragged panel (issue #2337 test
    /// seam): the source's committed [`PresentFrame`](super::layout_focus::PresentFrame)
    /// moved by the bounded ghost offset. Solver truth, PTY geometry, and
    /// hit-testing continue to read [`Self::present_frames`], which stays
    /// byte-identical during the gesture.
    #[must_use]
    pub fn tiled_drag_preview_frame(&self) -> Option<super::layout_focus::PresentFrame> {
        let drag = self.tiled_drag.as_ref()?;
        let mut frame = self
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == drag.session.source())?;
        let (dx, dy) = self.clamp_tiled_drag_delta(&frame.frame);
        frame.frame.x = frame.frame.x.saturating_add(dx);
        frame.frame.y = frame.frame.y.saturating_add(dy);
        frame.content.x = frame.content.x.saturating_add(dx);
        frame.content.y = frame.content.y.saturating_add(dy);
        Some(frame)
    }

    /// Physical container area in the coordinate space of
    /// [`PresentFrame`](super::layout_focus::PresentFrame) rectangles.
    fn container_pixel_area(&self) -> bitty_render::geometry::RectPx {
        let live = self.live_cell_metrics();
        let scale = |value: u16, cell: u32| -> i32 {
            let raw = i64::from(value) * i64::from(cell);
            raw.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        bitty_render::geometry::RectPx::new(
            scale(self.container.x, live.width),
            scale(self.container.y, live.height),
            (u64::from(self.container.width) * u64::from(live.width)).min(u64::from(u32::MAX))
                as u32,
            (u64::from(self.container.height) * u64::from(live.height)).min(u64::from(u32::MAX))
                as u32,
        )
    }

    /// Clamps the stored ghost translation so `frame` stays fully inside the
    /// container. Recomputed at paint time, so a container resize mid-drag
    /// can never paint the ghost off-screen.
    fn clamp_tiled_drag_delta(&self, frame: &bitty_render::geometry::RectPx) -> (i32, i32) {
        let Some(drag) = self.tiled_drag.as_ref() else {
            return (0, 0);
        };
        let area = self.container_pixel_area();
        let min_dx = area.x.saturating_sub(frame.x);
        let max_dx = area
            .x
            .saturating_add(area.width as i32)
            .saturating_sub(frame.width as i32)
            .saturating_sub(frame.x)
            .max(min_dx);
        let min_dy = area.y.saturating_sub(frame.y);
        let max_dy = area
            .y
            .saturating_add(area.height as i32)
            .saturating_sub(frame.height as i32)
            .saturating_sub(frame.y)
            .max(min_dy);
        (
            drag.ghost_dx.clamp(min_dx, max_dx),
            drag.ghost_dy.clamp(min_dy, max_dy),
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
    ///
    /// CTX-1147: a workspace-local detached float is composited the same
    /// way, so it counts as floating too.
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
        if self.floats.contains(id) {
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
    ///
    /// Takes the caller's event clock (CTX-0967 virtual-clock seam: tests arm
    /// the drag transition deterministically). There is deliberately no
    /// wall-clock default: the live path already runs through
    /// `handle_mouse_input_at`, and an `Instant::now()` convenience here is
    /// exactly the #2703 bug (a drag that settles off the event clock).
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
        // CTX-1149 (issue #1979): the allocation hit-test above cannot see
        // store-based floats (pinned leaves, workspace-detached floats):
        // they live outside the layout tree, so the press resolves to the
        // tiled base leaf painted beneath them. When the painted topmost
        // leaf under the press is a floating cover, the cover owns the
        // press — grabbing the hidden base would move a pane the pointer
        // never visibly touched (the Alt path is already safe: its
        // present-based grab runs first and consumes the press; this guard
        // closes the Super path, which skips that grab).
        if let Some((top, _)) = self.cursor_to_present_cell(cursor) {
            if top != leaf && self.leaf_is_floating(top) {
                return false;
            }
        }
        if self.layout.leaf_count() < 2 {
            return false;
        }
        let Ok(mut session) = DragMoveSession::start(leaf, true) else {
            return false;
        };
        // Issue #2337: capture the committed frame once at grab so the ghost
        // is pointer-relative and bounded for the whole gesture. A missing
        // frame means the leaf is not paintable; fail soft rather than arm a
        // drag that cannot show feedback.
        let Some(frame) = self
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == leaf)
        else {
            return false;
        };
        let area = self.container_pixel_area();
        let min_dx = area.x.saturating_sub(frame.frame.x);
        let max_dx = area
            .x
            .saturating_add(area.width as i32)
            .saturating_sub(frame.frame.width as i32)
            .saturating_sub(frame.frame.x)
            .max(min_dx);
        let min_dy = area.y.saturating_sub(frame.frame.y);
        let max_dy = area
            .y
            .saturating_add(area.height as i32)
            .saturating_sub(frame.frame.height as i32)
            .saturating_sub(frame.frame.y)
            .max(min_dy);
        let press_px = round_cursor_px(cursor);
        // Issue #2337: the candidate tile is live from pointer-down. The
        // press point normally resolves to the source itself (a no-op), so
        // no foreign tile is highlighted until motion reaches one.
        if let Some(point) = self.cursor_to_layout_point(cursor) {
            session.preview(&self.layout, self.container, self.gaps(), point);
        }
        self.tiled_drag = Some(TiledDragState {
            session,
            source_armed: false,
            armed_target: None,
            press_px,
            last_px: press_px,
            ghost_dx: 0,
            ghost_dy: 0,
            ghost_min_dx: min_dx,
            ghost_max_dx: max_dx,
            ghost_min_dy: min_dy,
            ghost_max_dy: max_dy,
        });
        self.set_focus(leaf);
        self.clear_hover_pending();
        // Issue #2337: the grab itself must present a frame — the ghost sits
        // at the source and the candidate highlight is armed below, so the
        // gesture never waits for a first motion to show feedback.
        self.pending_full_redraw = true;
        // CTX-1070 (issue #1811): the grab arms the drag transition on the
        // dragged frame (ghost outline). The layout is untouched — only the
        // leaf's chrome ring fades — and the gesture holds the arm so
        // progress accumulates toward full visibility instead of
        // restarting (CTX-1167, see `update_tiled_drag_at`). Admission is
        // recorded so motion never re-triggers a completed arm; a refused
        // grab (instant policy, full capacity) retries on later motion.
        let admitted = self.trigger_animation(AnimationKind::Drag, Some(leaf), now);
        if let Some(drag) = self.tiled_drag.as_mut() {
            drag.source_armed = admitted;
        }
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
    ///
    /// Takes the caller's event clock (CTX-0967 virtual-clock seam: tests arm
    /// the drag transition deterministically). No wall-clock default exists
    /// by design — see [`Self::begin_tiled_drag_at`].
    pub fn update_tiled_drag_at(&mut self, pos: CursorPosition, now: Instant) -> bool {
        let point = self.cursor_to_layout_point(pos);
        let (container, gaps) = (self.container, self.gaps());
        let Some(drag) = self.tiled_drag.as_mut() else {
            return false;
        };
        if self.layout.find_leaf(drag.session.source()).is_none() {
            self.tiled_drag = None;
            self.pending_full_redraw = true;
            return false;
        }
        // Issue #2337: the ghost follows the pointer one-to-one from the
        // grab point, clamped inside the container. This is present-only
        // motion: the solver tree, leaf grids, and PTYs are never touched.
        let (px_x, px_y) = round_cursor_px(pos);
        let raw_dx = px_x.saturating_sub(drag.press_px.0);
        let raw_dy = px_y.saturating_sub(drag.press_px.1);
        let next_dx = raw_dx.clamp(drag.ghost_min_dx, drag.ghost_max_dx);
        let next_dy = raw_dy.clamp(drag.ghost_min_dy, drag.ghost_max_dy);
        let moved =
            (next_dx, next_dy) != (drag.ghost_dx, drag.ghost_dy) || (px_x, px_y) != drag.last_px;
        drag.ghost_dx = next_dx;
        drag.ghost_dy = next_dy;
        drag.last_px = (px_x, px_y);
        let before = drag.session.preview_target();
        match point {
            Some(point) => {
                drag.session.preview(&self.layout, container, gaps, point);
            }
            // Pointer left the container: clear the candidate rather than
            // keeping a stale highlight from the last mapped cell.
            None => drag.session.clear_preview(),
        }
        let target_changed = drag.session.preview_target() != before;
        if moved || target_changed {
            // Issue #2573: ghost motion is paint-only. The solver tree, leaf
            // grids, and PTY winsizes stay untouched, so this must not force
            // the full leaf pass: the next present re-emits the translated
            // ghost and the untouched siblings from their retained
            // primitives. A full invalidation (grab, commit, cancel) still
            // wins when both flags are armed.
            self.pending_paint_redraw = true;
        }
        // CTX-1167 (issue #1981): accumulate, never restart. The grab
        // admitted the source ghost and the first hover admits the target
        // highlight; gesture-scoped flags hold those admissions so the
        // eased ring progress climbs once to full visibility and stays
        // there. A completed arm reads back `None` from the tracker but
        // already holds its final visible state, so motion must not
        // re-trigger it (CodeRabbit 2026: re-triggering re-pulsed the
        // ghost on drags past the animation duration). Only a refused arm
        // retries: instant-cut policies refuse every motion and still arm
        // nothing, exactly as before, and a capacity refusal re-arms on a
        // later motion once budget frees.
        let source = drag.session.source();
        let target = drag.session.preview_target();
        let source_held = drag.source_armed;
        let target_held = drag.armed_target;
        if !source_held
            && self
                .animation_progress(AnimationKind::Drag, Some(source), now)
                .is_none()
            && self.trigger_animation(AnimationKind::Drag, Some(source), now)
        {
            if let Some(drag) = self.tiled_drag.as_mut() {
                drag.source_armed = true;
            }
        }
        match target {
            Some(hovered) if target_held != Some(hovered) => {
                if self
                    .animation_progress(AnimationKind::Drag, Some(hovered), now)
                    .is_none()
                    && self.trigger_animation(AnimationKind::Drag, Some(hovered), now)
                {
                    if let Some(drag) = self.tiled_drag.as_mut() {
                        drag.armed_target = Some(hovered);
                    }
                }
            }
            None if target_held.is_some() => {
                // Hover left for the background: forget the last highlight
                // so a later re-hover re-arms per the hover-change rule.
                if let Some(drag) = self.tiled_drag.as_mut() {
                    drag.armed_target = None;
                }
            }
            _ => {}
        }
        true
    }

    /// Ends the active Mod+drag, committing exactly one move when it lands
    /// on another leaf (issue #1694; issue #2337 reflow).
    ///
    /// Computes the bounded Hyprland-style [`DropPlan`](bitty_ui::DropPlan)
    /// at the drag's own recorded pointer: a center target swaps the two
    /// panels in place, an edge target inserts beside it at a balanced split.
    /// The chosen mutation runs once through [`Runtime::set_layout`], so leaf
    /// Views, pane sessions, PTYs, and the primary grid reflow exactly like a
    /// keyboard move. Self-drops, background drops, unmappable releases, and
    /// releases after a mid-drag layout install end the gesture without a
    /// drop (the install's own tree remains). Always
    /// returns `true` when a drag was active (caller skips the
    /// selection-release commit/copy: the grabbing press never started a
    /// selection, so there is nothing to commit and stale highlights must
    /// not auto-copy); `false` when no drag was active.
    ///
    /// Takes the caller's event clock so the committed drop's `Move`
    /// transition starts at the release instant (CTX-0967 virtual-clock seam,
    /// CTX-1594 issue #2703). No wall-clock default exists by design: every
    /// release arm in `handle_mouse_input_at` passes the event's `now`, and a
    /// convenience wrapper would silently reintroduce the off-clock start.
    pub fn end_tiled_drag_at(&mut self, now: Instant) -> bool {
        let Some(drag) = self.tiled_drag.as_ref() else {
            return false;
        };
        let source = drag.session.source();
        let cursor = CursorPosition {
            x: f64::from(drag.last_px.0),
            y: f64::from(drag.last_px.1),
        };
        let plan = self.cursor_to_layout_point(cursor).and_then(|point| {
            drop_plan_for_point(&self.layout, self.container, self.gaps(), source, point)
        });
        self.tiled_drag = None;
        let Some(plan) = plan else {
            // Background, overlay, or self: a release is a no-op, never a
            // fallback insert.
            self.pending_full_redraw = true;
            return true;
        };
        let mut next = self.layout.clone();
        let committed = match plan {
            DropPlan::Swap { target } => next.swap_leaves(source, target),
            DropPlan::Insert(spec) => {
                next.reparent_leaf(source, spec.target, spec.axis, spec.ratio, spec.after)
            }
        };
        if !committed {
            self.pending_full_redraw = true;
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
    /// window, `Esc` cancelled mid-drag, or a layout install replaced the
    /// tree). The preview never mutates the tree, so cancel restores the
    /// pre-drag layout byte-identically for pointer/Esc cancellation; a
    /// layout install keeps its own new tree (for example a close) and only
    /// the drop is abandoned. No re-parent, no selection, no residue. Arms a
    /// repaint when a drag was active so the ghost and drop-target highlight
    /// clear on the next frame. Returns `true` when a drag was active.
    pub fn cancel_tiled_drag(&mut self) -> bool {
        if self.tiled_drag.is_none() {
            return false;
        }
        self.tiled_drag = None;
        self.pending_full_redraw = true;
        true
    }

    /// Applies the active Mod+drag ghost to the paint-only allocation list
    /// (issue #2337): translates the source frame by the bounded pointer
    /// delta, lifts it to the float tier, and moves it last so it paints
    /// above the untouched tiled layout. Logical allocations, hit-testing,
    /// leaf grids, and PTY sizes never see this transform.
    pub(super) fn apply_tiled_drag_paint(
        &self,
        frames: &mut Vec<super::layout_focus::PresentFrame>,
    ) -> bool {
        let Some(drag) = self.tiled_drag.as_ref() else {
            return false;
        };
        let source = drag.session.source();
        let Some(index) = frames.iter().position(|frame| frame.view == source) else {
            return false;
        };
        let mut frame = frames.remove(index);
        let (dx, dy) = self.clamp_tiled_drag_delta(&frame.frame);
        frame.frame.x = frame.frame.x.saturating_add(dx);
        frame.frame.y = frame.frame.y.saturating_add(dy);
        frame.content.x = frame.content.x.saturating_add(dx);
        frame.content.y = frame.content.y.saturating_add(dy);
        // A dragged panel is a transient overlay: painting it last (with the
        // float tier) keeps its content above every tiled sibling without
        // reflowing them.
        frame.tier = Some(OverlayTier::Float);
        frames.push(frame);
        true
    }

    /// Maps a physical cursor position to container-cell coordinates for
    /// split-handle and tiled-drop hit-testing (issues #1348, #1710).
    ///
    /// Removes the window padding inset plus the decoration outer inset
    /// (CTX-1168, issue #1983) and converts with live cell
    /// metrics, keeping the point in container coordinates: layout
    /// hit-tests (`hit_test_leaf`, `hit_test_split_handle(s)`,
    /// `drop_plan_for_point`) apply the cell outer gap inset themselves via
    /// `layout_with_gaps`, so subtracting cell `gaps_out` here would shift the
    /// drop target with nonzero outer gaps. The px decoration outer gap is
    /// different: cell allocations never include it, so headed frames sit
    /// `gaps_out` px inside the container origin and a pad-only mapping
    /// drifts sub-cell at scale 1. Unlike the leaf variant there
    /// is no leaf lookup: gap bands and zero-gap boundary lines own no
    /// leaf but may own a split handle.
    /// Returns `None` when the pointer maps outside the container origin
    /// or the cell metrics are degenerate.
    fn cursor_to_layout_point(&self, pos: CursorPosition) -> Option<UiPoint> {
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return None;
        }
        let live = self.live_cell_metrics();
        let cell_w = live.width as f64;
        let cell_h = live.height as f64;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let pad_px = f64::from(self.window_padding_physical());
        let deco_outer_px = {
            let scaled = f64::from(self.config.decoration.gaps_out) * self.dpi_scale();
            if !scaled.is_finite() || scaled <= 0.0 {
                0.0
            } else {
                scaled.round()
            }
        };
        let x = pos.x - pad_px - deco_outer_px;
        let y = pos.y - pad_px - deco_outer_px;
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

    /// Whether `pos` lies inside any present content rectangle (CTX-1168).
    ///
    /// Headed content starts inside the frame (outer plus border plus inset):
    /// a pointer there owns a selection, never a divider grab. The cell
    /// hit test below still reports the zero-gap boundary cols (39 and 40
    /// for a 40|40 split), so without this guard a content press would
    /// resize instead of selecting once the outer inset is accounted.
    /// Band, border, and inset presses (frame but outside content, or no
    /// frame) return false so dividers still grab.
    fn pointer_in_content(&self, pos: CursorPosition) -> bool {
        let pad = f64::from(self.window_padding_physical());
        let x = pos.x - pad;
        let y = pos.y - pad;
        if x < 0.0 || y < 0.0 {
            return false;
        }
        self.present_frames().into_iter().any(|frame| {
            let c = frame.content;
            if c.width == 0 || c.height == 0 {
                return false;
            }
            let right = f64::from(c.x) + f64::from(c.width);
            let bottom = f64::from(c.y) + f64::from(c.height);
            x >= f64::from(c.x) && x < right && y >= f64::from(c.y) && y < bottom
        })
    }

    /// Split axis under the pointer, if any (issue #1348 hover affordance).
    ///
    /// Pure query over [`LayoutNode::hit_test_split_handle`]: returns the
    /// divider axis so a future platform cursor-icon API can show a
    /// column/row-resize shape on hover. A cross intersection reports its
    /// column axis (the shared vertical divider). The platform layer
    /// currently exposes no cursor-icon setter, so no caller sets a shape
    /// yet; the drag itself works without it.
    #[must_use]
    pub fn border_drag_hover_at(&self, pos: CursorPosition) -> Option<SplitAxis> {
        if self.pointer_in_content(pos) {
            return None;
        }
        let point = self.cursor_to_layout_point(pos)?;
        let path = self
            .layout
            .hit_test_split_handle(self.container, self.gaps(), point)?;
        self.layout.split_axis_at(&path)
    }

    /// Whether a border-drag resize is currently active.
    #[must_use]
    pub fn border_drag_active(&self) -> bool {
        self.border_drag.as_ref().is_some_and(|drag| !drag.canceled)
    }

    /// Grabbed split divider(s) for the active border drag, if any (issues
    /// #1445 corner-drag and #2572 cross-drag test seam).
    ///
    /// Returns the divider paths and axes grabbed by
    /// [`Self::begin_border_drag`]: one entry for a single-edge drag, and
    /// every collinear segment of both crossed lines for a four-pane
    /// junction (a 2x2 cross yields three entries: the root column split
    /// plus both row segments). Headless-testable without a window.
    #[must_use]
    pub fn border_drag_splits(&self) -> Option<Vec<(Vec<usize>, SplitAxis)>> {
        self.border_drag
            .as_ref()
            .filter(|drag| !drag.canceled)
            .map(|drag| drag.splits.clone())
    }

    /// Attempts to grab the split divider(s) under the last known cursor for
    /// a border-drag resize (issue #1348, #1445 corner-drag, #2572
    /// cross-drag).
    ///
    /// Requires a plain press: Shift, Alt, and Super released (those force
    /// the selection, Alt+drag, and Mod tiled-drag paths per the
    /// CTX-0181/CTX-0260/CTX-0966 precedents)
    /// plus a known cursor over a split handle
    /// ([`LayoutNode::hit_test_split_handle_kind`]). Callers run this after
    /// the mouse-capture and scrollbar checks, so a mouse-mode app and the
    /// scroll thumb keep the pointer. Returns `true` when the drag started
    /// (caller consumes the press and skips selection); `false` leaves all
    /// state untouched so the press falls through to selection.
    ///
    /// Cross-drag (issues #1445/#2572): when the cursor is at the
    /// intersection of perpendicular splits (e.g., a 2x2 grid center), the
    /// hit test returns every collinear segment of both dividers. All of
    /// them are grabbed and resized together (Hyprland/Niri model), so the
    /// whole shared vertical and horizontal lines move with the pointer.
    pub fn begin_border_drag(&mut self) -> bool {
        if self.alt_pressed || self.super_pressed || self.shift_pressed {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        if self.pointer_in_content(cursor) {
            return false;
        }
        let Some(point) = self.cursor_to_layout_point(cursor) else {
            return false;
        };
        let gaps = self.gaps();
        let Some(handle) = self
            .layout
            .hit_test_split_handle_kind(self.container, gaps, point)
        else {
            return false;
        };
        let splits = handle.splits();
        if splits.is_empty() {
            return false;
        }
        // Bounded grab: the hit test only returns collinear segments of at
        // most two perpendicular dividers, so the segment count is bounded
        // by the tree's leaf count. Fail closed on an impossible oversized
        // set rather than driving an unbounded drag.
        if splits.len() > bitty_ui::MAX_VIEWS_PER_WORKSPACE_TREE {
            return false;
        }
        self.border_drag = Some(BorderDragState {
            splits,
            anchor_col: i32::from(point.x),
            anchor_row: i32::from(point.y),
            press_layout: self.layout.clone(),
            press_container: self.container,
            press_gaps: gaps,
            canceled: false,
        });
        // A border owns no leaf, so — unlike click-to-focus — the grab
        // moves no focus. A pending hover dwell is dropped: the pointer is
        // committed to a gesture, not a hover target.
        self.clear_hover_pending();
        true
    }

    /// Moves the active border drag to `pos`, adjusting the grabbed split
    /// ratio(s) live (issue #1348, #1445 corner-drag, #2572 cross-drag).
    ///
    /// Each grabbed split is driven absolutely from its press-time ratio and
    /// allocation: the pointer's cell delta since the press moves the
    /// divider, clamped by [`LayoutNode::drag_split_ratio`] to the
    /// gap-aware per-pane 10x4 minimum. Axis handling stays independent, so
    /// a vertical clamp at the minimum never disturbs horizontal motion and
    /// there is no clamp dead zone or single-axis snap when the pointer
    /// reverses (issue #2572). The clamped ratios commit immediately
    /// through [`Runtime::set_layout`], so leaf Views, pane sessions, and
    /// the primary grid reflow exactly like a keyboard resize (sizes
    /// persist in the tree per layout). When a grabbed split no longer
    /// exists (pane closed mid-drag) the drag ends fail-soft and `false` is
    /// returned so the motion falls through to normal handling.
    ///
    /// Cross-drag: a four-pane junction grabs the shared column split plus
    /// every collinear row split (or vice versa), so all adjacent panels
    /// resize together on a diagonal pointer motion.
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
        if drag.canceled {
            return false;
        }
        let Some(point) = self.cursor_to_layout_point(pos) else {
            // Unmappable motion (outside the container origin) keeps the
            // drag armed but changes nothing.
            return true;
        };
        let delta_col = i32::from(point.x) - drag.anchor_col;
        let delta_row = i32::from(point.y) - drag.anchor_row;
        // Recompute even for a zero delta: moving back to the press anchor
        // must restore the press-time allocation after an earlier motion.

        let mut next = self.layout.clone();
        let mut any_changed = false;

        // Apply the pointer delta to each grabbed split along its axis,
        // absolutely from the press geometry. A split that refuses the
        // delta (minimum pane size, degenerate extent) is skipped without
        // touching the other axis.
        for (path, axis) in &drag.splits {
            let raw_delta = match axis {
                SplitAxis::Horizontal => delta_col,
                SplitAxis::Vertical => delta_row,
            };
            if next.split_axis_at(path) != Some(*axis) {
                // Split no longer exists (closed mid-drag): end the drag.
                self.border_drag = None;
                return false;
            }
            let Some(base_ratio) = drag.press_layout.split_ratio_at(path) else {
                // Split vanished from the press layout: end the drag.
                self.border_drag = None;
                return false;
            };
            // `drag_split_ratio` narrows the delta to `i32` cells but the
            // ratio math is f32; clamp a pointer teleport to i16 so the
            // cell delta can never overflow the drag arithmetic.
            let delta = raw_delta.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            let Some(ratio) = drag.press_layout.drag_split_ratio(
                drag.press_container,
                drag.press_gaps,
                path,
                base_ratio,
                delta,
            ) else {
                // This axis refuses (min size / degenerate split); the
                // other axis still applies and the drag stays armed.
                continue;
            };
            let before = next.split_ratio_at(path);
            if !next.set_split_ratio_at(path, ratio) {
                // Split no longer exists (closed mid-drag): end the drag.
                self.border_drag = None;
                return false;
            }
            if next.split_ratio_at(path) != before {
                any_changed = true;
            }
        }

        if any_changed {
            // This layout is the internal commit of the active gesture, so
            // preserve its press-time anchor/container while reflowing the
            // newly committed ratios. Public layout replacements cancel the
            // drag before replacing the tree.
            self.commit_border_drag_layout(next);
            // CTX-0967: a live divider move arms the whole-surface resize
            // transition. The ratios commit immediately (terminal content is
            // never interpolated); only Core-owned chrome fades. `None`
            // because one gesture resizes every adjacent panel at once
            // (cross-drag touches up to four or more); repeat updates
            // restart the bounded transition instead of accumulating.
            self.trigger_animation(AnimationKind::Resize, None, now);
        }
        true
    }

    /// Ends the active border drag, if any. Returns `true` when one was
    /// active (caller skips the selection-release commit/copy: no selection
    /// was started by the grabbing press).
    pub fn end_border_drag(&mut self) -> bool {
        self.border_drag.take().is_some()
    }

    /// Consumes the left-button release owned by an externally canceled
    /// border drag. Returns `true` only for the tombstone, leaving a live
    /// drag untouched so capture routing keeps its existing behavior.
    pub(super) fn consume_canceled_border_drag_release(&mut self) -> bool {
        if self.border_drag.as_ref().is_some_and(|drag| drag.canceled) {
            self.border_drag = None;
            true
        } else {
            false
        }
    }

    /// Marks an active border drag canceled by an external geometry change.
    /// The tombstone is consumed by the matching left-button release through
    /// normal or capture routing, so selection-release/auto-copy cannot run.
    pub(super) fn cancel_border_drag(&mut self) {
        if let Some(drag) = self.border_drag.as_mut() {
            drag.canceled = true;
        }
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
    /// Content presses (inside any present content rect) are never dividers
    /// (CTX-1168): they own a selection and must still move focus.
    fn pointer_on_split_handle(&self, pos: CursorPosition) -> bool {
        if self.pointer_in_content(pos) {
            return false;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mod_state(alt: bool, super_: bool, shift: bool) -> Runtime {
        let mut rt = Runtime::with_defaults().expect("headless runtime must build");
        rt.alt_pressed = alt;
        rt.super_pressed = super_;
        rt.shift_pressed = shift;
        rt
    }

    #[test]
    fn float_mod_held_requires_alt_not_super_issue_2140() {
        // Issue #2140: Super+drag is the compositor's window gesture
        // (the window moves and re-tiles on release while the float
        // bounces back), so in-app float grabs are Alt-only. This
        // narrows the issue-#2137 acceptance: Super no longer grabs.
        assert!(!mod_state(false, false, false).float_mod_held());
        assert!(mod_state(true, false, false).float_mod_held());
        assert!(!mod_state(false, true, false).float_mod_held());
        assert!(mod_state(true, true, false).float_mod_held());
        assert!(!mod_state(true, false, true).float_mod_held());
        assert!(!mod_state(false, true, true).float_mod_held());
    }
}
