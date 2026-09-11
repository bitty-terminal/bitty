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
    /// Number of rounded fill/ring primitives in the presented draw list
    /// (CTX-0311).
    pub rounded_fills: usize,
    /// Number of glyph instances in the presented draw list.
    pub glyphs: usize,
    /// Whether the surface was the headless software fake.
    pub headless: bool,
    /// Snapshot generation that was presented.
    pub generation: u64,
    /// Number of Kitty image blits in the presented draw list.
    pub images: usize,
    /// Image blits the presenting path did not paint.
    ///
    /// Always `0` on the headless seam (every blit is blended). On a real
    /// surface the GPU pass uploads and paints every blit (CTX-0291);
    /// non-zero means individual blits were refused fail-closed
    /// (malformed, oversized, or over a per-frame bound).
    pub images_skipped: usize,
}

impl From<RenderPresentStats> for PresentStats {
    fn from(value: RenderPresentStats) -> Self {
        Self {
            frame: value.frame,
            fills: value.fills,
            rounded_fills: value.rounded_fills,
            glyphs: value.glyphs,
            headless: value.headless,
            generation: 0,
            images: value.images,
            images_skipped: value.images_skipped,
        }
    }
}

// CTX-0253 F4: pixel-origin math in `i64`/`u64` with saturation.
//
// Compositors clip in `i64`; mirror that here. The old
// `rect.x as i32 * live.width as i32 + pad_px` chains could wrap: `live`
// cell metrics come from local config (extreme values are hostile input),
// so a `u32` cell side above `i32::MAX` truncated on the `as i32` cast and
// the `i32` multiply/add then wrapped (debug panic / release wrap).
// Every helper below is total: products accumulate in `u64`, sums in
// `i64`, results clamp to the `i32`/`u32` ranges.

/// Saturating `i32` add for translated rect/glyph destinations.
pub(super) fn px_add(a: i32, b: i32) -> i32 {
    a.saturating_add(b)
}

/// Cell-span width in pixels (`cells * cell_px`), saturated to `u32`.
pub(super) fn px_span(cells: u16, cell_px: u32) -> u32 {
    u32::try_from(u64::from(cells).saturating_mul(u64::from(cell_px))).unwrap_or(u32::MAX)
}

/// `usize` cell-span width in pixels (banner pills), saturated to `u32`.
fn px_span_usize(cells: usize, cell_px: u32) -> u32 {
    u32::try_from((cells as u64).saturating_mul(u64::from(cell_px))).unwrap_or(u32::MAX)
}

/// Offset from a pixel base by a cell count (`base + cells * cell_px`).
pub(super) fn px_offset_cells(base: i32, cells: u16, cell_px: u32) -> i32 {
    let delta = u64::from(cells).saturating_mul(u64::from(cell_px));
    base.saturating_add(i32::try_from(delta).unwrap_or(i32::MAX))
}

