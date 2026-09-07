//! `Runtime` — Frame tick and software present path.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::*;
use bitty_render::gpu::PresentStats as RenderPresentStats;

/// Deterministic headless rasterizer: no font stack required, bit-identical
/// on every platform and on both Linux and Windows CI.
///
/// Blank characters (`' '` and zero-width-adjacent) return `None` (cacheable
/// miss). All other characters produce a deterministic square coverage mask
/// derived from the scalar value, sized `6..8` pixels `+` font-size scaling.
/// The bitmap is `Rgb` coverage averaged to luminance by the software
/// compositor, exactly as the GPU path will sample atlas texels.
#[derive(Debug)]
pub(super) struct HeadlessRasterizer {
    next_id: u64,
}

impl HeadlessRasterizer {
    pub(super) fn new() -> Self {
        Self { next_id: 0 }
    }
}

impl GlyphRasterizer for HeadlessRasterizer {
    fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next_id))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' || key.character == '\0' {
            return Ok(None);
        }
        // Deterministic size: 6..8 + small font-size contribution so different
        // point sizes hash to different bitmaps without affecting determinism
        // across platforms (no font metrics involved).
        let base = (u32::from(key.character) % 3 + 6) as i32;
        // Slight size bump from point_size to keep per-size raster keys distinct
        // without breaking the bounded bit model.
        let varied = if key.point_size > 13.0 {
            base + 1
        } else {
            base
        };
        let side = varied;
        let channels = BitmapFormat::Rgb.channels();
        let data_len = side as usize * side as usize * channels;
        let data = vec![0xAA; data_len];
        let metrics = GlyphMetrics {
            left: 0,
            top: 6,
            width: side,
            height: side,
            advance: [side, 0],
        };
        Ok(Some(
            GlyphBitmap::try_new(metrics, BitmapFormat::Rgb, data)
                .expect("deterministic bitmap must be valid"),
        ))
    }
}

/// Owned rasterizer that can be either deterministic headless or real crossfont.
///
/// Headless is the CI baseline (no font file, bit-identical everywhere).
/// Crossfont is the vertical-slice real path (platform font stack).
/// Both implement `GlyphRasterizer` so `GridRenderer` stays generic.
#[derive(Debug)]
pub(super) enum AnyRasterizer {
    Headless(HeadlessRasterizer),
    CrossFont(Box<CrossFontRasterizer>),
}

impl AnyRasterizer {
    pub(super) fn try_crossfont() -> Self {
        match CrossFontRasterizer::new() {
            Ok(cf) => Self::CrossFont(Box::new(cf)),
            Err(_) => Self::Headless(HeadlessRasterizer::new()),
        }
    }
    pub(super) fn is_crossfont(&self) -> bool {
        matches!(self, Self::CrossFont(_))
    }
}

impl GlyphRasterizer for AnyRasterizer {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        match self {
            Self::Headless(r) => r.load_font(query),
            Self::CrossFont(r) => r.load_font(query),
        }
    }
    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        match self {
            Self::Headless(r) => r.rasterize(key),
            Self::CrossFont(r) => r.rasterize(key),
        }
    }
}

/// Owned presentation statistics returned by [`Runtime::tick`].
///
/// This type is workspace-owned; no `wgpu` type leaks through it. The
/// `headless` flag is `true` for the software seam that CI exercises; a
/// real GPU present sets it to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStats {
    /// Logical frame counter for the surface.
    pub frame: u64,
    /// Number of fill rectangles in the presented draw list.
    pub fills: usize,
    /// Number of glyph instances in the presented draw list.
    pub glyphs: usize,
    /// Whether the surface was the headless software fake.
    pub headless: bool,
    /// Snapshot generation that was presented.
    pub generation: u64,
}

impl From<RenderPresentStats> for PresentStats {
    fn from(value: RenderPresentStats) -> Self {
        Self {
            frame: value.frame,
            fills: value.fills,
            glyphs: value.glyphs,
            headless: value.headless,
            generation: 0,
        }
    }
}

