//! `Runtime` — Frame tick and software present path.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::band_slots::BandEdge;
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
    /// Number of per-`View` background-image blits in the presented draw
    /// list (CTX-0347). Bound by the accepted BG-7 (32 blits / 64 MiB).
    pub backgrounds: usize,
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
            backgrounds: 0,
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

/// Visual bell flash accent (CTX-0577): translucent amber, painted as a
/// one-cell-high strip on the focused view's top edge. Deliberately subtle so
/// the flash signals without obscuring content.
const BELL_FLASH_BG: bitty_render::grid::Rgba8 = [0x5A, 0x4A, 0x00, 0xB0];

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
    /// CTX-0347 per-`View` background blits, emitted for every visible leaf
    /// (reused or re-rendered) because this retryable pass rebuilds the
    /// combined list from scratch on an atlas reset.
    backgrounds: Vec<bitty_render::grid::ImageBlit>,
    /// Bytes admitted by the CTX-0347 per-frame background budget so far.
    background_bytes: usize,
    glyphs: Vec<bitty_render::grid::GlyphInstance>,
    needs_draw: bool,
    cursor: Option<CursorPaint>,
    /// Atlas epoch the glyph slots collected so far were placed at
    /// (issue #1409, CTX-0797).
    ///
    /// Set by the first leaf of an attempt and then held fixed: every later
    /// leaf must plan against the same epoch or the slots already collected
    /// address dead texels.
    atlas_epoch: Option<u64>,
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