/// Saturating `u32` -> `i32` for pixel sides that feed `i32` geometry.
pub(super) fn px_side(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Zero-size erased snapshot (CTX-0234): [`viewport_snapshot`] pads it to the
/// leaf allocation with erased cells. Cursor/modes/title ride along so the
/// overlay gates (cursor paints on the focused view only) stay total.
fn erased_snapshot(base: &Snapshot) -> Snapshot {
    Snapshot {
        version: base.version,
        generation: base.generation,
        width: 0,
        height: 0,
        cells: Vec::new().into_boxed_slice(),
        cursor: base.cursor.clone(),
        modes: base.modes.clone(),
        title: base.title.clone(),
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
    /// sized to the leaf's dimensions is built from its own pane-session
    /// grid when it owns a shell, from the shared `State` snapshot when it
    /// is the focused session-less leaf (input routes there), and erased
    /// otherwise (CTX-0234: never duplicate one grid across tiles),
    /// rendered through the shared `GridRenderer` with a full-damage hint,
    /// and its `DrawList` translated to the leaf's pixel origin. The
    /// per-leaf `DrawList`s are combined and presented once via
    /// `Surface::headless_present`.
    ///
    /// The software seam composites `DrawList + Atlas` onto an owned RGBA
    /// buffer via `Surface::headless_present`; no display server or adapter
    /// is touched. Real GPU present remains env-gated (`BITTY_RENDER_GPU_TESTS=1`)
    /// and is not available on headless CI — `is_headless` is `true` for
    /// every present this method emits today.
    pub fn tick(&mut self) -> Option<PresentStats> {
        self.tick_at(std::time::Instant::now())
    }

    /// Records the per-origin generations consumed by this frame so the next
    /// `tick_at` can tell whether any primary or pane grid advanced
    /// (CTX-0289). `primary_gen` is the primary state's generation; every
    /// pane session contributes its own counter. Called on every present,
    /// including the idle write-backs that consume a generation without
    /// drawing pixels.
    fn mark_frame_presented(&mut self, primary_gen: u64) {
        self.last_presented_generation = primary_gen;
        self.last_presented_pane_generations.clear();
        for (id, sess) in &self.pane_sessions {
            self.last_presented_pane_generations
                .insert(*id, sess.state.generation());
        }
    }

    /// Tick with an explicit wall clock (CTX-0192 virtual-clock seam).
    ///
    /// `tick()` delegates with `Instant::now()`; tests pass virtual times to
    /// prove the transient banner: full summary → flash after
    /// [`PASTE_BANNER_FULL_DURATION`] → still pending (never-silent) → gone
    /// on confirm/cancel. Behavior is otherwise identical to `tick()`.
    pub fn tick_at(&mut self, now: std::time::Instant) -> Option<PresentStats> {
        // CTX-0334: commit a pending hover activation whose dwell deadline
        // has elapsed before the frame's focus/highlight is resolved. The
        // app arms a timed wake at `hover_activation_deadline`, so this
        // fires even when the pointer stopped moving.
        self.apply_hover_deadline(now);
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
        // frames. CTX-0294: frames come from the Core-owned px decoration
        // solver composed with the CTX-0177 cell gaps, so live present paints
        // the accepted gaps/border/radius instead of cell-aligned tiling.
        // The reflow mutates leaf Views to the decorated *content* grid so
        // scroll/selection/PTY geometry matches the painted viewport.
        let frames = self.present_frames();
        self.reflow_present_layout(&frames);

        let snapshot = self.state.snapshot();
        let mut pending_full = self.pending_full_redraw;
        // Collect allocations deterministically BEFORE the idle check so
        // geometry-only changes are visible (CTX-0228). `present_frames`
        // is pure and bounded by the leaf count.
        let allocations = frames;
        let focused = self.focus.focused();
        // CTX-0254: Kitty origin binding. The image layer is keyed by the
        // emitting PTY stream — `None` for the primary grid, `Some(id)`
        // for the split-pane session owning the focused leaf — so a
        // background pane can never paint over the focused pane
        // (cross-pane spoof prevention). Alt-screen and scrollback resolve
        // against the origin's own grid, never a global one; the latch
        // below therefore tracks the focused origin's alt state.
        let kitty_origin: Option<u64> = focused.and_then(|fid| {
            if self.pane_sessions.contains_key(&fid) {
                Some(fid.0)
            } else {
                None
            }
        });
        let (kitty_origin_alt, kitty_origin_scrollback) = match kitty_origin {
            Some(token) => match self.pane_sessions.get(&ViewId::new(token)) {
                Some(sess) => (sess.state.alt_screen_active(), sess.state.scrollback_len()),
                // Unreachable single-threaded (checked above); fail closed
                // to "alt active" so nothing paints against a wrong grid.
                None => (true, self.state.scrollback_len()),
            },
            None => (self.state.alt_screen_active(), self.state.scrollback_len()),
        };
        // CTX-0248: an alternate-screen transition on the focused origin
        // forces a full present even when the grid generation is unchanged,
        // so entering alt clears that origin's painted images (and leaving
        // alt repaints the restored grid) instead of idling on a stale
        // frame. Other origins are untouched (CTX-0254 `clear_origin`).
        if kitty_origin_alt != self.kitty_alt_screen_latched {
            self.kitty_alt_screen_latched = kitty_origin_alt;
            pending_full = true;
        }
        let last = self.last_presented_generation;
        // CTX-0176/CTX-0289: the primary state and every pane session own
        // independent grid generation counters, so a single scalar `max`
        // cannot detect that a lower-generation origin changed while a
        // higher-generation origin stayed quiet. `current_gen` stays the max
        // for present stats/damage, but frame-on-demand compares each origin
        // against its own last presented generation.
        let mut current_gen = snapshot.generation;
        let mut origins_changed = snapshot.generation != last;
        for (id, sess) in &self.pane_sessions {
            let pane_gen = sess.state.generation();
            current_gen = current_gen.max(pane_gen);
            if self.last_presented_pane_generations.get(id).copied() != Some(pane_gen) {
                origins_changed = true;
            }
        }
        // A pane added or removed since the last present is a change too
        // (a closed pane's `pending_full_redraw` already covers its pixels;
        // this keeps the origin bookkeeping exact).
        if self.last_presented_pane_generations.len() != self.pane_sessions.len() {
            origins_changed = true;
        }

        // CTX-0228: a layout or focus change forces a full present even
        // when no PTY bytes advanced the generation. This covers tree edits
        // through `layout_mut`/`focus_mut` borrows and any future
        // geometry-only path that misses an explicit dirty flag
        // (over-damage is safe; under-damage leaves a stale frame until
        // the next PTY output, which is the reported bug).
        if allocations != self.last_presented_allocations || focused != self.last_presented_focus {
            pending_full = true;
        }

        // Frame-on-demand: no origin advanced and no forced redraw -> idle.
        // `last == u64::MAX` marks the first frame (always present).
        if !pending_full && !origins_changed && last != u64::MAX {
            return None;
        }

        // Empty layout -> idle (no leaf to present).
        if allocations.is_empty() {
            self.mark_frame_presented(snapshot.generation);
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            self.pending_full_redraw = false;
            return None;
        }

        // Build the combined DrawList by rendering each leaf's viewport.
        let mut combined_fills = Vec::new();
        let mut combined_rounded: Vec<bitty_render::grid::RoundedFill> = Vec::new();
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
            for frame in &allocations {
                if let Some(v) = self.layout.find_leaf(frame.view) {
                    m.insert(frame.view, v.clone());
                } else {
                    // Fallback: synthesized view sized to the decorated
                    // content frame.
                    let live = self.live_cell_metrics();
                    let mut v = View::new(frame.view, frame.cols as usize, frame.rows as usize);
                    v.set_origin(bitty_ui::Point::new(
                        (frame.content.x.max(0) as u32 / live.width.max(1)).min(u32::from(u16::MAX))
                            as u16,
                        (frame.content.y.max(0) as u32 / live.height.max(1))
                            .min(u32::from(u16::MAX)) as u16,
                    ));
                    m.insert(frame.view, v);
                }
            }
            m
        };

        // CTX-0223: window padding translates every content origin below
        // (leaf grids, selection, IME, banner) by the inset; the padding
        // band itself keeps the surface clear color. Physical pixels at the
        // live scale so HiDPI placement matches grid derivation.
        // CTX-0253 F4: saturating conversion — the physical inset is
        // `u32` and must never wrap into a negative `i32` origin.
        let pad_px = i32::try_from(self.window_padding_physical()).unwrap_or(i32::MAX);

        for frame in &allocations {
            if frame.content.width == 0 || frame.content.height == 0 {
                continue;
            }
            let view_id = &frame.view;
            // CTX-0176: a leaf with its own shell renders that session's
            // grid. CTX-0234: a leaf WITHOUT a session renders the shared
            // primary snapshot ONLY while focused — multipane input routing
            // (`push_input_bytes_multipane`) sends typing to the primary
            // shell exactly through the focused session-less leaf, so the
            // fallback is what-you-see-is-what-you-type there. Every other
            // session-less leaf (ctl splits spawn no shell; spawn failures)
            // presents erased: cloning primary into all of them duplicates
            // one shell across N tiles (live three-column repeat + marker
            // in an unexpected tile after zoom-off). The single-pane path
            // is unchanged (the sole leaf is focused, so it keeps primary).
            // CTX-0255: mixed shape (session-less primary home plus live
            // pane sessions — the keymap-split live path) must co-paint:
            // every session-less leaf keeps the shared primary grid even
            // while unfocused, otherwise focusing the pane blanks the
            // primary home tile (live 02-focus-v2 left blank, 03-refocus-v1
            // both repaint). Pure session-less shape (no session anywhere)
            // keeps the CTX-0234 focused-only fallback so one grid never
            // duplicates across N tiles.
            let focused_id = self.focus.focused();
            let pane_snap: Option<Snapshot> = match self.pane_sessions.get(view_id) {
                Some(sess) => Some(sess.state.snapshot()),
                None if Some(*view_id) == focused_id => Some(snapshot.clone()),
                None if !self.pane_sessions.is_empty() => Some(snapshot.clone()),
                None => None,
            };
            // Erased source for session-less, unfocused leaves;
            // `viewport_snapshot` pads it to the allocation below.
            let erased_snap: Option<Snapshot> = if pane_snap.is_none() {
                Some(erased_snapshot(&snapshot))
            } else {
                None
            };
            let base_snap: &Snapshot = pane_snap
                .as_ref()
                .or(erased_snap.as_ref())
                .unwrap_or(&snapshot);
            // Determine viewport snapshot: when view scroll_offset !=0, visible_cells composites scrollback.
            let view = view_map.get(view_id);
            let view_snapshot = if let Some(v) = view {
                if v.scroll_offset() != 0 && pane_snap.is_some() {
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
                    viewport_snapshot(base_snap, frame.cols, frame.rows)
                }
            } else {
                viewport_snapshot(base_snap, frame.cols, frame.rows)
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

            // CTX-0311: Core-owned decoration ring as one rounded SDF
            // primitive (border == 0 paints nothing; a solid rounded fill
            // would cover the content). It paints after every plain fill —
            // including the cell backgrounds above — and before glyphs, so
            // the ring covers the corner cell backgrounds. `radius` clips
            // content: the derived inner clip is attached to this leaf's
            // glyphs below, so text never overdraws the inner corner curve.
            // gaps_out/gaps_in bands are the unallocated frame space and
            // keep the surface clear color, exactly like the CTX-0177 gap
            // bands.
            let mut frame_clip = None;
            if frame.frame.width > 0 && frame.frame.height > 0 {
                let ring_frame = bitty_render::geometry::RectPx::new(
                    px_add(pad_px, frame.frame.x),
                    px_add(pad_px, frame.frame.y),
                    frame.frame.width,
                    frame.frame.height,
                );
                if frame.border > 0 {
                    combined_rounded.push(bitty_render::grid::RoundedFill {
                        frame: ring_frame,
                        border: frame.border,
                        radius: frame.radius,
                        color: bitty_render::grid::DECORATION_BORDER,
                    });
                    any_needs_draw = true;
                }
                frame_clip =
                    bitty_render::grid::rounded_frame_clip(ring_frame, frame.border, frame.radius);
            }

            if !list.needs_draw() {
                continue;
            }
            any_needs_draw = true;

            let origin_px_x = px_add(pad_px, frame.content.x);
            let origin_px_y = px_add(pad_px, frame.content.y);
            for mut fill in list.fills {
                fill.rect.x = px_add(fill.rect.x, origin_px_x);
                fill.rect.y = px_add(fill.rect.y, origin_px_y);
                combined_fills.push(fill);
            }
            for mut glyph in list.glyphs {
                glyph.dest[0] = px_add(glyph.dest[0], origin_px_x);
                glyph.dest[1] = px_add(glyph.dest[1], origin_px_y);
                // CTX-0311 inner-arc clip: only decorated rounded frames set
                // it; square frames keep the documented overhang behavior.
                glyph.clip = frame_clip;
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
                        if let Some(frame) =
                            allocations.iter().find(|frame| frame.view == focused_id)
                        {
                            let rects = bitty_render::grid::selection_fill_rects(
                                (norm.start.row, norm.start.col),
                                (norm.end.row, norm.end.col),
                                snapshot.width,
                                snapshot.height,
                                live,
                            );
                            if !rects.is_empty() {
                                let origin_px_x = px_add(pad_px, frame.content.x);
                                let origin_px_y = px_add(pad_px, frame.content.y);
                                for mut fill in rects {
                                    fill.rect.x = px_add(fill.rect.x, origin_px_x);
                                    fill.rect.y = px_add(fill.rect.y, origin_px_y);
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
                    if let Some(frame) = allocations.iter().find(|frame| frame.view == fid) {
                        let live = self.live_cell_metrics();
                        // CTX-0176: the preedit overlay tracks the focused
                        // leaf's cursor, so IME lands on the pane receiving
                        // input (primary grid when focus owns no session).
                        let cur = self
                            .focused_view()
                            .and_then(|focused| self.pane_sessions.get(&focused))
                            .map(|sess| sess.state.snapshot().cursor.position)
                            .unwrap_or(snapshot.cursor.position);
                        let origin_px_x = px_add(pad_px, frame.content.x);
                        let origin_px_y = px_add(pad_px, frame.content.y);
                        let base_x = px_offset_cells(origin_px_x, cur.col, live.width);
                        let base_y = px_offset_cells(origin_px_y, cur.row, live.height);
                        // Simple IME overlay: underline background rect plus glyphs for preedit chars.
                        // For slice, render preedit as single underline fill plus per-char glyphs via renderer? Simplified: add a fill rect for underline.
                        // CTX-0253 F4: the char count times the cell width
                        // accumulates in `u64` before the 1024 clamp so a
                        // hostile count can never wrap the `u32` product.
                        let preedit_width = u32::try_from(
                            (preedit.chars().count() as u64)
                                .saturating_mul(u64::from(live.width))
                                .min(1024),
                        )
                        .unwrap_or(1024);
                        let underline_rect = bitty_render::geometry::RectPx::new(
                            base_x,
                            px_add(base_y, px_side(live.height).saturating_sub(2)),
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
                    if let Some(frame) = allocations.iter().find(|frame| frame.view == fid) {
                        if frame.rows > 0 && frame.cols > 0 {
                            let live = self.live_cell_metrics();
                            let max_cells = usize::from(frame.cols);
                            // Compact pill: only as wide as the text (clipped
                            // to the view), right-aligned so most of the row
                            // stays visible.
                            let text_cells = banner.chars().count().min(max_cells).max(1);
                            // CTX-0253 F4: pill/full widths and the
                            // right-aligned origin accumulate in `u64`/`i64`
                            // (see `px_span_usize`/`px_span`) so hostile cell
                            // metrics cannot wrap the products or the sums.
                            let pill_w = px_span_usize(text_cells, live.width);
                            let full_w = px_span(frame.cols, live.width);
                            let origin_px_x = px_add(
                                px_add(pad_px, frame.content.x),
                                px_side(full_w.saturating_sub(pill_w)),
                            );
                            let banner_y = px_add(
                                px_offset_cells(
                                    frame.content.y,
                                    frame.rows.saturating_sub(1),
                                    live.height,
                                ),
                                pad_px,
                            );
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

        // Pending workspace-close confirmation banner (CTX-0257): same
        // presentation-only overlay pill as the paste banner (steady text
        // while the arm holds — no full/flash phases — reusing the paste
        // pill colors so no new theme token is needed for the entry slice).
        // Gated on `has_pending_ws_close()`; text is the bounded
        // `ws_close_banner_text()`. Overlay only, never grid truth;
        // repeat-confirm and Esc-cancel paths repaint via
        // `pending_full_redraw`.
        if self.has_pending_ws_close() {
            if let Some(banner) = self.ws_close_banner_text() {
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some(frame) = allocations.iter().find(|frame| frame.view == fid) {
                        if frame.rows > 0 && frame.cols > 0 {
                            let live = self.live_cell_metrics();
                            let max_cells = usize::from(frame.cols);
                            let text_cells = banner.chars().count().min(max_cells).max(1);
                            let pill_w = px_span_usize(text_cells, live.width);
                            let full_w = px_span(frame.cols, live.width);
                            let origin_px_x = px_add(
                                px_add(pad_px, frame.content.x),
                                px_side(full_w.saturating_sub(pill_w)),
                            );
                            let banner_y = px_add(
                                px_offset_cells(
                                    frame.content.y,
                                    frame.rows.saturating_sub(1),
                                    live.height,
                                ),
                                pad_px,
                            );
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

        // Help popup panel (CTX-0265, 009 which-key): centered floating
        // overlay listing the live registry rows. Presentation-only like
        // the banners above (fills + glyphs, never grid truth); painted
        // after the pills so the panel reads on top on the rare frames
        // where both coincide. Visibility/readiness live in
        // `runtime::help`; toggle/dismiss paths repaint via
        // `pending_full_redraw`.
        if self.paint_help_panel(
            &allocations,
            &view_map,
            pad_px,
            &mut combined_fills,
            &mut combined_glyphs,
        ) {
            any_needs_draw = true;
        }

        // Overlay scrollbar thumb (CTX-0181): a presentation-only FillRect        // on the focused leaf's right edge, painted above grid content like
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

        // Kitty images (CTX-0248, budget + cache CTX-0252 F2, origin
        // binding CTX-0254): topmost present-layer blits on the focused
        // leaf, composited after fills and glyphs. Never grid truth: no
        // cell, scrollback, or layout mutation. Each visible placement is
        // scaled by the rich layer to its clamped cell-rect pixel extent
        // and translated to the leaf origin (plus the window padding
        // inset, like every other overlay).
        //
        // Only the focused leaf's origin paints here
        // (`placements_in_paint_order_for`): placements emitted by any
        // other pane stay retained but contribute zero pixels to this
        // frame, so a background program cannot spoof content over the
        // focused pane. Skipped while the focused view inspects scrollback
        // (live-grid anchors do not map to the history viewport).
        // Alternate-screen entry clears only the entering origin
        // (`clear_origin`); other panes' images survive.
        //
        // Per-frame budget ([`bitty_rich::KittyFrameBudget`]): at most 32
        // blits / 64 MiB of scaled bytes per frame, checked before
        // rasterizing so refused bytes are never allocated; over-budget
        // placements are skipped for the frame only (retained in paint
        // order). Raster cache ([`bitty_rich::KittyRasterCache`]): scaled
        // bytes keyed by placement + image identity, destination rect,
        // source dims, scrollback sequence, and geometry, so static frames
        // reuse blits while scroll/geometry changes miss (never stale).
        let mut combined_images: Vec<bitty_render::grid::ImageBlit> = Vec::new();
        if kitty_origin_alt {
            self.kitty_images.clear_origin(kitty_origin);
            self.kitty_raster_cache.clear();
        } else if !self
            .kitty_images
            .placement_for_origin_is_empty(kitty_origin)
        {
            let scrolled = self
                .focused_view()
                .and_then(|fid| view_map.get(&fid))
                .map(|v| v.scroll_offset() != 0)
                .unwrap_or(false);
            if !scrolled {
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some(frame) = allocations.iter().find(|frame| frame.view == fid) {
                        if frame.cols > 0 && frame.rows > 0 {
                            let live = self.live_cell_metrics();
                            let rich_metrics = bitty_rich::CellMetrics {
                                width: live.width,
                                height: live.height,
                            };
                            // CTX-0254: the origin's own scrollback sequence
                            // (resolved above), so a pane's image tracks its
                            // pane's content — never the primary grid's.
                            let scrollback = kitty_origin_scrollback;
                            let origin_px_x = px_add(pad_px, frame.content.x);
                            let origin_px_y = px_add(pad_px, frame.content.y);
                            let mut budget = bitty_rich::KittyFrameBudget::new();
                            for placement in self
                                .kitty_images
                                .placements_in_paint_order_for(kitty_origin)
                            {
                                let Some(img) = self.kitty_images.get(placement.image) else {
                                    continue;
                                };
                                let Some(rect_px) = bitty_rich::KittyImageLayer::placement_rect(
                                    placement,
                                    rich_metrics,
                                    frame.cols,
                                    frame.rows,
                                    scrollback,
                                ) else {
                                    continue;
                                };
                                // Budget before rasterize: refused bytes are
                                // never allocated, bounding the pathological
                                // 128-placement transient per frame.
                                let need = (u64::from(rect_px.width) * u64::from(rect_px.height))
                                    .checked_mul(4)
                                    .filter(|&n| n <= usize::MAX as u64)
                                    .map(|n| n as usize);
                                let Some(need) = need else { continue };
                                if !budget.admit(need) {
                                    continue;
                                }
                                let key = bitty_rich::KittyRasterKey {
                                    placement: placement.id.0,
                                    image: placement.image.0,
                                    rect: rect_px,
                                    src_w: img.width,
                                    src_h: img.height,
                                    scrollback,
                                    cell: rich_metrics,
                                    viewport_cols: frame.cols,
                                    viewport_rows: frame.rows,
                                };
                                let Some(scaled) = self
                                    .kitty_raster_cache
                                    .get_or_rasterize(key, || bitty_rich::rasterize(img, rect_px))
                                else {
                                    continue;
                                };
                                let dest = bitty_render::geometry::RectPx::new(
                                    px_add(rect_px.x, origin_px_x),
                                    px_add(rect_px.y, origin_px_y),
                                    rect_px.width,
                                    rect_px.height,
                                );
                                if let Ok(blit) =
                                    bitty_render::grid::ImageBlit::try_new(dest, scaled)
                                {
                                    combined_images.push(blit);
                                }
                            }
                            if !combined_images.is_empty() {
                                any_needs_draw = true;
                            }
                        }
                    }
                }
            }
        }

        self.pending_full_redraw = false;

        if !any_needs_draw
            && combined_fills.is_empty()
            && combined_rounded.is_empty()
            && combined_glyphs.is_empty()
            && combined_images.is_empty()
        {
            // Check if we had pending_full but produced no draws (e.g., all zero rects) -> still idle
            // But ensure generation advances for idle detection.
            self.mark_frame_presented(snapshot.generation);
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            return None;
        }

        // Synthesize a DrawList for the combined frame. Plan is not used by
        // headless_present beyond fill/glyph counts, so we create a minimal
        // plan that reports needs_draw == true when we have content.
        // CTX-0252 F2: latch the presented blit count before the move below.
        let kitty_blits = combined_images.len();
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
                    rounded_fills: Vec::new(),
                    glyphs: Vec::new(),
                    images: Vec::new(),
                });
            // Now replace fills/glyphs with combined, and reset the plan to
            // describe the combined pixel space: the 1x1 probe's extent must
            // not survive (see present_plan_extent), and the dirty rect must
            // cover the combined frame when content exists.
            tmp_list.generation = current_gen;
            tmp_list.fills = combined_fills;
            tmp_list.rounded_fills = combined_rounded;
            tmp_list.glyphs = combined_glyphs;
            tmp_list.images = combined_images;
            let plan_extent = self.present_plan_extent();
            tmp_list.plan.extent = plan_extent;
            if tmp_list.fills.is_empty()
                && tmp_list.rounded_fills.is_empty()
                && tmp_list.glyphs.is_empty()
            {
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
            self.mark_frame_presented(snapshot.generation);
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
        self.mark_frame_presented(snapshot.generation);
        self.last_presented_allocations = allocations;
        self.last_presented_focus = focused;
        self.kitty_last_frame_images = kitty_blits;
        // CTX-0244: publish the presented headless frame for `frameHash`
        // digesting — only while a digest grant is live (zero clone cost
        // otherwise) and only for headless presents (`stats.headless`;
        // the GPU path keeps no RGBA, and a stale buffer must never bind
        // a new frame number). Same-process publish, `&self` only.
        if stats.headless && bitty_ipc::devtools::frame_digest_publish_wanted() {
            if let (Some(extent), Some(rgba)) =
                (self.surface.extent(), self.surface.headless_rgba())
            {
                bitty_ipc::devtools::publish_frame_rgba(
                    extent.width(),
                    extent.height(),
                    stats.frame,
                    rgba,
                );
            }
        }
        // CTX-0159: publish grid plus latched input/focus so socket probes see
        // typed text without screenshots (`&self` only, bounded).
        self.publish_inspect_snapshot();
        Some(PresentStats {
            frame: stats.frame,
            fills: stats.fills,
            rounded_fills: stats.rounded_fills,
            glyphs: stats.glyphs,
            headless: stats.headless,
            generation: current_gen,
            images: stats.images,
            images_skipped: stats.images_skipped,
        })
    }
}

#[cfg(test)]
mod present_origin_tests {
    use super::{px_add, px_offset_cells, px_side, px_span, px_span_usize};

    #[test]
    fn origins_are_exact_on_normal_config() {
        // 9x19 cells (default geometry), 8px pad: the helpers must agree
        // with plain arithmetic where nothing overflows.
        assert_eq!(px_add(100, 728), 828);
        assert_eq!(px_offset_cells(8, 3, 9), 35);
        assert_eq!(px_span(80, 9), 720);
        assert_eq!(px_span_usize(10, 9), 90);
        assert_eq!(px_side(19), 19);
    }

    #[test]
    fn origins_saturate_on_hostile_config() {
        // CTX-0253 F4: extreme cell metrics from hostile local config must
        // clip like compositors do (saturate), never wrap the old `as i32`
        // casts or overflow `i32` multiply/add (debug panic / release wrap).
        assert_eq!(px_add(i32::MAX, 1), i32::MAX);
        assert_eq!(px_add(i32::MAX, i32::MAX), i32::MAX);
        assert_eq!(px_offset_cells(i32::MAX, u16::MAX, u32::MAX), i32::MAX);
        assert_eq!(px_offset_cells(0, 1, u32::MAX), i32::MAX);
        assert_eq!(px_span(u16::MAX, u32::MAX), u32::MAX);
        assert_eq!(px_span_usize(usize::MAX, u32::MAX), u32::MAX);
        assert_eq!(px_side(u32::MAX), i32::MAX);
    }
}
