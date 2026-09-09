//! `Runtime` — Layout tree, focus, container, and gap geometry.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;

pub(super) fn default_layout(cols: usize, rows: usize) -> LayoutNode {
    let view = View::new(ViewId::new(1), cols, rows);
    LayoutNode::leaf(view)
}

pub(super) fn default_container(cols: usize, rows: usize) -> UiRect {
    let w = cols.min(u16::MAX as usize) as u16;
    let h = rows.min(u16::MAX as usize) as u16;
    UiRect::new(0, 0, w, h)
}

impl Runtime {
    /// Converts a physical cursor position to a grid cell coordinate using
    /// the live (DPI-scaled) cell metrics. Clamped to the current snapshot bounds.
    ///
    /// When the position lies far outside the window it clamps to the nearest
    /// cell rather than returning `None`, so drag selections that leave the
    /// window still produce deterministic inclusive ranges.
    ///
    /// CTX-0177: the configured outer gap (`gaps_out`) shifts the grid
    /// origin, so it is subtracted (in live pixels) before dividing by the
    /// cell metrics — otherwise every mapping would be off by the gap. See
    /// [`Self::cursor_to_leaf_cell`] for the leaf-aware variant that also
    /// accounts for inner gap bands in multi-pane layouts.
    ///
    /// CTX-0223: the window padding inset shifts the grid origin the same
    /// way, so it is subtracted first (physical pixels at the live scale).
    #[must_use]
    pub fn cursor_to_cell(&self, pos: CursorPosition) -> CellPos {
        let snap = self.state.snapshot();
        let live = self.live_cell_metrics();
        let cell_w = live.width as f64;
        let cell_h = live.height as f64;
        let pad_px = f64::from(self.window_padding_physical());
        let gap_px_x = f64::from(self.config.gaps_out) * cell_w;
        let gap_px_y = f64::from(self.config.gaps_out) * cell_h;
        let col = if cell_w <= 0.0 {
            0
        } else {
            ((pos.x - pad_px - gap_px_x) / cell_w).floor() as i64
        };
        let row = if cell_h <= 0.0 {
            0
        } else {
            ((pos.y - pad_px - gap_px_y) / cell_h).floor() as i64
        };
        let max_col = snap.width.saturating_sub(1) as i64;
        let max_row = snap.height.saturating_sub(1) as i64;
        let clamped_col = col.clamp(0, max_col) as u16;
        let clamped_row = row.clamp(0, max_row) as u16;
        bitty_ui::snap_to_leading(&snap, CellPos::new(clamped_row, clamped_col))
    }

    /// Active panel gaps from the validated runtime config (CTX-0177).
    ///
    /// Threaded into every layout call ([`Self::layout_allocations`],
    /// [`Self::reflow_layout`], tick reflow, pane-geometry sync, and spatial
    /// focus) so per-leaf rendering and hit-testing share one gap source.
    #[must_use]
    pub fn gaps(&self) -> Gaps {
        Gaps::new(self.config.gaps_in, self.config.gaps_out)
    }

    /// Leaf whose gapped allocation contains container-cell `(col, row)`
    /// (CTX-0177).
    ///
    /// Returns `None` for cells inside a gap band (inner or outer) or outside
    /// every leaf — the mouse is over background, not a pane. Total and
    /// deterministic; zero gaps reduce to the plain tiling lookup.
    #[must_use]
    pub fn leaf_at_container_cell(&self, col: u16, row: u16) -> Option<ViewId> {
        let c = u32::from(col);
        let r = u32::from(row);
        self.layout_allocations()
            .into_iter()
            .find(|(_, rect)| {
                !rect.is_empty()
                    && c >= u32::from(rect.x)
                    && r >= u32::from(rect.y)
                    && c < rect.right()
                    && r < rect.bottom()
            })
            .map(|(id, _)| id)
    }