/// CTX-0979: Core draws no workspace display (Hyprland-style). The former
/// workspace bar band snapshot helpers (`band_snapshot`,
/// `overlay_status_bar`) are deleted; plugin bands paint through
/// `paint_chrome_bands`/`paint_band_row`.
/// First source row of a viewport window that keeps `cursor_row` visible.
///
/// The window is `window` rows tall inside `src_len` rows. It stays at the
/// top while the cursor fits in the first window; once the cursor is below,
/// the window scrolls the minimum amount and clamps to the screen bottom, so
/// it ends bottom-anchored on the last rows (the live prompt case). CTX-0361.
///
/// `pub(super)` (not private): the CW hint-overlay resolver
/// ([`Runtime::cw_hint_overlay_cells`](super::Runtime::cw_hint_overlay_cells),
/// #1344) mirrors the leaf-pass window math from the sibling `cw_live`
/// module, so both windows stay one formula.
pub(super) fn cursor_follow_window_start(
    cursor_row: usize,
    src_len: usize,
    window: usize,
) -> usize {
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

/// Clips a normalized owner-grid stream span into the painted frame window
/// (CTX-0803).
///
/// The exact inverse of the pointer mapping's row translation: owner-grid row
/// `r` paints at frame-local row `r - window_start`. A span that starts above
/// the window continues from the window's first cell, and a span that ends
/// below it runs to the window's last column, so the on-screen part is tinted
/// exactly as the stream fill would tint it without the window. Returns
/// frame-local `((row, col), (row, col))`, or `None` when the span is entirely
/// outside the window or the frame has no rows or columns.
fn clip_span_to_window(
    start: CellPos,
    end: CellPos,
    window_start: usize,
    frame_rows: u16,
    frame_cols: u16,
) -> Option<((u16, u16), (u16, u16))> {
    let rows = usize::from(frame_rows);
    if rows == 0 || frame_cols == 0 {
        return None;
    }
    let window_end = window_start.saturating_add(rows);
    let start_row = usize::from(start.row);
    let end_row = usize::from(end.row);
    if end_row < window_start || start_row >= window_end {
        return None;
    }
    let (local_start_row, local_start_col) = if start_row < window_start {
        (0, 0)
    } else {
        (start_row - window_start, usize::from(start.col))
    };
    let (local_end_row, local_end_col) = if end_row >= window_end {
        (rows - 1, usize::from(frame_cols - 1))
    } else {
        (end_row - window_start, usize::from(end.col))
    };
    let to_u16 = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
    Some((
        (to_u16(local_start_row), to_u16(local_start_col)),
        (to_u16(local_end_row), to_u16(local_end_col)),
    ))
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
                // CTX-0967: active move/resize/drag transitions fade the
                // ring the same way (Hyprland-style geometry feedback).
                // Each kind is read per-leaf with a whole-surface (`None`)
                // fallback for multi-panel gestures (border-drag resize);
                // co-active kinds multiply. Geometry and terminal content
                // are never interpolated: only this Core-owned alpha moves,
                // and every factor is the final `1.0` when its transition
                // is instant or complete.
                let mut motion_factor = 1.0f32;
                for kind in [
                    AnimationKind::Move,
                    AnimationKind::Resize,
                    AnimationKind::Drag,
                ] {
                    let per_leaf = self.animation_progress(kind, Some(view_id), ctx.now);
                    let global = self.animation_progress(kind, None, ctx.now);
                    if let Some(p) = per_leaf.or(global) {
                        motion_factor *= p.clamp(0.0, 1.0);
                    }
                }
                let ring_color = bitty_render::grid::scale_alpha(
                    animated_color,
                    open_factor * ctx.workspace_factor * motion_factor,
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
    /// platform-IME caret rect (CTX-0367). The fill lands in the CTX-0347
    /// overlay layer so it stays visible above a per-`View` background image.
    /// A no-op when the window lost focus; the cursor never leaves a stale
    /// fill because it is not part of any retained leaf list. Returns whether
    /// a fill was pushed.
    fn paint_cursor(
        &mut self,
        paint: &CursorPaint,
        combined_overlay: &mut Vec<bitty_render::grid::FillRect>,
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
            combined_overlay.push(bitty_render::grid::FillRect {
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
        // CTX-0783: the embedder ticks once per dispatched event batch
        // (`ApplicationHandler::about_to_wait`), which is the boundary a
        // compositor may or may not put the commit and its echoing key in. Age
        // the post-commit claim against the clock here instead of dropping it:
        // dropping it at the first tick loses the echo whenever the two arrive
        // in consecutive batches, which is a legal compositor flush and brings
        // issue #1449 straight back. The claim goes when it is observed (the
        // echo lands) or when its window elapses, whichever comes first.
        self.age_ime_commit_key_claim(now);
        if self.tick_time_gates(now) {
            return None;
        }
        let basis = self.collect_tick_basis(now)?;
        let mut layers = self.build_leaf_primitives(&basis, now);
        if let Some(paint) = layers.cursor.take() {
            layers.any_needs_draw |= self.paint_cursor(&paint, &mut layers.combined_overlay);
        }
        self.paint_frame_overlays(&basis, now, &mut layers);
        self.paint_chrome_bands(basis.pad_px, &mut layers);
        self.paint_plugin_overlay(&basis.allocations, basis.pad_px, &mut layers);
        self.paint_kitty_images(&basis, &mut layers);
        if self.reject_stale_atlas_frame(&layers) {
            return None;
        }
        self.present_assembled_frame(basis, layers)
    }

    /// Issue #1409 (CTX-0797): refuse to present a frame whose leaf glyph
    /// slots were placed at an older atlas epoch than the live atlas.
    ///
    /// [`Self::build_leaf_primitives`] is internally frame-consistent, but the
    /// overlay phase runs after it and places its own text through
    /// `GridRenderer::overlay_text_glyphs`. That call evicts and resets the
    /// atlas wholesale when it cannot fit its line, which silently kills every
    /// leaf slot already collected: the assembled frame would then sample two
    /// atlas epochs at once, which is exactly the frame-consistency violation
    /// issue #1409 describes. Drop such a frame instead. Damage is retained
    /// (the frame is never marked presented) and the retained per-leaf stores
    /// plus `pending_full_redraw` force a full rebuild against the fresh atlas
    /// on the next tick.
    ///
    /// Liveness bound: a layout whose leaves and overlays together never fit
    /// one epoch would reset on every tick and starve presentation forever, so
    /// after [`ATLAS_REBUILD_LIMIT`] consecutive rejections the frame is
    /// presented anyway. One frame of stale texels beats a window that never
    /// paints again.
    fn reject_stale_atlas_frame(&mut self, layers: &FrameLayers) -> bool {
        if self.renderer.atlas_epoch() == layers.atlas_epoch {
            self.stale_atlas_frame_rejects = 0;
            return false;
        }
        if self.stale_atlas_frame_rejects >= ATLAS_REBUILD_LIMIT {
            self.stale_atlas_frame_rejects = 0;
            return false;
        }
        self.stale_atlas_frame_rejects = self.stale_atlas_frame_rejects.saturating_add(1);
        self.presented_leaf_frames.clear();
        self.pending_full_redraw = true;
        true
    }

    /// Phase 1 (CTX-0474): time-based gates that can defer the whole frame.
    ///
    /// Commits a due hover activation, folds the CTX-0192 paste banner
    /// between its full and flash phases, and enforces the CTX-0380
    /// synchronized-update defer window. Returns `true` when the frame must
    /// be skipped this tick (defer window still open); the caller returns
    /// `None` and retains all damage.
    fn tick_time_gates(&mut self, now: std::time::Instant) -> bool {
        // fires even when the pointer stopped moving.
        self.apply_hover_deadline(now);
        // Issues #1759/#1760: re-resolve both hovers against live grid
        // truth once per tick, before the idle short-circuit below, so a
        // link that scrolled under a stationary pointer clears instead of
        // painting stale affordance. A change forces the frame inside.
        self.revalidate_hyperlink_hover();
        self.revalidate_plaintext_hover();
        // CTX-0577 (review PX-3067): the bounded bell flash and notification
        // banner are time-expiring presentation surfaces. Expire them here —
        // before the idle short-circuit in `collect_tick_basis` — and force
        // the frame so a quiet window (no PTY bytes, no layout change) still
        // clears them on time instead of painting them indefinitely. Advancing
        // the queued notification here keeps the one-banner-at-a-time rule
        // independent of unrelated activity.
        if self.expire_bell_flash(now) {
            self.pending_full_redraw = true;
        }
        if self.expire_notification_banner(now) {
            self.pending_full_redraw = true;
        }
        if self.advance_notification_banner_at(now) {
            self.pending_full_redraw = true;
        }
        // Issue #1438: expire the pending paste here — before the idle
        // short-circuit in `collect_tick_basis` — so a quiet runtime (no PTY
        // bytes, no layout change) still auto-cancels on time instead of
        // leaving the paste pending indefinitely. The paint-phase
        // `check_and_auto_cancel_paste` below stays as defense-in-depth.
        if self.check_and_auto_cancel_paste_at(now) {
            self.pending_full_redraw = true;
        }
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
                return true;
            }
            // Bound reached: commit this frame, then open a fresh window so
            // a still-active mode does not present on every later tick.
            self.sync_defer_since = Some(now);
        } else {
            self.sync_defer_since = None;
        }
        false
    }

    /// Phase 2 (CTX-0474): collect the per-frame basis or short-circuit idle.
    ///
    /// Reflows the layout, resolves focus/kitty-origin/alt-latch, compares
    /// per-origin generations and layout/focus edits, advances animations,
    /// and returns `None` (after recording the consumed generations) when
    /// the frame is idle or the layout is empty. The returned [`TickBasis`]
    /// is owned so every later phase can still take `&mut self`.
    fn collect_tick_basis(&mut self, now: std::time::Instant) -> Option<TickBasis> {
        // Reflow layout tree into container before rendering so leaf Views
        // carry deterministic origins/sizes for this frame. This is headless
        // and deterministic: same layout + container always yields same
        // frames. CTX-0294: frames come from the Core-owned px decoration
        // solver composed with the CTX-0177 cell gaps, so live present paints
        // the accepted gaps/border/radius instead of cell-aligned tiling.
        // The reflow mutates leaf Views to the decorated *content* grid so
        // scroll/selection/PTY geometry matches the painted viewport.
        // CTX-0873: safety net for any bar-presence change that bypassed the
        // explicit funnels (for example a `layout_mut` escape); a no-op when
        // the band already matches.
        self.refresh_chrome_band();
        let frames = self.present_frames();
        self.reflow_present_layout(&frames);

        let snapshot = self.state.snapshot();
        let mut pending_full = self.pending_full_redraw;
        // Collect allocations deterministically BEFORE the idle check so
        // geometry-only changes are visible (CTX-0228). `present_frames`
        // is pure and bounded by the leaf count.
        let allocations = frames;
        let focused = self.focus.focused();
        // CTX-0248 / CTX-0254 / #1550: Kitty alternate-screen tracking across all
        // visible allocations. An alternate-screen transition on any visible origin
        // forces a full present even when the grid generation is unchanged, so
        // entering alt clears that origin's painted images (and leaving alt repaints
        // the restored grid) instead of idling on a stale frame. Other origins are
        // untouched (CTX-0254 `clear_origin`).
        let mut current_alt_screens = std::collections::BTreeSet::new();
        for frame in &allocations {
            let pane_origin: Option<u64> = if self.pane_sessions.contains_key(&frame.view) {
                Some(frame.view.0)
            } else if Some(frame.view) == self.primary_view {
                None
            } else {
                continue;
            };
            let is_alt = match pane_origin {
                Some(token) => self
                    .pane_sessions
                    .get(&ViewId::new(token))
                    .map(|sess| sess.state.alt_screen_active())
                    .unwrap_or(false),
                None => self.state.alt_screen_active(),
            };
            if is_alt {
                current_alt_screens.insert(pane_origin);
            }
        }
        if current_alt_screens != self.kitty_alt_screens_latched {
            self.kitty_alt_screens_latched = current_alt_screens;
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

        // CTX-0979: Core draws no workspace display; workspace
        // switch/new/close/rename with a quiet grid still needs a frame
        // when plugin bands or layout changed (band damage only; geometry
        // reflows through the exclusive-zone budget in
        // `refresh_chrome_band` above). No Core bar text is compared.
        // CTX-0946 C2: a plugin mount/update/unmount with a quiet grid
        // still needs a frame (band damage only; geometry reflows through
        // the exclusive-zone budget in `refresh_chrome_band` above).
        let band_versions = self.chrome_band_versions();
        if band_versions != self.last_presented_bands {
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
        let full_frame = pending_full || last == u64::MAX;

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
        // outline color. CTX-0967 adds the move/resize/drag factors (applied
        // inside the loop beside `open_factor`): they fade the same ring
        // while a geometry gesture settles. All default to the final value
        // when the transition is instant or already complete. Purely
        // presentation: no grid, cursor, scrollback, or Terminal Truth is
        // interpolated.
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
        Some(TickBasis {
            snapshot,
            allocations,
            focused,
            full_frame,
            current_gen,
            view_map,
            pad_px,
            ring_ctx,
            cells_before,
            glyphs_before,
            band_versions,
        })
    }
    /// Phase 3 (CTX-0474): build the retryable combined leaf primitive pass.
    ///
    /// Walks every visible allocation, reusing a retained leaf whose origin
    /// and scroll offset are unchanged and re-rendering the rest from its
    /// own damage ring; retries once when an atlas exhaustion reset
    /// invalidates the retained lists mid-pass. Returns the assembled
    /// [`FrameLayers`] with the overlay and image layers still empty.
    fn build_leaf_primitives(&mut self, basis: &TickBasis, now: std::time::Instant) -> FrameLayers {
        let snapshot = &basis.snapshot;
        let allocations = &basis.allocations;
        let view_map = &basis.view_map;
        let pad_px = basis.pad_px;
        let current_gen = basis.current_gen;
        let ring_ctx = basis.ring_ctx;
        let mut full_frame = basis.full_frame;
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
            // Issue #1409 (CTX-0797): set when a leaf planned against a newer
            // atlas epoch than the slots already collected in this attempt.
            let mut stale_epoch = false;

            // CTX-1076 (issue #1815): the window background image paints
            // behind the grid (whole window, including the padding band),
            // below per-`View` backgrounds, overlay fills, and glyphs. The
            // decode already happened at config reconcile; this path is a
            // pure cache lookup plus a bounded nearest-neighbor scale, sharing
            // the BG-7 budget (32 blits / 64 MiB) with the per-`View` pass so
            // a hostile config can never allocate beyond the accepted present
            // bound. Resolution is skipped entirely when no window image is
            // configured (`has_window_background_image`), keeping the common
            // image-less frame free of any resolve or allocation.
            if self.config.has_window_background_image() {
                if let Some(path) = self.config.window_background_image.as_deref() {
                    if let Some(key) = self.background_keys.get(path).cloned() {
                        if let Some(image) = self.backgrounds.get(&key) {
                            let window_extent = self
                                .surface
                                .extent()
                                .unwrap_or_else(|| self.config.window_extent());
                            if window_extent.width() > 0 && window_extent.height() > 0 {
                                let outer = bitty_rich::RectPx::new(
                                    0,
                                    0,
                                    window_extent.width(),
                                    window_extent.height(),
                                );
                                let fit = bitty_rich::BackgroundFit::parse(
                                    &self.config.window_background_fit,
                                )
                                .unwrap_or_default();
                                let raster_key = bitty_rich::BackgroundRasterKey {
                                    source: bitty_rich::BackgroundRasterKeySource::from(&key),
                                    fit,
                                    dest: outer,
                                    dpi_bits: self.scale_factor.get().to_bits(),
                                };
                                if let Some(blit) =
                                    self.background_rasters.get_or_rasterize(raster_key, &image)
                                {
                                    let position =
                                        bitty_render::window::WindowBackgroundPosition::parse(
                                            &self.config.window_background_position,
                                        )
                                        .unwrap_or_default();
                                    // Reposition from the centered raster dest
                                    // to the configured position: the raster
                                    // cache stays position-agnostic (centered),
                                    // and only `fit`/`center` letterbox moves.
                                    let outer_render = bitty_render::geometry::RectPx::new(
                                        0,
                                        0,
                                        window_extent.width(),
                                        window_extent.height(),
                                    );
                                    let inner_render = bitty_render::geometry::RectPx::new(
                                        blit.dest.x,
                                        blit.dest.y,
                                        blit.dest.width,
                                        blit.dest.height,
                                    );
                                    let positioned =
                                        bitty_render::window::reposition_window_background(
                                            &outer_render,
                                            &inner_render,
                                            position,
                                        );
                                    let opacity =
                                        bitty_render::window::sanitize_window_background_opacity(
                                            self.config.window_background_opacity,
                                        );
                                    let mut rgba = blit.rgba.clone();
                                    bitty_render::window::dim_rgba_alpha(&mut rgba, opacity);
                                    let bytes = u64::from(positioned.width)
                                        .saturating_mul(u64::from(positioned.height))
                                        .saturating_mul(4)
                                        as usize;
                                    if built.backgrounds.len()
                                        < bitty_rich::BG_PRESENT_MAX_BLITS_PER_FRAME
                                        && built.background_bytes.saturating_add(bytes)
                                            <= bitty_rich::BG_PRESENT_MAX_BYTES_PER_FRAME
                                    {
                                        if let Ok(entry) =
                                            bitty_render::grid::ImageBlit::try_new(positioned, rgba)
                                        {
                                            built.background_bytes =
                                                built.background_bytes.saturating_add(bytes);
                                            built.backgrounds.push(entry);
                                            built.needs_draw = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for frame in allocations {
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

                // CTX-0347 (RFC-0001/OQ-042): the resolved per-`View` background
                // image paints inside the content rect, above cell backgrounds
                // and the decoration ring, below overlay fills and glyphs. It
                // is emitted for reused and freshly rendered leaves alike — the
                // combined list is rebuilt every frame, and a reused leaf only
                // retains fills/glyphs. The decode already happened at config
                // reconcile; this path is a pure cache lookup plus a bounded
                // nearest-neighbor scale. The per-frame budget mirrors BG-7
                // (32 blits / 64 MiB staging) so a hostile layout can never
                // allocate beyond the accepted present bound; refused entries
                // skip fail-closed. Resolution is skipped entirely when no
                // image can match (`has_background_images`), keeping the common
                // no-image frame free of the per-View resolve and its fit
                // allocation.
                if frame.content.width > 0
                    && frame.content.height > 0
                    && self.config.has_background_images()
                {
                    let resolved = self.view_background_for(view_id);
                    if let Some(path) = resolved.image.as_deref() {
                        if let Some(key) = self.background_keys.get(path).cloned() {
                            if let Some(image) = self.backgrounds.get(&key) {
                                let dest = bitty_rich::RectPx::new(
                                    origin_px_x,
                                    origin_px_y,
                                    frame.content.width,
                                    frame.content.height,
                                );
                                let fit = bitty_rich::BackgroundFit::parse(&resolved.fit)
                                    .unwrap_or_default();
                                let raster_key = bitty_rich::BackgroundRasterKey {
                                    source: bitty_rich::BackgroundRasterKeySource::from(&key),
                                    fit,
                                    dest,
                                    dpi_bits: self.scale_factor.get().to_bits(),
                                };
                                if let Some(blit) =
                                    self.background_rasters.get_or_rasterize(raster_key, &image)
                                {
                                    let bytes = u64::from(blit.dest.width)
                                        .saturating_mul(u64::from(blit.dest.height))
                                        .saturating_mul(4)
                                        as usize;
                                    if built.backgrounds.len()
                                        < bitty_rich::BG_PRESENT_MAX_BLITS_PER_FRAME
                                        && built.background_bytes.saturating_add(bytes)
                                            <= bitty_rich::BG_PRESENT_MAX_BYTES_PER_FRAME
                                    {
                                        if let Ok(entry) = bitty_render::grid::ImageBlit::try_new(
                                            bitty_render::geometry::RectPx::new(
                                                blit.dest.x,
                                                blit.dest.y,
                                                blit.dest.width,
                                                blit.dest.height,
                                            ),
                                            blit.rgba.clone(),
                                        ) {
                                            built.background_bytes =
                                                built.background_bytes.saturating_add(bytes);
                                            built.backgrounds.push(entry);
                                            built.needs_draw = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

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
                            snapshot,
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
                // decides: every other session-less leaf (spawn failures,
                // recipe-less leaves) presents erased, so a session-less View
                // never paints another View's grid.
                let pane_snap: Option<Snapshot> = match self.pane_sessions.get(&view_id) {
                    Some(sess) => Some(sess.state.snapshot()),
                    None if Some(view_id) == self.primary_view => Some(snapshot.clone()),
                    None => None,
                };
                // Erased source for session-less leaves that do not own the
                // primary; `viewport_snapshot` pads it to the allocation below.
                let erased_snap: Option<Snapshot> = if pane_snap.is_none() {
                    Some(erased_snapshot(snapshot))
                } else {
                    None
                };
                let base_snap: &Snapshot = pane_snap
                    .as_ref()
                    .or(erased_snap.as_ref())
                    .unwrap_or(snapshot);
                // Determine viewport snapshot: when view scroll_offset !=0,
                // visible_cells composites scrollback.
                let view_snapshot = if let Some(v) = view {
                    // #1338 fail-closed: the alternate screen owns no
                    // scrollback view — a stale offset (scrolled on primary,
                    // then entered alt) must not composite primary history
                    // over the alt grid.
                    let on_alt = match self.pane_sessions.get(&view_id) {
                        Some(sess) => sess.state.alt_screen_active(),
                        None => self.state.alt_screen_active(),
                    };
                    if v.scroll_offset() != 0 && pane_snap.is_some() && !on_alt {
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
                // Issue #1409 (CTX-0797): the first leaf of this attempt fixes
                // the atlas epoch every glyph slot in the frame must resolve
                // against. `render` is internally frame-consistent, but a later
                // leaf can exhaust the atlas and be planned against a fresh
                // epoch, which kills the slots earlier leaves already pushed
                // into `built`. Abandon the attempt rather than compose a
                // mixed-epoch frame; the enclosing loop rebuilds every leaf
                // against the fresh atlas. At the `ATLAS_REBUILD_LIMIT` bound
                // the attempt is kept as-is: a working set that never fits one
                // epoch must still present something (liveness over a
                // perfectly consistent frame that never arrives).
                let frame_epoch = *built.atlas_epoch.get_or_insert(list.atlas_epoch);
                if !list.is_atlas_epoch_valid(frame_epoch) && attempt < ATLAS_REBUILD_LIMIT {
                    stale_epoch = true;
                    break;
                }
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

            // CTX-0979: Core draws no workspace display; only plugin bands
            // paint (their own pass below). No Core bar paint here.

            if stale_epoch {
                // Partial attempt: every slot collected before the reset is
                // dead, so drop the retained stores and rebuild from scratch.
                self.presented_leaf_frames.clear();
                full_frame = true;
                continue;
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
        FrameLayers {
            combined_fills: built.fills,
            combined_rounded: built.rounded,
            combined_backgrounds: built.backgrounds,
            combined_glyphs: built.glyphs,
            any_needs_draw: built.needs_draw,
            cursor: built.cursor,
            // Issue #1409 (CTX-0797): the epoch the surviving attempt placed
            // its slots at. `built.atlas_epoch` is unset only when no leaf
            // rendered, in which case the live epoch is trivially correct.
            atlas_epoch: built
                .atlas_epoch
                .unwrap_or_else(|| self.renderer.atlas_epoch()),
            ..FrameLayers::default()
        }
    }

    /// Phase 4 (CTX-0474): paint every overlay layer in the accepted order.
    ///
    /// Closing rings, selection highlight, IME preedit, the three pending
    /// confirmation banners, the help panel, the scrollbar thumb, then the
    /// Leader hint overlay. The hint overlay paints last so its label pills
    /// stay above grid chrome while armed (issue #1344). The
    /// kitty image layer is painted afterwards by [`Self::paint_kitty_images`].
    fn paint_frame_overlays(
        &mut self,
        basis: &TickBasis,
        now: std::time::Instant,
        layers: &mut FrameLayers,
    ) {
        self.paint_closing_rings(basis.pad_px, now, layers);
        self.paint_selection_highlight(&basis.allocations, &basis.view_map, basis.pad_px, layers);
        self.paint_plaintext_hover(&basis.allocations, basis.pad_px, layers);
        self.paint_ime_preedit(layers);
        self.paint_pending_banners(
            &basis.allocations,
            &basis.view_map,
            basis.pad_px,
            now,
            layers,
        );
        self.paint_hyperlink_hover(&basis.allocations, basis.pad_px, layers);
        self.paint_help_overlay(&basis.allocations, &basis.view_map, basis.pad_px, layers);
        self.paint_bell_and_notification(&basis.allocations, basis.pad_px, now, layers);
        self.paint_scrollbar_overlay(layers);
        self.paint_hint_overlay(&basis.allocations, basis.pad_px, layers);
    }

    /// CTX-0577 (M1-16): the bounded visual bell flash and the single
    /// notification banner.
    ///
    /// Both are presentation-only overlays: the flash is a short, self-
    /// expiring border tint (never stacked), and at most one notification
    /// banner is shown at a time (bounded text). Neither mutates grid truth
    /// or the layout, and both are driven by the rate-limited policy state
    /// (`runtime::bell`), so hostile PTY output cannot paint an unbounded
    /// surface.
    fn paint_bell_and_notification(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        now: std::time::Instant,
        layers: &mut FrameLayers,
    ) {
        // Expiry and queue advancement already ran in `tick_time_gates` (so
        // they fire on a quiet window too); read the resolved surface here.
        let flash_active = self.visual_bell_active_at(now);
        let banner = self.notification_banner_at(now);
        let Some(frame) = self
            .focused_view()
            .or_else(|| allocations.first().map(|f| f.view))
            .and_then(|view| allocations.iter().find(|f| f.view == view))
        else {
            return;
        };
        let live = self.live_cell_metrics();
        // Visual bell: a one-cell-high accent strip along the top edge of the
        // focused view, repainted only while the bounded flash window is
        // open.
        if flash_active && frame.cols > 0 && frame.content.width > 0 {
            let width = px_span(frame.cols, live.width);
            layers.combined_overlay.push(bitty_render::grid::FillRect {
                rect: bitty_render::geometry::RectPx::new(
                    px_add(pad_px, frame.content.x),
                    px_add(pad_px, frame.content.y),
                    width,
                    live.height,
                ),
                color: BELL_FLASH_BG,
            });
            layers.any_needs_draw = true;
        }
        // Notification banner: right-aligned bottom pill, identical in shape
        // to the pending-confirmation banners (one per frame, never stacked).
        if let Some(text) = banner {
            self.paint_banner_pill(allocations, Some(frame.view), &text, pad_px, layers);
        }
    }

    /// RFC-0002 close transition: fade the retained rings of Views closed
    /// during the current transition (CTX-0474 extraction; body moved
    /// verbatim from `tick_at`).
    fn paint_closing_rings(
        &mut self,
        pad_px: i32,
        now: std::time::Instant,
        layers: &mut FrameLayers,
    ) {
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
            layers
                .combined_rounded
                .push(bitty_render::grid::RoundedFill {
                    frame: ring_frame,
                    border: closing.border,
                    radius: closing.radius,
                    color: bitty_render::grid::scale_alpha(closing.color, factor),
                });
            layers.any_needs_draw = true;
        }
    }

    /// CTX-0158 selection highlight, painted at the owner's frame (CTX-0803;
    /// CTX-1021 scrolled viewport for issue #1807).
    fn paint_selection_highlight(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        view_map: &std::collections::HashMap<ViewId, View>,
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        // Selection highlight overlay (CTX-0158, ghostty selection rendering):
        // presentation-only fills in the theme selection color, painted above
        // cell backgrounds. `DrawList` paint order is fills first, then
        // glyphs, so the highlight tints the background while text stays
        // legible on top. Bounded: at most one rect per selected row.
        //
        // CTX-0803 (#1476): the highlight paints at the frame of the View that
        // *owns* the selection, against that View's grid dimensions and
        // through the same row-window translation the pointer mapping uses
        // (`owner_row_window_start`), so hit testing and painting can never
        // disagree. CTX-1021 (#1807): when the owner is scrolled into history
        // the selection addresses the viewport composite, so it paints
        // directly in frame-local coordinates (the inverse of the scrolled
        // pointer mapping) instead of being suppressed.
        let Some(owner) = self.selection_owner() else {
            return;
        };
        let Some(sel) = self.selection() else {
            return;
        };
        if sel.is_empty() {
            return;
        }
        let scrolled = view_map
            .get(&owner)
            .is_some_and(|view| view.scroll_offset() != 0)
            && self.is_viewport_scrolled(owner);
        let Some(frame) = allocations.iter().find(|frame| frame.view == owner) else {
            return;
        };
        if scrolled {
            // Viewport selection is already frame-local: paint it directly,
            // clipped to the frame by the fill builder below.
            let norm = sel.normalized();
            let start = (norm.start.row, norm.start.col);
            let end = (norm.end.row, norm.end.col);
            let live = self.live_cell_metrics();
            let rects = bitty_render::grid::selection_fill_rects_in(
                &self.config.theme,
                start,
                end,
                usize::from(frame.cols),
                usize::from(frame.rows),
                live,
            );
            if rects.is_empty() {
                return;
            }
            let origin_px_x = px_add(pad_px, frame.content.x);
            let origin_px_y = px_add(pad_px, frame.content.y);
            for mut fill in rects {
                fill.rect.x = px_add(fill.rect.x, origin_px_x);
                fill.rect.y = px_add(fill.rect.y, origin_px_y);
                layers.combined_overlay.push(fill);
            }
            layers.any_needs_draw = true;
            return;
        }
        let Some(state) = self.live_view_state(owner) else {
            return;
        };
        let grid_w = state.width();
        let grid_h = state.height();
        let window_start = self.owner_row_window_start(owner, frame.rows);
        let norm = sel.normalized();
        // Frame-local span: the inverse of the pointer mapping. The part of
        // the span above or below the painted window is clipped away; the
        // fill builder clips columns to the frame below.
        let Some((start, end)) =
            clip_span_to_window(norm.start, norm.end, window_start, frame.rows, frame.cols)
        else {
            return;
        };
        let live = self.live_cell_metrics();
        let rects = bitty_render::grid::selection_fill_rects_in(
            &self.config.theme,
            start,
            end,
            grid_w.min(usize::from(frame.cols)),
            grid_h.min(usize::from(frame.rows)),
            live,
        );
        if rects.is_empty() {
            return;
        }
        let origin_px_x = px_add(pad_px, frame.content.x);
        let origin_px_y = px_add(pad_px, frame.content.y);
        for mut fill in rects {
            fill.rect.x = px_add(fill.rect.x, origin_px_x);
            fill.rect.y = px_add(fill.rect.y, origin_px_y);
            layers.combined_overlay.push(fill);
        }
        layers.any_needs_draw = true;
    }

    /// Plaintext URL hover feedback: underline highlight (issue #1760).
    ///
    /// Presentation-only like the selection highlight: one underline bar
    /// over the hovered span in the theme foreground. The owner-grid row
    /// translates through `owner_row_window_start` exactly like the
    /// selection highlight, so paint and hit-test agree. Skipped when the
    /// span scrolled out of the painted window (the tick revalidate clears
    /// it on the next frame). Thickness mirrors the OSC 8 underline
    /// (`height / 8` clamped to `1..=2`) without taking a `bitty-rich`
    /// dependency: the formula is duplicated by value and pinned by the
    /// existing `underline_thickness_mirror` test.
    fn paint_plaintext_hover(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        if self.hover_suppressed_by_overlay {
            return;
        }
        let Some(hover) = self.hovered_plaintext_span() else {
            return;
        };
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        let Some(frame) = allocations.iter().find(|frame| frame.view == hover.view) else {
            return;
        };
        if frame.rows == 0 || frame.cols == 0 {
            return;
        }
        let start = self.owner_row_window_start(hover.view, frame.rows);
        let Some(local_row) = hover.row.checked_sub(start) else {
            return;
        };
        if local_row >= usize::from(frame.rows) {
            return;
        }
        let max_col = usize::from(frame.cols).saturating_sub(1);
        let col_start = hover.col_start.min(max_col);
        let col_end = hover.col_end.min(max_col);
        if col_end < col_start {
            return;
        }
        let thickness = (live.height / 8).clamp(1, 2);
        let origin_x = px_add(pad_px, frame.content.x);
        let origin_y = px_add(pad_px, frame.content.y);
        let x = px_add(origin_x, px_side(px_span_usize(col_start, live.width)));
        let width = px_span_usize(col_end - col_start + 1, live.width);
        let row_y = px_add(origin_y, px_side(px_span_usize(local_row, live.height)));
        let y = px_add(
            row_y,
            px_side(live.height.saturating_sub(thickness.saturating_mul(2))),
        );
        layers.combined_overlay.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(x, y, width, thickness),
            color: self.config.theme.foreground,
        });
        layers.any_needs_draw = true;
    }

    /// CTX-0367 inline IME preedit overlay (CTX-0474 extraction; body moved
    /// verbatim from `tick_at`).
    fn paint_ime_preedit(&mut self, layers: &mut FrameLayers) {
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
                    layers.combined_overlay.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y,
                            preedit_width,
                            live.height.max(1),
                        ),
                        color: [0x33, 0x33, 0x33, 0xCC],
                    });
                    layers.combined_overlay.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            base_x,
                            underline_y,
                            preedit_width,
                            2,
                        ),
                        color: [0xFF, 0xFF, 0x00, 0xFF],
                    });
                    layers.combined_glyphs.extend(glyphs);
                    // Composition caret: static bar at the IME cursor cell
                    // (blink policy stays an embedder concern, matching the
                    // Terminal cursor gate). Clamped into the drawn span.
                    let caret_bar_cells = caret_cells.min(drawn_cells);
                    let caret_bar_x = px_offset_cells(base_x, caret_bar_cells as u16, live.width);
                    layers.combined_overlay.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            caret_bar_x,
                            base_y,
                            2,
                            live.height.max(1),
                        ),
                        color: self.config.theme.cursor,
                    });
                    layers.any_needs_draw = true;
                }
            }
        }
    }

    /// Paints one right-aligned bottom-row confirmation pill onto the overlay
    /// layer (CTX-0474 extraction: shared verbatim body of the CTX-0186 paste,
    /// CTX-0257 workspace-close, and CTX-0370 view/window-close banners).
    /// No-op when there is no target leaf or its frame is empty.
    fn paint_banner_pill(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        focused: Option<ViewId>,
        banner: &str,
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        let Some(fid) = focused else {
            return;
        };
        let Some(frame) = allocations.iter().find(|frame| frame.view == fid) else {
            return;
        };
        if frame.rows == 0 || frame.cols == 0 {
            return;
        }
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
            px_offset_cells(frame.content.y, frame.rows.saturating_sub(1), live.height),
            pad_px,
        );
        layers.combined_overlay.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(origin_px_x, banner_y, pill_w, live.height),
            color: bitty_render::grid::PENDING_PASTE_BANNER_BG,
        });
        let glyphs = self.renderer.overlay_text_glyphs(
            banner,
            (origin_px_x, banner_y),
            max_cells,
            bitty_render::grid::PENDING_PASTE_BANNER_FG,
        );
        layers.combined_glyphs.extend(glyphs);
        layers.any_needs_draw = true;
    }

    /// CTX-0186/CTX-0257/CTX-0370 pending confirmation banners in paint
    /// order (paste, workspace close, view/window close), each gated on its
    /// own pending predicate (CTX-0474 extraction).
    fn paint_pending_banners(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        view_map: &std::collections::HashMap<ViewId, View>,
        pad_px: i32,
        now: std::time::Instant,
        layers: &mut FrameLayers,
    ) {
        let focused = self.focused_view().or(view_map.keys().next().copied());
        // Issue #1438: auto-cancel expired pending paste before presenting banner.
        // Expiry already ran in `tick_time_gates`; this is defense-in-depth
        // for callers that paint without ticking.
        if self.has_pending_paste() {
            if self.check_and_auto_cancel_paste() {
                // Paste was auto-cancelled; request redraw to clear banner.
                self.pending_full_redraw = true;
            } else if let Some(banner) = self.paste_banner_text_at(now) {
                self.paint_banner_pill(allocations, focused, &banner, pad_px, layers);
            }
        }
        if self.has_pending_ws_close() {
            if let Some(banner) = self.ws_close_banner_text() {
                self.paint_banner_pill(allocations, focused, &banner, pad_px, layers);
            }
        }
        if self.has_pending_close_confirm() {
            if let Some(banner) = self.close_confirm_banner_text() {
                self.paint_banner_pill(allocations, focused, &banner, pad_px, layers);
            }
        }
    }

    /// OSC 8 hover feedback: underline highlight plus sanitized URL preview
    /// (issue #1759, R-005).
    ///
    /// Presentation-only like every other overlay: one underline bar over
    /// the hovered span (same `height / 8` clamped thickness as
    /// [`hyperlink_overlay_rects`](bitty_rich::hyperlink::hyperlink_overlay_rects))
    /// in the theme foreground, plus the sanitized target URL as a
    /// bottom-right pill in the hovered View (reusing `paint_banner_pill`,
    /// so the preview is bounded and anti-spoofed by
    /// [`Self::hovered_hyperlink_preview`]). The owner-grid row translates
    /// through [`owner_row_window_start`](super::Runtime::owner_row_window_start)
    /// exactly like the selection highlight, so paint and hit-test agree.
    /// Skipped when the span scrolled out of the painted window (the hover
    /// revalidate on tick clears it on the next frame).
    fn paint_hyperlink_hover(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        if self.hover_suppressed_by_overlay {
            return;
        }
        let Some(hover) = self.hovered_hyperlink_span() else {
            return;
        };
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        let Some(frame) = allocations.iter().find(|frame| frame.view == hover.view) else {
            return;
        };
        if frame.rows == 0 || frame.cols == 0 {
            return;
        }
        let start = self.owner_row_window_start(hover.view, frame.rows);
        let Some(local_row) = hover.row.checked_sub(start) else {
            return;
        };
        if local_row >= usize::from(frame.rows) {
            return;
        }
        let max_col = usize::from(frame.cols).saturating_sub(1);
        let col_start = hover.col_start.min(max_col);
        let col_end = hover.col_end.min(max_col);
        if col_end < col_start {
            return;
        }
        let thickness = bitty_rich::hyperlink::underline_thickness(live.height);
        let origin_x = px_add(pad_px, frame.content.x);
        let origin_y = px_add(pad_px, frame.content.y);
        let x = px_add(origin_x, px_side(px_span_usize(col_start, live.width)));
        let width = px_span_usize(col_end - col_start + 1, live.width);
        let row_y = px_add(origin_y, px_side(px_span_usize(local_row, live.height)));
        let y = px_add(
            row_y,
            px_side(live.height.saturating_sub(thickness.saturating_mul(2))),
        );
        layers.combined_overlay.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(x, y, width, thickness),
            color: self.config.theme.foreground,
        });
        layers.any_needs_draw = true;
        if let Some(preview) = self.hovered_hyperlink_preview() {
            self.paint_banner_pill(allocations, Some(hover.view), &preview, pad_px, layers);
        }
    }

    /// CTX-0265 which-key help panel (CTX-0474 extraction; body moved
    /// verbatim from `tick_at`).
    fn paint_help_overlay(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        view_map: &std::collections::HashMap<ViewId, View>,
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        // Help popup panel (CTX-0265, 009 which-key): centered floating
        // overlay listing the live registry rows. Presentation-only like
        // the banners above (fills + glyphs, never grid truth); painted
        // after the pills so the panel reads on top on the rare frames
        // where both coincide. Visibility/readiness live in
        // `runtime::help`; toggle/dismiss paths repaint via
        // `pending_full_redraw`.
        if self.paint_help_panel(
            allocations,
            view_map,
            pad_px,
            &mut layers.combined_overlay,
            &mut layers.combined_glyphs,
        ) {
            layers.any_needs_draw = true;
        }
    }

    /// CTX-0181 overlay scrollbar thumb (CTX-0474 extraction; body moved
    /// verbatim from `tick_at`).
    fn paint_scrollbar_overlay(&mut self, layers: &mut FrameLayers) {
        // Overlay scrollbar thumb (CTX-0181): a presentation-only FillRect        // on the focused leaf's right edge, painted above grid content like
        // the selection highlight. Never grid truth: no layout, container,
        // or cell mutation, and `hidden` (default) resolves to no fill.
        // Visibility is latched so `auto` hover/proximity transitions stay
        // headless-observable without screenshots.
        let scrollbar_now = self.scrollbar_thumb_fill();
        let paints = scrollbar_now.is_some();
        if let Some(fill) = scrollbar_now {
            layers.combined_overlay.push(fill);
            layers.any_needs_draw = true;
        }
        self.scrollbar_visible = paints;
    }

    /// Leader hint overlay (issue #1344: the missing
    /// [`HintOverlayPresent`](crate::cw_present::HintOverlayPresent)
    /// consumer).
    ///
    /// Resolves the armed session's overlay to paint cells via
    /// [`Runtime::cw_hint_overlay_cells`](super::Runtime::cw_hint_overlay_cells)
    /// and paints one single-cell-high pill per entry: a theme-selection
    /// fill (the pair the theme guarantees legible, like the selection
    /// highlight) plus label glyphs in the theme foreground through the
    /// same overlay-text path the IME preedit uses. kitty/ghostty feel:
    /// ephemeral label badges sitting on their targets, gone on disarm.
    ///
    /// Presentation-only like every other overlay: no grid, scrollback,
    /// fold, session, or layout mutation. Fail-closed twice — the resolver
    /// yields no cells while disarmed or unplaceable, and this paint skips
    /// any cell whose frame vanished or whose pill would overdraw past the
    /// frame's right edge (never into a neighbour pane).
    fn paint_hint_overlay(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        // Owned cells first: the resolver borrows `self` immutably and the
        // glyph path below needs `&mut`, so no borrow is held across.
        let cells = self.cw_hint_overlay_cells(allocations);
        if cells.is_empty() {
            return;
        }
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        for cell in &cells {
            let Some(frame) = allocations.iter().find(|frame| frame.view == cell.view) else {
                continue;
            };
            let width_cells = cell.width_cells();
            if width_cells == 0
                || cell.col as usize + width_cells > frame.cols as usize
                || cell.row >= frame.rows
            {
                continue;
            }
            let origin_x = px_add(
                px_add(pad_px, frame.content.x),
                px_offset_cells(0, cell.col, live.width),
            );
            let origin_y = px_add(
                px_add(pad_px, frame.content.y),
                px_offset_cells(0, cell.row, live.height),
            );
            layers.combined_overlay.push(bitty_render::grid::FillRect {
                rect: bitty_render::geometry::RectPx::new(
                    origin_x,
                    origin_y,
                    px_span_usize(width_cells, live.width),
                    live.height,
                ),
                color: self.config.theme.selection,
            });
            let glyphs = self.renderer.overlay_text_glyphs(
                &cell.label,
                (origin_x, origin_y),
                width_cells,
                self.config.theme.foreground,
            );
            layers.combined_glyphs.extend(glyphs);
            layers.any_needs_draw = true;
        }
    }

    /// Renders plugin-mounted chrome bands (CTX-0911, issue #1570; CTX-0946
    /// C2/C3).
    ///
    /// Walks each visible mounted UiNode tree and renders its flattened
    /// spans with host-resolved `fg`/`bg` theme tokens and synthetic-bold
    /// double-strike glyphs. `Row` children join horizontally; `List`/`Column`
    /// nodes are treated as `Row` (vertical stacking deferred). Bands render
    /// after overlays so they appear above terminal content.
    ///
    /// Horizontal bands stack from the window edge inward over visible bands
    /// only ([`Runtime::visible_band_row`], CTX-0923/CTX-0946): hidden
    /// (empty-text) bands take no row. Every band paints at most its granted
    /// one-row band rectangle — text is clipped to the window width, and a
    /// geometry violation (a band overlapping another band, the Core bar, or
    /// the layout container) skips the whole band, never a partial paint,
    /// and counts a diagnostic.
    fn paint_chrome_bands(&mut self, pad_px: i32, layers: &mut FrameLayers) {
        use super::band_host::{band_is_visible, flatten_band_runs};

        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        let window = self.window_cells();
        if window.width == 0 || window.height == 0 {
            return;
        }
        let band_cells = usize::from(window.width);
        let default_fg = self.config.theme.foreground;
        let default_bg = self.config.theme.background;

        // Owned snapshot first: the renderer takes `&mut self` below.
        let mut plan: Vec<(BandEdge, u16, super::BandContent)> = Vec::new();
        for edge in [BandEdge::Top, BandEdge::Bottom] {
            for (index, band) in self.visible_edge_bands(edge).into_iter().enumerate() {
                if !band_is_visible(&band.root) {
                    continue;
                }
                let Some(row) = self.visible_band_row(edge, index) else {
                    continue;
                };
                plan.push((edge, row, band.clone()));
            }
        }
        if plan.is_empty() {
            return;
        }
        // Overlap fail-closed: rows claimed twice deny every claimant on
        // the shared row (never partial paint). CTX-0979: no Core bar row
        // exists to collide with.
        let mut denied_rows: Vec<u16> = Vec::new();
        for (index, (_, row, _)) in plan.iter().enumerate() {
            let duplicate = plan[..index].iter().any(|(_, other, _)| other == row)
                || plan[index + 1..].iter().any(|(_, other, _)| other == row);
            if duplicate && !denied_rows.contains(row) {
                denied_rows.push(*row);
            }
        }
        // C3 backstop: a band row inside the layout container means the
        // exclusive zone lost a row it owns — skip the band, never paint it.
        let container = self.container;
        let container_end = container.y.saturating_add(container.height);
        let in_container = |row: u16| row >= container.y && row < container_end;

        for (_, row, band) in &plan {
            if denied_rows.contains(row) || in_container(*row) {
                self.band_stats.paint_violations =
                    self.band_stats.paint_violations.saturating_add(1);
                continue;
            }
            let (text, runs) = flatten_band_runs(&band.root);
            if text.is_empty() {
                continue;
            }
            self.paint_band_row(
                pad_px, layers, *row, &text, &runs, band_cells, window.x, default_fg, default_bg,
            );
        }
    }

    /// Paints one clipped band row from flattened spans (CTX-0946 C2).
    ///
    /// Damage is bounded to the granted band rectangle: spans are clipped to
    /// `band_cells` display cells, and every fill plus glyph batch derives
    /// from the clipped spans. Unknown theme tokens substitute the span
    /// default and count a diagnostic; bold spans gain a 1px x-offset glyph
    /// duplicate (synthetic double-strike — the overlay rasterizer owns one
    /// face, so emboldening stays a paint-layer concern).
    #[allow(clippy::too_many_arguments)]
    fn paint_band_row(
        &mut self,
        pad_px: i32,
        layers: &mut FrameLayers,
        row: u16,
        text: &str,
        runs: &[super::band_host::BandRun],
        band_cells: usize,
        band_x: u16,
        default_fg: bitty_render::grid::Rgba8,
        default_bg: bitty_render::grid::Rgba8,
    ) {
        use super::band_host::resolve_band_token;

        let live = self.live_cell_metrics();
        let cell_h = i32::try_from(live.height).unwrap_or(i32::MAX);
        let cell_w = live.width;
        let origin_y = px_add(pad_px, i32::from(row).saturating_mul(cell_h));
        let chars: Vec<char> = text.chars().collect();
        let mut painted_any = false;
        for run in runs {
            let start = run.start_col.min(band_cells);
            // Clip the run's chars to the remaining band cells, counting
            // wide cells exactly like the hit-test walk.
            let mut span = String::new();
            let mut cells = 0usize;
            for ch in chars
                .iter()
                .skip(run.start_char)
                .take(run.end_char.saturating_sub(run.start_char))
            {
                let width = usize::from(bitty_term_state::char_cell_width(*ch));
                if start.saturating_add(cells).saturating_add(width) > band_cells {
                    break;
                }
                span.push(*ch);
                cells = cells.saturating_add(width);
            }
            if span.is_empty() || cells == 0 {
                continue;
            }
            let fg = run.fg.as_deref().map_or(default_fg, |token| {
                resolve_band_token(token, &self.config.theme).unwrap_or_else(|| {
                    self.band_stats.unknown_tokens =
                        self.band_stats.unknown_tokens.saturating_add(1);
                    default_fg
                })
            });
            let bg = run.bg.as_deref().map_or(default_bg, |token| {
                resolve_band_token(token, &self.config.theme).unwrap_or_else(|| {
                    self.band_stats.unknown_tokens =
                        self.band_stats.unknown_tokens.saturating_add(1);
                    default_bg
                })
            });
            // Band-column to pixels through the window origin — the same
            // translation the hit-test uses, so paint and routing share one
            // geometry even for a non-zero window origin.
            let col_offset =
                u32::from(band_x).saturating_add(u32::try_from(start).unwrap_or(u32::MAX));
            let origin_x = pad_px.saturating_add(
                i32::try_from(col_offset.saturating_mul(cell_w)).unwrap_or(i32::MAX),
            );
            layers.combined_overlay.push(bitty_render::grid::FillRect {
                rect: bitty_render::geometry::RectPx::new(
                    origin_x,
                    origin_y,
                    px_span_usize(cells, cell_w),
                    live.height,
                ),
                color: bg,
            });
            let mut glyphs =
                self.renderer
                    .overlay_text_glyphs(&span, (origin_x, origin_y), cells, fg);
            if run.bold {
                // Synthetic double-strike: the overlay face is single-weight,
                // so bold repaints the same instances one pixel right. Glyph
                // overhang past the cell is documented cell behavior.
                let mut doubled = glyphs.clone();
                for glyph in &mut doubled {
                    glyph.dest[0] = glyph.dest[0].saturating_add(1);
                }
                glyphs.extend(doubled);
            }
            layers.combined_glyphs.extend(glyphs);
            painted_any = true;
        }
        if painted_any {
            layers.any_needs_draw = true;
        }
    }

    /// Core-hosted focusable-overlay surface (CTX-0943, W-28 follow-up).
    ///
    /// Paints the retained `overlay`-slot block the app pushed via
    /// [`Runtime::set_plugin_overlay`](super::Runtime::set_plugin_overlay)
    /// while the transient input capture holds. Same content the retained
    /// block carries: the identical
    /// [`extract_text_from_node`](Self::extract_text_from_node) walk the
    /// band renderer uses (so the v1 scene budgets enforced at mount time
    /// are the bounds painted here), centered over the focused frame and
    /// clipped to it, on the same overlay layer as the banner pills (fills
    /// plus glyphs, never grid truth). No-op with no surface or empty text,
    /// so frames without a capture are byte-identical to before.
    fn paint_plugin_overlay(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        let Some(overlay) = self.plugin_overlay.clone() else {
            return;
        };
        let text_line = self.extract_text_from_node(&overlay.root);
        if text_line.is_empty() {
            return;
        }
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        let frame = self
            .focused_view()
            .and_then(|view| allocations.iter().find(|frame| frame.view == view))
            .or_else(|| allocations.first());
        let Some(frame) = frame else {
            return;
        };
        if frame.rows == 0 || frame.cols == 0 {
            return;
        }
        // Width in cells (char count, not bytes), clipped to the frame.
        let text_cells = text_line
            .chars()
            .count()
            .min(usize::from(frame.cols))
            .max(1);
        let text_line: String = text_line.chars().take(text_cells).collect();
        // Center the pill in the focused frame, middle row.
        let full_w = px_span(frame.cols, live.width);
        let pill_w = px_span_usize(text_cells, live.width);
        let origin_px_x = px_add(
            px_add(pad_px, frame.content.x),
            px_side(full_w.saturating_sub(pill_w)) / 2,
        );
        let mid_row = frame.rows.saturating_sub(1) / 2;
        let origin_px_y = px_add(
            pad_px,
            px_offset_cells(frame.content.y, mid_row, live.height),
        );
        layers.combined_overlay.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                origin_px_x,
                origin_px_y,
                pill_w,
                live.height,
            ),
            color: self.config.theme.background,
        });
        let glyphs = self.renderer.overlay_text_glyphs(
            &text_line,
            (origin_px_x, origin_px_y),
            text_cells,
            self.config.theme.foreground,
        );
        layers.combined_glyphs.extend(glyphs);
        layers.any_needs_draw = true;
    }

    /// Extracts text content from a UiNode tree (CTX-0911).
    ///
    /// Walks Text nodes and Row/Column/List children, concatenating text.
    /// For v1 proof-of-concept: simple depth-first walk, Row children joined
    /// horizontally, vertical stacking deferred.
    fn extract_text_from_node(&self, node: &bitty_lua::ui::UiNode) -> String {
        use bitty_lua::ui::UiNode;
        let mut result = String::new();
        match node {
            UiNode::Text { text, .. } => {
                result.push_str(text);
            }
            UiNode::Row { children, .. }
            | UiNode::Column { children, .. }
            | UiNode::List { children, .. } => {
                for child in children {
                    result.push_str(&self.extract_text_from_node(child));
                }
            }
        }
        result
    }

    /// Phase 5 (CTX-0474): the CTX-0248/0252/0254 kitty image layer.
    ///
    /// Topmost per-pane blits for all visible allocations, budget-checked before
    /// rasterizing and clipped to each pane's content rectangle. Placements are
    /// skipped while a view inspects scrollback. Unfocused panes retain and
    /// display their own Kitty images without disappearing on focus switch.
    /// Alternate-screen entry clears only the entering origin (`clear_origin`).
    ///
    /// Raster is the Core-owned uncached nearest-neighbor step
    /// ([`bitty_rich::rasterize_kitty_clipped`]; the extension holds its own
    /// copy plus a cache): the image scales into the unclamped placement
    /// extent and only the viewport-visible window is allocated, so a
    /// partially visible placement paints at true scale instead of
    /// squeezing into its clip. Per-frame budget enforcement uses the
    /// Core-retained [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`] /
    /// [`bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME`] ceilings with
    /// skip-and-continue in paint order.
    fn paint_kitty_images(&mut self, basis: &TickBasis, layers: &mut FrameLayers) {
        let snapshot = &basis.snapshot;
        let allocations = &basis.allocations;
        let view_map = &basis.view_map;

        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return;
        }
        let rich_metrics = bitty_rich::CellMetrics {
            width: live.width,
            height: live.height,
        };

        let mut blits_this_frame = 0usize;
        let mut bytes_this_frame = 0usize;

        for frame in allocations {
            if frame.cols == 0 || frame.rows == 0 {
                continue;
            }

            let pane_origin: Option<u64> = if self.pane_sessions.contains_key(&frame.view) {
                Some(frame.view.0)
            } else if Some(frame.view) == self.primary_view {
                None
            } else {
                // CTX-0359: a session-less non-owner leaf presents erased: no images.
                continue;
            };

            let (alt_active, cursor_row, rows, scrollback_len) = match pane_origin {
                Some(token) => match self.pane_sessions.get(&ViewId::new(token)) {
                    Some(sess) => (
                        sess.state.alt_screen_active(),
                        usize::from(sess.state.cursor().position.row),
                        sess.state.height(),
                        sess.state.scrollback_len(),
                    ),
                    None => (
                        self.state.alt_screen_active(),
                        usize::from(snapshot.cursor.position.row),
                        snapshot.height,
                        self.state.scrollback_len(),
                    ),
                },
                None => (
                    self.state.alt_screen_active(),
                    usize::from(snapshot.cursor.position.row),
                    snapshot.height,
                    self.state.scrollback_len(),
                ),
            };

            if alt_active {
                if !self.kitty_images.placement_for_origin_is_empty(pane_origin) {
                    self.kitty_images.clear_origin(pane_origin);
                }
                continue;
            }

            if self.kitty_images.placement_for_origin_is_empty(pane_origin) {
                continue;
            }

            let scroll_offset = view_map
                .get(&frame.view)
                .map(|v| v.scroll_offset())
                .unwrap_or(0);

            let scrollback = (scrollback_len.saturating_sub(scroll_offset))
                + cursor_follow_window_start(cursor_row, rows, usize::from(frame.rows));

            for placement in self.kitty_images.placements_in_paint_order_for(pane_origin) {
                // Dangling placements (image evicted under the store caps)
                // fail closed here, as before.
                let Some(stored) = self.kitty_images.get(placement.image) else {
                    continue;
                };
                // Full (unclamped) extent for true-scale sampling plus the
                // viewport-clipped window to emit. `None` paints nothing
                // (scrolled fully off the top or outside the viewport).
                let Some(full_px) = bitty_rich::KittyImageLayer::placement_full_rect(
                    placement,
                    rich_metrics,
                    scrollback,
                ) else {
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
                let need = (u64::from(rect_px.width) * u64::from(rect_px.height))
                    .checked_mul(4)
                    .filter(|&n| n <= usize::MAX as u64)
                    .map(|n| n as usize);
                let Some(need) = need else { continue };
                // Per-frame budget enforcement (Core-retained ceilings):
                // skip-and-continue in paint order so small placements keep
                // painting when a huge one is shed.
                if blits_this_frame >= bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME {
                    continue;
                }
                let next = bytes_this_frame.saturating_add(need);
                if next > bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME {
                    continue;
                }
                let Some(rgba) = bitty_rich::rasterize_kitty_clipped(stored, full_px, rect_px)
                else {
                    continue;
                };
                // Leaf fills/glyphs translate by the pane content origin
                // (window padding already added); image blits use the same
                // window space so they land exactly over their cells.
                let dest = bitty_render::geometry::RectPx::new(
                    px_add(px_add(basis.pad_px, frame.content.x), rect_px.x),
                    px_add(px_add(basis.pad_px, frame.content.y), rect_px.y),
                    rect_px.width,
                    rect_px.height,
                );
                let Ok(blit) = bitty_render::grid::ImageBlit::try_new(dest, rgba) else {
                    continue;
                };
                blits_this_frame += 1;
                bytes_this_frame = next;
                layers.combined_images.push(blit);
                layers.any_needs_draw = true;
            }
        }
        if !layers.combined_images.is_empty() {
            layers.any_needs_draw = true;
        }
    }

    /// Phase 6 (CTX-0474): synthesize the combined draw list, present it,
    /// and build [`PresentStats`]; returns `None` on an empty or idle frame
    /// after recording the consumed generations. Body moved verbatim from
    /// `tick_at`.
    fn present_assembled_frame(
        &mut self,
        basis: TickBasis,
        layers: FrameLayers,
    ) -> Option<PresentStats> {
        let TickBasis {
            snapshot,
            allocations,
            focused,
            current_gen,
            cells_before,
            glyphs_before,
            band_versions,
            ..
        } = basis;
        self.pending_full_redraw = false;

        if !layers.any_needs_draw
            && layers.combined_fills.is_empty()
            && layers.combined_rounded.is_empty()
            && layers.combined_backgrounds.is_empty()
            && layers.combined_overlay.is_empty()
            && layers.combined_glyphs.is_empty()
            && layers.combined_images.is_empty()
        {
            // Check if we had pending_full but produced no draws (e.g., all zero rects) -> still idle
            // But ensure generation advances for idle detection.
            self.mark_frame_presented(snapshot.generation);
            self.last_presented_allocations = allocations;
            self.last_presented_focus = focused;
            self.last_presented_bands = band_versions;
            return None;
        }

        // Synthesize a DrawList for the combined frame. Plan is not used by
        // headless_present beyond fill/glyph counts, so we create a minimal
        // plan that reports needs_draw == true when we have content.
        // CTX-0252 F2: latch the presented blit count before the move below.
        let kitty_blits = layers.combined_images.len();
        // CTX-0347: latch the presented background-blit count too.
        let background_blits = layers.combined_backgrounds.len();
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
            let live_atlas_epoch = self.renderer.atlas_epoch();
            let mut tmp_list = self
                .renderer
                .render(&tmp_snap, &tmp_damage)
                .unwrap_or(DrawList {
                    generation: current_gen,
                    // Issue #1409 (CTX-0797): the live atlas epoch, never a
                    // literal 0. This fallback list carries no glyph slots of
                    // its own (the combined layers are moved in below), so it
                    // must report the epoch those slots were placed at;
                    // hardcoding 0 makes it born-stale the moment the atlas
                    // has ever reset.
                    atlas_epoch: live_atlas_epoch,
                    plan: FramePlan {
                        dirty_rects: Vec::new(),
                        extent: bitty_render::geometry::ExtentPx::new(0, 0),
                        mode: FrameMode::Clean,
                    },
                    fills: Vec::new(),
                    rounded_fills: Vec::new(),
                    backgrounds: Vec::new(),
                    overlay_fills: Vec::new(),
                    glyphs: Vec::new(),
                    images: Vec::new(),
                });
            // Now replace fills/glyphs with combined, and reset the plan to
            // describe the combined pixel space: the 1x1 probe's extent must
            // not survive (see present_plan_extent), and the dirty rect must
            // cover the combined frame when content exists.
            tmp_list.generation = current_gen;
            tmp_list.fills = layers.combined_fills;
            tmp_list.rounded_fills = layers.combined_rounded;
            tmp_list.backgrounds = layers.combined_backgrounds;
            tmp_list.overlay_fills = layers.combined_overlay;
            tmp_list.glyphs = layers.combined_glyphs;
            tmp_list.images = layers.combined_images;
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
            self.last_presented_bands = band_versions;
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
        self.last_presented_bands = band_versions;
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
            backgrounds: background_blits,
            images_skipped: stats.images_skipped,
        })
    }
}

