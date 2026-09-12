//! `Runtime` — Layout tree, focus, container, and gap geometry.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;
use crate::config::decoration_runtime_error;

pub(super) fn default_layout(cols: usize, rows: usize) -> LayoutNode {
    let view = View::new(ViewId::new(1), cols, rows);
    LayoutNode::leaf(view)
}

pub(super) fn default_container(cols: usize, rows: usize) -> UiRect {
    let w = cols.min(u16::MAX as usize) as u16;
    let h = rows.min(u16::MAX as usize) as u16;
    UiRect::new(0, 0, w, h)
}

/// One live-present View frame in physical pixels (CTX-0294).
///
/// Produced by [`Runtime::present_frames`]: the accepted Core-owned
/// decoration converted to physical px at the live DPI factor and composed
/// with the CTX-0177 cell gaps. `frame` is the hit-test rectangle and
/// `content` is the painted rectangle (inside the border and content inset);
/// both are relative to the container origin, before the window-padding
/// inset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentFrame {
    /// View this frame paints.
    pub view: ViewId,
    /// Decoration-inclusive hit-test rectangle, physical px.
    pub frame: bitty_render::geometry::RectPx,
    /// Painted content rectangle inside the border, physical px.
    pub content: bitty_render::geometry::RectPx,
    /// Content grid columns derived from `content` (floor, at least 1).
    pub cols: u16,
    /// Content grid rows derived from `content` (floor, at least 1).
    pub rows: u16,
    /// Border thickness in physical px.
    pub border: u16,
    /// Corner radius in physical px (carried; painted by the present ring).
    pub radius: u16,
}

impl Runtime {
    /// Resizes the primary terminal grid to `cols` x `rows` (CTX-0294).
    ///
    /// `State::resize` bottom-aligns real content on a combined width+height
    /// shrink while trailing blank viewport rows absorb the height reduction
    /// first (CTX-0312), so a single call preserves the visible content
    /// without the former two-phase resize workaround. Deterministic; one
    /// damage generation.
    fn resize_primary_grid(&mut self, cols: usize, rows: usize) {
        if cols == self.state.width() && rows == self.state.height() {
            return;
        }
        let _ = self.state.resize(cols, rows);
    }

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
        // CTX-0294/CTX-0333: the Core-owned decoration insets the content
        // inside each frame, so the primary grid origin moves by the outer
        // gap plus the border plus the content inset at the live DPI scale
        // (CTX-0177 cell gaps_out below stays in cells). Positions over the
        // decoration bands still clamp like before (total mapping, no None).
        let scale = self.dpi_scale();
        let deco = self.config.decoration;
        let deco_px =
            (f64::from(deco.gaps_out) + f64::from(deco.border) + f64::from(deco.content_inset))
                * scale;
        let gap_px_x = f64::from(self.config.gaps_out) * cell_w + deco_px;
        let gap_px_y = f64::from(self.config.gaps_out) * cell_h + deco_px;
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

    /// Core-owned workspace decoration from the validated runtime config
    /// (CTX-0292; accepted spec CTX-0118 defaults `4/6/2/6` logical px).
    ///
    /// Decoration is never part of the `LayoutTree`; it is applied by
    /// [`Self::decorated_allocations`] (and, in a later render stage, the
    /// present path).
    #[must_use]
    pub fn decoration(&self) -> bitty_ui::Decoration {
        self.config.decoration
    }

    /// Live-adopts a new Core-owned decoration without restart (CTX-0292).
    ///
    /// Validates fail-closed (accepted ranges: gaps `0..=32`, border
    /// `0..=8`, radius `0..=16` logical px) and stores the value. The
    /// present path does not paint px decoration yet (fractional-cell View
    /// frames are a later stage), so this forces one full redraw and
    /// nothing else changes today; the setter keeps the contract lock.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] naming the out-of-range property.
    pub fn set_decoration(&mut self, decoration: bitty_ui::Decoration) -> Result<(), RuntimeError> {
        if let Err(err) = decoration.validate() {
            return Err(decoration_runtime_error(err));
        }
        if decoration != self.config.decoration {
            self.config.decoration = decoration;
            self.pending_full_redraw = true;
        }
        Ok(())
    }