/// Creates a viewport snapshot of `snapshot` limited to `cols x rows`.
///
/// The viewport is the top-left `cols x rows` window of the active screen,
/// padded with erased cells when the requested size exceeds the snapshot
/// dimensions (honest padding for the deferred grid-resize reflow). Cursor and
/// modes are carried over; title/modes are snapshot-owned.
pub(super) fn viewport_snapshot(snapshot: &Snapshot, cols: u16, rows: u16) -> Snapshot {
    let req_w = cols as usize;
    let req_h = rows as usize;
    if snapshot.width == req_w && snapshot.height == req_h {
        return snapshot.clone();
    }
    let mut cells = Vec::with_capacity(req_w * req_h);
    let src_w = snapshot.width;
    let src_h = snapshot.height;
    for r in 0..req_h {
        for c in 0..req_w {
            if r < src_h && c < src_w {
                let idx = r * src_w + c;
                let cell = snapshot.cells.get(idx).cloned().unwrap_or_else(|| {
                    bitty_term_state::Cell::erased(bitty_term_state::Style::default())
                });
                // For the trailing spacer of a wide char that would be split
                // across the viewport edge, degrade to an erased cell to keep
                // invariants (no orphan spacer): the leading half outside the
                // viewport is not visible, so the spacer inside is erased.
                if cell.spacer && c == 0 {
                    cells.push(bitty_term_state::Cell::erased(cell.style));
                } else {
                    // If this is a wide leading cell at the right edge and its
                    // spacer would fall outside the viewport, truncate to single
                    // width to avoid unpaired wide.
                    let mut out = cell;
                    if out.width == 2 && c + 1 >= req_w && !out.spacer {
                        out.width = 1;
                    }
                    cells.push(out);
                }
            } else {
                cells.push(bitty_term_state::Cell::erased(
                    bitty_term_state::Style::default(),
                ));
            }
        }
    }
    Snapshot {
        version: snapshot.version,
        generation: snapshot.generation,
        width: req_w,
        height: req_h,
        cells: cells.into_boxed_slice(),
        cursor: snapshot.cursor.clone(),
        modes: snapshot.modes.clone(),
        title: snapshot.title.clone(),
    }
}

impl Runtime {
    /// Returns the in-memory RGBA buffer of the last presented headless frame,
    /// if any (premultiplied, `width*height*4` bytes, row-major RGBA).
    #[must_use]
    pub fn headless_rgba(&self) -> Option<Vec<u8>> {
        let raw = self.surface.headless_rgba()?;
        Some(raw)
    }

    /// Plans, records, and presents one frame when damage exists, returning
    /// [`PresentStats`] on a presented frame and `None` when idle.
    ///
    /// Frame-on-demand: zero damage (including pure scrollback damage that
    /// adds no pixels on this viewport) presents nothing and burns no CPU
    /// beyond the damage check — the idle resource budget (PB-7, ≤ 1% CPU)
    /// depends on this property. The first frame after creation or resize
    /// forces a full redraw.
    ///
    /// Multi-pane: the layout tree is reflowed into the current container;
    /// each leaf `View`'s `cols`/`rows`/`origin` are updated via
    /// `LayoutNode::reflow`. Then each leaf is rendered: a viewport snapshot
    /// sized to the leaf's dimensions is built from the shared `State`
    /// snapshot (headless seam, no GPU/window required), rendered through the
    /// shared `GridRenderer` with a full-damage hint, and its `DrawList`
    /// translated to the leaf's pixel origin. The per-leaf `DrawList`s are
    /// combined and presented once via `Surface::headless_present`.
    ///
    /// The software seam composites `DrawList + Atlas` onto an owned RGBA
    /// buffer via `Surface::headless_present`; no display server or adapter
    /// is touched. Real GPU present remains env-gated (`BITTY_RENDER_GPU_TESTS=1`)
    /// and is not available on headless CI — `is_headless` is `true` for
    /// every present this method emits today.
    pub fn tick(&mut self) -> Option<PresentStats> {
        self.tick_at(std::time::Instant::now())
    }