/// Owned per-frame inputs collected by [`Runtime::collect_tick_basis`] and
/// shared by every later `tick_at` phase (CTX-0474). Owned only — it never
/// borrows `Runtime`, so each phase method can still take `&mut self`.
struct TickBasis {
    /// Grid snapshot backing the primary origin and the overlays.
    snapshot: Snapshot,
    /// Decorated leaf allocations for this frame.
    allocations: Vec<layout_focus::PresentFrame>,
    /// Focused leaf at collection time.
    focused: Option<ViewId>,
    /// Whether any full-invalidation source armed a full frame.
    full_frame: bool,
    /// Max grid generation across origins (present stats + damage).
    current_gen: u64,
    /// id -> View map for scroll/selection/IME lookups.
    view_map: std::collections::HashMap<ViewId, View>,
    /// Physical window padding inset.
    pad_px: i32,
    /// Loop-invariant decoration-ring inputs.
    ring_ctx: RingFrameContext,
    /// Renderer cells-examined counter baseline for the stats delta.
    cells_before: u64,
    /// Renderer glyphs-emitted counter baseline for the stats delta.
    glyphs_before: u64,
    /// Mounted band versions for this frame (CTX-0946 C2).
    band_versions: Vec<(String, bitty_lua::ui::UiSlot, u32)>,
}

