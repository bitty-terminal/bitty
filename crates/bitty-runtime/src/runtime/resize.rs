//! `Runtime` — Resize, reflow, DPI adoption, and window padding.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::layout_focus::default_container;
use super::*;

impl Runtime {
    /// Handles a physical-pixel resize: recomputes the grid size from the
    /// live (possibly DPI-scaled) cell metrics, reconfigures the software
    /// surface, updates the layout container, reflows leaf views, resizes
    /// the terminal grid via the singular reflow (truncate/pad with orphan
    /// repair) and resizes the PTY when present. Zero-sized extents are
    /// skipped (minimized/occluded windows) per the
    /// `map_resize_to_surface_extent` contract. The reflow is deterministic
    /// and headless-testable.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Render`] when headless reconfiguration rejects the
    /// extent; [`RuntimeError::Pty`] when the PTY resize fails.
    pub fn handle_resize(&mut self, size: PhysicalSize) -> Result<(), RuntimeError> {
        if bitty_platform::map_resize_to_surface_extent(size).is_none() {
            return Ok(());
        }
        let (new_cols, new_rows) = self.grid_from_physical(size);
        self.reflow_to_grid(new_cols, new_rows, size)
    }

    /// Base (design) cell metrics from the validated config.
    ///
    /// The renderer starts here at scale 1.0; [`Self::apply_dpi_scale`]
    /// derives scaled cells from this base on every change so repeated scale
    /// changes never compound rounding (each rescale starts from the design
    /// cell instead of re-scaling the previous scaled cell).
    pub(super) fn base_cell_metrics(&self) -> CellMetrics {
        let ratio = if self.base_font_size > 0.0 && self.config.font_size > 0.0 {
            (self.config.font_size / self.base_font_size) as f64
        } else {
            1.0
        };
        let scaled_w = ((self.config.cell_width as f64 * ratio).round() as u32).max(1);
        let scaled_h = ((self.config.cell_height as f64 * ratio).round() as u32).max(1);
        CellMetrics::new(scaled_w, scaled_h).expect("scaled cell metrics are at least 1x1")
    }

    /// Base font query from the validated config.
    pub(super) fn base_font_query(&self) -> FontQuery {
        FontQuery {
            family: self.config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: self.config.font_size,
        }
    }

    /// Currently live cell metrics: the design cell at scale 1.0, the
    /// DPI-scaled cell after [`Self::apply_dpi_scale`].
    ///
    /// All physical-pixel geometry (grid derivation, cursor mapping,
    /// per-leaf pixel origins) flows through this so the renderer placement
    /// and the runtime geometry can never disagree about the cell size.
    pub(super) fn live_cell_metrics(&self) -> CellMetrics {
        self.renderer.cell_metrics()
    }

    /// Last adopted DPI scale (1.0 until [`Self::apply_dpi_scale`] or a
    /// `ScaleFactorChanged` event adopts another; always sanitized, so this
    /// never reports zero, negative, NaN, or infinite).
    #[must_use]
    pub fn dpi_scale(&self) -> f64 {
        self.scale_factor.get()
    }

    /// Derives `cols`/`rows` from a physical window extent over the live
    /// (possibly DPI-scaled) cell metrics, saturating to at least 1x1 and
    /// capping at the 1000x1000 grid bound so hostile extents cannot grow
    /// grid memory without limit (mirrors [`RuntimeConfig::grid_from_pixels`]).
    ///
    /// CTX-0223: the window padding inset is removed on every side before
    /// dividing, so the window — not the grid — absorbs the inset. A
    /// padding that covers the window still addresses one cell.
    pub(super) fn grid_from_physical(&self, extent: PhysicalSize) -> (usize, usize) {
        let pad = self.window_padding_physical();
        let content = PhysicalSize::new(
            extent.width().saturating_sub(pad.saturating_mul(2)),
            extent.height().saturating_sub(pad.saturating_mul(2)),
        );
        let (cols, rows) = grid_from_surface_extent(content, self.live_cell_metrics());
        (
            cols.clamp(1, bitty_term_state::MAX_GRID_DIM),
            rows.clamp(1, bitty_term_state::MAX_GRID_DIM),
        )
    }

    /// Configured window padding in logical pixels (CTX-0223
    /// `window.padding`; `0..=64`, default `8`).
    #[must_use]
    pub fn window_padding(&self) -> u32 {
        self.config.window_padding
    }

    /// Configured window corner radius in physical px (CTX-0241 S0
    /// `window.radius_px`; `0..=24`, default `0`).
    ///
    /// S0 is a parsed no-op: the value is accepted, stored, and reported
    /// (config check, reload diff) but no DrawList/present consumer reads
    /// it, so any value renders identically to the default.
    #[must_use]
    pub fn window_radius_px(&self) -> u32 {
        self.config.window_radius_px
    }

    /// Live-adopts a new window corner radius without restart (CTX-0241 S0).
    ///
    /// This is the `window.radius_px` side of the `Live` reload class: the
    /// running instance stores the value and nothing else changes — no grid
    /// re-derivation, no surface reconfiguration, no forced redraw — because
    /// S0 has zero render effect by contract. Later stages add the present
    /// consumer; this setter stays total so the contract lock holds.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `radius > 24`.
    pub fn set_window_radius_px(&mut self, radius: u32) -> Result<(), RuntimeError> {
        if radius > crate::config::MAX_WINDOW_RADIUS_PX {
            return Err(RuntimeError::InvalidConfig(
                "window_radius_px must be within [0, 24] physical pixels",
            ));
        }
        self.config.window_radius_px = radius;
        Ok(())
    }

    /// Focused/idle outline ring width in physical pixels at the live DPI
    /// scale (CTX-0344, RFC-0001/OQ-045).
    ///
    /// The resolved logical width scaled by the sanitized live factor,
    /// rounded half away from zero — the same rule as
    /// [`Self::window_padding_physical`]/[`Self::scrollbar_width_physical`]
    /// so chrome agrees about the pixel grid. `0` logical stays `0` (no ring:
    /// no 1px floor); the accepted `1` logical delta stays at least `1`
    /// physical px at any DPI. Saturates at [`u16::MAX`] for a hostile scale.
    #[must_use]
    pub fn outline_width_physical(&self, logical_px: u32) -> u16 {
        let scaled = f64::from(logical_px.min(crate::config::MAX_OUTLINE_WIDTH_PX))
            * sanitize_dpi_scale(self.dpi_scale());
        let rounded = scaled.round();
        if rounded < 1.0 {
            0
        } else if rounded >= f64::from(u16::MAX) {
            u16::MAX
        } else {
            rounded as u16
        }
    }