    /// Tick with an explicit wall clock (CTX-0192 virtual-clock seam).
    ///
    /// `tick()` delegates with `Instant::now()`; tests pass virtual times to
    /// prove the transient banner: full summary → flash after
    /// [`PASTE_BANNER_FULL_DURATION`] → still pending (never-silent) → gone
    /// on confirm/cancel. Behavior is otherwise identical to `tick()`.
    pub fn tick_at(&mut self, now: std::time::Instant) -> Option<PresentStats> {
        // CTX-0192 transient: collapse the full banner to the flash once its
        // duration expires. Force exactly one repaint for the transition so
        // the retained frame keeps a visible (smaller) signal while pending.
        if self.pending_paste.is_some() {
            match self.pending_paste_since {
                Some(since) => {
                    let collapsed =
                        now.saturating_duration_since(since) >= PASTE_BANNER_FULL_DURATION;
                    if collapsed != self.paste_banner_collapsed {
                        self.paste_banner_collapsed = collapsed;
                        self.pending_full_redraw = true;
                    }
                }
                None => {
                    self.pending_paste_since = Some(now);
                    self.paste_banner_collapsed = false;
                    self.pending_full_redraw = true;
                }
            }
        }
        // Reflow layout tree into container before rendering so leaf Views
        // carry deterministic origins/sizes for this frame. This is headless
        // and deterministic: same layout + container always yields same
        // allocations. CTX-0177: gap-aware so leaves exclude gap bands.
        self.layout.reflow_with_gaps(self.container, self.gaps());

        let snapshot = self.state.snapshot();
        let mut pending_full = self.pending_full_redraw;
        let last = self.last_presented_generation;
        // CTX-0176: the presented generation tracks the newest grid across
        // the primary state and every pane session, so frame-on-demand idles
        // only when all shells are quiet.
        let mut current_gen = snapshot.generation;
        for sess in self.pane_sessions.values() {
            current_gen = current_gen.max(sess.state.generation());
        }

        // Collect allocations deterministically BEFORE the idle check so
        // geometry-only changes are visible (CTX-0228). `layout_with_gaps`
        // is pure and bounded by the leaf count.
        // CTX-0177: gap-aware so per-leaf origins skip the gap bands.
        let allocations = self.layout.layout_with_gaps(self.container, self.gaps());
        let focused = self.focus.focused();
        // CTX-0228: a layout or focus change forces a full present even
        // when no PTY bytes advanced the generation. This covers tree edits
        // through `layout_mut`/`focus_mut` borrows and any future
        // geometry-only path that misses an explicit dirty flag
        // (over-damage is safe; under-damage leaves a stale frame until
        // the next PTY output, which is the reported bug).
        if allocations != self.last_presented_allocations || focused != self.last_presented_focus {
            pending_full = true;
        }

        // Frame-on-demand: no new generation and no forced redraw -> idle.
        if !pending_full && current_gen == last && last != u64::MAX {
            return None;
        }

        // Empty layout -> idle (no leaf to present).
        if allocations.is_empty() {
            self.last_presented_generation = current_gen;
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            self.pending_full_redraw = false;
            return None;
        }

        // Build the combined DrawList by rendering each leaf's viewport.
        let mut combined_fills = Vec::new();
        let mut combined_glyphs = Vec::new();
        let mut any_needs_draw = false;

        // For damage, we treat any new generation or pending_full as full
        // per leaf (over-damage safe, deterministic). If generation gap is
        // large and regions empty, also full. Otherwise still full for
        // correctness with viewport slicing.
        // CTX-0176: per-pane grids carry independent generations that the
        // shared damage ring cannot see, so any live pane session forces the
        // full per-leaf path (over-damage safe, deterministic).
        let use_full = !self.pane_sessions.is_empty()
            || pending_full
            || last == u64::MAX
            || {
                let gap = current_gen.saturating_sub(last);
                let regions = self.state.damage_since(last);
                gap > bitty_term_state::damage::DAMAGE_HISTORY_BATCHES as u64 && regions.is_empty()
            }
            || {
                let regions = self.state.damage_since(last);
                !regions.is_empty() || pending_full
            };

        // For single-window slice we need per-view scrollback viewport and cursor.
        // Build id->View map for scroll/IME lookups.
        let view_map: std::collections::HashMap<ViewId, View> = {
            let mut m = std::collections::HashMap::new();
            for (vid, r) in &allocations {
                if let Some(v) = self.layout.find_leaf(*vid) {
                    m.insert(*vid, v.clone());
                } else {
                    // Fallback: synthesized view sized to allocation
                    let mut v = View::new(*vid, r.width as usize, r.height as usize);
                    v.set_origin(bitty_ui::Point::new(r.x, r.y));
                    m.insert(*vid, v);
                }
            }
            m
        };

        // CTX-0223: window padding translates every content origin below
        // (leaf grids, selection, IME, banner) by the inset; the padding
        // band itself keeps the surface clear color. Physical pixels at the
        // live scale so HiDPI placement matches grid derivation.
        let pad_px = self.window_padding_physical() as i32;

        for (view_id, rect) in &allocations {
            if rect.is_empty() {
                continue;
            }
            // CTX-0176: a leaf with its own shell renders that session's
            // grid; leaves without a session share the primary snapshot
            // (the unchanged single-pane path while no session exists).
            let pane_snap: Option<Snapshot> = if self.pane_sessions.is_empty() {
                None
            } else {
                Some(match self.pane_sessions.get(view_id) {
                    Some(sess) => sess.state.snapshot(),
                    None => snapshot.clone(),
                })
            };
            let base_snap: &Snapshot = pane_snap.as_ref().unwrap_or(&snapshot);
            // Determine viewport snapshot: when view scroll_offset !=0, visible_cells composites scrollback.
            let view = view_map.get(view_id);
            let view_snapshot = if let Some(v) = view {
                if v.scroll_offset() != 0 {
                    let cells = match self.pane_sessions.get(view_id) {
                        Some(sess) => v.visible_cells(&sess.state),
                        None => v.visible_cells(&self.state),
                    };
                    Snapshot {
                        version: base_snap.version,
                        generation: base_snap.generation,
                        width: v.cols() as usize,
                        height: v.rows() as usize,
                        cells,
                        cursor: base_snap.cursor.clone(),
                        modes: base_snap.modes.clone(),
                        title: base_snap.title.clone(),
                    }
                } else {
                    viewport_snapshot(base_snap, rect.width, rect.height)
                }
            } else {
                viewport_snapshot(base_snap, rect.width, rect.height)
            };

            let damage = if use_full || pending_full || last == u64::MAX {
                Damage {
                    generation: current_gen,
                    regions: vec![DamagedRegion::Grid(DamageRect::full(
                        view_snapshot.height as u16,
                        view_snapshot.width as u16,
                    ))]
                    .into_boxed_slice(),
                }
            } else {
                // Incremental path: clip damage_since to viewport bounds.
                // For this slice we still produce a full per-leaf damage when
                // any damage exists (over-damage safe), keeping determinism.
                let regions = self.state.damage_since(last);
                if regions.is_empty() {
                    Damage {
                        generation: current_gen,
                        regions: Box::new([]),
                    }
                } else {
                    Damage {
                        generation: current_gen,
                        regions: vec![DamagedRegion::Grid(DamageRect::full(
                            view_snapshot.height as u16,
                            view_snapshot.width as u16,
                        ))]
                        .into_boxed_slice(),
                    }
                }
            };

            if damage.regions.is_empty() {
                continue;
            }

            let list = match self.renderer.render(&view_snapshot, &damage) {
                Ok(list) => list,
                Err(_) => continue,
            };
            // Cursor rendering: add a cursor fill when visible, focused, and live.
            // Single-window slice: cursor is presentation overlay, not terminal truth mutation.
            let mut list = list;
            if view_snapshot.cursor.visible
                && self.focused
                && view.map(|v| v.scroll_offset() == 0).unwrap_or(true)
            {
                // Only draw cursor when this view is the focused view (or single leaf default)
                let is_focused_view = self
                    .focused_view()
                    .map(|fid| fid == *view_id)
                    .unwrap_or(true);
                if is_focused_view {
                    let cur = &view_snapshot.cursor.position;
                    if (cur.row as usize) < view_snapshot.height
                        && (cur.col as usize) < view_snapshot.width
                    {
                        // Check not on spacer
                        let idx = cur.row as usize * view_snapshot.width + cur.col as usize;
                        let is_spacer = view_snapshot
                            .cells
                            .get(idx)
                            .map(|c| c.spacer)
                            .unwrap_or(false);
                        if !is_spacer {
                            let live = self.live_cell_metrics();
                            // DECSCUSR shape comes from the shared render primitive
                            // (`bitty_render::grid::cursor_fill`: block = full cell,
                            // bar = left strip, underline = bottom strip, 15% thickness
                            // per DEC-0017 ghostty/alacritty refs). Geometry is shared;
                            // the overlay hue is the designed theme cursor
                            // (`crate::palette::theme_cursor_rgba`, Bitty Dark
                            // rosewater) so the live cursor matches the palette
                            // out of the box (CTX-0219: no hardcoded white).
                            if let Some(fill) = bitty_render::grid::cursor_fill(
                                &view_snapshot.cursor,
                                live,
                                view_snapshot.width,
                                view_snapshot.height,
                            ) {
                                // Cursor color: the theme cursor hue at the existing
                                // translucent alpha (blinks stay with the
                                // embedder's visibility/focus gate, already
                                // checked above).
                                let cursor_color: bitty_render::grid::Rgba8 =
                                    if view_snapshot.cursor.visible {
                                        let mut themed = crate::palette::theme_cursor_rgba();
                                        themed[3] = 0xA0;
                                        themed
                                    } else {
                                        [0, 0, 0, 0]
                                    };
                                list.fills.push(bitty_render::grid::FillRect {
                                    rect: fill.rect,
                                    color: cursor_color,
                                });
                            }
                        }
                    }
                }
            }

            if !list.needs_draw() {
                continue;
            }
            any_needs_draw = true;

            let live = self.live_cell_metrics();
            let origin_px_x = rect.x as i32 * live.width as i32 + pad_px;
            let origin_px_y = rect.y as i32 * live.height as i32 + pad_px;
            for mut fill in list.fills {
                fill.rect.x += origin_px_x;
                fill.rect.y += origin_px_y;
                combined_fills.push(fill);
            }
            for mut glyph in list.glyphs {
                glyph.dest[0] += origin_px_x;
                glyph.dest[1] += origin_px_y;
                combined_glyphs.push(glyph);
            }
        }

        // Selection highlight overlay (CTX-0158, ghostty selection rendering):
        // presentation-only fills in the theme selection color, painted above
        // cell backgrounds. `DrawList` paint order is fills first, then
        // glyphs, so the highlight tints the background while text stays
        // legible on top. Bounded: at most one rect per selected row.
        // Skipped while the focused view is scrolled into history (the live
        // grid selection does not map to the scrollback viewport).
        if let Some(sel) = self.selection {
            if !sel.is_empty() {
                let norm = sel.normalized();
                let scrolled = self
                    .focused_view()
                    .and_then(|fid| view_map.get(&fid))
                    .map(|v| v.scroll_offset() != 0)
                    .unwrap_or(false);
                if !scrolled {
                    let live = self.live_cell_metrics();
                    let fid = self.focused_view().or(view_map.keys().next().copied());
                    if let Some(focused_id) = fid {
                        if let Some((_, rect)) =
                            allocations.iter().find(|(id, _)| *id == focused_id)
                        {
                            let rects = bitty_render::grid::selection_fill_rects(
                                (norm.start.row, norm.start.col),
                                (norm.end.row, norm.end.col),
                                snapshot.width,
                                snapshot.height,
                                live,
                            );
                            if !rects.is_empty() {
                                let origin_px_x = rect.x as i32 * live.width as i32 + pad_px;
                                let origin_px_y = rect.y as i32 * live.height as i32 + pad_px;
                                for mut fill in rects {
                                    fill.rect.x += origin_px_x;
                                    fill.rect.y += origin_px_y;
                                    combined_fills.push(fill);
                                }
                                any_needs_draw = true;
                            }
                        }
                    }
                }
            }
        }

        // IME preedit overlay: presentation-only, not state mutation. Paints atop cursor.
        if let Some(preedit) = self.ime_preedit.clone() {
            if !preedit.is_empty() && self.focused {
                // Determine focused view allocation origin and cursor pixel position.
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some((vid, rect)) = allocations.iter().find(|(id, _)| *id == fid) {
                        let live = self.live_cell_metrics();
                        // CTX-0176: the preedit overlay tracks the focused
                        // leaf's cursor, so IME lands on the pane receiving
                        // input (primary grid when focus owns no session).
                        let cur = self
                            .focused_view()
                            .and_then(|focused| self.pane_sessions.get(&focused))
                            .map(|sess| sess.state.snapshot().cursor.position)
                            .unwrap_or(snapshot.cursor.position);
                        let origin_px_x = rect.x as i32 * live.width as i32 + pad_px;
                        let origin_px_y = rect.y as i32 * live.height as i32 + pad_px;
                        let base_x = origin_px_x + cur.col as i32 * live.width as i32;
                        let base_y = origin_px_y + cur.row as i32 * live.height as i32;
                        // Simple IME overlay: underline background rect plus glyphs for preedit chars.
                        // For slice, render preedit as single underline fill plus per-char glyphs via renderer? Simplified: add a fill rect for underline.
                        let preedit_width = (preedit.chars().count() as u32 * live.width).min(1024);
                        let underline_rect = bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y + live.height as i32 - 2,
                            preedit_width,
                            2,
                        );
                        combined_fills.push(bitty_render::grid::FillRect {
                            rect: underline_rect,
                            color: [0xFF, 0xFF, 0x00, 0xFF],
                        });
                        // Also push a background fill for preedit area (semi-transparent)
                        let bg_rect = bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y,
                            preedit_width,
                            live.height,
                        );
                        combined_fills.push(bitty_render::grid::FillRect {
                            rect: bg_rect,
                            color: [0x33, 0x33, 0x33, 0xCC],
                        });
                        any_needs_draw = true;
                        let _ = vid; // keep
                    }
                }
            }
        }

        // Pending-paste confirmation banner (CTX-0186, transient CTX-0192):
        // presentation-only overlay on the focused view's bottom row,
        // right-aligned compact pill (not full-width) to avoid occluding the
        // grid. Gated on `has_pending_paste()`; text is the bounded compact
        // `pending_paste_summary()` for `PASTE_BANNER_FULL_DURATION`, then the
        // minimal `PASTE_BANNER_FLASH_TEXT` while pending (never-silent).
        // Overlay only: pushes fills+glyphs onto the combined frame, never
        // touches grid cells, scrollback, or the pending bytes. Esc-cancel
        // and repeat-confirm paths are unchanged; clearing pending repaints
        // once without the banner via `pending_full_redraw`.
        if self.has_pending_paste() {
            if let Some(banner) = self.paste_banner_text_at(now) {
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some((_, rect)) = allocations.iter().find(|(id, _)| *id == fid) {
                        if rect.height > 0 && rect.width > 0 {
                            let live = self.live_cell_metrics();
                            let max_cells = rect.width as usize;
                            // Compact pill: only as wide as the text (clipped
                            // to the view), right-aligned so most of the row
                            // stays visible.
                            let text_cells = banner.chars().count().min(max_cells).max(1);
                            let pill_w = text_cells as u32 * live.width;
                            let full_w = rect.width as u32 * live.width;
                            let origin_px_x = rect.x as i32 * live.width as i32
                                + (full_w.saturating_sub(pill_w)) as i32
                                + pad_px;
                            let banner_y = (rect.y as i32 + rect.height as i32 - 1)
                                * (live.height as i32)
                                + pad_px;
                            combined_fills.push(bitty_render::grid::FillRect {
                                rect: bitty_render::geometry::RectPx::new(
                                    origin_px_x,
                                    banner_y,
                                    pill_w,
                                    live.height,
                                ),
                                color: bitty_render::grid::PENDING_PASTE_BANNER_BG,
                            });
                            let glyphs = self.renderer.overlay_text_glyphs(
                                &banner,
                                (origin_px_x, banner_y),
                                max_cells,
                                bitty_render::grid::PENDING_PASTE_BANNER_FG,
                            );
                            combined_glyphs.extend(glyphs);
                            any_needs_draw = true;
                        }
                    }
                }
            }
        }

        // Overlay scrollbar thumb (CTX-0181): a presentation-only FillRect
        // on the focused leaf's right edge, painted above grid content like
        // the selection highlight. Never grid truth: no layout, container,
        // or cell mutation, and `hidden` (default) resolves to no fill.
        // Visibility is latched so `auto` hover/proximity transitions stay
        // headless-observable without screenshots.
        let scrollbar_now = self.scrollbar_thumb_fill();
        let paints = scrollbar_now.is_some();
        if let Some(fill) = scrollbar_now {
            combined_fills.push(fill);
            any_needs_draw = true;
        }
        self.scrollbar_visible = paints;

        self.pending_full_redraw = false;

        if !any_needs_draw && combined_fills.is_empty() && combined_glyphs.is_empty() {
            // Check if we had pending_full but produced no draws (e.g., all zero rects) -> still idle
            // But ensure generation advances for idle detection.
            self.last_presented_generation = current_gen;
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            return None;
        }

        // Synthesize a DrawList for the combined frame. Plan is not used by
        // headless_present beyond fill/glyph counts, so we create a minimal
        // plan that reports needs_draw == true when we have content.
        let combined_list = {
            // We need a FramePlan; construct via a dummy damage descriptor that
            // indicates full. Simplest: reuse empty plan but set dirty_rects
            // to surface extent so needs_draw is true. Instead we construct a
            // DrawList manually with a plan that has needs_draw true.
            // The easiest is to create a FramePlan::default-like but we don't
            // have that. So we synthesize via render's FramePlan by creating
            // a dummy DrawList from first leaf and replacing its fills/glyphs.
            // Instead, construct a DrawList with a plan that has one dirty rect.
            // Look at FramePlan structure: we can get it from rendering a full
            // snapshot once and reusing its plan.
            // For minimal, we will create a plan via the renderer's internal
            // but we can just create an empty plan with needs_draw = true by
            // using a helper: we know DrawList::needs_draw checks fills/glyphs,
            // not plan alone. So plan can be empty as headless_present doesn't
            // check plan.
            // Let's create a dummy plan via unsafe uninitialized? Better to just
            // reuse a plan from first allocation's render if available.
            // Instead, we will construct a DrawList with a plan that we know
            // will have dirty_rects covering the surface. We can synthesize by
            // directly constructing FramePlan via its public fields if any.
            // Check FramePlan fields - read its definition.
            // As a shortcut, we will create a DrawList with plan from a full
            // render of the viewport_snapshot for the container size.
            // But simpler: we can just create a DrawList with plan that has
            // needs_draw true by fabricating via bitty_render's test helper?
            // Most straightforward: create a DrawList with an empty plan that
            // we override to have a dirty rect, but we don't have constructor.
            // Workaround: create a DrawList via renderer.render of a 1x1 snapshot
            // and then replace its fills/glyphs.
            let tmp_snap = viewport_snapshot(&snapshot, 1, 1);
            let tmp_damage = Damage {
                generation: current_gen,
                regions: vec![DamagedRegion::Grid(DamageRect::full(1, 1))].into_boxed_slice(),
            };
            let mut tmp_list = self
                .renderer
                .render(&tmp_snap, &tmp_damage)
                .unwrap_or(DrawList {
                    generation: current_gen,
                    plan: FramePlan {
                        dirty_rects: Vec::new(),
                        extent: bitty_render::geometry::ExtentPx::new(0, 0),
                        mode: FrameMode::Clean,
                    },
                    fills: Vec::new(),
                    glyphs: Vec::new(),
                });
            // Now replace fills/glyphs with combined, and reset the plan to
            // describe the combined pixel space: the 1x1 probe's extent must
            // not survive (see present_plan_extent), and the dirty rect must
            // cover the combined frame when content exists.
            tmp_list.generation = current_gen;
            tmp_list.fills = combined_fills;
            tmp_list.glyphs = combined_glyphs;
            let plan_extent = self.present_plan_extent();
            tmp_list.plan.extent = plan_extent;
            if tmp_list.fills.is_empty() && tmp_list.glyphs.is_empty() {
                tmp_list.plan.dirty_rects = Vec::new();
            } else {
                tmp_list.plan.dirty_rects = vec![bitty_render::geometry::RectPx::new(
                    0,
                    0,
                    plan_extent.width,
                    plan_extent.height,
                )];
                tmp_list.plan.mode = FrameMode::Full;
            }
            tmp_list
        };

        if !combined_list.needs_draw() {
            self.last_presented_generation = current_gen;
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            return None;
        }

        let atlas_texels = self.renderer.atlas_texels().to_vec();
        let dims = self.renderer.atlas_dims();
        let stats = if let Some(gpu) = self.gpu.as_ref() {
            match self
                .surface
                .present_draw_list(gpu, &combined_list, Some((&atlas_texels, dims)))
            {
                Ok(s) => s,
                Err(_) => {
                    // GPU present failed (surface lost/outdated): fallback to headless for this frame
                    match self
                        .surface
                        .headless_present(&combined_list, Some((&atlas_texels, dims)))
                    {
                        Ok(h) => h,
                        Err(_) => return None,
                    }
                }
            }
        } else {
            match self
                .surface
                .headless_present(&combined_list, Some((&atlas_texels, dims)))
            {
                Ok(stats) => stats,
                Err(_) => return None,
            }
        };
        self.last_presented_generation = current_gen;
        self.last_presented_allocations = allocations;
        self.last_presented_focus = focused;
        // CTX-0159: publish grid plus latched input/focus so socket probes see
        // typed text without screenshots (`&self` only, bounded).
        self.publish_inspect_snapshot();
        Some(PresentStats {
            frame: stats.frame,
            fills: stats.fills,
            glyphs: stats.glyphs,
            headless: stats.headless,
            generation: current_gen,
        })
    }
}