/// Combined draw layers assembled by the present phases (CTX-0474).
///
/// The leaf pass fills the grid layers and the cursor; the overlay phases
/// append to the overlay, glyph, and image layers. [`Runtime::tick_at`]
/// drains the cursor and hands the struct to the finalize phase.
#[derive(Default)]
struct FrameLayers {
    /// Combined leaf cell fills (translated into window space).
    combined_fills: Vec<bitty_render::grid::FillRect>,
    /// Combined decoration rings.
    combined_rounded: Vec<bitty_render::grid::RoundedFill>,
    /// CTX-0347 per-`View` background blits.
    combined_backgrounds: Vec<bitty_render::grid::ImageBlit>,
    /// Overlay fills (cursor, selection, banners, scrollbar).
    combined_overlay: Vec<bitty_render::grid::FillRect>,
    /// Combined glyph instances (leaf plus overlay text).
    combined_glyphs: Vec<bitty_render::grid::GlyphInstance>,
    /// CTX-0248 kitty image blits.
    combined_images: Vec<bitty_render::grid::ImageBlit>,
    /// Whether any layer contributed draw work this frame.
    any_needs_draw: bool,
    /// Deferred focused-cursor overlay (CTX-0386); drained before overlays.
    cursor: Option<CursorPaint>,
    /// Atlas epoch the leaf glyph slots in `combined_glyphs` were placed at
    /// (issue #1409, CTX-0797).
    ///
    /// Compared against the live atlas epoch after the overlay phase: the
    /// overlay painters place text through `overlay_text_glyphs`, which can
    /// exhaust and reset the atlas long after the leaf pass settled.
    atlas_epoch: u64,
}

