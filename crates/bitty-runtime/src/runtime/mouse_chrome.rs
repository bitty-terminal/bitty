//! `Runtime` — Focus-follows-mouse hover and Alt+drag floating-pane moves.
//!
//! CTX-0260 follow-through of DEC-0034: an opt-in hover moves keyboard
//! focus (`RuntimeConfig::focus_follows_mouse`, default off to preserve
//! click-to-focus) plus Alt+drag to move a tiled/floating pane position
//! where the layout model permits (floating [`LayoutNode::Overlay`] bounds
//! move; tiled splits/stacks have no movable position, so the grab is a
//! fail-soft no-op and the press falls through to selection).
//!
//! Lane note: pointer chrome only. Selection, capture encoding, scrollbar
//! drags, and workspace switching belong to their owning paths; this module
//! only grabs/moves/releases the Alt-drag and applies the gated hover step.
//! Shift still forces the selection path (the CTX-0181 precedent): the grab
//! never starts while Shift is held and hover-focus is suppressed under
//! Shift.

use super::*;
use bitty_platform::CursorPosition;

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

    /// Applies the gated hover-focus step for cursor motion at `pos`.
    ///
    /// No-op unless [`crate::config::RuntimeConfig::focus_follows_mouse`]
    /// is set (default off preserves click-to-focus) and Shift is released
    /// (Shift forces the selection path). Hover over gap/padding bands
    /// (`cursor_to_leaf_cell` yields `None` there) keeps focus. Focus moves
    /// through [`Self::set_focus`], which dirties only on change, so steady
    /// hover costs no present.
    pub(super) fn hover_focus_at(&mut self, pos: CursorPosition) {
        if !self.config.focus_follows_mouse || self.shift_pressed {
            return;
        }
        if let Some((id, _)) = self.cursor_to_leaf_cell(pos) {
            self.set_focus(id);
        }
    }
}