    /// Maps a physical cursor position to its leaf and leaf-local cell
    /// (CTX-0177).
    ///
    /// Unlike [`Self::cursor_to_cell`] (global, clamped, single-grid), this
    /// is leaf-aware: the window padding inset (CTX-0223) and the outer gap
    /// are subtracted in live pixels, the remainder is divided by the live
    /// cell metrics (0157 math), the containing gapped allocation is
    /// resolved, and the leaf origin is subtracted for the local cell.
    /// Positions over the padding band, a gap band (inner or outer), or
    /// outside all leaves yield `None`.
    ///
    /// No wide-spacer snapping is applied (that needs the target pane's
    /// snapshot; callers use `bitty_ui::snap_to_leading` with it). Local
    /// cells are clamped to the leaf allocation defensively.
    #[must_use]
    pub fn cursor_to_leaf_cell(&self, pos: CursorPosition) -> Option<(ViewId, CellPos)> {
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
        let (id, rect) = self.layout_allocations().into_iter().find(|(_, r)| {
            !r.is_empty()
                && (col as u32) >= u32::from(r.x)
                && (row as u32) >= u32::from(r.y)
                && (col as u32) < r.right()
                && (row as u32) < r.bottom()
        })?;
        let local_col = (col as u16)
            .saturating_sub(rect.x)
            .min(rect.width.saturating_sub(1));
        let local_row = (row as u16)
            .saturating_sub(rect.y)
            .min(rect.height.saturating_sub(1));
        Some((id, CellPos::new(local_row, local_col)))
    }