/// Issue #1409 (CTX-0797): the production guard that keeps a presented frame
/// on one atlas epoch.
///
/// These are in-crate tests because the guard reads private runtime state (the
/// renderer, the retained per-leaf stores, and the bounded rejection counter);
/// no public test seam is added for them. The epoch is advanced through a real
/// wholesale atlas reset (`GridRenderer::apply_dpi_scale` clears the atlas and
/// bumps its placement generation), never by writing a fabricated number, so
/// the fixture exercises the same mechanism `overlay_text_glyphs` uses when it
/// exhausts the atlas mid-frame.
#[cfg(test)]
mod atlas_epoch_guard_tests {
    use super::{ATLAS_REBUILD_LIMIT, FrameLayers};
    use crate::runtime::{Runtime, RuntimeConfig};

    fn runtime() -> Runtime {
        Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
            .expect("deterministic runtime must build")
    }

    /// Advances the live atlas epoch by one wholesale reset.
    ///
    /// Drives the public `Runtime::apply_dpi_scale` entry point, which clears
    /// the atlas and bumps its placement generation exactly like the exhaustion
    /// reset inside `overlay_text_glyphs`. `pending_full_redraw` is cleared
    /// afterwards because the rescale sets it on its own; the guard's own
    /// invalidation must be observable independently.
    fn reset_atlas(rt: &mut Runtime, scale: f64) {
        let before = rt.renderer.atlas_epoch();
        rt.apply_dpi_scale(scale, None);
        assert_ne!(
            rt.renderer.atlas_epoch(),
            before,
            "a wholesale atlas reset must bump the epoch"
        );
        rt.pending_full_redraw = false;
    }

