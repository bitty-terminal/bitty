//! `Runtime` — Hover-to-activate focus and Alt+drag floating-pane moves.
//!
//! CTX-0260 follow-through of DEC-0034: an opt-in hover moves keyboard
//! focus (`RuntimeConfig::focus_follows_mouse`, default off to preserve
//! click-to-focus) plus Alt+drag to move a tiled/floating pane position
//! where the layout model permits (floating [`LayoutNode::Overlay`] bounds
//! move; tiled splits/stacks have no movable position, so the grab is a
//! fail-soft no-op and the press falls through to selection).
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
//! `focus_follows_mouse` is off (the default). It shares the hover
//! hit-test ([`Runtime::cursor_to_leaf_cell`]) and Shift suppression, so a
//! click and a hover agree on which pane owns the pointer.
//!
//! Lane note: pointer chrome only. Selection, capture encoding, scrollbar
//! drags, and workspace switching belong to their owning paths; this module
//! only grabs/moves/releases the Alt-drag and applies the gated hover step.
//! Shift still forces the selection path (the CTX-0181 precedent): the grab
//! never starts while Shift is held and hover-focus is suppressed under
//! Shift.

use super::*;
use bitty_platform::CursorPosition;
use bitty_ui::{Point as UiPoint, SplitAxis};
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