    /// Live-adopts a focused/idle outline pair without restart (CTX-0340).
    ///
    /// The pair is presentation-only chrome already validated by
    /// `bitty-config` (grammar + contrast contract); this setter is the
    /// boundary lock so an unvalidated caller cannot slip a pair in. A change
    /// forces one full redraw so the new focus/idle split appears on the next
    /// present.
    pub fn set_outline(
        &mut self,
        focused: bitty_render::grid::Rgba8,
        idle: bitty_render::grid::Rgba8,
    ) {
        if self.config.outline_focused != focused || self.config.outline_idle != idle {
            self.config.outline_focused = focused;
            self.config.outline_idle = idle;
            self.pending_full_redraw = true;
        }
    }

    /// Live-adopts a resolved panel animation policy (RFC-0002, CTX-0341).
    ///
    /// Presentation-only; the tracker keeps any in-flight transition timing
    /// and adopts the new policy for subsequent triggers. A change forces one
    /// full redraw so the new durations/easings are observable immediately.
    pub fn set_animations(&mut self, policy: AnimationPolicy) {
        if self.config.animations != policy {
            self.config.animations = policy;
            self.animator.set_policy(policy);
            self.pending_full_redraw = true;
        }
    }

    /// Arms a transition for `surface` at `now`, returning whether it will
    /// animate (RFC-0002). `false` means the caller applies the end state
    /// immediately (instant, reduced motion, safe mode, or capacity).
    pub fn trigger_animation(
        &mut self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: std::time::Instant,
    ) -> bool {
        let armed = self.animator.trigger(kind, surface, now);
        if armed {
            // A new animation needs a frame even when no PTY bytes moved.
            self.pending_full_redraw = true;
        }
        armed
    }

    /// Timer deadline for the next in-flight animation frame, if any
    /// (RFC-0002 frame-on-demand: `None` when idle, so the app can sleep).
    #[must_use]
    pub fn animation_deadline(&self) -> Option<std::time::Instant> {
        self.animator.next_deadline(std::time::Instant::now())
    }

    /// Whether any panel animation is currently active.
    #[must_use]
    pub fn animations_active(&self) -> bool {
        self.animator.is_active(std::time::Instant::now())
    }

    /// Eased progress `0..=1` of an active transition, or `None` when that
    /// transition is not animating (caller uses the final state directly).
    #[must_use]
    pub fn animation_progress(
        &self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: std::time::Instant,
    ) -> Option<f32> {
        self.animator.progress(kind, surface, now)
    }

    /// Detects RFC-0002 transitions between the last presented frame and the
    /// current one and arms the matching animations (CTX-0341).
    ///
    /// Called once per present before the idle short-circuit. Open/close are
    /// derived from the View-set delta (a new View fades in, a removed one
    /// fades out through a retained [`ClosingFrame`]); focus from the focused
    /// View change; workspace from the active index change. The first frame
    /// after startup arms nothing so a fresh window does not animate its
    /// initial layout. Presentation-only: allocates no terminal state and is
    /// bounded by the View count (itself bounded by the layout).
    pub(super) fn detect_panel_animations(
        &mut self,
        allocations: &[PresentFrame],
        focused: Option<ViewId>,
        active_workspace: usize,
        now: std::time::Instant,
    ) {
        let first_frame = self.last_presented_generation == u64::MAX;
        if first_frame {
            self.last_presented_workspace = active_workspace;
            return;
        }
        let policy = self.config.animations;

        if policy.animates(AnimationKind::Workspace)
            && active_workspace != self.last_presented_workspace
        {
            self.trigger_animation(AnimationKind::Workspace, None, now);
        }

        let new_ids: std::collections::HashSet<ViewId> =
            allocations.iter().map(|f| f.view).collect();
        let old_ids: std::collections::HashSet<ViewId> = self
            .last_presented_allocations
            .iter()
            .map(|f| f.view)
            .collect();

        if policy.animates(AnimationKind::Open) {
            for frame in allocations {
                if !old_ids.contains(&frame.view) {
                    self.trigger_animation(AnimationKind::Open, Some(frame.view), now);
                }
            }
        }
        if policy.animates(AnimationKind::Close) {
            let previous = self.last_presented_allocations.clone();
            for old in &previous {
                if new_ids.contains(&old.view) {
                    continue;
                }
                if self.closing_frames.iter().any(|c| c.view == old.view) {
                    continue;
                }
                if self.closing_frames.len() >= MAX_CONCURRENT_ANIMATIONS {
                    // Bounded: a storm of closes commits the end state of the
                    // excess rather than retaining unbounded frames.
                    break;
                }
                let was_focused = self.last_presented_focus == Some(old.view);
                let color = if was_focused {
                    self.config.outline_focused
                } else {
                    self.config.outline_idle
                };
                // CTX-0344: retain the painted ring width (the focus-state
                // outline width), not the content-geometry border, so the
                // closing frame matches what was on screen. `None` inherits
                // the already-physical geometry border.
                let ring_width = if was_focused {
                    self.config.outline_width_focused
                } else {
                    self.config.outline_width_idle
                };
                self.closing_frames.push(ClosingFrame {
                    view: old.view,
                    frame: old.frame,
                    border: ring_width.map_or(old.border, |w| self.outline_width_physical(w)),
                    radius: old.radius,
                    color,
                });
                self.trigger_animation(AnimationKind::Close, Some(old.view), now);
            }
        }
        if focused != self.last_presented_focus {
            if let Some(fid) = focused {
                if policy.animates(AnimationKind::Focus) {
                    self.trigger_animation(AnimationKind::Focus, Some(fid), now);
                }
            }
        }
        self.last_presented_workspace = active_workspace;
    }