    fn layers_at(epoch: u64) -> FrameLayers {
        FrameLayers {
            atlas_epoch: epoch,
            ..FrameLayers::default()
        }
    }

    #[test]
    fn epoch_consistent_frame_is_presented() {
        let mut rt = runtime();
        let layers = layers_at(rt.renderer.atlas_epoch());
        assert!(
            !rt.reject_stale_atlas_frame(&layers),
            "a frame planned at the live epoch must present"
        );
        assert_eq!(rt.stale_atlas_frame_rejects, 0);
    }

    #[test]
    fn overlay_phase_atlas_reset_rejects_the_frame() {
        let mut rt = runtime();
        rt.handle_pty_bytes(b"hello");
        rt.tick().expect("printed bytes must present a frame");
        assert!(
            !rt.presented_leaf_frames.is_empty(),
            "the present must retain at least one leaf store"
        );

        // The leaf pass planned at this epoch; the reset below stands in for an
        // overlay line that exhausted the atlas after the leaf pass settled.
        let planned_epoch = rt.renderer.atlas_epoch();
        let layers = layers_at(planned_epoch);
        reset_atlas(&mut rt, 2.0);

        assert!(
            rt.reject_stale_atlas_frame(&layers),
            "a frame whose leaf slots were wiped must not be presented"
        );
        assert!(
            rt.presented_leaf_frames.is_empty(),
            "retained leaf slots from the wiped epoch must be dropped"
        );
        assert!(
            rt.pending_full_redraw,
            "the next tick must rebuild against the fresh atlas"
        );
        assert_eq!(rt.stale_atlas_frame_rejects, 1);
    }