/// Active border-drag resize: which split divider is grabbed plus the
/// press-time anchor in container cells (issue #1348).
///
/// A plain left press on a split handle grabs the divider; motion adjusts
/// the adjacent split ratio live through the same clamped geometry the
/// keyboard resize path uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorderDragState {
    /// Path of the grabbed split (same indexing as
    /// [`LayoutNode::set_split_ratio_at`](bitty_ui::LayoutNode::set_split_ratio_at)).
    pub path: Vec<usize>,
    /// Axis of the grabbed split (selects the motion component).
    pub axis: SplitAxis,
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
        // Topmost hit-test (paint order): overlapping floats own the cursor
        // over the base layer. The shared `cursor_to_leaf_cell` returns the
        // first (base) match, which would never grab a visible float.
        let anchor = self.cursor_to_cell(cursor);
        let leaf = self
            .layout_allocations()
            .into_iter()
            .rev()
            .find(|(_, rect)| {
                !rect.is_empty()
                    && u32::from(anchor.col) >= u32::from(rect.x)
                    && u32::from(anchor.row) >= u32::from(rect.y)
                    && u32::from(anchor.col) < rect.right()
                    && u32::from(anchor.row) < rect.bottom()
            })
            .map(|(id, _)| id);
        let Some(leaf) = leaf else {
            return false;
        };
        // Probe: a zero-delta move succeeds only where the layout model
        // permits (an owning float exists); tiled leaves fail soft here.
        if !self
            .layout
            .move_overlay_containing(leaf, 0, 0, self.container)
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
            self.alt_drag = None;
            return false;
        }
        self.alt_drag = Some(AltDragState {
            leaf: drag.leaf,
            anchor_col: i32::from(cell.col),
            anchor_row: i32::from(cell.row),
        });
        self.pending_full_redraw = true;
        true
    }

    /// Ends the active Alt+drag, if any. Returns `true` when one was active
    /// (caller skips the selection-release commit/copy: no selection was
    /// started by the grabbing press).
    pub fn end_alt_drag(&mut self) -> bool {
        if self.alt_drag.is_none() {
            return false;
        }
        self.alt_drag = None;
        true
    }

    /// Maps a physical cursor position to container-cell coordinates for
    /// split-handle hit-testing (issue #1348).
    ///
    /// Uses the same origin mapping as [`Runtime::cursor_to_leaf_cell`]
    /// (window padding plus the outer gap inset, live cell metrics) so a
    /// border press and a leaf press agree on which cell owns the pointer.
    /// Unlike the leaf variant there is no leaf lookup: gap bands and
    /// zero-gap boundary lines own no leaf but may own a split handle.
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
        let x = pos.x - pad_px - f64::from(self.config.gaps_out) * cell_w;
        let y = pos.y - pad_px - f64::from(self.config.gaps_out) * cell_h;
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

    /// Attempts to grab the split divider under the last known cursor for
    /// a border-drag resize (issue #1348).
    ///
    /// Requires a plain press: Shift and Alt released (those force the
    /// selection and Alt+drag paths per the CTX-0181/CTX-0260 precedents)
    /// plus a known cursor over a split handle
    /// ([`LayoutNode::hit_test_split_handle`]). Callers run this after the
    /// mouse-capture and scrollbar checks, so a mouse-mode app and the
    /// scroll thumb keep the pointer. Returns `true` when the drag started
    /// (caller consumes the press and skips selection); `false` leaves all
    /// state untouched so the press falls through to selection.
    pub fn begin_border_drag(&mut self) -> bool {
        if self.alt_pressed || self.shift_pressed {
            return false;
        }
        let Some(cursor) = self.last_cursor else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(cursor) else {
            return false;
        };
        let gaps = self.gaps();
        let Some(path) = self
            .layout
            .hit_test_split_handle(self.container, gaps, point)
        else {
            return false;
        };
        let Some(axis) = self.layout.split_axis_at(&path) else {
            return false;
        };
        self.border_drag = Some(BorderDragState {
            path,
            axis,
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
    /// ratio live (issue #1348).
    ///
    /// The cell delta since the grab (or last move) along the split axis
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
    /// Returns `true` when a drag was active (caller consumes the motion:
    /// no selection update, no hover-focus, no capture motion encoding).
    pub fn update_border_drag(&mut self, pos: CursorPosition) -> bool {
        let Some(drag) = self.border_drag.clone() else {
            return false;
        };
        let Some(point) = self.cursor_to_layout_point(pos) else {
            // Unmappable motion (outside the container origin) keeps the
            // drag armed but changes nothing.
            return true;
        };
        let raw = match drag.axis {
            SplitAxis::Horizontal => i32::from(point.x) - drag.anchor_col,
            SplitAxis::Vertical => i32::from(point.y) - drag.anchor_row,
        };
        if raw == 0 {
            return true;
        }
        // `resize_split_by_drag` narrows to `i16`: clamp the cell delta so
        // a pointer teleport can never wrap the ratio step.
        let delta = raw.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
        let total = match drag.axis {
            SplitAxis::Horizontal => self.container.width,
            SplitAxis::Vertical => self.container.height,
        };
        let mut next = self.layout.clone();
        let before = next.split_ratio_at(&drag.path);
        if !next.resize_split_by_drag(&drag.path, delta, total) {
            self.border_drag = None;
            return false;
        }
        if next.split_ratio_at(&drag.path) != before {
            self.set_layout(next);
        }
        self.border_drag = Some(BorderDragState {
            path: drag.path,
            axis: drag.axis,
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
    /// Uses the same [`Self::cursor_to_leaf_cell`] hit-test as
    /// [`Self::hover_focus_at_at`], so gap/padding bands (no leaf) keep the
    /// current focus and a click and a hover agree on the target. Shift is
    /// the accessibility escape (CTX-0181) and never steals focus, matching
    /// the hover path. Mouse capture and scrollbar chrome never reach here:
    /// the caller consumes those presses first, so a mouse-tracking app or
    /// an active thumb drag keeps the pointer.
    ///
    /// Returns `true` when the pointer landed on a leaf (whether or not it
    /// already held focus), `false` over a gap or under Shift.
    pub(super) fn click_focus_at(&mut self, pos: CursorPosition) -> bool {
        if self.shift_pressed {
            return false;
        }
        let Some((id, _)) = self.cursor_to_leaf_cell(pos) else {
            return false;
        };
        // A click is an explicit focus choice: `set_focus` also drops any
        // pending hover dwell, so hover can never override it.
        self.set_focus(id);
        true
    }

    /// Dwell-delay-aware hover activation (CTX-0334 virtual-clock seam).
    ///
    /// No-op unless [`crate::config::RuntimeConfig::focus_follows_mouse`]
    /// is set (default off preserves click-to-focus) and Shift is released
    /// (Shift forces the selection path). Hover over gap/padding bands
    /// (`cursor_to_leaf_cell` yields `None` there) keeps focus and clears
    /// any pending dwell. With a zero delay focus moves immediately through
    /// [`Self::set_focus`] (which dirties only on change, so steady hover
    /// costs no present); with a positive delay the candidate is recorded
    /// and [`Self::apply_hover_deadline`] commits it once `now` reaches the
    /// deadline. Moving to a different candidate re-arms the dwell clock.
    pub(super) fn hover_focus_at_at(&mut self, pos: CursorPosition, now: Instant) {
        if !self.config.focus_follows_mouse || self.shift_pressed {
            self.hover_pending = None;
            return;
        }
        let Some((id, _)) = self.cursor_to_leaf_cell(pos) else {
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