    /// Immutable view of the owned layout tree.
    #[must_use]
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }

    /// Mutable view of the owned layout tree.
    ///
    /// CTX-0228: mutating the tree through this borrow is a geometry-only
    /// change with no PTY damage. `tick` detects allocation differences
    /// against the last presented frame and forces a full present, so a
    /// manual `mark_layout_dirty` call is not required — but prefer
    /// [`Self::set_layout`] (which also re-syncs pane geometry and focus)
    /// for split/close/zoom mutations.
    #[must_use]
    pub fn layout_mut(&mut self) -> &mut LayoutNode {
        &mut self.layout
    }

    /// Forces a full redraw on the next `tick` (CTX-0228).
    ///
    /// Call after mutating the tree through [`Self::layout_mut`] or focus
    /// through [`Self::focus_mut`] when the change must present even if no
    /// PTY bytes arrive. `tick` also detects allocation/focus differences
    /// automatically, so this is defense in depth for embedders that want
    /// an explicit dirty signal.
    pub fn mark_layout_dirty(&mut self) {
        self.pending_full_redraw = true;
    }

    /// Replaces the owned layout tree.
    ///
    /// The new tree's leaf `View`s are reflowed into the current container
    /// immediately (tick repeats this every frame; idempotent), and every
    /// grid follows its leaf: pane sessions via
    /// [`Self::sync_pane_geometry`](super::Runtime::sync_pane_geometry), and
    /// the shared primary grid (+ primary PTY winsize) via the focused
    /// leaf's allocation. Focus is retained when the focused `ViewId` still
    /// exists, otherwise it moves to the first leaf (if any) or clears.
    ///
    /// CTX-0269: session-less leaves share the primary grid and the focused
    /// one owns input/cursor, so the primary must shrink/grow with the
    /// focused allocation — previously `set_layout` left the stale
    /// pre-split grid and `tick` only clipped it via `viewport_snapshot`
    /// (live split showed a ~155-col grid in a ~77-col pane, tails
    /// invisible, reflow never firing). Best-effort like the pane sync:
    /// matching dims skip, PTY errors never fail the layout change.
    pub fn set_layout(&mut self, layout: LayoutNode) {
        self.layout = layout;
        let leaf_ids = self.layout.leaf_ids();
        if leaf_ids.is_empty() {
            self.focus.clear();
        } else if let Some(focused) = self.focus.focused() {
            if !leaf_ids.contains(&focused) {
                self.focus.set(leaf_ids[0]);
            }
        } else {
            self.focus.set(leaf_ids[0]);
        }
        // Leaf Views carry their allocation from here (not deferred to the
        // next tick) so per-leaf geometry is inspectable immediately.
        self.layout.reflow_with_gaps(self.container, self.gaps());
        // Primary grid + primary PTY winsize (SIGWINCH path) follow the
        // focused leaf — the tile that shows primary input/cursor.
        if let Some(focused) = self.focus.focused() {
            if let Some((_, rect)) = self
                .layout_allocations()
                .into_iter()
                .find(|(id, _)| *id == focused)
            {
                let cols = rect.width.max(1) as usize;
                let rows = rect.height.max(1) as usize;
                if self.state.width() != cols || self.state.height() != rows {
                    let _ = self.state.resize(cols, rows);
                    if let Some(pty) = self.pty.as_mut() {
                        let _ = pty.resize(rect.width.max(1), rect.height.max(1));
                    }
                }
            }
        }
        // CTX-0176: leaf boundaries may have moved (split/close/resize),
        // so re-sync every pane session's grid + PTY winsize to its leaf.
        self.sync_pane_geometry();
        self.pending_full_redraw = true;
    }

    /// Owned focus state.
    #[must_use]
    pub fn focus(&self) -> &Focus {
        &self.focus
    }

    /// Mutable focus state.
    ///
    /// CTX-0228: `tick` detects focus differences against the last
    /// presented frame and forces a full present (the cursor moves panes
    /// with no PTY damage). Prefer [`Self::set_focus`]/[`Self::move_focus`]
    /// which dirty explicitly; see [`Self::mark_layout_dirty`] for manual
    /// borrows.
    #[must_use]
    pub fn focus_mut(&mut self) -> &mut Focus {
        &mut self.focus
    }

    /// Currently focused view, if any.
    #[must_use]
    pub fn focused_view(&self) -> Option<ViewId> {
        self.focus.focused()
    }

    /// Sets focus to `id` when it exists in the current layout; otherwise
    /// leaves focus unchanged and returns `false`.
    ///
    /// CTX-0228: a focus change moves the cursor/highlight with no PTY
    /// damage, so a successful change forces a full redraw on the next
    /// `tick`.
    pub fn set_focus(&mut self, id: ViewId) -> bool {
        if self.layout.leaf_ids().contains(&id) {
            // Only dirty when the focus actually moves; re-selecting the
            // focused pane is a no-op present-wise.
            if self.focus.focused() != Some(id) {
                self.focus.set(id);
                self.pending_full_redraw = true;
            }
            true
        } else {
            false
        }
    }

    /// Moves focus in `dir` using the layout's deterministic adjacency.
    ///
    /// Returns the new focused view (if any) and updates internal focus.
    /// CTX-0177: adjacency is computed over the gapped allocation so spatial
    /// focus still crosses gap bands.
    ///
    /// CTX-0228: a focus move forces a full redraw (cursor/highlight moves
    /// with no PTY damage).
    pub fn move_focus(&mut self, dir: FocusDirection) -> Option<ViewId> {
        let next = self
            .focus
            .advance_with_gaps(&self.layout, self.container, self.gaps(), dir);
        if let Some(id) = next {
            if self.focus.focused() != Some(id) {
                self.focus.set(id);
                self.pending_full_redraw = true;
            }
        }
        next
    }

    /// Container rect (cell coordinates) that the layout is reflowed into.
    #[must_use]
    pub fn container(&self) -> UiRect {
        self.container
    }

    /// Sets the container rect directly (cell coordinates). The container is
    /// also updated automatically by `handle_resize` via pixel-to-cell
    /// conversion; this setter exists for headless tests that drive layout
    /// without a physical surface.
    pub fn set_container(&mut self, rect: UiRect) {
        self.container = rect;
        self.pending_full_redraw = true;
    }

    /// Current leaf allocations `(ViewId, Rect)` in deterministic depth-first
    /// order, computed from the last reflowed layout or the current container
    /// without mutating the tree (pure `LayoutNode::layout_with_gaps`).
    ///
    /// CTX-0177: allocations exclude the configured gap bands, so per-leaf
    /// rendering translates to gap-aware pixel origins and the bands stay
    /// background.
    #[must_use]
    pub fn layout_allocations(&self) -> Vec<(ViewId, UiRect)> {
        self.layout.layout_with_gaps(self.container, self.gaps())
    }

    /// Leaf count of the current layout.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        self.layout.leaf_count()
    }

    /// Reflows the layout into the current container, mutating each leaf
    /// `View`'s `cols`/`rows`/`origin` to match its allocation. Returns the
    /// allocations for inspection. Deterministic over the same layout and
    /// container.
    ///
    /// CTX-0177: reflows with the configured gaps so leaf sizes exclude the
    /// gap bands.
    ///
    /// CTX-0228: leaf geometry is presentation state — a reflow forces a
    /// full redraw on the next `tick` even when no PTY bytes arrive.
    pub fn reflow_layout(&mut self) -> Vec<(ViewId, UiRect)> {
        let gaps = self.gaps();
        self.layout.reflow_with_gaps(self.container, gaps);
        self.pending_full_redraw = true;
        self.layout.layout_with_gaps(self.container, gaps)
    }
}