    #[test]
    fn repeated_rejection_is_bounded_so_presentation_never_starves() {
        let mut rt = runtime();
        // A working set that never fits one epoch would reset on every tick.
        // The guard must give up after the bound instead of dropping frames
        // forever.
        for expected in 1..=ATLAS_REBUILD_LIMIT {
            let stale = layers_at(rt.renderer.atlas_epoch());
            reset_atlas(&mut rt, 2.0);
            assert!(rt.reject_stale_atlas_frame(&stale), "within the bound");
            assert_eq!(rt.stale_atlas_frame_rejects, expected);
        }

        let stale = layers_at(rt.renderer.atlas_epoch());
        reset_atlas(&mut rt, 1.5);
        assert!(
            !rt.reject_stale_atlas_frame(&stale),
            "at the bound the frame presents anyway (liveness over consistency)"
        );
        assert_eq!(
            rt.stale_atlas_frame_rejects, 0,
            "the counter rearms for the next episode"
        );
    }

    #[test]
    fn a_consistent_frame_clears_an_earlier_rejection() {
        let mut rt = runtime();
        let stale = layers_at(rt.renderer.atlas_epoch());
        reset_atlas(&mut rt, 2.0);
        assert!(rt.reject_stale_atlas_frame(&stale));
        assert_eq!(rt.stale_atlas_frame_rejects, 1);

        let fresh = layers_at(rt.renderer.atlas_epoch());
        assert!(!rt.reject_stale_atlas_frame(&fresh));
        assert_eq!(
            rt.stale_atlas_frame_rejects, 0,
            "a clean frame must not leave the bound partly consumed"
        );
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

#[cfg(test)]
mod content_padding_tests {
    //! Issue #1357: grid column 0 must never render flush against the
    //! window/panel left edge.
    //!
    //! The default composition is `window.padding` (8 logical px) plus the
    //! Core-owned decoration (`gaps_out` 6 + `border` 1 + `content_inset` 6).
    //! For reference, ghostty ships 2px symmetric window padding and kitty
    //! ships 0; the composed bitty default stays comfortably above both, so
    //! text always has breathing room out of the box. These tests pin the
    //! composed left offset end to end: frame geometry via
    //! [`Runtime::present_frames`] *and* painted pixels via the headless
    //! seam, for a single pane and for splits, plus fail-closed live
    //! adoption of out-of-range values.
    use super::*;
    use crate::Decoration;
    use crate::SplitAxis;
    use crate::config::{
        DEFAULT_WINDOW_PADDING, MAX_DECORATION_CONTENT_INSET_PX, MAX_WINDOW_PADDING,
    };
    use bitty_vt::GraphemeCell;

    /// Fresh default runtime with one 80x24 leaf in a 736x472 window (80x24
    /// at 9x19 cells = 720x456 grid plus twice the 8px window padding).
    fn single_pane_runtime() -> Runtime {
        let mut rt = Runtime::with_defaults().expect("default runtime builds");
        assert_eq!(rt.dpi_scale(), 1.0, "defaults pin scale 1.0");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.handle_resize(PhysicalSize::new(736, 472))
            .expect("resize applies");
        rt
    }

    #[test]
    fn default_single_pane_left_offset_is_painted() {
        let mut rt = single_pane_runtime();
        // Print at row 0 so the first glyph sits in grid column 0.
        for c in ['H', 'i'] {
            rt.state
                .apply(&TerminalAction::Print(GraphemeCell::from(c)));
        }
        let deco = rt.decoration();
        assert_eq!(rt.window_padding_physical(), DEFAULT_WINDOW_PADDING);

        let frames = rt.present_frames();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        // Content sits inside gaps_out + border + content_inset on the left:
        // the frame starts after the outer gap, the content after the
        // border ring and the inner content inset.
        assert_eq!(frame.frame.x, i32::from(deco.gaps_out));
        let frame_inset = i32::from(deco.border) + i32::from(deco.content_inset);
        assert_eq!(frame_inset, 7, "1 + 6 default border + content inset");
        assert_eq!(frame.content.x - frame.frame.x, frame_inset);
        // Symmetric on the right edge too.
        let frame_right = frame.frame.x + frame.frame.width as i32;
        let content_right = frame.content.x + frame.content.width as i32;
        assert_eq!(frame_right - content_right, frame_inset);

        rt.tick().expect("headless tick presents");
        let rgba = rt.headless_rgba().expect("headless rgba");
        let extent = rt.present_plan_extent();
        let (w, h) = (extent.width as usize, extent.height as usize);
        assert_eq!(rgba.len(), w * h * 4);
        let bg = [rgba[0], rgba[1], rgba[2], rgba[3]];
        let px = |x: usize, y: usize| rgba[(y * w + x) * 4..(y * w + x) * 4 + 4] != bg;

        // The decoration ring starts exactly at the window-padding inset.
        let pad = rt.window_padding_physical() as usize;
        let mut first_diff: Option<usize> = None;
        for x in 0..w {
            if (0..h).any(|y| px(x, y)) {
                first_diff = Some(x);
                break;
            }
        }
        assert_eq!(
            first_diff,
            Some(pad + frame.frame.x as usize),
            "nothing paints inside the window-padding band"
        );

        // First text row band: pad band clear, ring solid, inset clear,
        // glyph ink only at/after the content origin.
        let cell_h = rt.live_cell_metrics().height as usize;
        let y0 = pad + frame.content.y as usize;
        let ring_left = pad + frame.frame.x as usize;
        let content_left = pad + frame.content.x as usize;
        assert!(y0 + cell_h <= h);
        let col_has_ink = |x: usize| (y0..y0 + cell_h).any(|y| px(x, y));
        assert!(
            (0..ring_left).all(|x| !col_has_ink(x)),
            "window-padding band stays background"
        );
        assert!(
            (ring_left..ring_left + deco.border as usize).all(col_has_ink),
            "decoration ring paints at the frame edge"
        );
        assert!(
            (ring_left + deco.border as usize..content_left).all(|x| !col_has_ink(x)),
            "content inset stays background"
        );
        assert!(
            (content_left..w).any(col_has_ink),
            "first-column glyph ink paints at the content origin"
        );
        // Composed breathing room before the first ink: 8 + 6 + 1 + 6.
        assert_eq!(content_left, 21);
    }

    #[test]
    fn split_panes_keep_symmetric_content_inset() {
        let mut rt = Runtime::with_defaults().expect("default runtime builds");
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        ));
        rt.handle_resize(PhysicalSize::new(736, 472))
            .expect("resize applies");
        let deco = rt.decoration();
        let inset = i32::from(deco.border) + i32::from(deco.content_inset);
        let frames = rt.present_frames();
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert_eq!(
                frame.content.x - frame.frame.x,
                inset,
                "no pane paints flush against its left frame edge"
            );
            let frame_right = frame.frame.x + frame.frame.width as i32;
            let content_right = frame.content.x + frame.content.width as i32;
            assert_eq!(
                frame_right - content_right,
                inset,
                "inset is symmetric on the right edge"
            );
        }
        // The sibling gap between the two frames survives the insets.
        let (left, right) = (&frames[0], &frames[1]);
        let (left, right) = if left.frame.x < right.frame.x {
            (left, right)
        } else {
            (right, left)
        };
        assert_eq!(
            right.frame.x - (left.frame.x + left.frame.width as i32),
            i32::from(deco.gaps_in),
            "divider gap stays visible between pane frames"
        );
    }

    #[test]
    fn oversized_padding_and_inset_fail_closed_live() {
        let mut rt = Runtime::with_defaults().expect("default runtime builds");
        assert!(rt.set_window_padding(MAX_WINDOW_PADDING + 1).is_err());
        assert_eq!(
            rt.window_padding(),
            DEFAULT_WINDOW_PADDING,
            "rejected padding leaves the live value untouched"
        );
        assert!(rt.set_window_padding(0).is_ok(), "zero stays valid");
        assert_eq!(rt.window_padding(), 0);
        assert!(rt.set_window_padding(DEFAULT_WINDOW_PADDING).is_ok());

        let bad = Decoration::new(6, 6, 1, 6, MAX_DECORATION_CONTENT_INSET_PX + 1);
        assert!(rt.set_decoration(bad).is_err());
        assert_eq!(
            rt.decoration().content_inset,
            Decoration::default().content_inset,
            "rejected inset leaves the live decoration untouched"
        );
        assert!(rt.set_decoration(Decoration::ZERO).is_ok());
        assert_eq!(rt.decoration(), Decoration::ZERO);
    }
}