    /// Advances the animation tracker and drops completed transitions
    /// (RFC-0002). Returns `true` when any transition expired this frame, so
    /// the caller presents exactly one final frame that commits the end state
    /// (the next frame then idles).
    pub(super) fn advance_animations(&mut self, now: std::time::Instant) -> bool {
        let expired = self.animator.tick(now);
        let before = self.closing_frames.len();
        self.closing_frames.retain(|cf| {
            self.animator
                .progress(AnimationKind::Close, Some(cf.view), now)
                .is_some()
        });
        expired || self.closing_frames.len() != before
    }

    /// Decorated View frames in logical pixels for the current workspace
    /// area (CTX-0292 Core-owned decoration application).
    ///
    /// The workspace area is the layout container converted to logical px
    /// with the **base** (scale-1.0 design) cell metrics: accepted spec
    /// CTX-0118 rule 1 puts decoration and layout math in logical pixels and
    /// applies the Window DPI factor only at render time. Window padding is
    /// Window chrome, not workspace decoration, so it is not part of the
    /// area. The result applies the contract: `gaps_out` insets the area,
    /// `gaps_in` reserves the band between siblings, `border` insets each
    /// frame's content, and `radius` is carried for clipping. Pure and
    /// deterministic for the same layout, container, metrics, and
    /// decoration.
    #[must_use]
    pub fn decorated_allocations(&self) -> Vec<(ViewId, bitty_ui::DecoratedView)> {
        let base = self.base_cell_metrics();
        let area = UiRect::new(
            (u32::from(self.container.x).saturating_mul(base.width)).min(u32::from(u16::MAX))
                as u16,
            (u32::from(self.container.y).saturating_mul(base.height)).min(u32::from(u16::MAX))
                as u16,
            (u32::from(self.container.width).saturating_mul(base.width)).min(u32::from(u16::MAX))
                as u16,
            (u32::from(self.container.height).saturating_mul(base.height)).min(u32::from(u16::MAX))
                as u16,
        );
        self.layout
            .layout_with_decoration(area, self.config.decoration)
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

    /// Live-present View frames in physical pixels (CTX-0294).
    ///
    /// The accepted CTX-0118 decoration is logical px; this is the render-time
    /// step (spec rule 1) that converts it with the live DPI factor and
    /// composes it with the CTX-0177 cell gaps in one solver pass. Frames are
    /// relative to the container origin, before the window-padding inset:
    /// the present path adds [`Self::window_padding_physical`] exactly like
    /// the grid content translation it already owns, so decoration sits
    /// inside the Window and padding stays Window chrome.
    ///
    /// Per frame: `frame` is the hit-test rectangle (decoration-inclusive),
    /// `content` is the painted rectangle inside the border and content inset,
    /// and `cols`/`rows` are the content grid dimensions derived from
    /// `content` at the live cell metrics (floor, at least 1). The sub-cell
    /// remainder stays background,
    /// which is what makes the composition fractional-cell. Pure and
    /// deterministic; total for hostile containers/decoration.
    #[must_use]
    pub fn present_frames(&self) -> Vec<PresentFrame> {
        let live = self.live_cell_metrics();
        let cw = live.width;
        let ch = live.height;
        if cw == 0 || ch == 0 {
            return Vec::new();
        }
        let area = UiRect::new(
            (u32::from(self.container.x).saturating_mul(cw)).min(u32::from(u16::MAX)) as u16,
            (u32::from(self.container.y).saturating_mul(ch)).min(u32::from(u16::MAX)) as u16,
            (u32::from(self.container.width).saturating_mul(cw)).min(u32::from(u16::MAX)) as u16,
            (u32::from(self.container.height).saturating_mul(ch)).min(u32::from(u16::MAX)) as u16,
        );
        let cell = (
            u16::try_from(cw).unwrap_or(u16::MAX),
            u16::try_from(ch).unwrap_or(u16::MAX),
        );
        let max_dim = u32::try_from(bitty_term_state::MAX_GRID_DIM).unwrap_or(u32::MAX);
        self.layout
            .layout_with_decoration_scaled(
                area,
                self.config.decoration,
                self.dpi_scale(),
                cell,
                self.gaps(),
            )
            .into_iter()
            .map(|(view, dv)| PresentFrame {
                view,
                frame: bitty_render::geometry::RectPx::new(
                    i32::from(dv.frame.x),
                    i32::from(dv.frame.y),
                    u32::from(dv.frame.width),
                    u32::from(dv.frame.height),
                ),
                content: bitty_render::geometry::RectPx::new(
                    i32::from(dv.content.x),
                    i32::from(dv.content.y),
                    u32::from(dv.content.width),
                    u32::from(dv.content.height),
                ),
                cols: (u32::from(dv.content.width) / cw).clamp(1, max_dim) as u16,
                rows: (u32::from(dv.content.height) / ch).clamp(1, max_dim) as u16,
                border: dv.border,
                radius: dv.radius,
            })
            .collect()
    }

    /// Reflows leaf `View`s to the decorated content frames (CTX-0294).
    ///
    /// Origin is the content rectangle floored to whole cells (the sub-cell
    /// remainder is painted as background and never claimed by a cell) and
    /// size is the content-derived grid. The present path translates pixels
    /// directly from [`PresentFrame`], so this exists for the cell-path
    /// consumers — scroll bounds, selection clamping, scrollbar thumb
    /// sizing — and for pane grid/PTY sizing.
    pub(super) fn reflow_present_layout(&mut self, frames: &[PresentFrame]) {
        let live = self.live_cell_metrics();
        let cw = u32::from(u16::try_from(live.width).unwrap_or(u16::MAX)).max(1);
        let ch = u32::from(u16::try_from(live.height).unwrap_or(u16::MAX)).max(1);
        for frame in frames {
            let x = (frame.content.x.max(0) as u32) / cw;
            let y = (frame.content.y.max(0) as u32) / ch;
            if let Some(view) = self.layout.find_leaf_mut(frame.view) {
                view.reflow_to_rect(UiRect::new(
                    x.min(u32::from(u16::MAX)) as u16,
                    y.min(u32::from(u16::MAX)) as u16,
                    frame.cols,
                    frame.rows,
                ));
            }
        }
    }

    /// Decorated hit-testing: physical cursor position to `(view, local cell)`
    /// over the present frames (frame for hit testing, content for painting).
    ///
    /// Positions over the window-padding band, outside every frame, or in an
    /// empty frame yield `None`. A position over the border ring belongs to
    /// the frame and clamps to the nearest content cell, matching the accepted
    /// spec rule 4 (radius never affects hit testing beyond the frame). Total
    /// and deterministic; the CTX-0177 panel-path mapping
    /// ([`Self::cursor_to_leaf_cell`]) is unchanged.
    #[must_use]
    pub fn cursor_to_present_cell(&self, pos: CursorPosition) -> Option<(ViewId, CellPos)> {
        let live = self.live_cell_metrics();
        let cell_w = live.width as f64;
        let cell_h = live.height as f64;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let pad = f64::from(self.window_padding_physical());
        let x = pos.x - pad;
        let y = pos.y - pad;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let frames = self.present_frames();
        let (frame, content) = frames.iter().find_map(|frame| {
            let rect = frame.frame;
            if rect.width == 0 || rect.height == 0 {
                return None;
            }
            let right = rect.x as f64 + f64::from(rect.width);
            let bottom = rect.y as f64 + f64::from(rect.height);
            if x >= f64::from(rect.x) && x < right && y >= f64::from(rect.y) && y < bottom {
                Some((frame, frame.content))
            } else {
                None
            }
        })?;
        let local_col = ((x - f64::from(content.x)) / cell_w).floor().max(0.0);
        let local_row = ((y - f64::from(content.y)) / cell_h).floor().max(0.0);
        if local_col > f64::from(u16::MAX) || local_row > f64::from(u16::MAX) {
            return None;
        }
        let max_col = u32::from(frame.cols).saturating_sub(1);
        let max_row = u32::from(frame.rows).saturating_sub(1);
        Some((
            frame.view,
            CellPos::new(
                (local_row as u32).min(max_row) as u16,
                (local_col as u32).min(max_col) as u16,
            ),
        ))
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
    /// the shared primary grid (+ primary PTY winsize) via the primary
    /// owner leaf's allocation ([`Self::primary_view`]). Focus is retained
    /// when the focused `ViewId` still exists, otherwise it moves to the
    /// first leaf (if any) or clears.
    ///
    /// CTX-0269: session-less leaves clip the primary grid through
    /// `viewport_snapshot`, so the primary must shrink/grow with the leaf
    /// that paints it — previously `set_layout` left the stale pre-split
    /// grid and `tick` only clipped it via `viewport_snapshot` (live split
    /// showed a ~155-col grid in a ~77-col pane, tails invisible, reflow
    /// never firing). CTX-0359: that leaf is the primary owner, not the
    /// focused one (focus no longer moves the primary fallback); closing the
    /// owner re-homes the primary to the focused survivor, while a layout
    /// that never contained the owner (fresh workspace) leaves the grid
    /// untouched. Best-effort like the pane sync: matching dims skip, PTY
    /// errors never fail the layout change.
    pub fn set_layout(&mut self, layout: LayoutNode) {
        // CTX-0334: a structural layout change abandons any pending hover
        // dwell; the candidate may no longer exist or may have moved.
        self.clear_hover_pending();
        // CTX-0359: capture whether this change removes the primary owner
        // leaf (close) before the tree is replaced. Workspace switches and
        // creations install layouts outside this funnel, so a fresh
        // workspace leaf can never claim the primary through this path.
        let primary_removed = self.primary_view.is_some_and(|p| {
            self.layout.leaf_ids().contains(&p) && !layout.leaf_ids().contains(&p)
        });
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
        // CTX-0359: the primary shell keeps exactly one view. When its
        // owner leaf is closed (no primary-shell teardown yet), re-home the
        // primary to the focused survivor so the live shell still has a
        // tile to paint and type into; otherwise the owner never changes.
        if primary_removed {
            self.primary_view = self.focus.focused();
        }
        // Leaf Views carry their allocation from here (not deferred to the
        // next tick) so per-leaf geometry is inspectable immediately.
        // CTX-0294: decorated content frames (px decoration + cell gaps).
        let frames = self.present_frames();
        self.reflow_present_layout(&frames);
        // CTX-0359: primary grid + primary PTY winsize (SIGWINCH path) follow
        // the primary owner leaf — the tile that paints primary input/cursor
        // — not the focused leaf. A layout without the owner (fresh
        // workspace) leaves the grid untouched until the owner returns.
        if let Some(primary) = self.primary_view {
            if let Some(frame) = frames.iter().find(|frame| frame.view == primary) {
                let cols = usize::from(frame.cols.max(1));
                let rows = usize::from(frame.rows.max(1));
                if self.state.width() != cols || self.state.height() != rows {
                    self.resize_primary_grid(cols, rows);
                    if let Some(pty) = self.pty.as_mut() {
                        let _ = pty.resize(frame.cols.max(1), frame.rows.max(1));
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
        // CTX-0334: an explicit focus set abandons any pending hover dwell,
        // so hover activation can never override a deliberate choice.
        self.clear_hover_pending();
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
        // CTX-0334: a focus move abandons any pending hover dwell.
        self.clear_hover_pending();
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