    /// Chrome-style drop-shadow offset in physical pixels at the live DPI
    /// scale (CTX-1162 S3 `appearance.style.shadow_offset_*`).
    ///
    /// The configured logical offset scaled by the sanitized live factor,
    /// rounded half away from zero — the same rule as
    /// [`Self::outline_width_physical`] so shadow chrome agrees with ring
    /// chrome about the pixel grid. Sign-preserving (`-0` rounds to `0`) and
    /// saturating at [`i32::MAX`]/[`i32::MIN`] for a hostile scale.
    #[must_use]
    pub fn style_offset_physical(&self, logical_px: i32) -> i32 {
        let scaled = f64::from(logical_px) * sanitize_dpi_scale(self.dpi_scale());
        let rounded = scaled.round();
        if rounded >= f64::from(i32::MAX) {
            i32::MAX
        } else if rounded <= f64::from(i32::MIN) {
            i32::MIN
        } else {
            rounded as i32
        }
    }

    /// Window padding in physical pixels at the live DPI scale (CTX-0223).
    ///
    /// The configured logical padding scaled by the sanitized live factor,
    /// rounded half away from zero. Tick translates grid content by this
    /// offset and [`Self::grid_from_physical`] removes twice it before
    /// dividing, so render placement and grid derivation share one source.
    #[must_use]
    pub fn window_padding_physical(&self) -> u32 {
        let scaled = f64::from(
            self.config
                .window_padding
                .min(crate::config::MAX_WINDOW_PADDING),
        ) * sanitize_dpi_scale(self.dpi_scale());
        let rounded = scaled.round();
        if rounded < 1.0 {
            // Zero padding stays zero (no 1px floor: unlike scaled cells,
            // a zero inset is meaningful and must not shift content).
            0
        } else if rounded >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            rounded as u32
        }
    }

    /// Live-applies a new window padding without restart (CTX-0223).
    ///
    /// This is the `window.padding` side of the `Live` reload class: the
    /// running instance adopts the value, re-derives the grid from the
    /// current surface extent (the window keeps its size; the grid absorbs
    /// the inset), and repaints fully on the next tick. No restart, no
    /// surface reconfiguration — the extent is unchanged, only the
    /// content translation and grid division move.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `padding > 64`; any
    /// [`RuntimeError`] from the grid reflow (surface/PTY resize) after
    /// the value is stored — the padding itself is already adopted then,
    /// and the next resize reconciles the remainder.
    pub fn set_window_padding(&mut self, padding: u32) -> Result<(), RuntimeError> {
        if padding > crate::config::MAX_WINDOW_PADDING {
            return Err(RuntimeError::InvalidConfig(
                "window_padding must be within [0, 64] logical pixels",
            ));
        }
        if padding == self.config.window_padding {
            return Ok(());
        }
        self.config.window_padding = padding;
        // Re-derive the grid from the live surface extent (window size is
        // fixed; the grid absorbs the new inset). Without a configured
        // surface yet, the next resize/tick reconciles instead.
        if let Some(extent) = self.surface.extent() {
            if bitty_platform::map_resize_to_surface_extent(extent).is_some() {
                let (cols, rows) = self.grid_from_physical(extent);
                self.reflow_to_grid(cols, rows, extent)?;
            }
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Pixel extent the combined frame's plan covers: the configured
    /// surface extent, or the cell-aligned window grid plus the padding
    /// inset when no surface is configured (CTX-0223, #2520).
    ///
    /// The GPU present path recovers its per-frame NDC factor as
    /// `surface / plan` ([`bitty_render::batch::derive_scale`]), so this
    /// must describe the DrawList's own pixel space — not the 1x1 probe the
    /// combined list is synthesized from (a stale 1-cell extent clamps the
    /// factor to 4x and magnifies the whole frame, the dominant blur behind
    /// #232 alongside the unscaled surface/grid/atlas). Tick translates all
    /// content by the padding origin, so the plan spans the window extent.
    ///
    /// The draw list is authored in *physical* window pixels: cell metrics
    /// and the raster point size already carry the live DPI scale (see
    /// [`Self::apply_dpi_scale`]). At fractional scales the window is
    /// almost never an exact multiple of the cell, so the cell-aligned grid
    /// plan is a few pixels shorter than the surface. Reporting only the
    /// content plan made the GPU path treat that leftover strip as a scale
    /// factor (for example 1.01x on a 1496 px window with 32 px cells) and
    /// bilinearly stretch every glyph — the blurred/damaged text in #2520.
    /// The leftover strip is background, never content scale, so the plan
    /// spans the configured surface extent whenever one exists.
    pub fn present_plan_extent(&self) -> bitty_render::geometry::ExtentPx {
        // CTX-0873: the plan spans the whole window grid, chrome band
        // included (the band paints inside the plan, outside the container).
        let grid = self.live_cell_metrics().extent_for(
            usize::from(self.window_cells.width),
            usize::from(self.window_cells.height),
        );
        let inset = u64::from(self.window_padding_physical()).saturating_mul(2);
        let grid_width = u64::from(grid.width)
            .saturating_add(inset)
            .min(u64::from(u32::MAX)) as u32;
        let grid_height = u64::from(grid.height)
            .saturating_add(inset)
            .min(u64::from(u32::MAX)) as u32;
        let content = bitty_render::geometry::ExtentPx::new(grid_width, grid_height);
        match self.surface.extent() {
            Some(extent) if extent.width() > 0 && extent.height() > 0 => {
                // `max` keeps the full plan visible if a transient resize
                // left the surface smaller than the reflowed grid; a real
                // surface is never smaller in steady state.
                bitty_render::geometry::ExtentPx::new(
                    content.width.max(extent.width()),
                    content.height.max(extent.height()),
                )
            }
            _ => content,
        }
    }

    /// Adopts a DPI scale change (fail-safe, headless-testable, no I/O).
    ///
    /// Sanitizes `scale` via [`sanitize_dpi_scale`] (invalid input becomes
    /// 1.0, hostile input clamps to `[MIN_DPI_SCALE, MAX_DPI_SCALE]` — never
    /// panics), reloads the renderer's font at the scaled size through
    /// `GridRenderer::apply_dpi_scale`, and — when `physical_extent` carries
    /// a non-zero extent — derives the grid from physical pixels over the
    /// scaled cells ([`grid_from_surface_extent`]) and reflows state, layout,
    /// surface, and PTY to match.
    ///
    /// Callers holding only cached logical geometry should convert it first
    /// via [`bitty_platform::surface_extent_from_logical`] and pass the
    /// result here; callers with a live window must prefer re-reading the
    /// physical `inner_size` (already physical pixels — the winit logical
    /// size path is the suspected original sin behind #232). A following
    /// `Resized` event takes precedence either way: [`Self::handle_resize`]
    /// derives from the same live scaled cells.
    ///
    /// Passing `None` rescales the renderer and forces a full redraw while
    /// leaving the grid for the next `Resized` (this is what the
    /// `ScaleFactorChanged` event path does when no window handle is at
    /// hand).
    ///
    /// Never strands the window: renderer font-load failures keep the
    /// previous cells/grid and still force a full redraw; reflow failures
    /// after a successful rescale are absorbed for the same reason.
    pub fn apply_dpi_scale(&mut self, scale: f64, physical_extent: Option<PhysicalSize>) {
        let sanitized = sanitize_dpi_scale(scale);
        let base_cell = self.base_cell_metrics();
        let base_query = self.base_font_query();
        if self
            .renderer
            .apply_dpi_scale(base_cell, &base_query, sanitized)
            .is_err()
        {
            // Fail-safe: keep previous cells/grid; the window stays drawable.
            self.pending_full_redraw = true;
            return;
        }
        self.scale_factor = ScaleFactor::new_sanitized(sanitized);
        if let Some(extent) = physical_extent.and_then(bitty_platform::map_resize_to_surface_extent)
        {
            let (cols, rows) = self.grid_from_physical(extent);
            // Absorb reflow errors after a successful rescale so the adopted
            // renderer cells always win over stranding the window.
            let _ = self.reflow_to_grid(cols, rows, extent);
        }
        self.pending_full_redraw = true;
    }

    /// Live font size in points (CTX-0263 per-window font zoom).
    ///
    /// Starts at the validated config value; chrome zoom steps mutate this
    /// without touching the config file (per-window, not a global write).
    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.config.font_size
    }

    /// Startup font size this window resets to (CTX-0263 `ctrl+0`).
    #[must_use]
    pub fn base_font_size(&self) -> f32 {
        self.base_font_size
    }

    /// Live-apply a font size without restart (CTX-0263).
    ///
    /// Fail-closed: non-finite or out-of-range sizes (`[MIN, MAX]` below)
    /// return [`RuntimeError::InvalidConfig`] with no mutation. Valid sizes
    /// update the per-window config, re-derive the renderer at the live DPI
    /// scale, reflow the grid from the current surface extent (the window
    /// keeps its size; the grid absorbs the new cell), and repaint fully —
    /// so zoom survives reflow/resize (later resizes derive from the new
    /// base). Renderer font-load failures keep the previous cells/grid
    /// (fail-safe, window stays drawable) while the requested size is still
    /// stored so the next resize reconciles.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `size` is non-finite or outside
    /// `[FONT_ZOOM_MIN_PT, FONT_ZOOM_MAX_PT]`.
    pub fn set_font_size(&mut self, size: f32) -> Result<(), RuntimeError> {
        if !(size.is_finite()
            && (crate::config::FONT_ZOOM_MIN_PT..=crate::config::FONT_ZOOM_MAX_PT).contains(&size))
        {
            return Err(RuntimeError::InvalidConfig(
                "font_size must be finite within [6.0, 32.0]",
            ));
        }
        if (size - self.config.font_size).abs() < f32::EPSILON {
            return Ok(());
        }
        self.config.font_size = size;
        // Re-adopt at the live DPI scale so renderer + grid follow the new
        // base (same path as a DPI change; absorbs reflow errors).
        let scale = self.scale_factor.get();
        let extent = self.surface.extent();
        self.apply_dpi_scale(scale, extent);
        Ok(())
    }

    /// Grow the per-window font one step (CTX-0263 `ctrl+=`/`ctrl+plus`).
    ///
    /// Fail-closed at [`crate::config::FONT_ZOOM_MAX_PT`]: no wrap, no
    /// clamp-past-the-end, the size is left untouched.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when already at the maximum.
    pub fn zoom_in(&mut self) -> Result<(), RuntimeError> {
        let next = self.config.font_size + crate::config::FONT_ZOOM_STEP_PT;
        if next > crate::config::FONT_ZOOM_MAX_PT + f32::EPSILON {
            return Err(RuntimeError::InvalidConfig(
                "font_size already at maximum zoom",
            ));
        }
        self.set_font_size(next.min(crate::config::FONT_ZOOM_MAX_PT))
    }

    /// Shrink the per-window font one step (CTX-0263 `ctrl+-`).
    ///
    /// Fail-closed at [`crate::config::FONT_ZOOM_MIN_PT`].
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when already at the minimum.
    pub fn zoom_out(&mut self) -> Result<(), RuntimeError> {
        let next = self.config.font_size - crate::config::FONT_ZOOM_STEP_PT;
        if next < crate::config::FONT_ZOOM_MIN_PT - f32::EPSILON {
            return Err(RuntimeError::InvalidConfig(
                "font_size already at minimum zoom",
            ));
        }
        self.set_font_size(next.max(crate::config::FONT_ZOOM_MIN_PT))
    }

    /// Reset the per-window font to the startup size (CTX-0263 `ctrl+0`).
    ///
    /// Total: always succeeds for a validated runtime (the stored base came
    /// from a validated config).
    pub fn reset_zoom(&mut self) {
        let base = self.base_font_size;
        // Base came from a validated config inside the zoom range for all
        // shipped defaults; if a custom startup size sits outside the zoom
        // window (e.g. a 48pt accessibility config), still restore it
        // directly instead of failing the reset.
        if (base - self.config.font_size).abs() < f32::EPSILON {
            return;
        }
        if (crate::config::FONT_ZOOM_MIN_PT..=crate::config::FONT_ZOOM_MAX_PT).contains(&base) {
            let _ = self.set_font_size(base);
        } else {
            self.config.font_size = base;
            let scale = self.scale_factor.get();
            let extent = self.surface.extent();
            self.apply_dpi_scale(scale, extent);
        }
    }

    /// Reflows terminal state, layout, surface, and PTY to `cols`/`rows`
    /// with `surface_extent` as the configured extent. Shared by
    /// [`Self::handle_resize`] and [`Self::apply_dpi_scale`] so both paths
    /// stay consistent.
    pub(super) fn reflow_to_grid(
        &mut self,
        cols: usize,
        rows: usize,
        surface_extent: PhysicalSize,
    ) -> Result<(), RuntimeError> {
        // CTX-0375: the primary grid follows the primary owner leaf's
        // *decorated content* frame, not the window grid. The Core-owned
        // decoration (`gaps_out + border + content_inset`, CTX-0294/CTX-0333)
        // insets every leaf's content rectangle by px, so a window-sized grid
        // is cropped to the content frame by `viewport_snapshot`; the top
        // cursor keeps the window top-anchored and the bottom rows (for
        // example a nested tmux status bar on the last row) never painted.
        // This mirrors the CTX-0359 owner-follow reflow in `set_layout`; the
        // container keeps the window cell count so the decoration still lands
        // inside the Window. The container is set first so `present_frames`
        // can derive the decorated content frame; resizing state to it emits
        // full damage (grid + scrollback reflow) with a new generation.
        //
        // CTX-0873 (#1431): the window grid is recorded as `window_cells`;
        // the layout container is that grid minus the Core-owned chrome
        // band (workspace bar), so every leaf, the primary grid, and every
        // PTY winsize exclude the band through this one reflow path.
        self.window_cells = default_container(cols, rows);
        self.reflow_content_to_chrome();
        // Surface resize: real GPU path when attached, else headless
        if let Some(gpu) = self.gpu.as_ref() {
            self.surface
                .resize(gpu, surface_extent)
                .map_err(RuntimeError::from)?;
        } else {
            self.surface
                .headless_resize(surface_extent)
                .map_err(RuntimeError::from)?;
        }
        self.resize_primary_pty()?;
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Re-derives the layout container from `window_cells` minus the
    /// chrome band and reflows primary grid, leaves, pane sessions,
    /// selection, and search to it (CTX-0873). Infallible: surface and
    /// primary PTY resizes stay with the callers.
    pub(super) fn reflow_content_to_chrome(&mut self) {
        self.container = self.band_exclusive_container();
        // Preserve detached history windows across geometry changes. The
        // numeric offset is bottom-relative and a resize can move the same
        // top line by several rows; stable line identities are captured
        // before the terminal state reflows.
        let mut scroll_views = self.layout.leaf_ids();
        scroll_views.extend(self.floats.ids());
        scroll_views.extend(self.pinned.ids());
        scroll_views.retain(|id| self.view_scroll_offset(*id) != 0);
        for view in scroll_views {
            self.capture_view_scroll_anchor(view);
        }

        // Establish post-resize View geometry before resizing any backing
        // state. The primary state dimensions are derived from the presented
        // owner frame, and pane sessions are derived from the same frames;
        // anchor restoration must observe both the new View height and the
        // state geometry that those Views require.
        self.layout.reflow_with_gaps(self.container, self.gaps());
        let frames = self.present_frames();
        self.reflow_present_layout(&frames);
        let fallback = (
            usize::from(self.container.width).max(1),
            usize::from(self.container.height).max(1),
        );
        let (cols, rows) = {
            let frames = self.present_frames();
            // CTX-1088: a durable fixed-size primary owner keeps its grid
            // through window reflows; its paint dims stay slot-sized, so
            // the frame alone would shrink the grid back.
            if let Some(fixed) = self
                .primary_view
                .and_then(|primary| self.fixed_size_for_view(primary))
            {
                (
                    usize::from(fixed.width.max(1)),
                    usize::from(fixed.height.max(1)),
                )
            } else {
                self.primary_view
                    .and_then(|primary| frames.iter().find(|frame| frame.view == primary))
                    .map(|frame| {
                        (
                            usize::from(frame.cols.max(1)),
                            usize::from(frame.rows.max(1)),
                        )
                    })
                    .unwrap_or(fallback)
            }
        };
        let (_damage, primary_resize) = self.state.resize_with_report(cols, rows);
        self.cols = cols;
        self.rows = rows;

        // Reflow every presented View before restoring its anchor. In
        // particular, detached floats and pinned Views are not layout leaves,
        // and pane sessions may be rebuilt/resized by the geometry sync below.
        // Restoring before these steps would resolve against the old View
        // height and could be invalidated by the later backing-state resize.
        self.sync_pane_geometry_to(&frames);

        // Clamp any live View scroll offsets to its own backing store
        // (scrollback may have been truncated on shrink, though we preserve
        // ids; clamp keeps offset in-bounds deterministically). #1338: split
        // panes own their shells, so the primary length is the wrong bound
        // for a focused split leaf. Detached floats and pinned Views use the
        // same state lookup and are deliberately included here.
        let mut ids = self.layout.leaf_ids();
        ids.extend(self.floats.ids());
        ids.extend(self.pinned.ids());
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let max_scrollback = match self.pane_sessions.get(&id) {
                Some(sess) => sess.state.scrollback_len(),
                None => self.state.scrollback_len(),
            };
            if let Some(view) = self.layout.find_leaf_mut(id) {
                view.clamp_scroll_offset(max_scrollback);
            }
            self.restore_view_scroll_anchor(id);
        }
        // #2543: move a held grid-coordinate selection with the primary
        // content before clamping it to the new bounds. Pane owners were
        // already remapped by `sync_pane_geometry_to`.
        if let Some(owner) = self.primary_view {
            self.remap_selection_for_resize(owner, true, primary_resize);
        }
        self.clamp_view_bindings_to_geometry();
    }

    /// Keeps a held selection on the same content across a height-only
    /// resize.
    ///
    /// A `Selection` installed without a buffer anchor (`start_selection` and
    /// friends) stores live grid coordinates. A height shrink/grow moves the
    /// primary rows by [`ResizeReport::row_shift`], so applying that shift
    /// preserves the copied text and search identity. A width-changing
    /// reflow is not a constant shift, so it keeps the existing geometry
    /// clamp path; an endpoint pushed out of the grid into scrollback drops
    /// the selection instead of silently re-anchoring to different content.
    ///
    /// Buffer-anchored pointer selections carry combined-buffer rows and are
    /// re-projected by [`Self::clamp_view_bindings_to_geometry`], so they are
    /// left to that path. `is_primary_state` distinguishes the runtime-global
    /// primary grid from a pane session that may shadow the same `owner`.
    pub(super) fn remap_selection_for_resize(
        &mut self,
        owner: ViewId,
        is_primary_state: bool,
        report: ResizeReport,
    ) {
        if report.width_changed || report.row_shift == 0 {
            return;
        }
        let Some(sel) = self.selection_state else {
            return;
        };
        if sel.owner != owner {
            return;
        }
        let owner_is_primary_state =
            self.primary_view == Some(owner) && !self.pane_sessions.contains_key(&owner);
        if owner_is_primary_state != is_primary_state {
            return;
        }
        if sel.buffer.is_some() || self.is_viewport_scrolled(owner) {
            return;
        }
        let shift = report.row_shift;
        let shifted = |pos: CellPos| -> Option<CellPos> {
            let row = i32::from(pos.row).checked_add(shift)?;
            u16::try_from(row)
                .ok()
                .map(|row| CellPos::new(row, pos.col))
        };
        let (Some(anchor), Some(focus)) =
            (shifted(sel.selection.anchor), shifted(sel.selection.focus))
        else {
            self.drop_selection();
            return;
        };
        let pin = sel.anchor_press.and_then(shifted);
        let selection = Selection {
            anchor,
            focus,
            ..sel.selection
        };
        self.install_selection(owner, selection, pin, sel.dragging);
        self.restore_selection_meta(
            sel.press_boundary,
            sel.single_cell,
            sel.buffer,
            sel.press_buffer,
        );
    }

    /// Re-clamps the live selection and search matches after a geometry
    /// change (shared by the window reflow and the chrome-band refresh).
    pub(super) fn clamp_view_bindings_to_geometry(&mut self) {
        // Clamp selection to the owner's new snapshot bounds (keeps
        // invariants after reflow; wide-char snapping is preserved). Headless
        // so deterministic. CTX-0803 (#1476): the bound is the *owner's* grid,
        // not the primary grid — a split pane's selection is reclamped against
        // that pane. A selection whose owner is no longer live is dropped by
        // the fail-closed guard.
        if self.selection_state.is_some() && self.resolved_selection().is_none() {
            // Issue #2470: a range whose history identities were reassigned
            // by this geometry change must not resolve onto unrelated lines.
            self.drop_selection();
        }
        if let Some(sel) = self.resolved_selection() {
            // Issues #2480/#2470: pointer selections keep their buffer
            // anchors across a reflow/DPI change. Rows are re-projected
            // against the new window; a one-cell selection survives even
            // though its inclusive range is collapsed.
            if let Some(pers) = sel.buffer {
                let clamped = self
                    .selection_window(sel.owner)
                    .zip(self.live_view_state(sel.owner))
                    .and_then(|(window, state)| {
                        let mut clamped = pers.clamped(state);
                        if window.cols > 0 {
                            clamped.anchor.col = clamped.anchor.col.min(window.cols - 1);
                            clamped.focus.col = clamped.focus.col.min(window.cols - 1);
                        }
                        clamped.is_valid(state).then_some((window, clamped))
                    });
                match clamped {
                    Some((window, clamped)) => {
                        let selection = self
                            .project_persistent(&clamped, sel.owner)
                            .unwrap_or_else(|| {
                                let cell = CellPos::new(
                                    sel.selection.anchor.row.min(window.rows.saturating_sub(1)),
                                    sel.selection.anchor.col.min(window.cols.saturating_sub(1)),
                                );
                                Selection {
                                    anchor: cell,
                                    focus: cell,
                                    kind: clamped.kind,
                                    active: sel.dragging,
                                }
                            });
                        let pin = sel.anchor_press.map(|pin| {
                            CellPos::new(
                                pin.row.min(window.rows.saturating_sub(1)),
                                pin.col.min(window.cols.saturating_sub(1)),
                            )
                        });
                        self.install_selection(sel.owner, selection, pin, sel.dragging);
                        self.restore_selection_meta(
                            sel.press_boundary,
                            sel.single_cell,
                            Some(clamped),
                            sel.press_buffer,
                        );
                    }
                    None => self.drop_selection(),
                }
            } else if self.is_viewport_scrolled(sel.owner) {
                // CTX-1021 (#1807): a scrolled owner addresses the viewport
                // composite, so reclamp against it (not the live grid) to keep
                // a scrolled drag inside the visible window after reflow.
                match self.viewport_snapshot_for(sel.owner) {
                    Some(viewport) => {
                        let clamped = sel.selection.clamped(&viewport).snapped(Some(&viewport));
                        if clamped.is_empty() && !sel.single_cell {
                            self.drop_selection();
                        } else {
                            let max_row = viewport.height.saturating_sub(1) as u16;
                            let max_col = viewport.width.saturating_sub(1) as u16;
                            let pin = sel.anchor_press.map(|pin| {
                                CellPos::new(pin.row.min(max_row), pin.col.min(max_col))
                            });
                            self.install_selection(sel.owner, clamped, pin, sel.dragging);
                            self.restore_selection_meta(
                                sel.press_boundary,
                                sel.single_cell,
                                None,
                                None,
                            );
                        }
                    }
                    None => self.drop_selection(),
                }
            } else {
                match self.live_view_state(sel.owner) {
                    Some(state) => {
                        let snap = state.snapshot();
                        let clamped = sel.selection.clamped(&snap).snapped(Some(&snap));
                        if clamped.is_empty() && !sel.single_cell {
                            self.drop_selection();
                        } else {
                            // Keep the drag pin in bounds for future word/line
                            // extension.
                            let max_row = snap.height.saturating_sub(1) as u16;
                            let max_col = snap.width.saturating_sub(1) as u16;
                            let pin = sel.anchor_press.map(|pin| {
                                CellPos::new(pin.row.min(max_row), pin.col.min(max_col))
                            });
                            self.install_selection(sel.owner, clamped, pin, sel.dragging);
                            self.restore_selection_meta(
                                sel.press_boundary,
                                sel.single_cell,
                                None,
                                None,
                            );
                        }
                    }
                    None => self.drop_selection(),
                }
            }
        }
        // Search UI integration (CTX-0061): clamp matches to new geometry; refresh
        // is bounded and deterministic. Keeps current index clamped.
        if self.search_state.is_active() {
            self.search_refresh();
        }
    }

    /// Pixel geometry a PTY of `cols` x `rows` reports through
    /// `TIOCGWINSZ` at the live cell metrics, saturating to `u16`.
    ///
    /// Image-aware children (kitty `icat`, ratatui-image sizing) read these
    /// fields directly; zero makes them treat the terminal as unable to
    /// report pixel sizes (#2442).
    pub(super) fn pty_pixel_span(&self, cols: u16, rows: u16) -> (u16, u16) {
        let cell = self.live_cell_metrics();
        let width = u64::from(cell.width).saturating_mul(u64::from(cols));
        let height = u64::from(cell.height).saturating_mul(u64::from(rows));
        (
            u16::try_from(width.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
            u16::try_from(height.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
        )
    }

    /// Resizes the primary PTY winsize to the primary grid (`cols`/`rows`).
    pub(super) fn resize_primary_pty(&mut self) -> Result<(), RuntimeError> {
        let pty_cols = self.cols.min(u16::MAX as usize) as u16;
        let pty_rows = self.rows.min(u16::MAX as usize) as u16;
        let (pixel_width, pixel_height) = self.pty_pixel_span(pty_cols, pty_rows);
        if let Some(pty) = self.pty.as_mut() {
            pty.resize_pixels(pty_cols, pty_rows, pixel_width, pixel_height)
                .map_err(RuntimeError::from)?;
        }
        Ok(())
    }

    /// Handles one platform event, returning `true` when the event asks the
    /// application loop to exit (window close requested or `Exiting` phase).
    ///
    /// Resize events are routed through [`Self::handle_resize`]; keyboard
    /// input is encoded via the legacy xterm table plus Kitty opt-in (7727) and
    /// routed to the PTY writer when live, otherwise buffered as bounded pending input
    /// for headless observation. Mouse and cursor events drive the
    /// headless-tested selection state via `bitty-ui::Selection`
    /// with wide-char snapping, or when mouse 1000/1002/1003 + 1006 SGR capture active
    /// encode to bounded SGR bytes (≤32 per event). Focus 1004 emits CSI I/O,
    /// bracketed paste 2004 wraps commits, wheel accumulates pixel deltas to cell lines,
    /// and IME preedit is presentation overlay.
    pub fn handle_platform_event(&mut self, event: PlatformEvent) -> bool {
        match event {
            PlatformEvent::Window { window_id: _, kind } => match kind {
                WindowEventKind::Resized(size) => {
                    let _ = self.handle_resize(size);
                    false
                }
                WindowEventKind::ScaleFactorChanged(factor) => {
                    // Adopt immediately (fail-safe: sanitized, never panics,
                    // never strands the window). The grid follows when the
                    // embedder re-reads the physical inner_size (see
                    // apply_dpi_scale) or from the next Resized, which takes
                    // precedence either way.
                    self.apply_dpi_scale(factor.get(), None);
                    false
                }
                WindowEventKind::CloseRequested => {
                    // CTX-0370: window close passes the `close_confirm` gate.
                    // The first request may arm a bounded confirmation
                    // (return `false`: keep running, paint the banner);
                    // repeating the request confirms the quit, `Esc` cancels.
                    self.window_close_request()
                }
                WindowEventKind::Closed => true,
                WindowEventKind::RedrawRequested => {
                    // The embedder will call `tick` on `AboutToWait`; we do
                    // not present eagerly here so frame-on-demand stays
                    // honest (no periodic wakeups when idle).
                    false
                }
                WindowEventKind::KeyboardInput(key) => {
                    let _ = self.handle_key_event(key);
                    false
                }
                WindowEventKind::MouseInput(mouse) => {
                    self.handle_mouse_input(mouse);
                    if mouse.button == MouseButton::Left && mouse.state == PressState::Released {
                        // CTX-1006 review: press/release pairing. Take (clear)
                        // both stored press URIs on every left release so a
                        // release alone can never arm a link; the mint below
                        // requires an exact press/release URI match.
                        let hyperlink_press = self.hyperlink_press_uri.take();
                        let plaintext_press = self.plaintext_press_uri.take();
                        if let Some(pos) = self.last_cursor {
                            // CTX-0181: a release over the painted scrollbar
                            // ends a scroll gesture — it must not activate a
                            // hyperlink beneath the mapped cell.
                            if self.scrollbar_hit_at(pos) {
                                return false;
                            }
                            // Issues #1759/#1760: Ctrl-gated mint with pairing
                            // for both OSC 8 and plaintext (OSC 8 wins where
                            // both claim a cell). CTX-0804: resolve in the
                            // View under the pointer, never the primary grid.
                            // A plain release keeps selection meaning.
                            if !self.hyperlink_activation_modifier_held() {
                                return false;
                            }
                            let uri = if let Some(uri) = self.safe_hyperlink_uri_at(pos) {
                                if hyperlink_press.as_ref() != Some(&uri) {
                                    return false;
                                }
                                uri
                            } else if let Some(uri) = self.safe_plaintext_url_at(pos) {
                                if plaintext_press.as_ref() != Some(&uri) {
                                    return false;
                                }
                                uri
                            } else {
                                return false;
                            };
                            let token = ActivationGesture(self.next_activation_gesture);
                            self.next_activation_gesture =
                                self.next_activation_gesture.wrapping_add(1).max(1);
                            self.pending_activation_gesture = Some(token);
                            // CTX-0577: bind the exact URI to the
                            // gesture so the live consumer cannot
                            // be handed a substitute target.
                            self.pending_activation_uri = Some(uri);
                        }
                    }
                    false
                }
                WindowEventKind::CursorMoved(pos) => {
                    self.handle_cursor_moved(pos);
                    false
                }
                WindowEventKind::CursorLeft => {
                    // Cursor left window: end drag if active (deterministic).
                    self.end_selection_drag_in_place();
                    // CTX-0181: leaving the window ends a thumb drag and
                    // disengages auto-hide (tracked separately from
                    // `last_cursor`, whose selection-path meaning is kept).
                    // A painted thumb needs one repaint to clear.
                    // CTX-0260: leaving also ends an Alt+drag move.
                    self.cursor_left_window = true;
                    self.scrollbar_release();
                    self.end_alt_drag();
                    // CTX-1150: leaving the window also ends a float
                    // edge-resize (deterministic teardown like the Alt+drag
                    // move above).
                    self.end_float_resize();
                    // Issue #1348: leaving the window also ends a
                    // border-drag resize (deterministic teardown like the
                    // Alt+drag move above).
                    self.end_border_drag();
                    // Issue #1694 (CTX-0966): leaving also cancels a Mod
                    // tiled-drag without committing (no drop target outside
                    // the window; the tree stays untouched).
                    self.cancel_tiled_drag();
                    // CTX-0334: leaving the window also drops a pending
                    // hover dwell so a re-entry starts a fresh clock.
                    self.clear_hover_pending();
                    // Issues #1759/#1760: leaving drops both hovers so
                    // no stale underline, preview, or pointer survives.
                    self.clear_hyperlink_hover();
                    self.clear_plaintext_hover();
                    // Issue #2411: leaving also ends plugin band hover
                    // ownership with one `leave`, so a tooltip cannot stay
                    // anchored after the pointer is outside the window.
                    self.clear_band_hover();
                    if self.scrollbar_visible {
                        self.pending_full_redraw = true;
                    }
                    false
                }
                WindowEventKind::MouseWheel(delta) => {
                    self.handle_wheel(delta);
                    false
                }
                WindowEventKind::Focused(focused) => {
                    self.set_focused(focused);
                    false
                }
                WindowEventKind::ModifiersChanged(mods) => {
                    self.shift_pressed = mods.shift;
                    self.control_pressed = mods.control;
                    self.alt_pressed = mods.alt;
                    // Issue #1759 (R-005): the Super latch (macOS Cmd) is
                    // part of the hyperlink gesture modifier. The named-key
                    // tracker in `input` converges on the same latch; the
                    // platform snapshot here is authoritative per event.
                    self.super_pressed = mods.super_pressed;
                    // CTX-0159: retain modifier latch changes for probes.
                    self.inspect_ring.push_modifiers(
                        self.shift_pressed,
                        self.control_pressed,
                        self.alt_pressed,
                    );
                    self.publish_inspect_snapshot();
                    // Issues #1759/#1760: the plaintext hover is Ctrl-gated,
                    // so a latch change can arm or disarm it under a
                    // stationary pointer. Re-resolve both now; the per-tick
                    // revalidate covers the quiet-window case.
                    self.revalidate_hyperlink_hover();
                    self.revalidate_plaintext_hover();
                    false
                }
                WindowEventKind::Ime(ime) => {
                    match ime {
                        bitty_platform::ImeEvent::Preedit(text, cursor) => {
                            if text.is_empty() {
                                self.handle_ime_preedit(None, None);
                            } else {
                                let cur = cursor.map(|(s, _)| s);
                                self.handle_ime_preedit(Some(text), cur);
                            }
                        }
                        bitty_platform::ImeEvent::Commit(text) => {
                            self.handle_ime_commit(text);
                        }
                        bitty_platform::ImeEvent::Enabled | bitty_platform::ImeEvent::Disabled => {
                            // No grid mutation; just ensure overlay cleared on disabled
                            if matches!(ime, bitty_platform::ImeEvent::Disabled) {
                                self.handle_ime_preedit(None, None);
                                // CTX-0783: a disabled input method will never
                                // echo a committing key, so nothing is owed
                                // and the claim must not outlive the
                                // composition to eat a later keystroke. This
                                // runs after the preedit clear, which is what
                                // arms the claim for a cancel.
                                self.force_release_ime_key_claim();
                            }
                        }
                    }
                    false
                }
            },
            PlatformEvent::Exiting => true,
            PlatformEvent::Resumed => {
                self.pending_full_redraw = true;
                false
            }
            // CTX-0951: the OS appearance toggle is embedder-owned. `Runtime`
            // holds only the installed palette, not the raw
            // `appearance.theme` selection, so it cannot re-resolve the dual
            // half itself; the embedder (`TerminalApp`) calls
            // `apply_system_appearance` with the live `EffectiveConfig`.
            // Returning `false` keeps the loop alive and routes the event to
            // that handler instead of silently dropping it in a catch-all.
            PlatformEvent::SystemAppearanceChanged(_) => false,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Runtime;
    use crate::config::RuntimeConfig;
    use bitty_render::glyph::FontStyle;
    use bitty_ui::{LayoutNode, SplitAxis, View, ViewId};

    fn two_pane_layout() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        )
    }

    fn visible_rows(rt: &Runtime, view: ViewId) -> Vec<String> {
        let state = rt
            .live_view_state(view)
            .expect("view must have a live backing state");
        rt.find_view(view)
            .expect("view must be present")
            .visible_text_rows(state)
    }

    #[test]
    fn base_font_query_reflects_config() {
        // CTX-0295: the configured family/size must reach the renderer
        // `FontQuery` used for font loading and DPI rescale. The app-level
        // mapping tests prove file -> RuntimeConfig; this closes the last
        // link to the rasterizer input.
        let cfg = RuntimeConfig {
            font_family: "Mono".to_string(),
            font_size: 17.5,
            ..RuntimeConfig::default()
        };
        let rt = Runtime::new(cfg).expect("runtime builds with custom font config");
        let query = rt.base_font_query();
        assert_eq!(query.family, "Mono");
        assert!((query.point_size - 17.5).abs() < f32::EPSILON);
        assert_eq!(query.style, FontStyle::Normal);

        // Defaults ride through unchanged too.
        let rt = Runtime::with_defaults().expect("default runtime builds");
        let query = rt.base_font_query();
        assert_eq!(query.family, crate::config::font_default_family());
        assert!((query.point_size - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn present_plan_covers_the_surface_for_non_cell_aligned_windows() {
        // #2520: the plan must be the draw list's physical pixel space. A
        // cell-aligned grid stops short of a window that is not an exact
        // multiple of the cell; deriving the present scale from that
        // remainder bilinearly stretched every glyph.
        use bitty_platform::PhysicalSize;

        let mut rt = Runtime::with_defaults().expect("runtime builds");
        rt.handle_resize(PhysicalSize::new(2505, 1496))
            .expect("resize reflows");
        let surface = rt.surface_extent().expect("surface configured");
        let plan = rt.present_plan_extent();

        assert_eq!(
            (plan.width, plan.height),
            (surface.width(), surface.height()),
            "plan must span the configured surface, not only the cell-aligned grid"
        );
        assert_eq!(
            bitty_render::batch::derive_scale(surface.width(), surface.height(), plan),
            1.0,
            "a leftover window strip must never become a content scale factor"
        );
    }

    #[test]
    fn present_plan_keeps_owner_size_11_at_scale_1_6_crisp() {
        // #2520 reproduction numbers: the owner's 11pt base cell is 9x20.
        // Adopting 1.6 DPI gives the 14x32 physical cell actually rendered on
        // the 2506x1496 window; the cell-aligned plan stops at 2504x1466, and
        // the old GPU path stretched all text by ~1.01x (visible vertical
        // smearing). Use the deterministic rasterizer so this exercises the
        // real `apply_dpi_scale` path without requiring host fonts in CI.
        use bitty_platform::PhysicalSize;

        let cfg = RuntimeConfig {
            font_family: "DejaVu Sans Mono".to_string(),
            font_size: 11.0,
            cell_width: 9,
            cell_height: 20,
            ..RuntimeConfig::default()
        };
        let mut rt = Runtime::with_deterministic_rasterizer(cfg).expect("runtime builds");
        rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2506, 1496)));
        assert_eq!(rt.dpi_scale(), 1.6);
        assert_eq!(
            (rt.live_cell_metrics().width, rt.live_cell_metrics().height),
            (14, 32),
            "11pt at 1.6 DPI must adopt the 14x32 physical cell"
        );

        let surface = rt.surface_extent().expect("surface configured");
        let plan = rt.present_plan_extent();
        assert_eq!(
            (plan.width, plan.height),
            (surface.width(), surface.height())
        );
        let scale = bitty_render::batch::derive_scale(surface.width(), surface.height(), plan);
        assert!(
            (scale - 1.0).abs() < f32::EPSILON,
            "glyph sampling scale must be exactly 1.0, got {scale}"
        );
    }

    #[test]
    fn detached_float_anchor_restores_after_height_reflow_and_output() {
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        rt.set_layout(two_pane_layout());
        let float = ViewId::new(1);
        rt.float_detach(float).expect("float detaches");
        assert!(rt.set_focus(float), "detached float remains focusable");

        for i in 0..60 {
            rt.handle_pty_bytes(format!("FLOAT-{i:03}\r\n").as_bytes());
        }
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 4.0));
        let before = visible_rows(&rt, float);
        assert!(rt.view_scroll_offset(float) > 0, "float must be detached");

        let width = rt.surface_extent().expect("surface extent").width();
        rt.handle_resize(bitty_platform::PhysicalSize::new(width, 12 * 19 + 16))
            .expect("height resize");
        let after_resize = visible_rows(&rt, float);
        assert_eq!(
            after_resize.first(),
            before.first(),
            "resize must restore the float's top logical line after View reflow"
        );

        rt.handle_pty_bytes(b"FLOAT-GROWTH\r\n");
        assert_eq!(
            visible_rows(&rt, float).first(),
            before.first(),
            "PTY growth must not move detached float history"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_pane_anchor_restores_after_height_reflow_and_output() {
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        rt.set_layout(two_pane_layout());
        let pinned = ViewId::new(1);
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == pinned)
            .expect("pinned pane frame");
        rt.spawn_shell_for_view(
            pinned,
            "/bin/sh",
            &["-c", "exec sleep 30"],
            frame.cols,
            frame.rows,
        )
        .expect("pane shell starts");
        rt.pin_floating(pinned).expect("pane pins");
        assert!(rt.set_focus(pinned), "pinned view remains focusable");

        for i in 0..60 {
            rt.handle_pane_bytes(pinned, format!("PIN-{i:03}\r\n").as_bytes());
        }
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 4.0));
        let before = visible_rows(&rt, pinned);
        assert!(rt.view_scroll_offset(pinned) > 0, "pinned view must scroll");

        let width = rt.surface_extent().expect("surface extent").width();
        rt.handle_resize(bitty_platform::PhysicalSize::new(width, 12 * 19 + 16))
            .expect("height resize");
        let after_resize = visible_rows(&rt, pinned);
        assert_eq!(
            after_resize.first(),
            before.first(),
            "resize must restore the pinned pane's top logical line after sync"
        );

        rt.handle_pane_bytes(pinned, b"PIN-GROWTH\r\n");
        assert_eq!(
            visible_rows(&rt, pinned).first(),
            before.first(),
            "PTY growth must not move pinned history"
        );
    }
}
