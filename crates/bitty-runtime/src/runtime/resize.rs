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
        CellMetrics::new(self.config.cell_width, self.config.cell_height)
            .expect("validated config guarantees non-zero cell metrics")
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
        (cols.clamp(1, 1000), rows.clamp(1, 1000))
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

    /// Pixel extent the combined frame's plan covers: container cells at the
    /// live (DPI-scaled) cell metrics, plus the window padding inset on
    /// every side (CTX-0223).
    ///
    /// The GPU present path recovers its per-frame NDC factor as
    /// `surface / plan` ([`bitty_render::batch::derive_scale`]), so this
    /// must describe the DrawList's own pixel space — not the 1x1 probe the
    /// combined list is synthesized from (a stale 1-cell extent clamps the
    /// factor to 4x and magnifies the whole frame, the dominant blur behind
    /// #232 alongside the unscaled surface/grid/atlas). Tick translates all
    /// content by the padding origin, so the plan spans the window extent.
    pub fn present_plan_extent(&self) -> bitty_render::geometry::ExtentPx {
        let grid = self.live_cell_metrics().extent_for(
            usize::from(self.container.width),
            usize::from(self.container.height),
        );
        let inset = u64::from(self.window_padding_physical()).saturating_mul(2);
        let width = u64::from(grid.width)
            .saturating_add(inset)
            .min(u64::from(u32::MAX)) as u32;
        let height = u64::from(grid.height)
            .saturating_add(inset)
            .min(u64::from(u32::MAX)) as u32;
        bitty_render::geometry::ExtentPx::new(width, height)
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
        // Resize terminal state first so snapshot dimensions reflect the new
        // geometry before layout and surface work; resize also emits full
        // damage (grid + scrollback reflow) with a new generation.
        let _damage = self.state.resize(cols, rows);
        self.cols = cols;
        self.rows = rows;
        self.container = default_container(cols, rows);
        // Clamp any leaf View scroll offsets to the new scrollback limit
        // (scrollback may have been truncated on shrink, though we preserve
        // ids; clamp keeps offset in-bounds deterministically).
        let max_scrollback = self.state.scrollback_len();
        let ids = self.layout.leaf_ids();
        for id in ids {
            if let Some(view) = self.layout.find_leaf_mut(id) {
                view.clamp_scroll_offset(max_scrollback);
            }
        }
        self.layout.reflow_with_gaps(self.container, self.gaps());
        // CTX-0176: the container moved, so every pane session's grid +
        // PTY winsize follows its leaf (primary state/PTY handled below).
        self.sync_pane_geometry();
        // Clamp selection to new snapshot bounds (keeps invariants after reflow;
        // wide-char snapping is preserved). Headless so deterministic.
        if let Some(sel) = self.selection {
            let snap = self.state.snapshot();
            let clamped = sel.clamped(&snap).snapped(Some(&snap));
            if clamped.is_empty() {
                self.selection = None;
                self.selection_dragging = false;
            } else {
                self.selection = Some(clamped);
            }
        }
        // Search UI integration (CTX-0061): clamp matches to new geometry; refresh
        // is bounded and deterministic. Keeps current index clamped.
        if self.search_state.is_active() {
            self.search_state.refresh(&self.state);
        }
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
        if let Some(pty) = self.pty.as_mut() {
            let pty_cols = self.cols.min(u16::MAX as usize) as u16;
            let pty_rows = self.rows.min(u16::MAX as usize) as u16;
            pty.resize(pty_cols, pty_rows).map_err(RuntimeError::from)?;
        }
        self.pending_full_redraw = true;
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
                WindowEventKind::CloseRequested | WindowEventKind::Closed => true,
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
                        if let Some(pos) = self.last_cursor {
                            // CTX-0181: a release over the painted scrollbar
                            // ends a scroll gesture — it must not activate a
                            // hyperlink beneath the mapped cell.
                            if self.scrollbar_hit_at(pos) {
                                return false;
                            }
                            let cell = self.cursor_to_cell(pos);
                            let snapshot = self.state.snapshot();
                            let Some(index) = (cell.row as usize)
                                .checked_mul(snapshot.width)
                                .and_then(|base| base.checked_add(cell.col as usize))
                            else {
                                return false;
                            };
                            if let Some(id) = snapshot.cells.get(index).and_then(|c| c.hyperlink) {
                                if let Some((_, uri)) = self.state.hyperlink_entry(id) {
                                    let is_safe = if uri.starts_with("file:") {
                                        bitty_platform::validate_file_url(uri).is_ok()
                                    } else {
                                        bitty_platform::validate_url(uri).is_ok()
                                    };
                                    if is_safe {
                                        let token = ActivationGesture(self.next_activation_gesture);
                                        self.next_activation_gesture =
                                            self.next_activation_gesture.wrapping_add(1).max(1);
                                        self.pending_activation_gesture = Some(token);
                                    }
                                }
                            }
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
                    if self.selection_dragging {
                        self.selection_dragging = false;
                        if let Some(mut sel) = self.selection {
                            sel.active = false;
                            self.selection = Some(sel);
                        }
                    }
                    // CTX-0181: leaving the window ends a thumb drag and
                    // disengages auto-hide (tracked separately from
                    // `last_cursor`, whose selection-path meaning is kept).
                    // A painted thumb needs one repaint to clear.
                    // CTX-0260: leaving also ends an Alt+drag move.
                    self.scrollbar_cursor_left = true;
                    self.scrollbar_release();
                    self.end_alt_drag();
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
                    // CTX-0159: retain modifier latch changes for probes.
                    self.inspect_ring.push_modifiers(
                        self.shift_pressed,
                        self.control_pressed,
                        self.alt_pressed,
                    );
                    self.publish_inspect_snapshot();
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
            _ => false,
        }
    }
}
