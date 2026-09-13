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
    /// Cells examined by the grid renderer while producing this frame
    /// (per-frame delta, CTX-0386).
    ///
    /// The presented `glyphs` count includes primitives reused from a
    /// retained leaf list, so it does not measure work; this counter does.
    /// A frame that only re-renders one damaged pane examines roughly that
    /// pane's cell count, while a full frame examines every visible leaf.
    pub cells_examined: u64,
    /// Glyph instances emitted by the grid renderer while producing this
    /// frame (per-frame delta, CTX-0386).
    ///
    /// The work-side companion to `cells_examined`: a reused leaf emits no
    /// glyphs, while the presented `glyphs` count still carries them all.
    pub glyphs_emitted: u64,
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
            cells_examined: 0,
            glyphs_emitted: 0,
            headless: value.headless,
            generation: 0,
            images: value.images,
            images_skipped: value.images_skipped,
        }
    }
}

/// Physical-pixel caret rectangle reported to the platform IME (CTX-0367).
///
/// The embedder forwards this to
/// `bitty_platform::WindowHandle::set_ime_cursor_area` so the OS
/// preedit/candidate window anchors at the terminal cursor cell. Coordinates
/// are window-relative physical pixels, already DPI-scaled through the live
/// cell metrics; the rect spans one cursor cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImeCursorArea {
    /// Window-relative physical x of the caret cell's left edge.
    pub x: i32,
    /// Window-relative physical y of the caret cell's top edge.
    pub y: i32,
    /// Caret cell width in physical pixels (>= 1).
    pub width: u32,
    /// Caret cell height in physical pixels (>= 1).
    pub height: u32,
}

/// Private caret bookkeeping for the inline preedit overlay (CTX-0367).
///
/// Exposed publicly only through [`Runtime::ime_cursor_area`]; the cell
/// budget stays internal because it is a presentation clip, not part of the
/// platform contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImeCaret {
    /// Platform rect for the candidate window.
    pub(super) area: ImeCursorArea,
    /// Cell columns between the caret and the right edge of its pane content
    /// (at least `1`), used to clip the inline preedit overlay so it never
    /// paints outside the pane.
    pub(super) cells_available: u16,
}

/// Bounded retries when a glyph-atlas exhaustion reset invalidates the
/// retained leaf lists mid-frame (CTX-0386). A reset clears every slot,
/// including slots emitted earlier in the same pass, so the whole leaf set is
/// rebuilt; one retry is enough for every realistic pass and the bound keeps
/// a pathological atlas from spinning the frame loop.
const ATLAS_REBUILD_LIMIT: u8 = 2;

/// One leaf's retained present primitives (CTX-0386).
///
/// The software and GPU present paths clear the surface every frame and
/// composite a single complete draw list, so a leaf that produced no damage
/// must still contribute its previous primitives. Retaining them per leaf is
/// what lets a split stop re-examining (and re-emitting glyphs for) every
/// pane when only one pane is busy. Coordinates are already translated into
/// window space and glyph clips are already attached, so the retained list is
/// presented verbatim.
pub(super) struct PresentedLeaf {
    /// Painted content origin (physical px, before window padding) the list
    /// was translated against. A mismatch forces a fresh leaf render.
    pub(super) origin: (i32, i32),
    /// Scroll offset the list was rendered at. A mismatch forces a fresh
    /// leaf render (scrollback composites a different cell window).
    pub(super) scroll_offset: usize,
    /// Complete translated cell fills.
    pub(super) fills: Vec<bitty_render::grid::FillRect>,
    /// Complete translated glyph instances (inner-arc clip attached).
    pub(super) glyphs: Vec<bitty_render::grid::GlyphInstance>,
}

/// Deferred per-frame cursor overlay (CTX-0386).
///
/// Captured while walking the focused leaf and painted after all leaves so a
/// reused (retained) leaf never carries a stale cursor fill: cursor-only
/// batches (`DECTCEM` visibility, `CUP` moves) advance the generation without
/// grid damage, and the overlay must track the current snapshot even when the
/// leaf's retained list is reused.
struct CursorPaint {
    /// Content origin in physical px, window padding already added.
    origin_x: i32,
    /// Content origin in physical px, window padding already added.
    origin_y: i32,
    /// Cursor with its row translated into viewport coordinates.
    cursor: bitty_term_state::Cursor,
    /// Viewport dimensions the cursor was translated against.
    cols: usize,
    /// Viewport dimensions the cursor was translated against.
    rows: usize,
    /// Cell columns between the caret and the pane's right edge (>= 1).
    cells_available: u16,
    /// Whether the cursor cell is the trailing half of a wide char.
    on_spacer: bool,
}

/// One frame's combined leaf primitives, built in a retryable pass.
#[derive(Default)]
struct CombinedLeaves {
    fills: Vec<bitty_render::grid::FillRect>,
    rounded: Vec<bitty_render::grid::RoundedFill>,
    glyphs: Vec<bitty_render::grid::GlyphInstance>,
    needs_draw: bool,
    cursor: Option<CursorPaint>,
}