// CTX-0979: Core workspace bar overlay tests deleted with
// `overlay_status_bar` (Core draws no workspace display).
#[cfg(test)]
mod selection_window_clip_tests {
    //! CTX-0803: the selection paint's frame-window clip is the inverse of
    //! the pointer mapping's row translation. These pin the stream-span
    //! continuation rules at the window edges.
    use super::clip_span_to_window;
    use bitty_ui::CellPos;

    const ROWS: u16 = 4;
    const COLS: u16 = 10;

    #[test]
    fn span_inside_the_window_translates_by_the_window_start() {
        let clip = clip_span_to_window(CellPos::new(6, 2), CellPos::new(7, 5), 5, ROWS, COLS);
        assert_eq!(clip, Some(((1, 2), (2, 5))));
    }

    #[test]
    fn span_starting_above_the_window_continues_from_its_first_cell() {
        let clip = clip_span_to_window(CellPos::new(1, 7), CellPos::new(6, 3), 5, ROWS, COLS);
        assert_eq!(clip, Some(((0, 0), (1, 3))));
    }

    #[test]
    fn span_ending_below_the_window_runs_to_its_last_column() {
        let clip = clip_span_to_window(CellPos::new(6, 4), CellPos::new(20, 1), 5, ROWS, COLS);
        assert_eq!(clip, Some(((1, 4), (ROWS - 1, COLS - 1))));
    }

    #[test]
    fn span_outside_the_window_paints_nothing() {
        assert_eq!(
            clip_span_to_window(CellPos::new(0, 0), CellPos::new(4, 9), 5, ROWS, COLS),
            None
        );
        assert_eq!(
            clip_span_to_window(CellPos::new(9, 0), CellPos::new(12, 9), 5, ROWS, COLS),
            None
        );
    }

    #[test]
    fn degenerate_frames_paint_nothing() {
        let (start, end) = (CellPos::new(0, 0), CellPos::new(1, 1));
        assert_eq!(clip_span_to_window(start, end, 0, 0, COLS), None);
        assert_eq!(clip_span_to_window(start, end, 0, ROWS, 0), None);
    }
}