/// Loop-invariant inputs for the per-leaf decoration ring (CTX-0386).
#[derive(Clone, Copy)]
struct RingFrameContext {
    focused_id: Option<ViewId>,
    workspace_factor: f32,
    now: std::time::Instant,
    pad_px: i32,
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

/// First source row of a viewport window that keeps `cursor_row` visible.
///
/// The window is `window` rows tall inside `src_len` rows. It stays at the
/// top while the cursor fits in the first window; once the cursor is below,
/// the window scrolls the minimum amount and clamps to the screen bottom, so
/// it ends bottom-anchored on the last rows (the live prompt case). CTX-0361.
fn cursor_follow_window_start(cursor_row: usize, src_len: usize, window: usize) -> usize {
    if window == 0 {
        return 0;
    }
    let max_start = src_len.saturating_sub(window);
    if cursor_row < window {
        0
    } else {
        (cursor_row + 1 - window).min(max_start)
    }
}

/// Creates a viewport snapshot of `snapshot` limited to `cols x rows`.
///
/// When the requested size matches the snapshot the snapshot is returned
/// unchanged. Otherwise the window is derived from the active screen, padded
/// with erased cells when the requested size exceeds the snapshot dimensions
/// (honest padding for the deferred grid-resize reflow). Rows follow the
/// cursor: a screen taller than the viewport (the decorated content frame is
/// smaller than the PTY grid until reflow) shows the rows around the live
/// cursor instead of cropping the bottom, and the cursor is translated into
/// window coordinates so the overlay paints the prompt row (CTX-0361). Cursor
/// and modes are carried over; title/modes are snapshot-owned.
pub(super) fn viewport_snapshot(snapshot: &Snapshot, cols: u16, rows: u16) -> Snapshot {
    let req_w = cols as usize;
    let req_h = rows as usize;
    if snapshot.width == req_w && snapshot.height == req_h {
        return snapshot.clone();
    }
    let start_row = cursor_follow_window_start(
        snapshot.cursor.position.row as usize,
        snapshot.height,
        req_h,
    );
    let mut cells = Vec::with_capacity(req_w * req_h);
    let src_w = snapshot.width;
    let src_h = snapshot.height;
    for r in 0..req_h {
        for c in 0..req_w {
            let src_r = start_row + r;
            if src_r < src_h && c < src_w {
                let idx = src_r * src_w + c;
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
    let mut cursor = snapshot.cursor.clone();
    cursor.position.row = (usize::from(cursor.position.row).saturating_sub(start_row))
        .min(req_h.saturating_sub(1)) as u16;
    Snapshot {
        version: snapshot.version,
        generation: snapshot.generation,
        width: req_w,
        height: req_h,
        cells: cells.into_boxed_slice(),
        cursor,
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
    /// `LayoutNode::reflow`. Then each leaf contributes its complete draw
    /// primitives: a leaf whose own origin produced damage is re-rendered
    /// from a viewport snapshot sized to its dimensions — built from its own
    /// pane-session grid when it owns a shell, from the shared `State`
    /// snapshot only for the primary owner leaf (`primary_view`, CTX-0359),
    /// and erased otherwise (never duplicate one grid across tiles) — through
    /// the shared `GridRenderer` with a full leaf damage hint and translated
    /// to the leaf's pixel origin. CTX-0386: a clean leaf instead reuses the
    /// primitive list retained from its last render, so one busy pane no
    /// longer re-examines every other pane. The per-leaf lists are combined
    /// and presented once via `Surface::headless_present`.
    ///
    /// Full invalidation (first frame, resize, DPI/font, layout/focus edit,
    /// appearance/animation transition) still re-renders every leaf.
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
    /// pane session carries its own consumed counter (CTX-0386), updated
    /// whether or not that pane was visible this frame — a pane that returns
    /// to the visible layout is covered by the full-frame invalidation that
    /// the visibility change already forces. Called on every present,
    /// including the idle write-backs that consume a generation without
    /// drawing pixels.
    fn mark_frame_presented(&mut self, primary_gen: u64) {
        self.last_presented_generation = primary_gen;
        for sess in self.pane_sessions.values_mut() {
            sess.last_presented_generation = sess.state.generation();
        }
    }

    /// Pushes the Core-owned decoration ring for one leaf and returns the
    /// inner content clip for its glyphs (CTX-0311, moved verbatim from the
    /// leaf loop under CTX-0386 so a reused retained leaf still owns a live
    /// ring). `border == 0` paints nothing; the returned clip is `None` for
    /// square or zero-size frames.
    fn push_leaf_ring(
        &self,
        frame: &layout_focus::PresentFrame,
        view_id: ViewId,
        open_factor: f32,
        ctx: &RingFrameContext,
        combined_rounded: &mut Vec<bitty_render::grid::RoundedFill>,
    ) -> (Option<bitty_render::grid::RoundedClip>, bool) {
        let mut frame_clip = None;
        let mut painted = false;
        if frame.frame.width > 0 && frame.frame.height > 0 {
            let ring_frame = bitty_render::geometry::RectPx::new(
                px_add(ctx.pad_px, frame.frame.x),
                px_add(ctx.pad_px, frame.frame.y),
                frame.frame.width,
                frame.frame.height,
            );
            // CTX-0340/CTX-0343: the focused View paints the accent
            // outline, every idle View the subtle outline. The pair and
            // the ring *width* now resolve per `View` (RFC-0001/OQ-041)
            // from the global values plus any matching `views` rule, so a
            // per-panel override paints only its own panel. Widths scale
            // at the live DPI factor exactly like the geometry border; the
            // ring paints inside the View rectangle and the content grid
            // stays inset by `border + content_inset`, so an override never
            // moves content.
            let is_focused_view = ctx.focused_id == Some(view_id);
            let view_outline = self.view_outline_for(view_id);
            let ring_border = if is_focused_view {
                view_outline
                    .width_focused
                    .map_or(frame.border, |w| self.outline_width_physical(w))
            } else {
                view_outline
                    .width_idle
                    .map_or(frame.border, |w| self.outline_width_physical(w))
            };
            if ring_border > 0 {
                let outline_color = if is_focused_view {
                    view_outline.focused
                } else {
                    view_outline.idle
                };
                // RFC-0002 (CTX-0341): the focus transition cross-fades
                // the ring color from idle toward the focused accent over
                // the accepted duration. Geometry never moves; only the
                // Core-owned color interpolates. When not animating (or
                // instant), the final color is used unchanged.
                let animated_color = if is_focused_view {
                    match self.animation_progress(AnimationKind::Focus, Some(view_id), ctx.now) {
                        Some(p) => bitty_render::grid::lerp_rgba(
                            view_outline.idle,
                            view_outline.focused,
                            p,
                        ),
                        None => outline_color,
                    }
                } else {
                    outline_color
                };
                // Panel-open fades the ring in from transparent; the final
                // committed color is applied at animation end.
                let ring_color = bitty_render::grid::scale_alpha(
                    animated_color,
                    open_factor * ctx.workspace_factor,
                );
                combined_rounded.push(bitty_render::grid::RoundedFill {
                    frame: ring_frame,
                    border: ring_border,
                    radius: frame.radius,
                    color: ring_color,
                });
                painted = true;
            }
            // The content clip is tied to the geometry border (not the
            // outline width) so the content grid and glyph clipping are
            // unchanged by a focused/idle width override.
            frame_clip =
                bitty_render::grid::rounded_frame_clip(ring_frame, frame.border, frame.radius);
        }
        (frame_clip, painted)
    }

    /// Repaints the focused cursor from live state without building a
    /// viewport snapshot (CTX-0386). Used when a leaf's retained list is
    /// reused: the grid content is unchanged, but a cursor-only batch
    /// (`DECTCEM`, `CUP`) may have moved the overlay, so it is recomputed
    /// from the origin's current cursor with the same cursor-follow row
    /// translation [`viewport_snapshot`] applies.
    fn reused_cursor_paint(
        &self,
        frame: &layout_focus::PresentFrame,
        view_id: ViewId,
        snapshot: &Snapshot,
        origin_x: i32,
        origin_y: i32,
    ) -> Option<CursorPaint> {
        let (mut cursor, base_height) = match self.pane_sessions.get(&view_id) {
            Some(sess) => (sess.state.cursor().clone(), sess.state.height()),
            None => (snapshot.cursor.clone(), snapshot.height),
        };
        if !cursor.visible {
            return None;
        }
        let rows = usize::from(frame.rows);
        let cols = usize::from(frame.cols);
        let start = cursor_follow_window_start(usize::from(cursor.position.row), base_height, rows);
        cursor.position.row = (usize::from(cursor.position.row).saturating_sub(start))
            .min(rows.saturating_sub(1)) as u16;
        if usize::from(cursor.position.row) >= rows || usize::from(cursor.position.col) >= cols {
            return None;
        }
        let cells_available = frame.cols.saturating_sub(cursor.position.col).max(1);
        // `State` steps the cursor off any spacer half after every action
        // batch and resize (`enforce_cursor_invariants`), so a reused leaf
        // needs no cell probe to resolve the spacer flag.
        Some(CursorPaint {
            origin_x,
            origin_y,
            cursor,
            cols,
            rows,
            cells_available,
            on_spacer: false,
        })
    }

    /// Paints the deferred focused-cursor overlay (CTX-0386) and arms the
    /// platform-IME caret rect (CTX-0367). A no-op when the window lost
    /// focus; the cursor never leaves a stale fill because it is not part of
    /// any retained leaf list. Returns whether a fill was pushed.
    fn paint_cursor(
        &mut self,
        paint: &CursorPaint,
        combined_fills: &mut Vec<bitty_render::grid::FillRect>,
    ) -> bool {
        if !self.focused {
            return false;
        }
        let live = self.live_cell_metrics();
        // CTX-0367: arm the platform-IME caret rect (window-relative
        // physical pixels) for the focused, visible cursor. The inline
        // preedit overlay and the OS candidate window both anchor here;
        // `cells_available` clips the inline preedit to the pane's right
        // edge.
        self.ime_caret = Some(ImeCaret {
            area: ImeCursorArea {
                x: px_offset_cells(paint.origin_x, paint.cursor.position.col, live.width),
                y: px_offset_cells(paint.origin_y, paint.cursor.position.row, live.height),
                width: live.width.max(1),
                height: live.height.max(1),
            },
            cells_available: paint.cells_available,
        });
        if paint.on_spacer {
            return false;
        }
        // DECSCUSR shape comes from the shared render primitive
        // (`bitty_render::grid::cursor_fill`: block = full cell, bar = left
        // strip, underline = bottom strip, 15% thickness per DEC-0017
        // ghostty/alacritty refs). Geometry is shared; the overlay hue is
        // the resolved theme cursor (CTX-0355) so the live cursor matches
        // the selected preset (CTX-0219: no hardcoded white).
        if let Some(fill) = bitty_render::grid::cursor_fill_in(
            &self.config.theme,
            &paint.cursor,
            live,
            paint.cols,
            paint.rows,
        ) {
            // Cursor color: the theme cursor hue at the existing translucent
            // alpha (blinks stay with the embedder's visibility/focus gate,
            // already checked above).
            let mut themed = self.config.theme.cursor;
            themed[3] = 0xA0;
            let rect = bitty_render::geometry::RectPx::new(
                px_add(fill.rect.x, paint.origin_x),
                px_add(fill.rect.y, paint.origin_y),
                fill.rect.width,
                fill.rect.height,
            );
            combined_fills.push(bitty_render::grid::FillRect {
                rect,
                color: themed,
            });
            return true;
        }
        false
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
        // CTX-0380 synchronized updates: while any visible grid has
        // `DECSET 2026` active, defer committing frames so the application
        // can redraw atomically. Damage is retained because the frame is
        // never marked presented; the mode exit presents the batched state.
        // The window is bounded by `SYNC_UPDATE_DEFER_TIMEOUT` (100 ms, the
        // contour/iTerm2-proposal consensus bound): a hung or buggy process
        // that never sends the reset still gets its latest state committed
        // at the bound instead of stalling presentation indefinitely. After
        // the bound, each subsequent window commits at most every timeout.
        if self.synchronized_update_active() {
            let since = *self.sync_defer_since.get_or_insert(now);
            if now.saturating_duration_since(since) < SYNC_UPDATE_DEFER_TIMEOUT {
                return None;
            }
            // Bound reached: commit this frame, then open a fresh window so
            // a still-active mode does not present on every later tick.
            self.sync_defer_since = Some(now);
        } else {
            self.sync_defer_since = None;
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
        // against its own last presented generation (CTX-0386: held on the
        // session itself, so an added session starts at the `u64::MAX`
        // sentinel and always reads as changed even before `mark_frame_`
        // `presented` consumes it).
        let mut current_gen = snapshot.generation;
        let mut origins_changed = snapshot.generation != last;
        for sess in self.pane_sessions.values() {
            let pane_gen = sess.state.generation();
            current_gen = current_gen.max(pane_gen);
            if sess.last_presented_generation != pane_gen {
                origins_changed = true;
            }
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

        // RFC-0002 (CTX-0341): derive open/close/focus/workspace transitions
        // from the last presented frame and arm the bounded animations. This
        // runs before the idle short-circuit so an armed transition forces the
        // frame even when no PTY bytes advanced the generation. It mutates
        // presentation-only animation state, never terminal truth.
        // `advance_animations` first drops transitions that expired since the
        // last frame and reports it, so this frame is the one that commits the
        // final end state (the next frame then idles).
        let expired_animations = self.advance_animations(now);
        let active_workspace = self.active_workspace_index();
        self.detect_panel_animations(&allocations, focused, active_workspace, now);
        let animations_active = self.animator.is_active(now);
        if animations_active || expired_animations {
            // A live (or just-completed) animation is a presentation-only
            // change: force the full per-leaf path for this frame.
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

        // CTX-0386: genuine full invalidation only. `pending_full` already
        // funnels every window resize, DPI/font change, layout or focus edit,
        // appearance/animation transition, alt-screen latch, and explicit
        // `pending_full_redraw`; `last == u64::MAX` marks the first frame.
        // Everything else re-renders per origin from its own damage ring, so a
        // split no longer repaints every pane just because sessions exist.
        let mut full_frame = pending_full || last == u64::MAX;

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

        // RFC-0002 (CTX-0341): per-surface animation factors for this frame.
        // `open_factor` scales the core-owned ring alpha during a panel open;
        // the workspace factor cross-fades Core-owned chrome on a workspace
        // switch; the focus factor (applied inside the loop) cross-fades the
        // outline color. All default to the final value when the transition is
        // instant or already complete. Purely presentation: no grid, cursor,
        // scrollback, or Terminal Truth is interpolated.
        let workspace_factor = self
            .animation_progress(AnimationKind::Workspace, None, now)
            .map(|p| p.clamp(0.0, 1.0))
            .unwrap_or(1.0);
        let ring_ctx = RingFrameContext {
            focused_id: self.focus.focused(),
            workspace_factor,
            now,
            pad_px,
        };

        // CTX-0367: recompute the IME caret for this frame; `paint_cursor`
        // re-arms it for the focused leaf. A stale rect must never survive a
        // frame where the focused cursor is hidden or the focused leaf has no
        // damage.
        self.ime_caret = None;
        // CTX-0386: renderer work baseline for the per-frame deltas reported in
        // `PresentStats::cells_examined`/`glyphs_emitted`.
        let cells_before = self.renderer.counters().cells_examined;
        let glyphs_before = self.renderer.counters().glyphs_emitted;

        // CTX-0386: build the combined leaf primitives. Both present paths
        // clear the surface and composite one complete list per frame, so a
        // clean leaf contributes its retained primitives verbatim instead of a
        // fresh (partial, hence blanking) list; a leaf re-renders only when
        // its own origin's damage ring advanced. An atlas exhaustion reset
        // invalidates every retained slot (including slots emitted earlier in
        // the same pass), so the whole leaf set is rebuilt when a reset is
        // observed, bounded by [`ATLAS_REBUILD_LIMIT`].
        let mut attempt = 0u8;
        let built = loop {
            attempt += 1;
            let mut built = CombinedLeaves::default();
            let evictions_before = self.renderer.atlas_stats().2;

            for frame in &allocations {
                if frame.content.width == 0 || frame.content.height == 0 {
                    continue;
                }
                let view_id = frame.view;
                let view = view_map.get(&view_id);
                let scrolled = view.map(|v| v.scroll_offset() != 0).unwrap_or(false);
                let open_factor = self
                    .animation_progress(AnimationKind::Open, Some(view_id), now)
                    .map(|p| p.clamp(0.0, 1.0))
                    .unwrap_or(1.0);
                let origin_px_x = px_add(pad_px, frame.content.x);
                let origin_px_y = px_add(pad_px, frame.content.y);
                // The ring arm uses the strict focus match; the cursor arm
                // keeps the `unwrap_or(true)` default for a layout with no
                // resolvable focus. Both preserve their pre-CTX-0386 shape.
                let is_focused_view = ring_ctx
                    .focused_id
                    .map(|fid| fid == view_id)
                    .unwrap_or(true);

                // Each origin's damage comes from its own ring (CTX-0386).
                // `last_presented_generation` is consumed per present: the
                // session field for panes, the runtime scalar for the primary
                // owner. A gap past the retained history window is treated as
                // damage so an evicted ring can never under-damage.
                let (origin_gen, origin_last, leaf_damage) = match self.pane_sessions.get(&view_id)
                {
                    Some(sess) => (
                        sess.state.generation(),
                        sess.last_presented_generation,
                        sess.state.damage_since(sess.last_presented_generation),
                    ),
                    None if Some(view_id) == self.primary_view => (
                        snapshot.generation,
                        self.last_presented_generation,
                        self.state.damage_since(self.last_presented_generation),
                    ),
                    None => (snapshot.generation, snapshot.generation, Vec::new()),
                };
                let damage_evicted = origin_gen.saturating_sub(origin_last)
                    > bitty_term_state::damage::DAMAGE_HISTORY_BATCHES as u64;
                let leaf_damaged = !leaf_damage.is_empty() || damage_evicted;
                let reuse = !full_frame
                    && !leaf_damaged
                    && self
                        .presented_leaf_frames
                        .get(&view_id)
                        .is_some_and(|leaf| {
                            leaf.origin == (frame.content.x, frame.content.y)
                                && leaf.scroll_offset
                                    == view.map(|v| v.scroll_offset()).unwrap_or(0)
                        });

                // CTX-0311 ring: shared by the reuse and re-render paths so a
                // retained leaf still paints its live focus/open outline.
                let (frame_clip, ring_painted) =
                    self.push_leaf_ring(frame, view_id, open_factor, &ring_ctx, &mut built.rounded);
                built.needs_draw |= ring_painted;

                if reuse {
                    let leaf = self
                        .presented_leaf_frames
                        .get(&view_id)
                        .expect("reuse requires a retained leaf");
                    built.needs_draw |= !leaf.fills.is_empty() || !leaf.glyphs.is_empty();
                    built.fills.extend(leaf.fills.iter().cloned());
                    built.glyphs.extend(leaf.glyphs.iter().cloned());
                    if is_focused_view && !scrolled {
                        built.cursor = self.reused_cursor_paint(
                            frame,
                            view_id,
                            &snapshot,
                            origin_px_x,
                            origin_px_y,
                        );
                    }
                    continue;
                }

                // CTX-0176: a leaf with its own shell renders that session's
                // grid. CTX-0359: a leaf WITHOUT a session renders the shared
                // primary snapshot only when it IS the primary owner
                // (`primary_view`: the leaf focused when the primary shell
                // attached). Ownership — not focus and not session-presence —
                // decides: every other session-less leaf (ctl splits spawn no
                // shell, spawn failures, fresh workspace leaves) presents
                // erased, so a session-less View never paints another View's
                // grid.
                let pane_snap: Option<Snapshot> = match self.pane_sessions.get(&view_id) {
                    Some(sess) => Some(sess.state.snapshot()),
                    None if Some(view_id) == self.primary_view => Some(snapshot.clone()),
                    None => None,
                };
                // Erased source for session-less leaves that do not own the
                // primary; `viewport_snapshot` pads it to the allocation below.
                let erased_snap: Option<Snapshot> = if pane_snap.is_none() {
                    Some(erased_snapshot(&snapshot))
                } else {
                    None
                };
                let base_snap: &Snapshot = pane_snap
                    .as_ref()
                    .or(erased_snap.as_ref())
                    .unwrap_or(&snapshot);
                // Determine viewport snapshot: when view scroll_offset !=0,
                // visible_cells composites scrollback.
                let view_snapshot = if let Some(v) = view {
                    if v.scroll_offset() != 0 && pane_snap.is_some() {
                        let cells = match self.pane_sessions.get(&view_id) {
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

                // A re-rendered leaf is redrawn in full. Sub-pane partial
                // merges are deliberately not attempted: retained glyphs can
                // paint outside their cell (metric overhang), so intersecting
                // them with sub-cell damage would under-damage neighbouring
                // cells. Pane granularity is the containment unit and
                // over-damage stays the only failure direction.
                let damage = Damage {
                    generation: current_gen,
                    regions: vec![DamagedRegion::Grid(DamageRect::full(
                        view_snapshot.height as u16,
                        view_snapshot.width as u16,
                    ))]
                    .into_boxed_slice(),
                };
                let list = match self.renderer.render(&view_snapshot, &damage) {
                    Ok(list) => list,
                    Err(_) => continue,
                };
                if is_focused_view && view_snapshot.cursor.visible && !scrolled {
                    let cur = &view_snapshot.cursor.position;
                    if (cur.row as usize) < view_snapshot.height
                        && (cur.col as usize) < view_snapshot.width
                    {
                        let idx = cur.row as usize * view_snapshot.width + cur.col as usize;
                        let on_spacer = view_snapshot
                            .cells
                            .get(idx)
                            .map(|c| c.spacer)
                            .unwrap_or(false);
                        built.cursor = Some(CursorPaint {
                            origin_x: origin_px_x,
                            origin_y: origin_px_y,
                            cursor: view_snapshot.cursor.clone(),
                            cols: view_snapshot.width,
                            rows: view_snapshot.height,
                            cells_available: frame.cols.saturating_sub(cur.col).max(1),
                            on_spacer,
                        });
                    }
                }

                let leaf_painted = list.needs_draw();
                let mut fills = list.fills;
                for fill in &mut fills {
                    fill.rect.x = px_add(fill.rect.x, origin_px_x);
                    fill.rect.y = px_add(fill.rect.y, origin_px_y);
                }
                let mut glyphs = list.glyphs;
                for glyph in &mut glyphs {
                    glyph.dest[0] = px_add(glyph.dest[0], origin_px_x);
                    glyph.dest[1] = px_add(glyph.dest[1], origin_px_y);
                    // CTX-0311 inner-arc clip: only decorated rounded frames
                    // set it; square frames keep the documented overhang.
                    glyph.clip = frame_clip;
                }
                built.needs_draw |= leaf_painted || !fills.is_empty() || !glyphs.is_empty();
                self.presented_leaf_frames.insert(
                    view_id,
                    PresentedLeaf {
                        origin: (frame.content.x, frame.content.y),
                        scroll_offset: view.map(|v| v.scroll_offset()).unwrap_or(0),
                        fills: fills.clone(),
                        glyphs: glyphs.clone(),
                    },
                );
                built.fills.extend(fills);
                built.glyphs.extend(glyphs);
            }

            if self.renderer.atlas_stats().2 == evictions_before || attempt >= ATLAS_REBUILD_LIMIT {
                break built;
            }
            // Atlas exhaustion reset: every retained slot is dead, so drop the
            // stores and rebuild every leaf against the fresh atlas.
            self.presented_leaf_frames.clear();
            full_frame = true;
        };

        // CTX-0386: a retained list is only valid for a leaf visible this
        // frame; a hidden or closed leaf re-renders when it returns.
        self.presented_leaf_frames
            .retain(|id, _| allocations.iter().any(|frame| frame.view == *id));

        let mut combined_fills = built.fills;
        let mut combined_rounded = built.rounded;
        let mut combined_glyphs = built.glyphs;
        let mut any_needs_draw = built.needs_draw;
        if let Some(paint) = built.cursor {
            any_needs_draw |= self.paint_cursor(&paint, &mut combined_fills);
        }

        // RFC-0002 (CTX-0341): paint the retained frames of Views closed
        // during the current close transition. A closed View has no live
        // allocation, so its last presented ring fades out over the accepted
        // close duration; the final removed state is committed at animation
        // end (the closing frame is dropped once the tracker reports no
        // progress). Terminal content is never retained or interpolated —
        // only the Core-owned ring. Bounded by MAX_CONCURRENT_ANIMATIONS.
        for closing in &self.closing_frames {
            if closing.frame.width == 0 || closing.frame.height == 0 {
                continue;
            }
            let remaining = self.animation_progress(AnimationKind::Close, Some(closing.view), now);
            let factor = remaining.map(|p| 1.0 - p.clamp(0.0, 1.0)).unwrap_or(0.0);
            if factor <= 0.0 {
                continue;
            }
            let ring_frame = bitty_render::geometry::RectPx::new(
                px_add(pad_px, closing.frame.x),
                px_add(pad_px, closing.frame.y),
                closing.frame.width,
                closing.frame.height,
            );
            combined_rounded.push(bitty_render::grid::RoundedFill {
                frame: ring_frame,
                border: closing.border,
                radius: closing.radius,
                color: bitty_render::grid::scale_alpha(closing.color, factor),
            });
            any_needs_draw = true;
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
                            let rects = bitty_render::grid::selection_fill_rects_in(
                                &self.config.theme,
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

        // IME preedit overlay (CTX-0367): presentation-only, never Terminal
        // Truth. The preedit string, a single-pixel underline, and the
        // composition caret paint at the focused caret armed inside the
        // render loop above. `State`, `Snapshot`, scrollback, damage, and
        // replies are untouched by composition: commit is the only path that
        // reaches the PTY (`handle_ime_commit`), and cancel/disable clears
        // the overlay without bytes. Bounded: the model preedit is capped at
        // `IME_PREEDIT_MAX_CHARS` (128) by `handle_ime_preedit`, and the
        // overlay additionally clips to the pane's remaining columns so a
        // hostile composition can never overdraw another pane or grow the
        // frame without limit.
        if let Some(preedit) = self.ime_preedit.clone() {
            if !preedit.is_empty() && self.focused {
                if let Some(caret) = self.ime_caret {
                    let live = self.live_cell_metrics();
                    // Cell-accurate layout: advance by the terminal cell
                    // width so a wide CJK preedit glyph consumes two cells
                    // (matching grid geometry) and zero-width marks compose
                    // onto their base. The IME cursor is a character index
                    // into the preedit; the caret bar snaps to its cell.
                    let max_cells = usize::from(caret.cells_available);
                    let mut clipped = String::new();
                    let mut used_cells = 0usize;
                    let mut caret_cells = 0usize;
                    let mut caret_seen = false;
                    for (char_idx, ch) in preedit.chars().enumerate() {
                        if char_idx == self.ime_cursor && !caret_seen {
                            caret_cells = used_cells;
                            caret_seen = true;
                        }
                        let width = usize::from(bitty_term_state::char_cell_width(ch));
                        if used_cells + width > max_cells {
                            break;
                        }
                        clipped.push(ch);
                        used_cells += width;
                    }
                    if !caret_seen {
                        caret_cells = used_cells;
                    }
                    // CTX-0253 F4: cell count times the cell width
                    // accumulates in `u64` before the `u32` clamp so hostile
                    // metrics can never wrap the product.
                    let drawn_cells = used_cells.clamp(1, max_cells.max(1));
                    let preedit_width = px_span_usize(drawn_cells, live.width);
                    let base_x = caret.area.x;
                    let base_y = caret.area.y;
                    let underline_y = px_add(base_y, px_side(live.height).saturating_sub(2));
                    let glyphs = self.renderer.overlay_text_glyphs(
                        &clipped,
                        (base_x, base_y),
                        max_cells,
                        self.config.theme.foreground,
                    );
                    // Background, underline, then glyphs: fills paint before
                    // glyphs in the `DrawList` order, so the text stays
                    // legible on the tint.
                    combined_fills.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y,
                            preedit_width,
                            live.height.max(1),
                        ),
                        color: [0x33, 0x33, 0x33, 0xCC],
                    });
                    combined_fills.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            base_x,
                            underline_y,
                            preedit_width,
                            2,
                        ),
                        color: [0xFF, 0xFF, 0x00, 0xFF],
                    });
                    combined_glyphs.extend(glyphs);
                    // Composition caret: static bar at the IME cursor cell
                    // (blink policy stays an embedder concern, matching the
                    // Terminal cursor gate). Clamped into the drawn span.
                    let caret_bar_cells = caret_cells.min(drawn_cells);
                    let caret_bar_x = px_offset_cells(base_x, caret_bar_cells as u16, live.width);
                    combined_fills.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            caret_bar_x,
                            base_y,
                            2,
                            live.height.max(1),
                        ),
                        color: self.config.theme.cursor,
                    });
                    any_needs_draw = true;
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

        // Pending view/window close confirmation banner (CTX-0370): the
        // same presentation-only overlay pill as the paste and
        // workspace-close banners (steady text while the arm holds).
        // Gated on `has_pending_close_confirm()`; text is the bounded
        // `close_confirm_banner_text()`. Overlay only, never grid truth;
        // repeat-confirm and Esc-cancel paths repaint via
        // `pending_full_redraw`. Painted after the workspace-close pill so
        // a window arm reads on top when both are somehow armed.
        if self.has_pending_close_confirm() {
            if let Some(banner) = self.close_confirm_banner_text() {
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
                            // CTX-0361: fold the live cursor-follow window
                            // start into the sequence so placements track the
                            // same rows the text viewport presents (the
                            // decorated content frame is smaller than the PTY
                            // grid until reflow).
                            let (origin_cursor_row, origin_rows) = match kitty_origin {
                                Some(token) => match self.pane_sessions.get(&ViewId::new(token)) {
                                    Some(sess) => (
                                        usize::from(sess.state.cursor().position.row),
                                        sess.state.height(),
                                    ),
                                    None => {
                                        (usize::from(snapshot.cursor.position.row), snapshot.height)
                                    }
                                },
                                None => {
                                    (usize::from(snapshot.cursor.position.row), snapshot.height)
                                }
                            };
                            let scrollback = kitty_origin_scrollback
                                + cursor_follow_window_start(
                                    origin_cursor_row,
                                    origin_rows,
                                    usize::from(frame.rows),
                                );
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
        // CTX-0386: the 1x1 plan probe below is the frame's last renderer
        // call; an atlas exhaustion reset inside it would invalidate every
        // retained slot, so drop the stores afterwards and let the next frame
        // rebuild. This frame stays best-effort, the same class the
        // full-render path already had.
        let evictions_before_probe = self.renderer.atlas_stats().2;
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

        // Drop retained lists if the plan probe reset the atlas above.
        if self.renderer.atlas_stats().2 != evictions_before_probe {
            self.presented_leaf_frames.clear();
        }

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
            cells_examined: self
                .renderer
                .counters()
                .cells_examined
                .saturating_sub(cells_before),
            glyphs_emitted: self
                .renderer
                .counters()
                .glyphs_emitted
                .saturating_sub(glyphs_before),
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

#[cfg(test)]
mod viewport_follow_tests {
    use super::{cursor_follow_window_start, viewport_snapshot};
    use bitty_term_state::{Snapshot, State, TerminalAction};
    use bitty_vt::{Col, Row};

    /// Snapshot with one sentinel glyph per row (`a`..) so the tests can see
    /// which source rows the viewport window selected.
    fn marked_snapshot(width: usize, height: usize, cursor_row: u16, cursor_col: u16) -> Snapshot {
        let mut state = State::new();
        state.resize(width, height);
        let _ = state.apply(&TerminalAction::CursorPosition {
            row: Row(cursor_row + 1),
            col: Col(cursor_col + 1),
        });
        let mut snapshot = state.snapshot();
        for row in 0..height {
            snapshot.cells[row * width].glyph = char::from(b'a' + row as u8);
        }
        snapshot
    }

    #[test]
    fn window_start_follows_cursor_then_clamps_to_screen_bottom() {
        // Cursor inside the first window -> window stays at the top.
        assert_eq!(cursor_follow_window_start(0, 24, 22), 0);
        assert_eq!(cursor_follow_window_start(21, 24, 22), 0);
        // Cursor one/two rows below -> minimal scroll, bottom-anchored.
        assert_eq!(cursor_follow_window_start(22, 24, 22), 1);
        assert_eq!(cursor_follow_window_start(23, 24, 22), 2);
        // Hostile cursor beyond the screen clamps to the last window.
        assert_eq!(cursor_follow_window_start(9_999, 24, 22), 2);
        // Window covers the whole screen or is degenerate -> top.
        assert_eq!(cursor_follow_window_start(23, 24, 24), 0);
        assert_eq!(cursor_follow_window_start(5, 24, 0), 0);
    }

    #[test]
    fn viewport_snapshot_keeps_cursor_row_visible_and_translates_cursor() {
        let snapshot = marked_snapshot(4, 4, 3, 1);

        let window = viewport_snapshot(&snapshot, 4, 2);
        assert_eq!(window.height, 2);
        assert_eq!(
            window.cursor.position.row, 1,
            "cursor row must be translated into the window (bottom row)"
        );
        assert_eq!(window.cursor.position.col, 1);
        assert_eq!(window.cells[0].glyph, 'c', "window shows source row 2");
        assert_eq!(window.cells[4].glyph, 'd', "window shows source row 3");
    }

    #[test]
    fn viewport_snapshot_keeps_top_window_while_cursor_fits() {
        let snapshot = marked_snapshot(4, 4, 1, 0);

        let window = viewport_snapshot(&snapshot, 4, 2);
        assert_eq!(window.cursor.position.row, 1);
        assert_eq!(window.cells[0].glyph, 'a', "window stays at the top");
        assert_eq!(window.cells[4].glyph, 'b');
    }

    #[test]
    fn viewport_snapshot_equal_dims_is_identity() {
        let snapshot = marked_snapshot(4, 4, 2, 3);
        let window = viewport_snapshot(&snapshot, 4, 4);
        assert_eq!(window, snapshot);
    }
}
