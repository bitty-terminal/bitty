//! `Runtime` — Frame tick and software present path.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::band_slots::BandEdge;
use super::*;
use bitty_render::gpu::PresentStats as RenderPresentStats;
use bitty_rich::kitty_seam::KittyRasterBackend as _;

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
    /// Inactive-`View` fade factor applied to the stored fills/glyphs
    /// (CTX-1162 S3). A mismatch forces a fresh leaf render: the retained
    /// list carries baked alpha, so a focus flip or a `fade_unfocused`
    /// live-adopt must never reuse a stale factor (DEC-0113 Fix 2a,
    /// bounded to the visible leaf set).
    pub(super) fade_factor: f32,
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

/// Leaf-loop inputs for the CTX-1207 scroll fast path. Bundled so the
/// helper stays under clippy's argument limit (same pattern as the
/// state-side `KittyCommand` bundle).
struct ScrollShiftInput<'a> {
    view_id: ViewId,
    frame: &'a layout_focus::PresentFrame,
    leaf_damage: &'a [DamagedRegion],
    view_snapshot: &'a Snapshot,
    base_snap: &'a Snapshot,
    view_scroll_offset: usize,
    origin_px: (i32, i32),
    fade: f32,
    frame_clip: Option<bitty_render::grid::RoundedClip>,
    higher_floats: &'a [bitty_render::geometry::RectPx],
    scrolled_viewport: bool,
    full_frame: bool,
}

/// Shifted retained primitives for one scrolled leaf (CTX-1207
/// region-copy): the retained span moved by the scroll delta, plus the
/// fresh repaint regions (exposed band + companion print damage from the
/// same tick). Shifting primitives — not pixels — keeps glyph overhang
/// byte-exact, because each glyph carries its geometry across the move.
struct ScrollShift {
    /// Fresh repaint regions in leaf-local grid coordinates.
    regions: Vec<DamageRect>,
    /// Retained fills shifted by the scroll delta (window px, translated).
    fills: Vec<bitty_render::grid::FillRect>,
    /// Retained glyphs shifted by the scroll delta (window px, translated).
    glyphs: Vec<bitty_render::grid::GlyphInstance>,
}

/// Leaf-local grid damage rectangle to window-pixel rectangle (CTX-1207).
/// Total like the CTX-0253 helpers: `i64` intermediates, clamped into
/// `i32`/`u32` ranges, so hostile cell metrics cannot wrap.
fn damage_rect_to_window_px(
    rect: &DamageRect,
    origin: (i32, i32),
    cell: CellMetrics,
) -> bitty_render::geometry::RectPx {
    let x = i64::from(origin.0) + i64::from(rect.left) * i64::from(cell.width);
    let y = i64::from(origin.1) + i64::from(rect.top) * i64::from(cell.height);
    let w = (i64::from(rect.right) - i64::from(rect.left) + 1) * i64::from(cell.width);
    let h = (i64::from(rect.bottom) - i64::from(rect.top) + 1) * i64::from(cell.height);
    bitty_render::geometry::RectPx::new(
        x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        u32::try_from(w.max(0)).unwrap_or(u32::MAX),
        u32::try_from(h.max(0)).unwrap_or(u32::MAX),
    )
}

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

/// CTX-1165 (#1993): whether a leaf glyph is occluded by a higher detached float.
///
/// The combined draw list paints all fills before all glyphs, so the CTX-1148
/// opaque float fill already covers lower fills but not lower glyphs. A lower
/// glyph whose bitmap bounds share positive area with a higher float frame
/// would otherwise paint over the upper float. Dropping it extends the same
/// opaque mechanism to text rows. Empty bounds never occlude: an unmeasurable
/// glyph stays visible. Bounded to one intersection test per higher float.
fn glyph_occluded_by_floats(
    dest: [i32; 2],
    size: [u32; 2],
    occluders: &[bitty_render::geometry::RectPx],
) -> bool {
    if size[0] == 0 || size[1] == 0 {
        return false;
    }
    let bounds = bitty_render::geometry::RectPx::new(dest[0], dest[1], size[0], size[1]);
    if bounds.is_empty() {
        return false;
    }
    for occluder in occluders {
        if occluder.is_empty() {
            continue;
        }
        if bounds.intersection(occluder).is_some() {
            return true;
        }
    }
    false
}

/// CTX-1173 (#2031): whether the focused cursor cell is occluded by a higher
/// detached float.
///
/// Reuses the CTX-1165 occluder table and intersection test: the cursor cell
/// shares the same window space as leaf glyph bounds, so a cell sharing
/// positive area with a higher float frame must not leak through the upper
/// opaque fill. The full cell rect is tested (not the shaped bar/underline
/// strip) so partial coverage still hides fail-closed. Bounded to one
/// intersection test per higher float.
fn cursor_occluded_by_floats(
    paint: &CursorPaint,
    cell: [u32; 2],
    occluders: &[bitty_render::geometry::RectPx],
) -> bool {
    let dest = [
        px_offset_cells(paint.origin_x, paint.cursor.position.col, cell[0]),
        px_offset_cells(paint.origin_y, paint.cursor.position.row, cell[1]),
    ];
    glyph_occluded_by_floats(dest, cell, occluders)
}

/// CTX-1181 (#2041): upper bound on occlusion pieces per image blit.
///
/// Each occluder splits every surviving piece into at most four, so pieces
/// grow as 4^n. Float frames are bounded by the visible leaf set; past this
/// cap the remaining pieces are dropped fail-closed (hidden, never leaked).
const MAX_OCCLUSION_PIECES: usize = 64;

/// CTX-1181 (#2041): saturate an `i64` edge back into `i32` pixels.
///
/// Subtraction pieces stay inside their source rect, so this only fires on
/// hostile geometry; saturating keeps the piece empty instead of wrapping.
fn saturate_px(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// CTX-1181 (#2041): visible pieces of `rect` outside `occluder`.
///
/// Axis-aligned rect subtraction in window space. The left/right strips span
/// the full height and the top/bottom strips span the intersection width, so
/// the pieces partition the visible area without overlap. A missed
/// intersection returns `rect` verbatim; full coverage returns no pieces.
/// Overflow-free `i64` edges with saturating conversion throughout, so
/// hostile geometry degrades to dropped pieces, never address wrap.
/// Bounded to four pieces per call.
fn subtract_float_occluder(
    rect: bitty_render::geometry::RectPx,
    occluder: bitty_render::geometry::RectPx,
) -> Vec<bitty_render::geometry::RectPx> {
    if rect.is_empty() {
        return Vec::new();
    }
    if occluder.is_empty() {
        return vec![rect];
    }
    let rx0 = i64::from(rect.x);
    let ry0 = i64::from(rect.y);
    let rx1 = rect.right_exclusive();
    let ry1 = rect.bottom_exclusive();
    let ix0 = rx0.max(i64::from(occluder.x));
    let iy0 = ry0.max(i64::from(occluder.y));
    let ix1 = rx1.min(occluder.right_exclusive());
    let iy1 = ry1.min(occluder.bottom_exclusive());
    if ix0 >= ix1 || iy0 >= iy1 {
        return vec![rect];
    }
    let span = |from: i64, to: i64| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
    let spans = [
        (rect.x, rect.y, span(rx0, ix0), rect.height),
        (saturate_px(ix1), rect.y, span(ix1, rx1), rect.height),
        (saturate_px(ix0), rect.y, span(ix0, ix1), span(ry0, iy0)),
        (
            saturate_px(ix0),
            saturate_px(iy1),
            span(ix0, ix1),
            span(iy1, ry1),
        ),
    ];
    spans
        .into_iter()
        .map(|(x, y, width, height)| bitty_render::geometry::RectPx::new(x, y, width, height))
        .filter(|piece| !piece.is_empty())
        .collect()
}

/// CTX-1181 (#2041): copy the `rect` sub-window out of `blit`.
///
/// `rect` must lie inside `blit.dest`; anything else (including arithmetic
/// overflow) returns `None` fail-closed so the caller drops the piece
/// instead of leaking it. Row-major straight-alpha RGBA8, four bytes per
/// pixel, checked math before every index.
fn slice_blit_piece(
    blit: &bitty_render::grid::ImageBlit,
    rect: bitty_render::geometry::RectPx,
) -> Option<bitty_render::grid::ImageBlit> {
    if rect.is_empty() {
        return None;
    }
    let mask = |value: i64| -> Option<u64> { u64::try_from(value).ok() };
    let dx = mask(i64::from(rect.x).saturating_sub(i64::from(blit.dest.x)))?;
    let dy = mask(i64::from(rect.y).saturating_sub(i64::from(blit.dest.y)))?;
    let piece_w = u64::from(rect.width);
    let piece_h = u64::from(rect.height);
    if dx.saturating_add(piece_w) > u64::from(blit.dest.width) {
        return None;
    }
    if dy.saturating_add(piece_h) > u64::from(blit.dest.height) {
        return None;
    }
    let row_bytes = piece_w.checked_mul(4)?;
    let stride = u64::from(blit.dest.width).checked_mul(4)?;
    let stride = usize::try_from(stride).ok()?;
    let row_len = usize::try_from(row_bytes).ok()?;
    let dx_bytes = usize::try_from(dx.checked_mul(4)?).ok()?;
    let mut rgba = Vec::new();
    for row in 0..piece_h {
        let row_start = dy.checked_add(row)?;
        let row_start =
            usize::try_from(row_start.checked_mul(u64::try_from(stride).ok()?)?).ok()?;
        let start = row_start.checked_add(dx_bytes)?;
        let end = start.checked_add(row_len)?;
        rgba.extend_from_slice(blit.rgba.get(start..end)?);
    }
    bitty_render::grid::ImageBlit::try_new(rect, rgba).ok()
}

/// CTX-1181 (#2041): clip an image blit around higher float frames.
///
/// The combined draw list paints backgrounds above fills and kitty images
/// topmost, so the CTX-1148 opaque fill covers neither layer: a lower blit
/// sharing area with a higher float frame would otherwise paint over the
/// upper float. Splitting the blit around each occluder keeps the visible
/// part outside the float pixel-identical while the covered part is owned
/// by the upper float alone. Bytes are sliced per piece from the source
/// rows, so no piece ever allocates beyond its own extent. Empty occluders
/// never clip; an empty result means full coverage. Bounded to
/// [`MAX_OCCLUSION_PIECES`] pieces.
fn clip_blit_around_floats(
    blit: bitty_render::grid::ImageBlit,
    occluders: &[bitty_render::geometry::RectPx],
) -> Vec<bitty_render::grid::ImageBlit> {
    if blit.dest.is_empty() {
        return Vec::new();
    }
    let mut pieces = vec![blit];
    for occluder in occluders {
        if occluder.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        for piece in pieces {
            if piece.dest.intersection(occluder).is_none() {
                next.push(piece);
                continue;
            }
            for rect in subtract_float_occluder(piece.dest, *occluder) {
                let Some(sliced) = slice_blit_piece(&piece, rect) else {
                    continue;
                };
                next.push(sliced);
                if next.len() >= MAX_OCCLUSION_PIECES {
                    break;
                }
            }
            if next.len() >= MAX_OCCLUSION_PIECES {
                break;
            }
        }
        pieces = next;
        if pieces.is_empty() {
            break;
        }
    }
    pieces
}

/// One stored image's visible `U+10EEEE` coverage inside one leaf.
///
/// Runs of one prototype are gathered across every visible row so the
/// present layer rasterizes and blits the image once (review of #2442): a
/// per-row blit truncated previews taller than
/// [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`] and starved direct
/// placements queued behind them.
struct KittyUnicodeRunGroup {
    /// Wire `i=` image id of the prototype (animation frame lookup).
    wire_image: u32,
    /// Wire `p=` placement id of the prototype (`0` anonymous).
    wire_placement: u32,
    /// Stored image: the prototype's linked handle (combined `a=T,U=1`) or
    /// the origin-scoped wire-map root (bodiless `a=p,U=1`).
    image: bitty_rich::KittyImageId,
    /// The complete image fitted into the prototype's cell span. Every run
    /// of the group resolves the same span, so this is shared.
    full: bitty_rich::geometry::RectPx,
    /// Viewport-clipped crop painted by each run.
    rects: Vec<bitty_rich::geometry::RectPx>,
}

/// Bounding box of the visible run rects, in content space.
///
/// The present layer scales the image into the group's fitted `full` rect
/// and emits the union window once; callers mask the raster back to the
/// exact run coverage when the union holds gaps.
fn unicode_rect_union(
    rects: &[bitty_rich::geometry::RectPx],
) -> Option<bitty_rich::geometry::RectPx> {
    let first = *rects.first()?;
    let mut x0 = i64::from(first.x);
    let mut y0 = i64::from(first.y);
    let mut x1 = x0.saturating_add(i64::from(first.width));
    let mut y1 = y0.saturating_add(i64::from(first.height));
    for rect in &rects[1..] {
        x0 = x0.min(i64::from(rect.x));
        y0 = y0.min(i64::from(rect.y));
        x1 = x1.max(i64::from(rect.x).saturating_add(i64::from(rect.width)));
        y1 = y1.max(i64::from(rect.y).saturating_add(i64::from(rect.height)));
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let to_i32 =
        |value: i64| i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX });
    Some(bitty_rich::geometry::RectPx::new(
        to_i32(x0),
        to_i32(y0),
        u32::try_from(x1 - x0).unwrap_or(u32::MAX),
        u32::try_from(y1 - y0).unwrap_or(u32::MAX),
    ))
}

/// Whether `rects` tile `union` exactly: every rect spans the union's full
/// width and the rects cover its rows once, without gaps or overlap.
///
/// Full-bleed placeholder previews (yazi's one run per image row) hit this
/// shape, so their union raster is byte-identical to the image and stays
/// cacheable; sparse runs take the masked path instead.
fn unicode_rects_cover_union(
    union: bitty_rich::geometry::RectPx,
    rects: &[bitty_rich::geometry::RectPx],
) -> bool {
    if rects.is_empty() {
        return false;
    }
    let x0 = i64::from(union.x);
    let x1 = x0.saturating_add(i64::from(union.width));
    let y0 = i64::from(union.y);
    let y1 = y0.saturating_add(i64::from(union.height));
    if rects.iter().any(|rect| {
        i64::from(rect.x) != x0 || i64::from(rect.x).saturating_add(i64::from(rect.width)) != x1
    }) {
        return false;
    }
    let mut bands: Vec<(i64, i64)> = rects
        .iter()
        .map(|rect| {
            (
                i64::from(rect.y),
                i64::from(rect.y).saturating_add(i64::from(rect.height)),
            )
        })
        .collect();
    bands.sort_unstable();
    let mut cursor = y0;
    for (start, end) in bands {
        if start != cursor {
            return false;
        }
        cursor = end;
    }
    cursor == y1
}

/// Zeroes the pixels of a union raster that no `U+10EEEE` run covers.
///
/// The union blit is one bounding box for the whole image; between runs
/// (an erased line, a non-placeholder column) the grid text must stay
/// visible, so uncovered pixels are made transparent (straight alpha) and
/// composite as a no-op. Coverage is one byte per union pixel, filled from
/// the disjoint run rects, so the work is bounded by the union area already
/// admitted to the frame budget.
fn mask_uncovered_unicode_pixels(
    rgba: &mut [u8],
    union: bitty_rich::geometry::RectPx,
    rects: &[bitty_rich::geometry::RectPx],
) {
    let width = union.width as usize;
    let height = union.height as usize;
    let Some(area) = width.checked_mul(height) else {
        return;
    };
    if area == 0 || rgba.len() != area.saturating_mul(4) {
        return;
    }
    let mut covered = vec![0u8; area];
    let union_x0 = i64::from(union.x);
    let union_y0 = i64::from(union.y);
    let union_x1 = union_x0.saturating_add(i64::from(union.width));
    let union_y1 = union_y0.saturating_add(i64::from(union.height));
    for rect in rects {
        let x0 = i64::from(rect.x).max(union_x0);
        let x1 = i64::from(rect.x)
            .saturating_add(i64::from(rect.width))
            .min(union_x1);
        let y0 = i64::from(rect.y).max(union_y0);
        let y1 = i64::from(rect.y)
            .saturating_add(i64::from(rect.height))
            .min(union_y1);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let col_start = usize::try_from(x0 - union_x0).unwrap_or(0);
        let col_end = usize::try_from(x1 - union_x0).unwrap_or(width);
        for row in y0..y1 {
            let Ok(offset) = usize::try_from(row - union_y0) else {
                continue;
            };
            let Some(base) = offset.checked_mul(width) else {
                continue;
            };
            let (Some(start), Some(end)) = (base.checked_add(col_start), base.checked_add(col_end))
            else {
                continue;
            };
            if let Some(slice) = covered.get_mut(start..end) {
                slice.fill(1);
            }
        }
    }
    for (index, flag) in covered.iter().enumerate() {
        if *flag == 0 {
            let base = index.saturating_mul(4);
            if let Some(pixel) = rgba.get_mut(base..base.saturating_add(4)) {
                pixel.fill(0);
            }
        }
    }
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

/// Selection highlight rectangles for one painted frame (issues #2523, #2470).
///
/// Unlike [`bitty_render::grid::selection_fill_rects_in`], this paints by
/// selection kind:
///
/// - `Block` fills the same column window on every row (a rectangle);
/// - the other kinds fill row-major (first row from the anchor column, last
///   row through the focus column, body rows full width).
///
/// Wide leaders ending at the right edge extend by one cell so a 2-cell
/// glyph is never half-tinted. `row_offset` maps a frame row to the snapshot
/// row (the cursor-follow window for a live grid; zero for a viewport
/// composite). Total: clamps every coordinate, never panics, bounded by the
/// frame height.
#[allow(clippy::too_many_arguments)]
fn selection_row_rects(
    palette: &bitty_render::grid::ThemePalette,
    kind: SelectionKind,
    start: CellPos,
    end: CellPos,
    frame_cols: u16,
    frame_rows: u16,
    cells: &[Cell],
    cells_width: usize,
    cells_height: usize,
    row_offset: usize,
    cell: CellMetrics,
) -> Vec<bitty_render::grid::FillRect> {
    if frame_cols == 0 || frame_rows == 0 || cell.width == 0 || cell.height == 0 {
        return Vec::new();
    }
    let (start, end) = if (start.row, start.col) <= (end.row, end.col) {
        (start, end)
    } else {
        (end, start)
    };
    let max_row = frame_rows - 1;
    let max_col = frame_cols - 1;
    let first_row = start.row.min(max_row);
    let last_row = end.row.min(max_row);
    let block = kind == SelectionKind::Block;
    let (block_col0, block_col1) = (start.col.min(end.col), start.col.max(end.col));
    let mut rects = Vec::with_capacity(usize::from(last_row.saturating_sub(first_row)) + 1);
    for row in first_row..=last_row {
        let (mut col0, mut col1) = if block {
            (block_col0, block_col1)
        } else {
            (
                if row == start.row { start.col } else { 0 },
                if row == end.row { end.col } else { max_col },
            )
        };
        col0 = col0.min(max_col);
        col1 = col1.min(max_col);
        if col1 < col0 {
            continue;
        }
        // Wide-glyph coverage: never leave half of a 2-cell glyph unshaded.
        let snapshot_row = row_offset.saturating_add(usize::from(row));
        if snapshot_row < cells_height && cells_width > 0 {
            let at = |col: u16| -> Option<&Cell> {
                cells.get(snapshot_row.saturating_mul(cells_width) + usize::from(col))
            };
            if let Some(cell) = at(col0) {
                if cell.spacer && col0 > 0 {
                    col0 -= 1;
                }
            }
            if let Some(cell) = at(col1) {
                if cell.width == 2 && !cell.spacer && col1 < max_col {
                    col1 += 1;
                }
            }
        }
        let span = u64::from(col1 - col0 + 1);
        let x = i32::try_from(
            u64::from(col0)
                .saturating_mul(u64::from(cell.width))
                .min(u64::from(i32::MAX as u32)),
        )
        .unwrap_or(i32::MAX);
        let y = i32::try_from(
            u64::from(row)
                .saturating_mul(u64::from(cell.height))
                .min(u64::from(i32::MAX as u32)),
        )
        .unwrap_or(i32::MAX);
        let width = u32::try_from(span.saturating_mul(u64::from(cell.width))).unwrap_or(u32::MAX);
        rects.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(x, y, width, cell.height),
            color: bitty_render::grid::selection_fill_in(palette),
        });
    }
    rects
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
    /// CTX-1207 scroll fast path: shift the retained leaf primitives by a
    /// scroll-region motion (region-copy) and repaint only the exposed band
    /// plus companion print damage (damage-only repaint).
    ///
    /// Soundness: [`GridRenderer::render`] places every glyph as a pure
    /// function of its row, column, cell content, and font metrics (no
    /// cursor- or neighbour-dependent shaping on this path), so a shifted
    /// retained primitive is bit-identical to a freshly placed one —
    /// including glyph overhang, which travels with its glyph instead of
    /// being clipped at a pixel-copy boundary. Fresh repaints are always
    /// correct (they render the post-scroll snapshot); stale-coordinate
    /// companion rects can only over-damage, never under-damage.
    ///
    /// Every condition is fail-closed to `None` (the caller re-renders the
    /// leaf in full): full-frame invalidation, scrollback-composited
    /// viewports, float occluders, glyph-clip mismatch (decorated frames
    /// clip glyphs; the retained clip must equal the live one),
    /// per-view background images, viewport/grid dimension mismatch,
    /// out-of-range or degenerate motions, multi-region motions, and
    /// companion print damage outside the exposed band and margin row
    /// (a translated companion rect repaints fresh but never drops the
    /// retained pre-print primitives at those cells, so a stale glyph
    /// would paint over the fresh fill).
    /// Same-region motions from the tick's batches sum (per-action
    /// batching would otherwise decline every real scroll frame);
    /// cross-tick accumulation merges are the frame-merge follow-up, not
    /// smuggled here. Atlas growth mid-render is detected by the caller
    /// via the epoch check and falls back the same way (kept UVs would be
    /// stale).
    fn scroll_retained_shift(&self, input: ScrollShiftInput<'_>) -> Option<ScrollShift> {
        let ScrollShiftInput {
            view_id,
            frame,
            leaf_damage,
            view_snapshot,
            base_snap,
            view_scroll_offset,
            origin_px,
            fade,
            frame_clip,
            higher_floats,
            scrolled_viewport,
            full_frame,
        } = input;
        if full_frame || scrolled_viewport || !higher_floats.is_empty() {
            return None;
        }
        // Scroll motions merged across the tick's batches: every action
        // finalizes its own batch, so one tick's damage carries one motion
        // per scroll action. Same-region motions sum (the foot
        // `term_damage_scroll` pattern applied at consumption); companion
        // grid rectangles (prints from the same tick) repaint alongside
        // the band, and scrollback ranges ride along (viewport paint
        // ignores them). Different regions, a zero net motion, or a net
        // motion reaching the region height decline to the full path;
        // cross-tick accumulation merges are the frame-merge follow-up,
        // not smuggled here.
        let mut region: Option<(u16, u16)> = None;
        let mut net_lines: i32 = 0;
        for region_damage in leaf_damage.iter() {
            if let DamagedRegion::Scroll { top, bottom, lines } = region_damage {
                let (top, bottom) = (top.min(bottom), top.max(bottom));
                let (top, bottom) = (*top, *bottom);
                match region {
                    Some((known_top, known_bottom))
                        if known_top == top && known_bottom == bottom =>
                    {
                        net_lines = net_lines.saturating_add(*lines);
                    }
                    Some(_) => return None,
                    None => {
                        region = Some((top, bottom));
                        net_lines = *lines;
                    }
                }
            }
        }
        let (top, bottom) = region?;
        let lines = net_lines;
        let height = u32::from(bottom) - u32::from(top) + 1;
        let moved = lines.unsigned_abs();
        if moved == 0 || moved >= height {
            return None;
        }
        if view_scroll_offset != 0 {
            return None;
        }
        // The motion addresses origin-grid coordinates, so the viewport
        // must be the whole origin grid (no scrollback-composited viewport,
        // no crop/pad) and the allocation must match it.
        if view_snapshot.width != base_snap.width || view_snapshot.height != base_snap.height {
            return None;
        }
        if usize::from(frame.cols) != base_snap.width || usize::from(frame.rows) != base_snap.height
        {
            return None;
        }
        if bottom as usize >= view_snapshot.height || view_snapshot.height == 0 {
            return None;
        }
        // Full-grid-height motions only. Sub-region scrolls (IL/DL,
        // DECSTBM) keep rows outside the region stationary, and a retained
        // primitive records no cell row — only its destination — so a
        // glyph overhanging the region boundary cannot be attributed to
        // the moving or the stationary side. Full-height motion has no
        // stationary rows, which removes the ambiguity; sub-region support
        // is follow-up work alongside frame-merge.
        if top != 0 || bottom as usize != view_snapshot.height - 1 {
            return None;
        }
        let retained = self.presented_leaf_frames.get(&view_id)?;
        if retained.origin != (frame.content.x, frame.content.y) {
            return None;
        }
        if retained.scroll_offset != view_scroll_offset || retained.fade_factor != fade {
            return None;
        }
        // Decorated frames clip glyphs to the content ring; a retained
        // clip from an older geometry must never ride a copy, so every
        // kept glyph must already carry the live clip (square frames
        // carry none on either side).
        if retained.glyphs.iter().any(|glyph| glyph.clip != frame_clip) {
            return None;
        }
        // A per-view background image paints the full content rect every
        // frame and does not scroll with the grid; leave those leaves on
        // the full path. (The window image paints behind the grid and is
        // unaffected.)
        if self.config.has_background_images() && self.view_background_for(view_id).image.is_some()
        {
            return None;
        }
        // Exposed band plus one margin row on the scrolled-out side, all
        // full width, plus companion print damage translated to
        // post-tick coordinates — in leaf-local grid coordinates for one
        // partial render call. Translation matters: every action finalizes
        // its own batch, so a print recorded before a same-tick scroll
        // addresses pre-shift rows (the print rides the scroll to its post
        // position, which the retained pre-tick primitives cannot carry).
        // A backward pass accumulates the suffix scroll delta and shifts
        // each grid rectangle into post coordinates; clamped to the grid
        // (clamping only over-damages — fresh renders are always correct —
        // and fully departed rectangles are dropped). The margin row
        // repaints the boundary the copy cannot prove: scrolled-out glyph
        // overhang and surviving cells whose bitmap starts in the vacated
        // rows. Companion rectangles repaint alongside (partial width is
        // fine for painting; only full-width rows gate the drop rule
        // below).
        let (exposed_top, exposed_bottom) = ScrollCopy { top, bottom, lines }.exposed_rows();
        let grid_h = view_snapshot.height;
        // Full-height motion (gated above), so `grid_h >= 1` and the margin
        // row below always exists inside the grid.
        let margin_row: u16 = if lines > 0 {
            0
        } else {
            (grid_h - 1).min(usize::from(u16::MAX)) as u16
        };
        let last_col = view_snapshot
            .width
            .saturating_sub(1)
            .min(usize::from(u16::MAX)) as u16;
        let grid_last_row = (grid_h - 1).min(usize::from(u16::MAX)) as u16;
        let mut regions = vec![
            DamageRect {
                top: exposed_top,
                left: 0,
                bottom: exposed_bottom,
                right: last_col,
            },
            DamageRect {
                top: margin_row,
                left: 0,
                bottom: margin_row,
                right: last_col,
            },
        ];
        // Suffix scroll delta in lines (post = pre - suffix for motion in
        // the merged region; every motion here shares it by construction).
        let mut suffix: i32 = 0;
        let mut translated: Vec<DamageRect> = Vec::new();
        for region_damage in leaf_damage.iter().rev() {
            match region_damage {
                DamagedRegion::Scroll { lines, .. } => {
                    suffix = suffix.saturating_add(*lines);
                }
                DamagedRegion::Grid(rect) => {
                    let shift_row = |row: u16| -> Option<u16> {
                        let post = i64::from(row) - i64::from(suffix);
                        if post < 0 || post > i64::from(grid_last_row) {
                            None
                        } else {
                            Some(post as u16)
                        }
                    };
                    match (shift_row(rect.top), shift_row(rect.bottom)) {
                        (Some(top), Some(bottom)) => translated.push(DamageRect {
                            top: top.min(bottom),
                            left: rect.left,
                            bottom: top.max(bottom),
                            right: rect.right,
                        }),
                        // Partially departed rectangles clamp to the grid:
                        // over-damage only, never under-damage.
                        (Some(top), None) => translated.push(DamageRect {
                            top,
                            left: rect.left,
                            bottom: grid_last_row,
                            right: rect.right,
                        }),
                        (None, Some(bottom)) => translated.push(DamageRect {
                            top: 0,
                            left: rect.left,
                            bottom,
                            right: rect.right,
                        }),
                        // Fully departed: its content left the viewport.
                        (None, None) => {}
                    }
                }
                _ => {}
            }
        }
        // Retained primitives inside a companion rect show pre-print
        // content and are never dropped by the full-width `covered`
        // rule below, so a stale glyph would paint over the fresh
        // fill. Only companion damage the band or margin row already
        // owns may ride the fast path; anything else declines.
        for rect in &translated {
            let in_band = rect.top >= exposed_top && rect.bottom <= exposed_bottom;
            let on_margin = rect.top == margin_row && rect.bottom == margin_row;
            if !in_band && !on_margin {
                return None;
            }
        }
        regions.extend(translated);
        // Full-width repaint rows in window pixels: the band plus the
        // margin row. A shifted primitive fully inside one of these is
        // dropped (the fresh render owns those pixels); anything else is
        // kept unless it is provably stale (see below). Kept primitives
        // always carry correct post-scroll values: the guard above
        // declined every companion overwrite outside these rows, so no
        // kept cell was modified by the tick — shifting preserves
        // content adjacency — and kept/fresh overlaps agree, making
        // paint order between them irrelevant.
        let cell = self.live_cell_metrics();
        let fullwidth_repaint: Vec<bitty_render::geometry::RectPx> = (exposed_top..=exposed_bottom)
            .chain(std::iter::once(margin_row))
            .map(|row| {
                damage_rect_to_window_px(
                    &DamageRect {
                        top: row,
                        left: 0,
                        bottom: row,
                        right: last_col,
                    },
                    origin_px,
                    cell,
                )
            })
            .collect();
        let covered = |rect: &bitty_render::geometry::RectPx| {
            fullwidth_repaint
                .iter()
                .any(|fresh| fresh.contains_rect(rect))
        };
        let dy = -(i64::from(lines)) * i64::from(cell.height);
        let shift_y = |y: i32| -> i32 {
            (i64::from(y) + dy).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        // Pre-shift destination row in content coordinates (Euclidean, so
        // above-content destinations classify stale, never surviving).
        // Fills sit exactly on their cell row; glyph bitmaps may start up
        // to one row off their cell (bearings/overhang), so glyphs carry
        // a one-row margin. Anything beyond one row is pathological and
        // documented as out of scope.
        let dest_row = |y: i32| -> i64 {
            (i64::from(y) - i64::from(origin_px.1)).div_euclid(i64::from(cell.height).max(1))
        };
        let grid_h_i64 = grid_h as i64;
        let stale = |pre_row: i64, is_glyph: bool| -> bool {
            if lines > 0 {
                pre_row < i64::from(moved) + (is_glyph as i64)
            } else {
                pre_row > grid_h_i64 - 1 - i64::from(moved) - (is_glyph as i64)
            }
        };
        let mut fills = Vec::with_capacity(retained.fills.len());
        for fill in &retained.fills {
            let mut kept = fill.clone();
            kept.rect.y = shift_y(kept.rect.y);
            // Covered rows repaint fresh; provably scrolled-out rows are
            // gone. Anything else shifts with its content row.
            if covered(&kept.rect) || stale(dest_row(fill.rect.y), false) {
                continue;
            }
            fills.push(kept);
        }
        let mut glyphs = Vec::with_capacity(retained.glyphs.len());
        for glyph in &retained.glyphs {
            let mut kept = glyph.clone();
            kept.dest[1] = shift_y(kept.dest[1]);
            let bounds = bitty_render::geometry::RectPx::new(
                kept.dest[0],
                kept.dest[1],
                kept.size[0],
                kept.size[1],
            );
            if covered(&bounds) || stale(dest_row(glyph.dest[1]), true) {
                continue;
            }
            glyphs.push(kept);
        }
        Some(ScrollShift {
            regions,
            fills,
            glyphs,
        })
    }

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
        // CTX-1231: every path through here consumes damage (real presents
        // and idle write-backs alike), so no deferred frame is pending after
        // it. The atlas-reject path intentionally bypasses this function and
        // keeps a pending deferral armed.
        self.coalesced_pending = false;
    }

    /// Minimum interval between presented frames (CTX-1231, #2093).
    ///
    /// Present-latest coalescing cadence: dirty ticks inside the interval
    /// defer with damage retained. `ZERO` (default) disables the gate.
    #[must_use]
    pub fn min_present_interval(&self) -> std::time::Duration {
        self.min_present_interval
    }

    /// Sets the present-latest coalescing cadence (CTX-1231, #2093).
    ///
    /// A 60 Hz compositor pace is `Duration::from_millis(16)`. `ZERO`
    /// restores every-dirty-tick presents; a pending deferral still
    /// presents on the next dirty tick because the gate reads the live
    /// value.
    pub fn set_min_present_interval(&mut self, interval: std::time::Duration) {
        self.min_present_interval = interval;
    }

    /// Frames dropped by the coalescing gate with damage retained
    /// (CTX-1231). Monotonic, saturating; idle ticks never count.
    #[must_use]
    pub fn presents_coalesced(&self) -> u64 {
        self.presents_coalesced
    }

    /// Wake deadline for a deferred coalesced frame, if any (CTX-1231).
    ///
    /// `Some(last_present + interval)` while a deferred frame holds
    /// un-presented damage; `None` otherwise (gate off, nothing deferred,
    /// or already presented). The app arms `set_wait_until` at it so a
    /// burst followed by quiet still presents the latest frame on time
    /// instead of holding it until unrelated activity (frame-on-demand
    /// liveness). `checked_add` fail-closed: an unrepresentable deadline
    /// reports `None` and the next dirty tick presents immediately.
    #[must_use]
    pub fn present_coalescing_deadline(&self) -> Option<std::time::Instant> {
        if !self.coalesced_pending {
            return None;
        }
        let last = self.last_present_at?;
        last.checked_add(self.min_present_interval)
    }

    /// Present-latest coalescing gate (CTX-1231, #2093).
    ///
    /// Returns `true` when this dirty tick must skip its present because
    /// the last presented frame is younger than `min_present_interval`.
    /// Damage is retained (the caller returns `None` without marking
    /// presented), so the next tick past the deadline presents the
    /// accumulated latest state: intermediate frames are dropped, never
    /// torn-presented, and the final frame matches an uncoalesced run at
    /// the same clock (same snapshots, same time-driven animation state;
    /// only surface commits are skipped). The first frame always presents
    /// (`last_present_at` is `None`), and a zero interval disables the
    /// gate entirely.
    fn defer_coalesced_present(&mut self, now: std::time::Instant) -> bool {
        if self.min_present_interval.is_zero() {
            return false;
        }
        let Some(last) = self.last_present_at else {
            return false;
        };
        // CTX-1231 review: an unrepresentable wake deadline must never
        // defer without a wake — present immediately instead (fail-closed
        // liveness; only fires for absurd intervals near `Instant::MAX`).
        if last.checked_add(self.min_present_interval).is_none() {
            return false;
        }
        if now.saturating_duration_since(last) >= self.min_present_interval {
            return false;
        }
        self.coalesced_pending = true;
        self.presents_coalesced = self.presents_coalesced.saturating_add(1);
        true
    }

    /// Pushes the Core-owned decoration ring for one leaf and returns the
    /// inner content clip for its glyphs (CTX-0311, moved verbatim from the
    /// leaf loop under CTX-0386 so a reused retained leaf still owns a live
    /// ring). `border == 0` paints nothing; the returned clip is `None` for
    /// square or zero-size frames.
    ///
    /// Issue #2522: every leaf on every tier paints its own ring. The retired
    /// CTX-1280 backdrop suppression hid the tiled leaves' chrome whenever a
    /// detached float was presented, which erased the outlines of panels the
    /// float did not cover; the float's opaque cover (CTX-1271) already owns
    /// occlusion of the area it actually covers.
    fn push_leaf_ring(
        &self,
        frame: &layout_focus::PresentFrame,
        view_id: ViewId,
        open_factor: f32,
        fade: f32,
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
                // CTX-1139 (DEC-0110): a plugin-requested ring pulse fades
                // the same Core-owned ring from transparent to its committed
                // color over the fixed fx duration. Only this Core-owned
                // alpha moves; geometry and terminal content are never
                // interpolated, and the factor is the final `1.0` when no
                // pulse is active on this view.
                let fx_factor = self
                    .fx_progress(view_id, ctx.now)
                    .map(|p| p.clamp(0.0, 1.0))
                    .unwrap_or(1.0);
                // CTX-1162 S3: unfocused leaves dim their Core-owned chrome
                // with the same fade factor the fills/glyphs below carry, so
                // the ring never stays bright on a faded pane. The focused
                // leaf reads exactly `1.0` (no-op fast path). `fade` is
                // computed once per leaf by the caller so the gate, the
                // stored alpha, and the chrome agree bit-for-bit.
                let ring_color = bitty_render::grid::scale_alpha(
                    animated_color,
                    open_factor * ctx.workspace_factor * motion_factor * fx_factor * fade,
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

    /// Inactive-`View` fade factor for one leaf (CTX-1162 S3, DEC-0111
    /// section 4 as corrected by DEC-0113 Fix 2).
    ///
    /// The focused leaf always reads `1.0` (no fade). An unfocused leaf reads
    /// the global `fade_unfocused`, animated through the EXISTING
    /// `animation_progress(Focus)` value armed on the newly focused view when
    /// it animates — so the fade obeys the accepted RFC-0002
    /// durations/easings/reduced-motion/safe gates with zero new
    /// `AnimationKind` — and the committed factor otherwise (instant policies
    /// and the steady state snap to the end value). Sanitized
    /// (non-finite reads as opaque) so a directly constructed config can
    /// never poison the retained alpha.
    fn leaf_fade_factor(&self, is_focused_view: bool, now: std::time::Instant) -> f32 {
        if is_focused_view {
            return 1.0;
        }
        let fade = self.config.style_fade_unfocused;
        if !fade.is_finite() {
            return 1.0;
        }
        let fade = fade.clamp(0.0, 1.0);
        if fade >= 1.0 {
            return 1.0;
        }
        // The fade animates with the focus transition armed on the newly
        // focused view: at flip instant its progress is `0.0` (the leaf that
        // just lost focus starts bright) and it settles at the committed
        // factor as the ring cross-fades in.
        let progress = self
            .focus
            .focused()
            .and_then(|focused| self.animation_progress(AnimationKind::Focus, Some(focused), now));
        match progress {
            Some(p) => 1.0 + (fade - 1.0) * p.clamp(0.0, 1.0),
            None => fade,
        }
    }

    /// Pushes the Core-owned drop shadow for one leaf (CTX-1162 S3, DEC-0113
    /// Fix 1).
    ///
    /// The shadow is one `RoundedFill { border: 0 }` (the solid rounded-rect
    /// SDF path, shared with the glow halo and the ring — zero new
    /// primitives, zero new shaders) at
    /// `(frame.x + shadow_offset_x, frame.y + shadow_offset_y)` with the
    /// frame's outer radius, so it inherits corner-correct AA instead of a
    /// square `FillRect` overhang. Painted FIRST in the leaf's
    /// `combined.rounded` order (shadow, then glow, then ring,
    /// back-to-front); cell fills composite beneath while glyphs and
    /// background blits composite above. Paint-only:
    /// cell metrics, insets, and clip rects are untouched. Returns whether a
    /// shadow was pushed. An unset shadow color pushes nothing, so unstyled
    /// configs present exactly as before.
    fn push_leaf_shadow(
        &self,
        frame: &layout_focus::PresentFrame,
        open_factor: f32,
        fade: f32,
        ctx: &RingFrameContext,
        combined_rounded: &mut Vec<bitty_render::grid::RoundedFill>,
    ) -> bool {
        let Some(shadow_color) = frame.style.shadow_color else {
            return false;
        };
        if frame.frame.width == 0 || frame.frame.height == 0 {
            return false;
        }
        let color = bitty_render::grid::scale_alpha(
            shadow_color,
            open_factor * ctx.workspace_factor * fade,
        );
        let shadow_frame = bitty_render::geometry::RectPx::new(
            px_add(
                px_add(ctx.pad_px, frame.frame.x),
                self.style_offset_physical(frame.style.shadow_offset_x),
            ),
            px_add(
                px_add(ctx.pad_px, frame.frame.y),
                self.style_offset_physical(frame.style.shadow_offset_y),
            ),
            frame.frame.width,
            frame.frame.height,
        );
        combined_rounded.push(bitty_render::grid::RoundedFill {
            frame: shadow_frame,
            border: 0,
            radius: frame.radius,
            color,
        });
        true
    }

    /// Pushes the Core-owned glow halo for one leaf (CTX-1162 S3, DEC-0111
    /// section 4; CTX-1163 S4 pulse-reads-style).
    ///
    /// The halo is one `RoundedFill { border: 0 }` (same struct, same SDF
    /// shader as the shadow and the ring) sized `frame + width` per side
    /// with a concentric radius (square frames stay square), painted after
    /// the shadow and before the ring. Same factor chain as the ring (open,
    /// workspace, motion, fx) plus the unfocused fade. Paint-only: cell
    /// metrics, insets, and clip rects are untouched. Returns whether a
    /// halo was pushed.
    ///
    /// CTX-1163 S4: the fx pulse reads the RESOLVED per-`View` style. A leaf
    /// whose style carries a glow (color set with a nonzero width) pulses
    /// its style halo: color, width, and intensity all come from
    /// `frame.style`. A leaf with no steady-state halo that carries a live
    /// fx pulse instead paints one ring-derived fallback halo (the leaf's
    /// committed ring color and ring width resolved per `View` through
    /// `view_outline_for`, never a hardcoded constant) under the same
    /// envelope, so the default pulse envelope differs from the styled one
    /// exactly by its style contribution. With no style halo and no live
    /// pulse nothing is pushed, so unstyled steady-state configs present
    /// exactly as before.
    fn push_leaf_glow(
        &self,
        frame: &layout_focus::PresentFrame,
        view_id: ViewId,
        open_factor: f32,
        fade: f32,
        ctx: &RingFrameContext,
        combined_rounded: &mut Vec<bitty_render::grid::RoundedFill>,
    ) -> bool {
        if frame.frame.width == 0 || frame.frame.height == 0 {
            return false;
        }
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
        let fx_progress = self
            .fx_progress(view_id, ctx.now)
            .map(|p| p.clamp(0.0, 1.0));
        let fx_factor = fx_progress.unwrap_or(1.0);
        // S4 styled source: the RESOLVED glow halo (color, width, and the
        // alpha carried in the color all come from the per-`View` style).
        // The halo width scales at the live DPI factor exactly like the
        // ring width; `0` logical stays `0` (no halo from rounding).
        let styled = frame.style.glow_color.and_then(|glow_color| {
            if frame.style.glow_width == 0 {
                return None;
            }
            let halo = self.outline_width_physical(frame.style.glow_width);
            if halo == 0 {
                None
            } else {
                Some((glow_color, halo))
            }
        });
        // S4 fallback source: no steady-state halo but a live pulse on this
        // leaf paints the leaf's committed ring color/width (resolved per
        // `View`, mirroring `push_leaf_ring`, including the focus-transition
        // color lerp). A borderless leaf (`ring_border == 0`) paints nothing.
        let fallback = if styled.is_none() && fx_progress.is_some() {
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
            if ring_border == 0 {
                None
            } else {
                let base = if is_focused_view {
                    view_outline.focused
                } else {
                    view_outline.idle
                };
                let ring_color = if is_focused_view {
                    match self.animation_progress(AnimationKind::Focus, Some(view_id), ctx.now) {
                        Some(p) => bitty_render::grid::lerp_rgba(
                            view_outline.idle,
                            view_outline.focused,
                            p,
                        ),
                        None => base,
                    }
                } else {
                    base
                };
                Some((ring_color, ring_border))
            }
        } else {
            None
        };
        let Some((halo_color, halo)) = styled.or(fallback) else {
            return false;
        };
        let color = bitty_render::grid::scale_alpha(
            halo_color,
            open_factor * ctx.workspace_factor * motion_factor * fx_factor * fade,
        );
        let pad = u32::from(halo);
        let glow_frame = bitty_render::geometry::RectPx::new(
            px_add(ctx.pad_px, frame.frame.x).saturating_sub(i32::from(halo)),
            px_add(ctx.pad_px, frame.frame.y).saturating_sub(i32::from(halo)),
            frame.frame.width.saturating_add(pad.saturating_mul(2)),
            frame.frame.height.saturating_add(pad.saturating_mul(2)),
        );
        // Concentric halo: the radius grows with the inflation so the band
        // stays uniform; square frames (`radius == 0`) stay square.
        let radius = if frame.radius == 0 {
            0
        } else {
            frame.radius.saturating_add(halo)
        };
        combined_rounded.push(bitty_render::grid::RoundedFill {
            frame: glow_frame,
            border: 0,
            radius,
            color,
        });
        true
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

    /// Arms the platform-IME caret rect for the deferred focused cursor
    /// (CTX-0367). Split verbatim out of the pre-S7 `paint_cursor` under
    /// #1849 S7 so the caret stays armed before the overlay phase —
    /// the inline preedit overlay and the OS candidate window both anchor
    /// here — while the cursor fill itself paints after the kitty image
    /// layer. A no-op when the window lost focus; like the fill, the caret
    /// tracks the focused leaf only.
    fn arm_ime_caret(&mut self, paint: &CursorPaint) {
        if !self.focused {
            return;
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
    }

    /// Paints the deferred focused-cursor overlay after the kitty image
    /// layer (#1849 S7 cursor-on-top). The fill lands in the CTX-0347
    /// overlay layer so it stays visible above a per-`View` background
    /// image, and the same rect is cleared out of every covering kitty
    /// blit below so the cursor wins over images regardless of image z on
    /// the focused leaf. A no-op when the window lost focus; the cursor
    /// never leaves a stale fill because it is not part of any retained
    /// leaf list. Returns whether a fill was pushed.
    ///
    /// Layer contract (S7): kitty blits keep their 32-blit / 64-MiB
    /// per-frame budget, and the cursor overlay is not counted in it — the
    /// only cursor cost is the pre-existing 1-cell overlay fill (no new
    /// allocation; the punch zeroes owned blit bytes in place). Selection,
    /// banner, scrollbar, and hint fills keep their overlay order under
    /// the kitty layer; only the focused cursor is topmost. The unfocused
    /// leaf is unchanged: `CursorPaint` is built for the focused leaf
    /// only, and the window-focus gate below matches the pre-S7 behavior.
    fn paint_cursor_on_top(
        &mut self,
        paint: &CursorPaint,
        combined_overlay: &mut Vec<bitty_render::grid::FillRect>,
        combined_images: &mut [bitty_render::grid::ImageBlit],
    ) -> bool {
        if !self.focused {
            return false;
        }
        if paint.on_spacer {
            return false;
        }
        let live = self.live_cell_metrics();
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
            // S7: the compositor blends kitty blits after overlay fills, so
            // the fill alone cannot win. Clear the identical rect out of
            // every covering blit (all image z orders); the overlay fill
            // then shows through the transparent window.
            Self::punch_cursor_from_kitty_images(combined_images, rect);
            combined_overlay.push(bitty_render::grid::FillRect {
                rect,
                color: themed,
            });
            return true;
        }
        false
    }

    /// Clears `cursor` out of every kitty blit it overlaps (#1849 S7).
    ///
    /// Zeroes the intersecting bytes of the owned blit buffers in place:
    /// no allocation beyond the pre-existing 1-cell cursor fill, and the
    /// per-frame 32-blit / 64-MiB budget is untouched (counts were already
    /// latched at budget enforcement). All arithmetic is `i64` with
    /// saturation (CTX-0253 F4): blit and cursor origins may be negative
    /// and hostile cell metrics must never wrap the products. A zero-span
    /// cursor punches nothing. The punch window is the exact rect pushed
    /// as the overlay fill, so fill and hole can never disagree.
    ///
    /// Owned-buffer invariant (S6, #1849): every blit here owns its `rgba`
    /// (`Vec<u8>`, built from an owned raster output — cache hits are
    /// cloned before compositing, never shared). The in-place zeroing is
    /// safe only because of that; a future cache must keep handing out
    /// owned buffers (clone-before-write) and never alias resident bytes
    /// into these blits.
    fn punch_cursor_from_kitty_images(
        images: &mut [bitty_render::grid::ImageBlit],
        cursor: bitty_render::geometry::RectPx,
    ) {
        if cursor.width == 0 || cursor.height == 0 {
            return;
        }
        let cursor_left = i64::from(cursor.x);
        let cursor_top = i64::from(cursor.y);
        let cursor_right = cursor_left.saturating_add(i64::from(cursor.width));
        let cursor_bottom = cursor_top.saturating_add(i64::from(cursor.height));
        for blit in images.iter_mut() {
            let blit_left = i64::from(blit.dest.x);
            let blit_top = i64::from(blit.dest.y);
            let blit_right = blit_left.saturating_add(i64::from(blit.dest.width));
            let blit_bottom = blit_top.saturating_add(i64::from(blit.dest.height));
            let left = cursor_left.max(blit_left);
            let top = cursor_top.max(blit_top);
            let right = cursor_right.min(blit_right);
            let bottom = cursor_bottom.min(blit_bottom);
            if right <= left || bottom <= top {
                continue;
            }
            // Blit-local window: the intersection lies inside the blit
            // extent, so the differences are non-negative and fit `usize`.
            let local_left = (left - blit_left) as usize;
            let local_top = (top - blit_top) as usize;
            let local_bottom = (bottom - blit_top) as usize;
            let punch_width = (right - left) as usize;
            let stride = blit.dest.width as usize * 4;
            for row in local_top..local_bottom {
                let start = row * stride + local_left * 4;
                let end = start + punch_width * 4;
                if let Some(span) = blit.rgba.get_mut(start..end) {
                    span.fill(0);
                }
            }
        }
    }

    /// Tick with an explicit wall clock (CTX-0192 virtual-clock seam).
    ///
    /// `tick()` delegates with `Instant::now()`; tests pass virtual times to
    /// prove the transient banner: full summary → flash after
    /// [`PASTE_BANNER_FULL_DURATION`] → still pending (never-silent) → gone
    /// on confirm/cancel. Behavior is otherwise identical to `tick()`.
    pub fn tick_at(&mut self, now: std::time::Instant) -> Option<PresentStats> {
        // CTX-1361: drive native-component lifecycle (handshake deadlines,
        // request deadlines, idle stop, crash backoff) on the same owner
        // thread that owns the runtime and its broker.
        self.pump_component_events(now);
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
        // CTX-1231: present-latest coalescing — drop this intermediate
        // frame when ahead of the present cadence; damage stays retained
        // for the deadline tick. Time-driven state (animations, kitty
        // playback, hover) already advanced in `collect_tick_basis`, so the
        // eventual frame shows the latest state, byte-identical to an
        // uncoalesced run at the same clock.
        if self.defer_coalesced_present(now) {
            return None;
        }
        let mut layers = self.build_leaf_primitives(&basis, now);
        // #1849 S7: arm the IME caret before the overlay phase (the
        // preedit overlay anchors here), but paint the cursor fill after
        // the kitty image layer so the focused cursor wins over covering
        // images regardless of image z.
        let cursor = layers.cursor.take();
        if let Some(ref paint) = cursor {
            self.arm_ime_caret(paint);
        }
        self.paint_frame_overlays(&basis, now, &mut layers);
        self.paint_chrome_bands(basis.pad_px, &mut layers);
        self.paint_plugin_overlay(&basis.allocations, basis.pad_px, &mut layers);
        self.paint_target_annotations(basis.pad_px, &mut layers);
        self.paint_kitty_images(&basis, &mut layers);
        if let Some(paint) = cursor {
            layers.any_needs_draw |= self.paint_cursor_on_top(
                &paint,
                &mut layers.combined_overlay,
                &mut layers.combined_images,
            );
        }
        if self.reject_stale_atlas_frame(&layers) {
            return None;
        }
        let stats = self.present_assembled_frame(basis, layers);
        // CTX-1231: the pacing gate measures real present cadence — stamp
        // only ticks that actually committed a frame, never deferred, idle,
        // or rejected ones.
        if stats.is_some() {
            self.last_present_at = Some(now);
        }
        stats
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

        let mut pending_full = self.pending_full_redraw;
        // Collect allocations deterministically BEFORE the idle check so
        // geometry-only changes are visible (CTX-0228). `present_frames`
        // is pure and bounded by the leaf count.
        let allocations = frames;
        let focused = self.focus.focused();
        // CTX-0248 / CTX-0254 / #1550: Kitty alternate-screen tracking across all
        // visible allocations. An alternate-screen transition on any visible origin
        // forces a full present even when the grid generation is unchanged, so
        // entering alt hides that origin's painted images (and leaving alt repaints
        // the restored grid) instead of idling on a stale frame. Other origins are
        // untouched (CTX-0254 per-origin latch).
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
            // #2442: kitty keeps separate image planes per screen. Every
            // transition swaps the origin's live rich layer (main entries
            // are detached on entry and restored on leave); the sync also
            // bumps the delete epoch so local mediums queued for the hidden
            // plane cannot paint across the switch. Idempotent: the PTY
            // mode-switch hook normally performs the swap first, and this
            // latch then observes no change.
            let changed: Vec<Option<u64>> = current_alt_screens
                .symmetric_difference(&self.kitty_alt_screens_latched)
                .copied()
                .collect();
            for origin in changed {
                self.sync_kitty_screen_for(origin);
            }
            self.kitty_alt_screens_latched = current_alt_screens.clone();
            pending_full = true;
        }
        // S1 (#1849) Kitty animation playback: advance visible origins'
        // descriptors by the present-tick elapsed. Runs before the idle
        // short-circuit (like the panel animations below) so a quiet grid
        // with a running animation still presents its frame changes. A
        // changed frame bumps that origin's generation through the state
        // seam, so the `origins_changed` comparison below observes it and
        // the frame presents damage-driven (no forced full redraw).
        self.advance_kitty_animations_at(now, &allocations, &current_alt_screens);
        let snapshot = self.state.snapshot();
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
        // CTX-1332 C-D4/F7: origin+geometry identity, including the band edge,
        // so an edge move at an unchanged version and a version-1 replacement
        // both force a frame.
        let band_identity = self.band_frame_identity();
        if band_identity != self.last_presented_bands {
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
        // CTX-1139: a live fx pulse is presentation-only chrome motion on the
        // same footing as a live transition — it forces the frame.
        let animations_active = self.animator.is_active(now) || self.fx.is_active(now);
        if animations_active || expired_animations {
            // A live (or just-completed) animation is a presentation-only
            // change: force the full per-leaf path for this frame.
            pending_full = true;
        }
        // CTX-1335: apply paint-only bounds interpolation. The logical
        // `allocations` below stay final (hit-testing, PTY size, and terminal
        // truth never observe the animated rectangles). Issue #2337: the
        // Mod+drag ghost is layered on top as another paint-only transform,
        // so a drag never changes solver geometry or PTY winsize.
        let mut paint_allocations = self.paint_frames_from(&allocations, now);
        if self.tiled_drag_active() {
            let mut paint = paint_allocations
                .take()
                .unwrap_or_else(|| allocations.clone());
            if self.apply_tiled_drag_paint(&mut paint) {
                paint_allocations = Some(paint);
            }
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
        // Issue #2522: the retired CTX-1280 backdrop suppression hid every
        // background-tier leaf's chrome (shadow/glow/ring) whenever a detached
        // float was presented, which erased the outlines of tiled panels the
        // float did not cover. Tiled chrome now always paints; the float's
        // opaque frame cover (parsed in `build_leaf_primitives`, CTX-1271)
        // still owns occlusion of the area the float actually covers, and the
        // stable tier sort keeps every float's chrome above the tiled base.
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
            paint_allocations,
            focused,
            full_frame,
            current_gen,
            view_map,
            pad_px,
            ring_ctx,
            cells_before,
            glyphs_before,
            band_identity,
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
        // CTX-1335: leaf painting reads the interpolated bounds. Every other
        // consumer of `basis.allocations` (hit-testing, PTY, damage identity)
        // keeps the committed logical geometry.
        let allocations = basis.painted_allocations();
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
        // CTX-1165 (#1993): higher detached-float frames in window space, in
        // paint order. A lower leaf glyph sharing positive area with any of
        // these is dropped so the upper float fully occludes lower text, the
        // same opaque boundary the CTX-1148 fill already gives lower fills.
        // Float tier only, so the tiled baseline and higher tiers are
        // unchanged. Empty frames never occlude. Bounded to the visible leaf
        // set.
        let float_occluders: Vec<(usize, bitty_render::geometry::RectPx)> = allocations
            .iter()
            .enumerate()
            .filter(|(_, frame)| {
                frame.tier == Some(OverlayTier::Float)
                    && frame.frame.width > 0
                    && frame.frame.height > 0
            })
            .map(|(index, frame)| {
                (
                    index,
                    bitty_render::geometry::RectPx::new(
                        px_add(pad_px, frame.frame.x),
                        px_add(pad_px, frame.frame.y),
                        frame.frame.width,
                        frame.frame.height,
                    ),
                )
            })
            .collect();
        let built = loop {
            attempt += 1;
            let mut built = CombinedLeaves::default();
            let evictions_before = self.renderer.atlas_stats().2;
            // Issue #1409 (CTX-0797): set when a leaf planned against a newer
            // atlas epoch than the slots already collected in this attempt.
            let mut stale_epoch = false;

            for (leaf_index, frame) in allocations.iter().enumerate() {
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
                // CTX-1162 S3 (DEC-0113 Fix 2a): the leaf fade factor gates
                // retained reuse. The stored fills/glyphs carry baked alpha,
                // so a factor change (focus flip, `fade_unfocused`
                // live-adopt, workspace switch) re-renders even when origin
                // and scroll offset match. Bounded: at most the visible leaf
                // set. The factor is computed once here so the gate, the
                // stored alpha, and the chrome below agree bit-for-bit.
                let fade = self.leaf_fade_factor(ring_ctx.focused_id == Some(view_id), now);
                // CTX-1165 (#1993): higher detached-float frames occlude this
                // leaf glyphs. Later allocations paint above, so only floats
                // after this leaf matter. The slice is rebuilt per leaf from
                // the frame-sized occluder table, so retained reuse and fresh
                // renders agree bit-for-bit. Empty when no higher float
                // exists, keeping the common tiled path allocation-free
                // beyond the small table filter.
                let higher_floats: Vec<bitty_render::geometry::RectPx> = float_occluders
                    .iter()
                    .filter(|(index, _)| *index > leaf_index)
                    .map(|(_, rect)| *rect)
                    .collect();
                let reuse = !full_frame
                    && !leaf_damaged
                    && self
                        .presented_leaf_frames
                        .get(&view_id)
                        .is_some_and(|leaf| {
                            leaf.origin == (frame.content.x, frame.content.y)
                                && leaf.scroll_offset
                                    == view.map(|v| v.scroll_offset()).unwrap_or(0)
                                && leaf.fade_factor == fade
                        });

                // CTX-1148 (#1978) + CTX-1271 (#2138): detached floats
                // must paint opaque. The grid fills cover only the content
                // rect; the decoration frame (border plus content-inset
                // band, sub-cell remainder) would otherwise show tiled
                // content beneath (siblings refill the container after
                // detach, unlike the keep-slot model). One theme-bg fill
                // covering the float frame occludes the base beneath the
                // cell fills. The `DrawList` order paints fills before
                // rounded fills, so lower leaves' rings/shadows/glows would
                // still draw over that fill (#2138): a second solid
                // rounded cover (same frame, `border: 0`) occludes lower
                // chrome in the rounded pass too, ahead of this float's
                // own shadow/glow/ring below. Float tier only, so the tiled
                // baseline is unchanged. Empty frames skip fail-closed.
                // Bounded: one fill plus one rounded cover per float frame.
                if frame.tier == Some(OverlayTier::Float)
                    && frame.frame.width > 0
                    && frame.frame.height > 0
                {
                    let bg = self.renderer.theme_palette().background;
                    let cover = bitty_render::geometry::RectPx::new(
                        px_add(pad_px, frame.frame.x),
                        px_add(pad_px, frame.frame.y),
                        frame.frame.width,
                        frame.frame.height,
                    );
                    built.fills.push(bitty_render::grid::FillRect {
                        rect: cover,
                        color: bg,
                    });
                    built.rounded.push(bitty_render::grid::RoundedFill {
                        frame: cover,
                        border: 0,
                        radius: frame.radius,
                        color: bg,
                    });
                    built.needs_draw = true;
                }

                // CTX-0311 ring (+ CTX-1162 S3 shadow/glow): shared by the
                // reuse and re-render paths so a retained leaf still paints
                // its live focus/open chrome. Back-to-front in
                // `combined.rounded`: shadow, then glow, then ring.
                let shadow_painted =
                    self.push_leaf_shadow(frame, open_factor, fade, &ring_ctx, &mut built.rounded);
                let glow_painted = self.push_leaf_glow(
                    frame,
                    view_id,
                    open_factor,
                    fade,
                    &ring_ctx,
                    &mut built.rounded,
                );
                let (frame_clip, ring_painted) = self.push_leaf_ring(
                    frame,
                    view_id,
                    open_factor,
                    fade,
                    &ring_ctx,
                    &mut built.rounded,
                );
                built.needs_draw |= shadow_painted || glow_painted || ring_painted;

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
                                        let mut rgba = blit.rgba.clone();
                                        // CTX-1162 S3: the resolved per-`View`
                                        // tint washes the blit at strength
                                        // (opaque full-strength stamps the
                                        // tint RGB, coverage preserved). A
                                        // tint without an image stays inert:
                                        // this is the only tint consumer.
                                        if let Some(tint) = frame.style.background_tint {
                                            if frame.style.background_tint_strength > 0.0 {
                                                bitty_render::grid::tint_rgba(
                                                    &mut rgba,
                                                    tint,
                                                    frame.style.background_tint_strength,
                                                );
                                            }
                                        }
                                        // CTX-1162 S3: unfocused leaves fade
                                        // the blit exactly like their
                                        // fills/glyphs. Rebuilt every frame,
                                        // so no retained gate is needed here.
                                        if fade < 1.0 {
                                            bitty_render::window::dim_rgba_alpha(&mut rgba, fade);
                                        }
                                        if let Ok(entry) = bitty_render::grid::ImageBlit::try_new(
                                            bitty_render::geometry::RectPx::new(
                                                blit.dest.x,
                                                blit.dest.y,
                                                blit.dest.width,
                                                blit.dest.height,
                                            ),
                                            rgba,
                                        ) {
                                            // CTX-1181 (#2041): backgrounds
                                            // paint above the CTX-1148 opaque
                                            // fill, so a lower leaf image
                                            // would otherwise show through a
                                            // higher float. Split it around
                                            // the higher float frames; the
                                            // covered part is owned by the
                                            // upper float alone while the
                                            // visible part stays
                                            // pixel-identical. Pieces share
                                            // the admitted byte budget
                                            // below, skip-and-continue past
                                            // the caps like the kitty layer.
                                            for piece in
                                                clip_blit_around_floats(entry, &higher_floats)
                                            {
                                                let piece_bytes = piece.rgba.len();
                                                if built.backgrounds.len()
                                                    >= bitty_rich::BG_PRESENT_MAX_BLITS_PER_FRAME
                                                    || built
                                                        .background_bytes
                                                        .saturating_add(piece_bytes)
                                                        > bitty_rich::BG_PRESENT_MAX_BYTES_PER_FRAME
                                                {
                                                    continue;
                                                }
                                                built.background_bytes = built
                                                    .background_bytes
                                                    .saturating_add(piece_bytes);
                                                built.backgrounds.push(piece);
                                                built.needs_draw = true;
                                            }
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
                    // CTX-1165 (#1993): a retained lower leaf still occludes
                    // under a higher float. The stored list is reused verbatim
                    // except for glyphs sharing area with a higher float
                    // frame, which the upper opaque fill must own alone.
                    // Fills stay: the upper opaque fill already covers them in
                    // paint order.
                    if higher_floats.is_empty() {
                        built.glyphs.extend(leaf.glyphs.iter().cloned());
                    } else {
                        for glyph in &leaf.glyphs {
                            if glyph_occluded_by_floats(glyph.dest, glyph.size, &higher_floats) {
                                continue;
                            }
                            built.glyphs.push(glyph.clone());
                        }
                    }
                    if is_focused_view && !scrolled {
                        built.cursor = self.reused_cursor_paint(
                            frame,
                            view_id,
                            snapshot,
                            origin_px_x,
                            origin_px_y,
                        );
                        // CTX-1173 (#2031): a focused lower cursor under a
                        // higher float must not leak through the upper opaque
                        // fill. Same occluder table and intersection test as
                        // the CTX-1165 glyph path above; the IME caret stays
                        // hidden with the cursor because it arms from the
                        // drained paint below. Empty table skips the probe so
                        // the common path pays nothing.
                        let occluded = if higher_floats.is_empty() {
                            false
                        } else {
                            built.cursor.as_ref().is_some_and(|paint| {
                                let live = self.live_cell_metrics();
                                cursor_occluded_by_floats(
                                    paint,
                                    [live.width, live.height],
                                    &higher_floats,
                                )
                            })
                        };
                        if occluded {
                            built.cursor = None;
                        }
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
                let mut view_snapshot = if let Some(v) = view {
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

                // Kitty Unicode placeholders are image anchors, not text:
                // the glyph is a private-use scalar with no printable
                // form, and painting a fallback box would leave a solid
                // band wherever an image is pending or its prototype was
                // dropped (issue #2453). Blank the cell for the glyph pass;
                // the real image is blitted on top by
                // `paint_kitty_unicode_runs`, and the terminal-truth cell
                // keeps the placeholder for reflow/deletion.
                for cell in &mut view_snapshot.cells {
                    if bitty_term_state::is_kitty_placeholder(cell.glyph) {
                        cell.glyph = ' ';
                        cell.zerowidth = bitty_term_state::Zerowidth::new();
                    }
                }

                // A re-rendered leaf is redrawn in full, except for the
                // CTX-1207 scroll fast path: a single scroll-region motion
                // shifts the retained primitives (region-copy) and repaints
                // only the exposed band plus companion print damage
                // (damage-only repaint). Arbitrary sub-pane partial merges
                // stay off: retained glyphs can paint outside their cell
                // (metric overhang), so intersecting them with sub-cell
                // damage would under-damage neighbouring cells — the scroll
                // motion is the proven exception (shifted primitives are
                // bit-identical to fresh placement, overhang included).
                // Pane granularity stays the containment unit otherwise and
                // over-damage stays the only failure direction.
                let mut shift = self.scroll_retained_shift(ScrollShiftInput {
                    view_id,
                    frame,
                    leaf_damage: &leaf_damage,
                    view_snapshot: &view_snapshot,
                    base_snap,
                    view_scroll_offset: view.map(|v| v.scroll_offset()).unwrap_or(0),
                    origin_px: (origin_px_x, origin_px_y),
                    fade,
                    frame_clip,
                    higher_floats: &higher_floats,
                    scrolled_viewport: scrolled,
                    full_frame,
                });
                let mut damage = match &shift {
                    Some(shift) => Damage {
                        generation: current_gen,
                        regions: shift
                            .regions
                            .iter()
                            .map(|rect| DamagedRegion::Grid(*rect))
                            .collect::<Vec<_>>()
                            .into_boxed_slice(),
                    },
                    None => Damage {
                        generation: current_gen,
                        regions: vec![DamagedRegion::Grid(DamageRect::full(
                            view_snapshot.height as u16,
                            view_snapshot.width as u16,
                        ))]
                        .into_boxed_slice(),
                    },
                };
                let epoch_before = self.renderer.atlas_epoch();
                let mut list = match self.renderer.render(&view_snapshot, &damage) {
                    Ok(list) => list,
                    Err(_) => continue,
                };
                if shift.is_some() && self.renderer.atlas_epoch() != epoch_before {
                    // Atlas growth (or reset) mid-render leaves the shifted
                    // UVs stale while the fresh list is already healed; a
                    // full re-render heals the leaf the same way. Rare and
                    // bounded: at most two renders for this leaf.
                    shift = None;
                    damage = Damage {
                        generation: current_gen,
                        regions: vec![DamagedRegion::Grid(DamageRect::full(
                            view_snapshot.height as u16,
                            view_snapshot.width as u16,
                        ))]
                        .into_boxed_slice(),
                    };
                    list = match self.renderer.render(&view_snapshot, &damage) {
                        Ok(list) => list,
                        Err(_) => continue,
                    };
                }
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
                        let paint = CursorPaint {
                            origin_x: origin_px_x,
                            origin_y: origin_px_y,
                            cursor: view_snapshot.cursor.clone(),
                            cols: view_snapshot.width,
                            rows: view_snapshot.height,
                            cells_available: frame.cols.saturating_sub(cur.col).max(1),
                            on_spacer,
                        };
                        // CTX-1173 (#2031): same higher-float occluder as the
                        // reuse path and the CTX-1165 glyph filter. A cursor
                        // cell covered by a higher float is skipped so the
                        // focused lower cursor never paints over the upper
                        // opaque fill. Fail-closed on partial coverage; the
                        // retained store is unaffected because the cursor is
                        // never retained.
                        let occluded = if higher_floats.is_empty() {
                            false
                        } else {
                            let live = self.live_cell_metrics();
                            cursor_occluded_by_floats(
                                &paint,
                                [live.width, live.height],
                                &higher_floats,
                            )
                        };
                        if !occluded {
                            built.cursor = Some(paint);
                        }
                    }
                }

                let leaf_painted = list.needs_draw();
                let mut fills = list.fills;
                for fill in &mut fills {
                    fill.rect.x = px_add(fill.rect.x, origin_px_x);
                    fill.rect.y = px_add(fill.rect.y, origin_px_y);
                    // CTX-1162 S3: unfocused leaves bake the fade factor into
                    // cell fills at build time. `1.0` skips the write so
                    // focused (and default) leaves stay byte-identical.
                    if fade < 1.0 {
                        fill.color = bitty_render::grid::scale_alpha(fill.color, fade);
                    }
                }
                let mut glyphs = list.glyphs;
                for glyph in &mut glyphs {
                    glyph.dest[0] = px_add(glyph.dest[0], origin_px_x);
                    glyph.dest[1] = px_add(glyph.dest[1], origin_px_y);
                    // CTX-0311 inner-arc clip: only decorated rounded frames
                    // set it; square frames keep the documented overhang.
                    glyph.clip = frame_clip;
                    // CTX-1162 S3: same fade for glyph-instance alpha (the
                    // retained list reuses these verbatim, guarded by the
                    // `fade_factor` gate above).
                    if fade < 1.0 {
                        glyph.color = bitty_render::grid::scale_alpha(glyph.color, fade);
                    }
                }
                built.needs_draw |= leaf_painted || !fills.is_empty() || !glyphs.is_empty();
                // CTX-1207: the shifted retained primitives paint beneath
                // the fresh partial list, which owns every repaint pixel
                // (dropped intersections guarantee no double-paint).
                if let Some(shift) = shift {
                    fills = shift.fills.into_iter().chain(fills).collect();
                    glyphs = shift.glyphs.into_iter().chain(glyphs).collect();
                }
                built.needs_draw |= !fills.is_empty() || !glyphs.is_empty();
                // CTX-1165 (#1993): the retained store keeps the full leaf so
                // a moved upper float never strands a filtered reuse. Only the
                // combined frame drops glyphs under higher floats.
                self.presented_leaf_frames.insert(
                    view_id,
                    PresentedLeaf {
                        origin: (frame.content.x, frame.content.y),
                        scroll_offset: view.map(|v| v.scroll_offset()).unwrap_or(0),
                        fade_factor: fade,
                        fills: fills.clone(),
                        glyphs: glyphs.clone(),
                    },
                );
                built.fills.extend(fills);
                if higher_floats.is_empty() {
                    built.glyphs.extend(glyphs);
                } else {
                    for glyph in glyphs {
                        if glyph_occluded_by_floats(glyph.dest, glyph.size, &higher_floats) {
                            continue;
                        }
                        built.glyphs.push(glyph);
                    }
                }
            }

            // CTX-1076 (issue #1815): the window background image paints
            // behind the grid (whole window, including the padding band),
            // below per-`View` backgrounds, overlay fills, and glyphs. The
            // decode already happened at config reconcile; this path is a
            // pure cache lookup plus a bounded nearest-neighbor scale. Per-View
            // blits are admitted first above, and the window blit is prepended
            // only when frame capacity and byte budget remain, sharing the BG-7
            // budget (32 blits / 64 MiB) so a hostile config can never allocate
            // beyond the accepted present bound. Resolution is skipped entirely
            // when no window image is configured
            // (`has_window_background_image`), and when the sanitized opacity
            // is zero (fully transparent: no visual effect), keeping the common
            // image-less frame free of any resolve or allocation.
            if self.config.has_window_background_image() {
                let opacity = bitty_render::window::sanitize_window_background_opacity(
                    self.config.window_background_opacity,
                );
                if opacity > 0.0 {
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
                                                bitty_render::grid::ImageBlit::try_new(
                                                    positioned, rgba,
                                                )
                                            {
                                                // CTX-1181 (#2041): the
                                                // window blit spans the whole
                                                // window behind every leaf,
                                                // including the gap bands and
                                                // the float frames above the
                                                // CTX-1148 opaque fill. Split
                                                // it around every float frame
                                                // so gaps under a float stay
                                                // opaque while the visible
                                                // window image is unchanged.
                                                // Pieces prepend behind the
                                                // per-View blits in order.
                                                let window_occluders: Vec<
                                                    bitty_render::geometry::RectPx,
                                                > = float_occluders
                                                    .iter()
                                                    .map(|(_, rect)| *rect)
                                                    .collect();
                                                let mut insert_at = 0usize;
                                                for piece in clip_blit_around_floats(
                                                    entry,
                                                    &window_occluders,
                                                ) {
                                                    let piece_bytes = piece.rgba.len();
                                                    if built.backgrounds.len()
                                                        >= bitty_rich::BG_PRESENT_MAX_BLITS_PER_FRAME
                                                        || built
                                                            .background_bytes
                                                            .saturating_add(piece_bytes)
                                                            > bitty_rich::BG_PRESENT_MAX_BYTES_PER_FRAME
                                                    {
                                                        continue;
                                                    }
                                                    built.background_bytes = built
                                                        .background_bytes
                                                        .saturating_add(piece_bytes);
                                                    // Prepend behind the per-View
                                                    // blits admitted above.
                                                    built.backgrounds.insert(insert_at, piece);
                                                    insert_at = insert_at.saturating_add(1);
                                                    built.needs_draw = true;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
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
    /// kitty image layer is painted afterwards by [`Self::paint_kitty_images`],
    /// and the focused cursor after that by [`Self::paint_cursor_on_top`]
    /// (#1849 S7 cursor-on-top).
    fn paint_frame_overlays(
        &mut self,
        basis: &TickBasis,
        now: std::time::Instant,
        layers: &mut FrameLayers,
    ) {
        self.paint_closing_rings(basis.pad_px, now, layers);
        // CTX-1335: overlays anchored to a leaf follow its interpolated paint
        // bounds so content, cursor, and selection never tear apart during a
        // geometry transition. Their logical state and hit-testing remain
        // final.
        let paint = basis.painted_allocations();
        self.paint_selection_highlight(paint, basis.pad_px, layers);
        self.paint_plaintext_hover(paint, basis.pad_px, layers);
        self.paint_ime_preedit(layers);
        self.paint_pending_banners(paint, &basis.view_map, basis.pad_px, now, layers);
        self.paint_hyperlink_hover(paint, basis.pad_px, layers);
        self.paint_help_overlay(paint, &basis.view_map, basis.pad_px, layers);
        self.paint_bell_and_notification(paint, basis.pad_px, now, layers);
        self.paint_scrollbar_overlay(layers);
        self.paint_hint_overlay(paint, basis.pad_px, layers);
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
        let Some((selection_state, state)) = self.selection_owner_state() else {
            return;
        };
        let owner = selection_state.owner;
        let sel = selection_state.selection;
        // Issue #2480: a caret drag over exactly one cell is a real
        // selection even though the inclusive range is collapsed. A pointer
        // press installs a collapsed buffer range immediately, so a collapsed
        // buffer without the one-cell flag is still just a click and must not
        // paint.
        let buffer_selected = selection_state
            .buffer
            .is_some_and(|pers| pers.anchor != pers.focus);
        if sel.is_empty() && !selection_state.single_cell && !buffer_selected {
            return;
        }
        // Resolve the owner's scroll state through the shared owner lookup:
        // detached floats and pinned leaves are not `view_map` layout leaves,
        // so a map-only check read offset 0 and painted their scrolled range
        // against the live grid (issue #2523 review).
        let scrolled = self.is_viewport_scrolled(owner);
        let Some(frame) = allocations.iter().find(|frame| frame.view == owner) else {
            return;
        };
        let live = self.live_cell_metrics();
        // Issue #2470: pointer selections paint from their buffer anchors so
        // a range spanning several scrollback pages stays continuous.
        let (snapshot, row_offset, start, end) = if let Some(pers) = selection_state.buffer {
            // Match `selection_text`: a pruned or drifted buffer range has no
            // text, so it must not paint stale rows either (issue #2523
            // review).
            if !pers.is_valid(state) {
                return;
            }
            let (paint_start, snapshot, row_offset) = if scrolled {
                let Some(window) = self.selection_window(owner) else {
                    return;
                };
                let Some(viewport) = self.viewport_snapshot_for(owner) else {
                    return;
                };
                (window.start, viewport, 0usize)
            } else {
                let window_start = self.owner_row_window_start(owner, frame.rows);
                let snap = state.snapshot();
                (
                    state.scrollback_len().saturating_add(window_start),
                    snap,
                    window_start,
                )
            };
            let Some((start, end)) =
                self.project_persistent_to_frame(&pers, paint_start, frame.rows, frame.cols)
            else {
                return;
            };
            (snapshot, row_offset, start, end)
        } else if scrolled {
            let norm = sel.normalized();
            let Some(viewport) = self.viewport_snapshot_for(owner) else {
                return;
            };
            (viewport, 0, norm.start, norm.end)
        } else {
            let window_start = self.owner_row_window_start(owner, frame.rows);
            let norm = sel.normalized();
            let (start, end) = if sel.kind == SelectionKind::Block {
                // Block keeps its column window on every visible row; a
                // text-flow clip would widen the first visible row to the
                // frame edge and reintroduce the zigzag (issue #2523).
                let window_end = window_start.saturating_add(usize::from(frame.rows));
                let start_row = usize::from(norm.start.row);
                let end_row = usize::from(norm.end.row);
                if end_row < window_start || start_row >= window_end {
                    return;
                }
                let local_start = start_row.saturating_sub(window_start);
                let local_end = end_row
                    .saturating_sub(window_start)
                    .min(usize::from(frame.rows - 1));
                (
                    CellPos::new(local_start as u16, norm.start.col),
                    CellPos::new(local_end as u16, norm.end.col),
                )
            } else {
                // Frame-local span: the inverse of the pointer mapping. The
                // part of the span above or below the painted window is
                // clipped away.
                let Some((start, end)) =
                    clip_span_to_window(norm.start, norm.end, window_start, frame.rows, frame.cols)
                else {
                    return;
                };
                (CellPos::new(start.0, start.1), CellPos::new(end.0, end.1))
            };
            (state.snapshot(), window_start, start, end)
        };
        let rects = selection_row_rects(
            &self.config.theme,
            sel.kind,
            start,
            end,
            frame.cols,
            frame.rows,
            &snapshot.cells,
            snapshot.width,
            snapshot.height,
            row_offset,
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
                        color: bitty_render::grid::ime_preedit_bg_in(&self.config.theme),
                    });
                    layers.combined_overlay.push(bitty_render::grid::FillRect {
                        rect: bitty_render::geometry::RectPx::new(
                            base_x,
                            underline_y,
                            preedit_width,
                            2,
                        ),
                        color: bitty_render::grid::ime_preedit_underline_in(&self.config.theme),
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
            color: bitty_render::grid::banner_fill_in(&self.config.theme),
        });
        let glyphs = self.renderer.overlay_text_glyphs(
            banner,
            (origin_px_x, banner_y),
            max_cells,
            bitty_render::grid::banner_text_in(&self.config.theme),
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
        use super::band_host::band_is_visible;

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
        //
        // Only horizontal edges exist here: `ui_slot_placement` rejects
        // `left`/`right` at mount until a vertical band host ships
        // (CTX-1341), so no vertical block can reach the paint plan.
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
            let (text, runs) = super::band_host::flatten_band_runs_in_width(&band.root, band_cells);
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
    /// block carries: the identical flattening walk the band renderer uses
    /// (so the v1 scene budgets enforced at mount time are the bounds painted
    /// here), centered over the focused frame and clipped to it, on the same
    /// overlay layer as the banner pills (fills plus glyphs, never grid
    /// truth). No-op with no surface, so capture-less frames are
    /// byte-identical to before. A live surface with empty text still paints
    /// its one-cell pill chrome: the modal holds keyboard capture while up,
    /// so pixel-invisibility would be a fail-open UX (CTX-1143, issue
    /// #1966).
    ///
    /// CTX-1170 (issue #1972) scene chrome: a titled `List`/`Column` scene
    /// paints one pill row per child — the title row plus the input/item
    /// rows — instead of one flattened pill, so the modal reads as a titled
    /// command line (never a bare pill). Rows past the frame bottom clip
    /// away; per-row width clips to the frame like the single pill did.
    ///
    /// CTX-1419 (issue #2191) style parity: the overlay surface accepts the
    /// same `UiNode` vocabulary as a band, including `fg`/`bg`/`bold`, so
    /// those declarations are painted instead of silently dropped. See
    /// [`Self::paint_overlay_row`].
    fn paint_plugin_overlay(
        &mut self,
        allocations: &[layout_focus::PresentFrame],
        pad_px: i32,
        layers: &mut FrameLayers,
    ) {
        let Some(overlay) = self.plugin_overlay.clone() else {
            return;
        };
        let rows = Self::overlay_scene_rows(&overlay.root);
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
        let full_w = px_span(frame.cols, live.width);
        // Issue #2411: a hover-anchored tooltip places the row stack just
        // below a top band / just above a bottom band, horizontally centered
        // on the hovered region and clamped to the window (issue #2530). The
        // anchor names a live band item, so the row is resolved now, never
        // baked in at set time. A missing band or violated geometry falls
        // back to the centered modal placement instead of painting against
        // stale coordinates.
        let window = self.window_cells();
        // Widest row of the stack decides the centering rectangle; anchored
        // empty rows paint nothing and do not contribute width.
        let stack_cells = rows
            .iter()
            .filter(|row| !row.text.is_empty())
            .map(|row| {
                u16::try_from(clip_to_width_cells(&row.text, window.width).1).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(0);
        let anchored = self.plugin_overlay_anchor.and_then(|anchor| {
            if window.width == 0 || window.height == 0 || self.band_geometry_violated() {
                return None;
            }
            let band_row = self.visible_band_row(anchor.edge, anchor.visible_index)?;
            let count = u16::try_from(rows.len()).unwrap_or(u16::MAX);
            let (start_row, limit_row) = match anchor.edge {
                BandEdge::Top => (band_row.saturating_add(1), None),
                // The stack ends at the band row (exclusive): a tooltip taller
                // than the space above the band clips instead of painting over
                // its own band (CodeRabbit, PR #2423).
                BandEdge::Bottom => (band_row.saturating_sub(count), Some(band_row)),
                BandEdge::Left | BandEdge::Right => return None,
            };
            let x = centered_tooltip_x(anchor.start_col, anchor.end_col, stack_cells, window.width);
            Some(AnchoredOverlayStack {
                start_row,
                x,
                limit_row,
            })
        });
        let mid_row = frame.rows.saturating_sub(1) / 2;
        // Center the row stack on the middle row: the title row lands just
        // above middle for a two-row modal, exactly on it for one row.
        let half = u16::try_from(rows.len().saturating_sub(1) / 2).unwrap_or(u16::MAX);
        let start_row = mid_row.saturating_sub(half);
        let mut painted = false;
        for (index, overlay_row) in rows.iter().enumerate() {
            let offset = u16::try_from(index).unwrap_or(u16::MAX);
            let (row, placement, limit_row) = match anchored {
                Some(stack) => {
                    let row = stack.start_row.saturating_add(offset);
                    (
                        row,
                        Some(AnchoredOverlayRow {
                            x: stack.x,
                            cols: window.width.saturating_sub(stack.x),
                            row,
                            window_x: window.x,
                        }),
                        stack.limit_row,
                    )
                }
                None => (start_row.saturating_add(offset), None, None),
            };
            let out_of_bounds = match placement {
                Some(placement) => {
                    placement.row >= window.height
                        || limit_row.is_some_and(|limit| placement.row >= limit)
                        || placement.cols == 0
                        || placement.x >= window.width
                }
                None => row >= frame.rows,
            };
            if out_of_bounds {
                continue;
            }
            painted |=
                self.paint_overlay_row(pad_px, layers, frame, row, overlay_row, full_w, placement);
        }
        if painted {
            layers.any_needs_draw = true;
        }
    }

    /// Paints one overlay scene row (CTX-1170; style parity CTX-1419).
    ///
    /// A row whose runs declare no `fg`/`bg`/`bold` keeps the pre-parity
    /// single-pill path byte-for-byte: one `background` fill and one
    /// `overlay_text_glyphs` call for the clipped line. A row that declares
    /// style paints per flattened run with the same closed host token table
    /// the band renderer uses
    /// ([`resolve_band_token`](super::band_host::resolve_band_token)), so a
    /// plugin's declared style is honored instead of silently dropped. An
    /// unknown token falls back to the span default and counts
    /// [`BandHostStats::unknown_tokens`](super::band_host::BandHostStats), and
    /// `bold` reuses the band renderer's synthetic double-strike. A styled row
    /// that paints no cell (empty text, or every span clipped away) falls back
    /// to the same single pill, so CTX-1143's fail-open chrome holds on both
    /// paths.
    ///
    /// Returns whether the row painted anything. An anchored (non-modal)
    /// tooltip row with empty text paints nothing: the fail-open pill belongs
    /// to the capture-holding modal, not to hover presentation (issue #2411).
    #[allow(clippy::too_many_arguments)]
    fn paint_overlay_row(
        &mut self,
        pad_px: i32,
        layers: &mut FrameLayers,
        frame: &layout_focus::PresentFrame,
        row: u16,
        overlay_row: &OverlayRow,
        full_w: u32,
        anchored: Option<AnchoredOverlayRow>,
    ) -> bool {
        use super::band_host::resolve_band_token;

        if anchored.is_some() && overlay_row.text.is_empty() {
            return false;
        }
        let live = self.live_cell_metrics();
        let cell_w = live.width;
        let cell_h = live.height;
        // Clipping is to the focused frame for a centered modal and to the
        // remaining window width for a hover-anchored tooltip (issue #2411).
        let clip_cols = anchored.map_or(frame.cols, |anchor| anchor.cols);
        // Width in display cells (wide glyphs count 2), clipped to the surface.
        let (clipped, text_cells) = clip_to_width_cells(&overlay_row.text, clip_cols);
        // Center the pill in the focused frame on its row. `text_cells` is
        // already floored at one cell by `clip_to_width_cells`, and the floor
        // is repeated here so the pill width never depends on that helper: an
        // empty composer buffer must still paint a one-cell rect instead of a
        // zero-width one (CTX-1143, CTX-1419, issue #2191).
        let pill_w = px_span_usize(text_cells.max(1), cell_w);
        let (origin_px_x, origin_px_y) = match anchored {
            Some(anchor) => (
                px_add(
                    pad_px,
                    px_offset_cells(i32::from(anchor.window_x), anchor.x, cell_w),
                ),
                px_add(pad_px, px_offset_cells(0, anchor.row, cell_h)),
            ),
            None => (
                px_add(
                    px_add(pad_px, frame.content.x),
                    px_side(full_w.saturating_sub(pill_w)) / 2,
                ),
                px_add(pad_px, px_offset_cells(frame.content.y, row, cell_h)),
            ),
        };
        let styled = overlay_row
            .runs
            .iter()
            .any(|run| run.fg.is_some() || run.bg.is_some() || run.bold);
        let pill = OverlayPill {
            origin_x: origin_px_x,
            origin_y: origin_px_y,
            width_px: pill_w,
            height_px: cell_h,
            text_cells,
        };
        if !styled {
            self.paint_overlay_pill(layers, &clipped, pill);
            return true;
        }
        let default_fg = self.config.theme.foreground;
        let default_bg = self.config.theme.background;
        let chars: Vec<char> = overlay_row.text.chars().collect();
        let cols = usize::from(clip_cols);
        let mut painted_any = false;
        for run in &overlay_row.runs {
            let start = run.start_col;
            if start >= cols {
                break;
            }
            let mut span = String::new();
            let mut cells = 0usize;
            for ch in chars
                .iter()
                .skip(run.start_char)
                .take(run.end_char.saturating_sub(run.start_char))
            {
                let width = usize::from(bitty_term_state::char_cell_width(*ch));
                if start.saturating_add(cells).saturating_add(width) > cols {
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
            let run_x = px_add(
                pill.origin_x,
                px_offset_cells(0, u16::try_from(start).unwrap_or(u16::MAX), cell_w),
            );
            layers.combined_overlay.push(bitty_render::grid::FillRect {
                rect: bitty_render::geometry::RectPx::new(
                    run_x,
                    pill.origin_y,
                    px_span_usize(cells, cell_w),
                    cell_h,
                ),
                color: bg,
            });
            let mut glyphs =
                self.renderer
                    .overlay_text_glyphs(&span, (run_x, pill.origin_y), cells, fg);
            if run.bold {
                // Same synthetic double-strike the band renderer uses.
                let mut doubled = glyphs.clone();
                for glyph in &mut doubled {
                    glyph.dest[0] = glyph.dest[0].saturating_add(1);
                }
                glyphs.extend(doubled);
            }
            layers.combined_glyphs.extend(glyphs);
            painted_any = true;
        }
        if !painted_any {
            // Fail-open chrome (CTX-1143): a styled row whose runs produce no
            // cell (empty text, or every span clipped away) still paints the
            // default pill. The overlay holds keyboard capture while up, so an
            // invisible surface would be a fail-open UX (CTX-1419, #2191).
            self.paint_overlay_pill(layers, &clipped, pill);
        }
        true
    }

    /// Paints the default single pill for one overlay row (CTX-1170).
    ///
    /// Shared by the unstyled fast path and the styled-row fallback so a live
    /// surface always keeps its one-cell chrome even when no styled run paints
    /// (CTX-1143, issue #1966; CTX-1419, issue #2191).
    fn paint_overlay_pill(&mut self, layers: &mut FrameLayers, clipped: &str, pill: OverlayPill) {
        layers.combined_overlay.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                pill.origin_x,
                pill.origin_y,
                pill.width_px,
                pill.height_px,
            ),
            color: self.config.theme.background,
        });
        let glyphs = self.renderer.overlay_text_glyphs(
            clipped,
            (pill.origin_x, pill.origin_y),
            pill.text_cells,
            self.config.theme.foreground,
        );
        layers.combined_glyphs.extend(glyphs);
    }

    /// Present-time target annotation batch (W-29 successor, issue #2253).
    ///
    /// Paints one bounded layer of label pills at the annotation anchors, in
    /// batch (paint) order, above the plugin overlay and below kitty images.
    /// Presentation-only: no grid, cursor, scrollback, or layout mutation.
    /// Anchors are window cell coordinates; the window-padding inset is added
    /// exactly like every other overlay phase. No-op with no batch.
    fn paint_target_annotations(&mut self, pad_px: i32, layers: &mut FrameLayers) {
        let Some(annotations) = self.target_annotations.clone() else {
            return;
        };
        let live = self.live_cell_metrics();
        let cell_w = live.width.max(1);
        let cell_h = live.height.max(1);
        if cell_w == 0 || cell_h == 0 {
            return;
        }
        let viewport = annotations.viewport();
        let mut painted = false;
        for annotation in annotations.annotations() {
            if !viewport.contains_point(annotation.anchor) {
                continue;
            }
            let cols_left = viewport
                .right()
                .saturating_sub(u32::from(annotation.anchor.x))
                .min(u32::from(u16::MAX)) as u16;
            if cols_left == 0 {
                continue;
            }
            let (text, text_cells) = clip_to_width_cells(&annotation.label, cols_left);
            if text_cells == 0 {
                continue;
            }
            let origin_x = px_add(pad_px, px_offset_cells(0, annotation.anchor.x, cell_w));
            let origin_y = px_add(pad_px, px_offset_cells(0, annotation.anchor.y, cell_h));
            layers.combined_overlay.push(bitty_render::grid::FillRect {
                rect: bitty_render::geometry::RectPx::new(
                    origin_x,
                    origin_y,
                    px_span_usize(text_cells, cell_w),
                    px_span(1, cell_h),
                ),
                color: self.config.theme.background,
            });
            layers
                .combined_glyphs
                .extend(self.renderer.overlay_text_glyphs(
                    &text,
                    (origin_x, origin_y),
                    text_cells,
                    self.config.theme.foreground,
                ));
            painted = true;
        }
        if painted {
            layers.any_needs_draw = true;
        }
    }

    /// One text row per scene line for the overlay surface (CTX-1170).
    ///
    /// A `List`/`Column` root yields one row per direct child, each child
    /// flattened under the root's own declaration
    /// ([`flatten_band_runs_under`](super::band_host::flatten_band_runs_under)),
    /// so a root-level `fg`/`bg`/`bold` reaches its rows exactly like a band
    /// container's declaration reaches its spans and nested `Row` children
    /// still join horizontally (CTX-1419, issue #2191); any other root yields
    /// its single flattened line. Never empty: a childless scene falls back to
    /// one empty row so a live surface keeps its one-cell pill chrome
    /// (CTX-1143). Total text stays inside the mount-time `SCN-1`/`SCN-3`
    /// budgets the flattened walk already relied on.
    fn overlay_scene_rows(node: &bitty_lua::ui::UiNode) -> Vec<OverlayRow> {
        use super::band_host::{BandInherited, flatten_band_runs, flatten_band_runs_under};
        use bitty_lua::ui::UiNode;
        let mut rows: Vec<OverlayRow> = match node {
            UiNode::Column { children, .. } | UiNode::List { children, .. } => {
                let inherited = BandInherited::of(node);
                children
                    .iter()
                    .map(|child| {
                        let (text, runs) = flatten_band_runs_under(child, &inherited);
                        OverlayRow { text, runs }
                    })
                    .collect()
            }
            _ => {
                let (text, runs) = flatten_band_runs(node);
                vec![OverlayRow { text, runs }]
            }
        };
        if rows.is_empty() {
            rows.push(OverlayRow {
                text: String::new(),
                runs: Vec::new(),
            });
        }
        rows
    }

    /// Phase 5 (CTX-0474): the CTX-0248/0252/0254 kitty image layer.
    ///
    /// Per-pane blits for all visible allocations, budget-checked before
    /// rasterizing and clipped to each pane's content rectangle. Placements are
    /// skipped while a view inspects scrollback. Unfocused panes retain and
    /// display their own Kitty images without disappearing on focus switch.
    /// Each origin paints the live plane its grid shows (main or alt,
    /// swapped by [`Self::sync_kitty_screen_for`]); real images behind
    /// `U+10EEEE` placeholder runs paint through
    /// [`Self::paint_kitty_unicode_runs`] (#2442).
    ///
    /// #1849 S7: blits composite above cells, text, and overlay fills, but
    /// below the focused cursor — [`Self::paint_cursor_on_top`] runs after
    /// this phase and clears the cursor rect out of every covering blit, so
    /// the cursor wins regardless of image z. The 32-blit / 64-MiB budget
    /// counts kitty blits only; the cursor overlay is never counted.
    ///
    /// Raster is the Core-owned nearest-neighbor step behind the
    /// [`bitty_rich::kitty_seam`] traits (the extension implements the same
    /// traits on its side): the image scales into the unclamped placement
    /// extent and only the viewport-visible window is allocated, so a
    /// partially visible placement paints at true scale instead of
    /// squeezing into its clip. S6 (#1849) serves repeats from the bounded
    /// [`bitty_rich::KittyRasterCache`] (32 entries / 64 MiB, owned clones
    /// so the S7 punch stays safe); a miss — or a disabled cache — falls
    /// back to the uncached path with byte-identical bytes, so correctness
    /// never depends on it. Per-frame budget enforcement uses the
    /// Core-retained [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`] /
    /// [`bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME`] ceilings with
    /// skip-and-continue in paint order.
    /// Advances Kitty animation descriptors by the present-tick elapsed
    /// (S1, #1849).
    ///
    /// Tick source (CTX-1090 tick-source question, decided): present-tick
    /// elapsed time is the only clock — no dedicated timer. The first tick
    /// stamps without advancing; every later tick advances by the
    /// saturating elapsed since the stamp, so virtual-clock tests
    /// (`tick_at`) pace animations deterministically and a wall-clock jump
    /// never rewinds.
    ///
    /// Pause policy (CTX-1090 hidden-pane/scrolled-off question, decided):
    /// an origin advances only while it owns a visible non-alt leaf this
    /// frame. Hidden panes (no allocation), alternate-screen origins, and
    /// session-less leaves pause — alt playback remains a follow-up slice;
    /// the live alt image plane still paints static frames — so advancing
    /// would only burn raster work and desync the resume point. Scrolled-off
    /// placements keep time (they advance with their origin): only
    /// descriptor math runs for unpainted placements (no raster, no blits),
    /// so on scroll-back the animation is time-correct with its siblings
    /// instead of frozen mid-cycle. `Stopped` descriptors freeze and
    /// `Loading` ones park on the last frame (the state seam owns that).
    ///
    /// Each advancing state steps through
    /// [`bitty_term_state::State::advance_kitty_animations`] (IMG-9 33ms
    /// gap floor, at most 4 timed transitions, leftover dropped), which is
    /// pure descriptor math returning the changed wire ids. The tick then
    /// batches each changed image's terminal-truth spans plus its rich
    /// spans ([`Self::kitty_wire_spans_for`]) and finalizes once per
    /// origin — a single damage path per tick with exactly one generation
    /// bump when anything visible changed. A frozen animation costs the
    /// descriptor scan and never wakes the frame-on-demand present.
    /// Empty and out-of-bounds spans damage nothing and bump nothing, and
    /// untouched images gain no damage.
    ///
    /// Single-bump note (CTX-1122): the pre-fix tick bumped twice per
    /// image (truth once in `State::advance`, again in
    /// `damage_kitty_wire_for_origin`). Collapsing to one bump drops only
    /// duplicate history batches: the S6 raster cache key binds the rich
    /// image content generation (not this grid generation) and animation
    /// ticks never scroll, so neither cache keys nor scroll regions
    /// depend on the double bump.
    fn advance_kitty_animations_at(
        &mut self,
        now: std::time::Instant,
        allocations: &[layout_focus::PresentFrame],
        alt_screens: &std::collections::BTreeSet<Option<u64>>,
    ) {
        let elapsed = match self.kitty_anim_last_tick {
            Some(last) => now.saturating_duration_since(last),
            None => std::time::Duration::ZERO,
        };
        self.kitty_anim_last_tick = Some(now);
        if elapsed.is_zero() {
            return;
        }
        let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let mut visible: std::collections::BTreeSet<Option<u64>> =
            std::collections::BTreeSet::new();
        for frame in allocations {
            if frame.cols == 0 || frame.rows == 0 {
                continue;
            }
            if self.pane_sessions.contains_key(&frame.view) {
                visible.insert(Some(frame.view.0));
            } else if Some(frame.view) == self.primary_view {
                visible.insert(None);
            }
        }
        let steps = bitty_term_state::KITTY_ANIM_MAX_STEPS_PER_TICK;
        if visible.contains(&None) && !alt_screens.contains(&None) {
            let changed = self.state.advance_kitty_animations(elapsed_ms, steps);
            if !changed.is_empty() {
                let scrollback_now = self.state.scrollback_len();
                let grid_width = self.state.width();
                let grid_height = self.state.height();
                let mut pushed = false;
                for id in changed {
                    pushed |= self.state.damage_kitty_image_batched(id);
                    let spans = self.kitty_wire_spans_for(
                        None,
                        id,
                        scrollback_now,
                        grid_width,
                        grid_height,
                    );
                    pushed |= self.state.damage_kitty_spans_batched(&spans);
                }
                if pushed {
                    self.state.finalize_pending_damage();
                }
            }
        }
        for origin in visible {
            let Some(token) = origin else { continue };
            if alt_screens.contains(&Some(token)) {
                continue;
            }
            let changed = match self.pane_sessions.get_mut(&ViewId::new(token)) {
                Some(sess) => sess.state.advance_kitty_animations(elapsed_ms, steps),
                None => Vec::new(),
            };
            if changed.is_empty() {
                continue;
            }
            let (scrollback_now, grid_width, grid_height) =
                match self.pane_sessions.get(&ViewId::new(token)) {
                    Some(sess) => (
                        sess.state.scrollback_len(),
                        sess.state.width(),
                        sess.state.height(),
                    ),
                    None => continue,
                };
            let mut pushed = false;
            for id in changed {
                if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                    pushed |= sess.state.damage_kitty_image_batched(id);
                }
                let spans = self.kitty_wire_spans_for(
                    Some(token),
                    id,
                    scrollback_now,
                    grid_width,
                    grid_height,
                );
                if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                    pushed |= sess.state.damage_kitty_spans_batched(&spans);
                }
            }
            if pushed {
                if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                    sess.state.finalize_pending_damage();
                }
            }
        }
    }

    fn paint_kitty_images(&mut self, basis: &TickBasis, layers: &mut FrameLayers) {
        let snapshot = &basis.snapshot;
        // CTX-1335: image blits follow the interpolated leaf bounds.
        let allocations = basis.painted_allocations();
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

        // CTX-1181 (#2041): float frames in window space, in paint order.
        // Kitty blits composite topmost, above the CTX-1148 opaque fill, so
        // a lower leaf image sharing area with a higher float frame is split
        // around it below. Same table shape as the CTX-1165 glyph path in
        // `build_leaf_primitives`; empty frames never occlude.
        let float_occluders: Vec<(usize, bitty_render::geometry::RectPx)> = allocations
            .iter()
            .enumerate()
            .filter(|(_, frame)| {
                frame.tier == Some(OverlayTier::Float)
                    && frame.frame.width > 0
                    && frame.frame.height > 0
            })
            .map(|(index, frame)| {
                (
                    index,
                    bitty_render::geometry::RectPx::new(
                        px_add(basis.pad_px, frame.frame.x),
                        px_add(basis.pad_px, frame.frame.y),
                        frame.frame.width,
                        frame.frame.height,
                    ),
                )
            })
            .collect();

        for (leaf_index, frame) in allocations.iter().enumerate() {
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

            let (_alt_active, cursor_row, rows, scrollback_len) = match pane_origin {
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

            let scroll_offset = view_map
                .get(&frame.view)
                .map(|v| v.scroll_offset())
                .unwrap_or(0);

            if self.kitty_images.placement_for_origin_is_empty(pane_origin)
                && self.kitty_images.virtual_count_for_origin(pane_origin) == 0
            {
                continue;
            }

            let scrollback = (scrollback_len.saturating_sub(scroll_offset))
                + cursor_follow_window_start(cursor_row, rows, usize::from(frame.rows));

            // CTX-1181 (#2041): floats painted after this leaf own the
            // covered part of its images. Later allocations paint above, so
            // only floats after this leaf matter; empty for the topmost
            // leaf, keeping the common path allocation-free beyond the
            // small table filter.
            let higher_floats: Vec<bitty_render::geometry::RectPx> = float_occluders
                .iter()
                .filter(|(index, _)| *index > leaf_index)
                .map(|(_, rect)| *rect)
                .collect();

            // Real images on top of `U+10EEEE` placeholder runs (kitty
            // `U=1`): virtual prototypes are never blit placements
            // themselves, so an origin holding only prototypes must still
            // reach this paint step (yazi's preview path is exactly that
            // shape).
            self.paint_kitty_unicode_runs(
                pane_origin,
                frame,
                rich_metrics,
                basis.pad_px,
                scroll_offset,
                &higher_floats,
                &mut blits_this_frame,
                &mut bytes_this_frame,
                layers,
            );

            for placement in self.kitty_images.placements_in_paint_order_for(pane_origin) {
                // Dangling placements (image evicted under the store caps)
                // fail closed here, as before.
                let Some(stored) = self.kitty_images.get(placement.image) else {
                    continue;
                };
                // S1 (#1849): the current frame comes from the origin's
                // terminal-truth descriptor (1-based, frame 1 when
                // static); a truth frame with no stored pixels (past the
                // pixel caps) paints nothing, fail closed.
                let frame_no = self.kitty_current_frame_for(pane_origin, placement.wire_image);
                let Some((frame_width, frame_height, frame_bytes)) =
                    self.kitty_images.frame_bytes(placement.image, frame_no)
                else {
                    continue;
                };
                // Full (unclamped) cell rectangle for the placement, then
                // the image fitted into it with its aspect ratio preserved
                // and centered (kitty letterboxes a client-requested
                // `c`/`r` rect instead of stretching it:
                // docs/graphics-protocol.rst "Controlling displayed image
                // layout"). The viewport-clipped window is the visible
                // crop; `None` paints nothing (scrolled fully off the top
                // or outside the viewport).
                let Some(cell_rect) = bitty_rich::KittyImageLayer::placement_full_rect(
                    placement,
                    rich_metrics,
                    scrollback,
                ) else {
                    continue;
                };
                let full_px = bitty_rich::fit_aspect_rect(cell_rect, frame_width, frame_height);
                let viewport =
                    rich_metrics.extent_for(usize::from(frame.cols), usize::from(frame.rows));
                let Some(rect_px) = full_px.intersection(&bitty_rich::RectPx::new(
                    0,
                    0,
                    viewport.width,
                    viewport.height,
                )) else {
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
                if bytes_this_frame.saturating_add(need)
                    > bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME
                {
                    continue;
                }
                // S6 (#1849): serve repeats from the bounded raster cache;
                // a miss (or a disabled cache) falls back to the uncached
                // path with byte-identical bytes, so correctness never
                // depends on the cache. The key binds (id, generation) —
                // never a bare id — plus the S1 animation frame and the
                // full/visible geometry.
                let cache_key =
                    bitty_rich::KittyRasterKey::for_image(stored, frame_no, full_px, rect_px);
                let rgba = if let Some(hit) = self.kitty_raster_cache.get(&cache_key) {
                    hit
                } else {
                    let Some(fresh) = self.kitty_graphics.rasterize_frame(
                        frame_width,
                        frame_height,
                        frame_bytes,
                        full_px,
                        rect_px,
                    ) else {
                        continue;
                    };
                    // Admit an owned clone; an oversize blit is simply not
                    // admitted yet still paints uncached this frame.
                    let _ = self.kitty_raster_cache.insert(cache_key, fresh.clone());
                    fresh
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
                // CTX-1181 (#2041): the upper float owns the covered part of
                // a lower leaf image. Split the blit around the higher float
                // frames so the visible part outside stays pixel-identical;
                // the float's own images keep an empty occluder table and
                // paint verbatim. Pieces share the admitted byte budget
                // above, skip-and-continue past the caps.
                let visible = if higher_floats.is_empty() {
                    vec![blit]
                } else {
                    clip_blit_around_floats(blit, &higher_floats)
                };
                for piece in visible {
                    if blits_this_frame >= bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME {
                        continue;
                    }
                    let piece_need = piece.rgba.len();
                    if bytes_this_frame.saturating_add(piece_need)
                        > bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME
                    {
                        continue;
                    }
                    blits_this_frame = blits_this_frame.saturating_add(1);
                    bytes_this_frame = bytes_this_frame.saturating_add(piece_need);
                    layers.combined_images.push(piece);
                    layers.any_needs_draw = true;
                }
            }
        }
        if !layers.combined_images.is_empty() {
            layers.any_needs_draw = true;
        }
    }

    /// Paints the real image behind every `U+10EEEE` run of one leaf
    /// (kitty Unicode placeholders, `U=1`).
    ///
    /// A virtual placement is only a prototype (it has no screen position
    /// and never paints by itself); the visible image is the text run of
    /// placeholder cells, resolved here into cell tiles. The run's
    /// `(row, col)` diacritics name the tile inside the prototype's
    /// `c × r` span; the span is fitted with its aspect ratio preserved
    /// (letterboxed) exactly like a direct placement, and the run's cells
    /// become the destination rect of a crop of that fitted image.
    ///
    /// A prototype is either linked to a stored image (combined
    /// `a=T,U=1`) or bodiless (`a=p,U=1`, `image: None`): the bodiless
    /// form resolves its image through the origin-scoped wire map the
    /// animation seams use
    /// ([`bitty_rich::KittyImageLayer::find_root_for_origin`]), so the
    /// canonical `a=t` then `a=p,U=1` two-step form paints.
    ///
    /// Runs resolve in grid space and are translated to viewport space by
    /// the same cursor-follow window as [`viewport_snapshot`]. Scrolled
    /// views paint nothing here: placeholder runs decode from the active
    /// grid only, so a scrollback composition has no run cells to anchor
    /// them. The alternate screen is no exception: the origin paints the
    /// live plane its grid shows (main or alt, swapped by
    /// [`Self::sync_kitty_screen_for`]), and the alt plane's own
    /// prototypes serve its runs.
    ///
    /// Every run of one image is gathered first and rasterized once: the
    /// visible rects are unioned (clipped to the viewport) and the image
    /// is emitted as one blit, masked back to the exact run coverage when
    /// the runs leave gaps. A per-row blit truncated spans taller than
    /// [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`] and starved
    /// direct placements behind them.
    ///
    /// Same bounded budget as direct placements (32 blits / 64 MiB per
    /// frame): past the caps the remaining images are skipped fail-closed
    /// in paint order.
    #[allow(clippy::too_many_arguments)]
    fn paint_kitty_unicode_runs(
        &mut self,
        origin: Option<u64>,
        frame: &layout_focus::PresentFrame,
        metrics: bitty_rich::CellMetrics,
        pad_px: i32,
        scroll_offset: usize,
        higher_floats: &[bitty_render::geometry::RectPx],
        blits_this_frame: &mut usize,
        bytes_this_frame: &mut usize,
        layers: &mut FrameLayers,
    ) {
        if self.kitty_images.virtual_count_for_origin(origin) == 0
            || scroll_offset != 0
            || frame.rows == 0
            || frame.cols == 0
        {
            return;
        }
        let (grid_rows, cursor_row) = match origin {
            Some(token) => match self.pane_sessions.get(&ViewId::new(token)) {
                Some(sess) => (
                    sess.state.height(),
                    usize::from(sess.state.cursor().position.row),
                ),
                None => return,
            },
            None => (
                self.state.height(),
                usize::from(self.state.cursor().position.row),
            ),
        };
        let window_start =
            cursor_follow_window_start(cursor_row, grid_rows, usize::from(frame.rows));
        // Pane content viewport in content space (the blit destination
        // later adds the leaf origin + window padding).
        let viewport = bitty_rich::geometry::RectPx::new(
            0,
            0,
            metrics.width.saturating_mul(u32::from(frame.cols)),
            metrics.height.saturating_mul(u32::from(frame.rows)),
        );
        // Gather every visible run per image before rasterizing: one image
        // must cost one blit no matter how many rows its span covers, or a
        // tall preview truncates at the per-frame cap.
        let mut groups: Vec<KittyUnicodeRunGroup> = Vec::new();
        for local_row in 0..usize::from(frame.rows) {
            let grid_row = window_start.saturating_add(local_row);
            let runs = match origin {
                Some(token) => match self.pane_sessions.get(&ViewId::new(token)) {
                    Some(sess) => sess.state.kitty_unicode_runs_on_row(grid_row),
                    None => return,
                },
                None => self.state.kitty_unicode_runs_on_row(grid_row),
            };
            for (cells, key) in runs {
                let Some(first) = cells.first().copied() else {
                    continue;
                };
                let Some(proto) = self.kitty_images.get_virtual(origin, key.0, key.1) else {
                    continue;
                };
                // Bodiless `a=p,U=1` carries no handle: its pixels arrive
                // in an earlier transmit, so resolve the prototype's image
                // through the origin-scoped wire map (canonical two-step
                // form). The combined form keeps its linked handle.
                let image = proto.image.or_else(|| {
                    self.kitty_images
                        .find_root_for_origin(origin, proto.wire_image)
                });
                let Some(image) = image else {
                    continue;
                };
                let run = bitty_term_state::KittyUnicodeRun {
                    image_id: key.0,
                    placement_id: key.1,
                    row: first.id.row,
                    col: first.id.col,
                    grid_row: first.grid_row,
                    grid_col: first.grid_col,
                    width: cells.len(),
                };
                // Live-grid rows are already scrolled truth, so the
                // scrollback delta is the viewport window start only.
                let Some(tile) = bitty_rich::unicode_run_rect(
                    &run,
                    &cells,
                    &proto.as_unicode_virtual(),
                    metrics,
                    window_start,
                    0,
                ) else {
                    continue;
                };
                let Some(visible) = tile.rect.intersection(&tile.full) else {
                    continue;
                };
                let Some(visible) = visible.intersection(&viewport) else {
                    continue;
                };
                // Group by (wire identity, fitted span): a well-formed
                // client's rows all name one span, so its whole image is
                // one group; inconsistent run geometry keeps its own
                // group instead of painting under the wrong transform.
                match groups.iter_mut().find(|group| {
                    group.wire_image == proto.wire_image
                        && group.wire_placement == proto.wire_placement
                        && group.full == tile.full
                }) {
                    Some(group) => group.rects.push(visible),
                    None => groups.push(KittyUnicodeRunGroup {
                        wire_image: proto.wire_image,
                        wire_placement: proto.wire_placement,
                        image,
                        full: tile.full,
                        rects: vec![visible],
                    }),
                }
            }
        }
        for group in groups {
            let Some(stored) = self.kitty_images.get(group.image) else {
                continue;
            };
            let Some(union) = unicode_rect_union(&group.rects) else {
                continue;
            };
            let frame_no = self.kitty_current_frame_for(origin, group.wire_image);
            let Some((frame_width, frame_height, frame_bytes)) =
                self.kitty_images.frame_bytes(group.image, frame_no)
            else {
                continue;
            };
            let need = (u64::from(union.width) * u64::from(union.height))
                .checked_mul(4)
                .filter(|&n| n <= usize::MAX as u64)
                .map(|n| n as usize);
            let Some(need) = need else {
                continue;
            };
            if *blits_this_frame >= bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME {
                continue;
            }
            if bytes_this_frame.saturating_add(need) > bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME
            {
                continue;
            }
            // Full-bleed rows (one run per image row) cover the union
            // exactly, so the union raster is byte-identical to the image
            // and stays cacheable. Sparse runs are masked to their exact
            // coverage and stay out of the cache: the key names geometry,
            // not the mask.
            let dense = unicode_rects_cover_union(union, &group.rects);
            let rgba = if dense {
                let cache_key =
                    bitty_rich::KittyRasterKey::for_image(stored, frame_no, group.full, union);
                if let Some(hit) = self.kitty_raster_cache.get(&cache_key) {
                    hit
                } else {
                    let Some(fresh) = self.kitty_graphics.rasterize_frame(
                        frame_width,
                        frame_height,
                        frame_bytes,
                        group.full,
                        union,
                    ) else {
                        continue;
                    };
                    let _ = self.kitty_raster_cache.insert(cache_key, fresh.clone());
                    fresh
                }
            } else {
                let Some(mut fresh) = self.kitty_graphics.rasterize_frame(
                    frame_width,
                    frame_height,
                    frame_bytes,
                    group.full,
                    union,
                ) else {
                    continue;
                };
                mask_uncovered_unicode_pixels(&mut fresh, union, &group.rects);
                fresh
            };
            let dest = bitty_render::geometry::RectPx::new(
                px_add(px_add(pad_px, frame.content.x), union.x),
                px_add(px_add(pad_px, frame.content.y), union.y),
                union.width,
                union.height,
            );
            let Ok(blit) = bitty_render::grid::ImageBlit::try_new(dest, rgba) else {
                continue;
            };
            let pieces = if higher_floats.is_empty() {
                vec![blit]
            } else {
                clip_blit_around_floats(blit, higher_floats)
            };
            for piece in pieces {
                if *blits_this_frame >= bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME {
                    continue;
                }
                let piece_need = piece.rgba.len();
                if bytes_this_frame.saturating_add(piece_need)
                    > bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME
                {
                    continue;
                }
                *blits_this_frame = blits_this_frame.saturating_add(1);
                *bytes_this_frame = bytes_this_frame.saturating_add(piece_need);
                layers.combined_images.push(piece);
                layers.any_needs_draw = true;
            }
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
            band_identity,
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
            // CTX-1332 C-D4: a no-draw/bookkeeping path publishes no
            // executable origin and does not advance the presented-band
            // identity (the next tick keeps trying to commit a real frame).
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
            // CTX-1332 C-D4: a no-draw/bookkeeping path publishes no
            // executable origin and does not advance the presented-band
            // identity (the next tick keeps trying to commit a real frame).
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
        // CTX-1332 C-D4: eligible publication boundary. Only a frame that
        // actually presented commits its origins as executable.
        self.last_presented_bands = band_identity;
        self.published_band_origins = self.pending_band_origins.clone();
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
    /// Decorated leaf allocations for this frame (logical, final geometry).
    allocations: Vec<layout_focus::PresentFrame>,
    /// Paint-only allocations with active bounds interpolation applied
    /// (CTX-1335). `None` whenever no geometry transition is active, in which
    /// case painting reuses `allocations`; never consumed by hit-testing or
    /// terminal-truth paths.
    paint_allocations: Option<Vec<layout_focus::PresentFrame>>,
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
    /// Mounted band identity for this frame (CTX-0946 C2, CTX-1332 C-D4/F7).
    band_identity: Vec<crate::runtime::band_host::BandFrameIdentity>,
}

impl TickBasis {
    /// Leaf allocations used for painting: interpolated when a geometry
    /// transition is active, otherwise the committed logical frames.
    fn painted_allocations(&self) -> &[layout_focus::PresentFrame] {
        self.paint_allocations
            .as_deref()
            .unwrap_or(&self.allocations)
    }
}

/// One overlay scene row: the flattened text of one line plus the runs it was
/// flattened from (CTX-1170; style/binding parity CTX-1419).
///
/// The overlay surface reuses the band flattening walk so a row's declared
/// `fg`/`bg`/`bold` and command bindings are the same effective values a band
/// span would carry. Bindings are retained on the row even though the overlay
/// paint path does not route them (the overlay hit-test contract is not
/// accepted yet); keeping one flattening source prevents the two hosted
/// surfaces from disagreeing about inheritance.
struct OverlayRow {
    /// Flattened row text in declaration order.
    text: String,
    /// One run per `Text` leaf, in the same order as `text`.
    runs: Vec<super::band_host::BandRun>,
}

/// Row stack placed against a live band item (issue #2411).
///
/// `limit_row` is the exclusive window row the stack must not reach: a bottom
/// band sets it to its own row, so a tooltip taller than the space above the
/// band clips at the band instead of painting over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchoredOverlayStack {
    /// Window row of the first tooltip row.
    start_row: u16,
    /// Window column the rows start at.
    x: u16,
    /// Exclusive window row the stack must not reach.
    limit_row: Option<u16>,
}

/// One overlay row placed against a live band item (issue #2411).
///
/// Window-cell coordinates, not frame-relative: an anchored tooltip belongs to
/// the band's window row and is centered on the hovered region (issue #2530).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchoredOverlayRow {
    /// Window column the row starts at.
    x: u16,
    /// Display cells available from `x` to the window edge.
    cols: u16,
    /// Window row the row paints.
    row: u16,
    /// Window column origin the cells are measured from.
    window_x: u16,
}

/// Clipped geometry of one overlay pill row (CTX-1170).
///
/// Shared by the unstyled fast path and the styled-row fallback so both paint
/// the exact same one-cell chrome (CTX-1419, issue #2191).
#[derive(Debug, Clone, Copy)]
struct OverlayPill {
    /// Left edge in window pixels.
    origin_x: i32,
    /// Top edge in window pixels.
    origin_y: i32,
    /// Pill width in pixels (never narrower than one cell; a row that clips to
    /// zero text cells still reserves its one-cell chrome).
    width_px: u32,
    /// Pill height in pixels (one cell).
    height_px: u32,
    /// Width of the clipped row text in display cells.
    text_cells: usize,
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
mod plugin_overlay_paint_tests {
    use super::{FrameLayers, centered_tooltip_x};
    use crate::runtime::layout_focus::PresentFrame;
    use crate::runtime::{BandContent, Runtime, RuntimeConfig};
    use crate::{BandEdge, BandOverlayAnchor, ChromeBands};
    use bitty_platform::PhysicalSize;

    fn runtime() -> Runtime {
        Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
            .expect("deterministic runtime must build")
    }

    fn surface_with_text(text: &str) -> BandContent {
        BandContent {
            plugin_id: String::from("bitty-terminal.composer"),
            slot: bitty_lua::ui::UiSlot::Overlay,
            root: bitty_lua::ui::UiNode::text(text),
            version: 1,
        }
    }

    fn surface_with_rows(rows: &[&str]) -> BandContent {
        BandContent {
            plugin_id: String::from("bitty-terminal.composer"),
            slot: bitty_lua::ui::UiSlot::Overlay,
            root: bitty_lua::ui::UiNode::list(
                rows.iter()
                    .map(|row| bitty_lua::ui::UiNode::text(*row))
                    .collect(),
            ),
            version: 1,
        }
    }

    fn one_frame() -> PresentFrame {
        use bitty_render::geometry::RectPx;
        PresentFrame {
            view: bitty_ui::ViewId::new(1),
            frame: RectPx::new(0, 0, 720, 360),
            content: RectPx::new(8, 8, 704, 344),
            cols: 80,
            rows: 24,
            border: 1,
            radius: 0,
            tier: None,
            style: crate::config::RuntimeViewStyle::default(),
        }
    }

    #[test]
    fn live_empty_overlay_surface_still_paints_pill_chrome() {
        // CTX-1143 (issue #1966): a live overlay surface with empty text (a
        // fresh composer buffer, an empty palette list) must still paint its
        // pill chrome. The modal holds keyboard capture while up, so
        // pixel-invisibility is a fail-open UX: the user types into a box
        // that is not there. Glyphs stay empty; the fill plus the draw flag
        // prove the modal opened.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with_text("")));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            1,
            "a live surface with empty text must still paint one pill fill"
        );
        assert!(
            layers.combined_glyphs.is_empty(),
            "empty text paints no glyphs"
        );
        assert!(
            layers.any_needs_draw,
            "the pill chrome must mark the frame for draw"
        );
    }

    #[test]
    fn live_nonempty_overlay_surface_paints_text_pill() {
        // Non-empty content keeps its exact behavior: one fill plus glyphs
        // for the clipped line.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with_text("hello")));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(layers.combined_overlay.len(), 1, "one pill fill");
        assert!(
            !layers.combined_glyphs.is_empty(),
            "non-empty text paints glyphs"
        );
        assert!(layers.any_needs_draw);
    }

    #[test]
    fn hover_anchored_surface_paints_below_the_top_band_region() {
        // Issue #2411 / #2530: a tooltip overlay anchored to a live top-band
        // region paints one row below the band, horizontally centered on the
        // region, in window cells (never at the region's left edge, never
        // centered on the focused frame).
        let mut rt = runtime();
        let window = rt.window_cells();
        rt.set_chrome_bands(ChromeBands {
            top: vec![surface_with_rows(&["  bar content"])],
            ..Default::default()
        });
        rt.tick()
            .expect("tick presents the band and insets the container");
        assert_eq!(rt.visible_band_row(BandEdge::Top, 0), Some(0));
        assert!(!rt.band_geometry_violated());
        rt.set_plugin_overlay(Some(surface_with_text("detail")));
        rt.set_plugin_overlay_anchor(Some(BandOverlayAnchor {
            edge: BandEdge::Top,
            visible_index: 0,
            start_col: 10,
            end_col: 20,
        }));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        let (cell_w, cell_h) = rt.live_cell_size();
        let fill = layers.combined_overlay.first().expect("tooltip fill");
        // Region center is column 15; a six-cell stack starts at 15 - 3 = 12.
        let expected_x =
            8 + i32::try_from(u32::from(window.x) + 12).unwrap() * i32::try_from(cell_w).unwrap();
        let expected_y = 8 + i32::try_from(cell_h).unwrap();
        assert_eq!(fill.rect.x, expected_x, "centered on the region center");
        assert_eq!(fill.rect.y, expected_y, "one row below the top band");
        assert_eq!(fill.rect.width, cell_w * 6, "six text cells wide");

        // Clearing the anchor restores the centered modal placement.
        rt.set_plugin_overlay_anchor(None);
        let mut centered = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut centered);
        let fill = centered.combined_overlay.first().expect("centered fill");
        assert_ne!(fill.rect.x, expected_x, "no stale anchor placement");
        assert_ne!(fill.rect.y, expected_y);
    }

    #[test]
    fn centered_tooltip_clamps_inside_the_window() {
        // Issue #2530: centering must never push the tooltip past either
        // window edge; an over-wide stack starts at column 0 and clips.
        assert_eq!(centered_tooltip_x(10, 20, 6, 80), 12, "plain centering");
        assert_eq!(
            centered_tooltip_x(0, 2, 10, 80),
            0,
            "left edge saturates at the window start"
        );
        assert_eq!(
            centered_tooltip_x(70, 80, 20, 80),
            60,
            "right edge clamps so the stack fits"
        );
        assert_eq!(
            centered_tooltip_x(20, 30, 8, 80),
            21,
            "the widest row centers the stack"
        );
        assert_eq!(
            centered_tooltip_x(20, 30, 100, 80),
            0,
            "an over-wide stack starts at column 0"
        );
        assert_eq!(
            centered_tooltip_x(79, 80, 0, 80),
            79,
            "a zero-width stack keeps the region start"
        );
    }

    #[test]
    fn right_edge_tooltip_paints_fully_inside_the_window() {
        // Issue #2530: a right-group module's tooltip is centered on the
        // module and clamped, so it paints the whole text instead of only the
        // few columns between the module and the window edge.
        let mut rt = runtime();
        let window = rt.window_cells();
        let text = "a long tooltip line";
        rt.set_chrome_bands(ChromeBands {
            top: vec![surface_with_rows(&["bar"])],
            ..Default::default()
        });
        rt.tick().expect("tick presents the band");
        rt.set_plugin_overlay(Some(surface_with_text(text)));
        let width = window.width;
        rt.set_plugin_overlay_anchor(Some(BandOverlayAnchor {
            edge: BandEdge::Top,
            visible_index: 0,
            start_col: usize::from(width.saturating_sub(10)),
            end_col: usize::from(width),
        }));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        let (cell_w, _) = rt.live_cell_size();
        let fill = layers.combined_overlay.first().expect("tooltip fill");
        let text_cells = u32::try_from(text.chars().count()).expect("text cells");
        assert_eq!(
            fill.rect.width,
            cell_w * text_cells,
            "the whole tooltip text paints"
        );
        let cell_w = i32::try_from(cell_w).expect("cell width");
        let window_start = 8 + i32::from(window.x) * cell_w;
        let window_end = window_start + i32::from(window.width) * cell_w;
        assert!(fill.rect.x >= window_start, "starts inside the window");
        assert!(
            fill.rect.x + i32::try_from(fill.rect.width).expect("width") <= window_end,
            "ends inside the window"
        );
    }

    #[test]
    fn bottom_band_anchor_paints_above_the_region() {
        // Review N4 / #2530: the bottom-edge branch centers the tooltip on the
        // region and stacks its rows above the band (last row at band_row - 1).
        let mut rt = runtime();
        let window = rt.window_cells();
        rt.set_chrome_bands(ChromeBands {
            bottom: vec![surface_with_rows(&["bar"])],
            ..Default::default()
        });
        rt.tick().expect("tick presents the band");
        let band_row = rt
            .visible_band_row(BandEdge::Bottom, 0)
            .expect("bottom band row");
        rt.set_plugin_overlay(Some(surface_with_rows(&["detail", "more"])));
        rt.set_plugin_overlay_anchor(Some(BandOverlayAnchor {
            edge: BandEdge::Bottom,
            visible_index: 0,
            start_col: 4,
            end_col: 10,
        }));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        let (cell_w, cell_h) = rt.live_cell_size();
        assert_eq!(layers.combined_overlay.len(), 2, "two tooltip rows");
        for (index, fill) in layers.combined_overlay.iter().enumerate() {
            let expected_row = band_row - 2 + u16::try_from(index).unwrap();
            let expected_y = 8 + i32::from(expected_row) * i32::try_from(cell_h).unwrap();
            // Region center 7 and the six-cell widest row start at 4.
            let expected_x = 8 + i32::try_from(u32::from(window.x) + 4).unwrap()
                * i32::try_from(cell_w).unwrap();
            assert_eq!(fill.rect.x, expected_x, "centered on the region center");
            assert_eq!(fill.rect.y, expected_y, "stacked above the bottom band");
        }
    }

    #[test]
    fn tall_bottom_anchor_clips_at_the_band_row() {
        // CodeRabbit (PR #2423): a tooltip taller than the space above a
        // bottom band must clip at the band row instead of painting over its
        // own band.
        let mut rt = runtime();
        let (cell_w, cell_h) = rt.live_cell_size();
        let pad = rt.window_padding_physical();
        rt.handle_resize(PhysicalSize::new(
            cell_w
                .saturating_mul(80)
                .saturating_add(pad.saturating_mul(2)),
            cell_h
                .saturating_mul(6)
                .saturating_add(pad.saturating_mul(2)),
        ))
        .expect("resize to six rows");
        rt.set_chrome_bands(ChromeBands {
            bottom: vec![surface_with_rows(&["bar"])],
            ..Default::default()
        });
        rt.tick().expect("present the bottom band");
        let band_row = rt
            .visible_band_row(BandEdge::Bottom, 0)
            .expect("bottom band row");
        let rows = vec!["detail"; 10];
        rt.set_plugin_overlay(Some(surface_with_rows(&rows)));
        rt.set_plugin_overlay_anchor(Some(BandOverlayAnchor {
            edge: BandEdge::Bottom,
            visible_index: 0,
            start_col: 0,
            end_col: 6,
        }));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            usize::from(band_row),
            "only the rows above the band paint"
        );
        for fill in &layers.combined_overlay {
            let row = u16::try_from((fill.rect.y - 8) / i32::try_from(cell_h).unwrap())
                .expect("row offset");
            assert!(
                row < band_row,
                "row {row} must stay above the band row {band_row}"
            );
        }
    }

    #[test]
    fn missing_surface_paints_nothing() {
        // No surface stays a no-op: capture-less frames are byte-identical.
        let mut rt = runtime();
        rt.set_plugin_overlay(None);
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert!(layers.combined_overlay.is_empty());
        assert!(layers.combined_glyphs.is_empty());
        assert!(!layers.any_needs_draw);
    }

    #[test]
    fn titled_list_scene_paints_stacked_title_and_input_rows() {
        // CTX-1170 (issue #1972) scene chrome: the composer open scene
        // (`List` of title plus input line) paints one pill row per child
        // instead of one flattened pill, so the modal reads as a titled
        // command line. The rows stack vertically in declared order; each
        // keeps its own centered pill.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with_rows(&["Composer", "hi"])));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            2,
            "title plus input paint two pill fills"
        );
        assert!(!layers.combined_glyphs.is_empty(), "both rows paint glyphs");
        let first = &layers.combined_overlay[0].rect;
        let second = &layers.combined_overlay[1].rect;
        assert_eq!(
            second.y - first.y,
            first.height as i32,
            "rows stack exactly one cell tall"
        );
        assert_eq!(first.height, second.height, "uniform pill height");
        assert!(
            first.y < second.y,
            "declared order is kept: title above input"
        );
        assert!(layers.any_needs_draw);
    }

    #[test]
    fn title_row_geometry_survives_an_input_repaint() {
        // Typing repaints the input row while the title row stays pixel
        // put: the chrome is stable across buffer arrivals.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with_rows(&["Composer", ""])));
        let mut before = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut before);
        rt.set_plugin_overlay(Some(surface_with_rows(&["Composer", "hi"])));
        let mut after = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut after);
        assert_eq!(before.combined_overlay.len(), 2);
        assert_eq!(after.combined_overlay.len(), 2);
        assert_eq!(
            before.combined_overlay[0].rect, after.combined_overlay[0].rect,
            "the title pill must not move when the input line changes"
        );
        assert_ne!(
            before.combined_overlay[1].rect, after.combined_overlay[1].rect,
            "the input pill re-centers as its text grows"
        );
    }

    #[test]
    fn empty_list_scene_keeps_one_pill_chrome() {
        // A childless scene still paints its one-cell pill: the modal holds
        // capture while up, so pixel-invisibility would be fail-open
        // (CTX-1143 guard applied to the row path).
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with_rows(&[])));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            1,
            "an empty scene keeps one pill fill"
        );
        assert!(layers.combined_glyphs.is_empty());
        assert!(layers.any_needs_draw);
    }

    #[test]
    fn rows_past_the_frame_bottom_clip_away() {
        // A tall scene stays bounded: rows past the frame bottom never
        // paint, so a hostile plugin cannot turn one modal into unbounded
        // per-frame glyph work.
        let mut rt = runtime();
        let many: Vec<String> = (0..40).map(|index| format!("row-{index}")).collect();
        let borrowed: Vec<&str> = many.iter().map(String::as_str).collect();
        rt.set_plugin_overlay(Some(surface_with_rows(&borrowed)));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            24,
            "only the 24 visible frame rows paint"
        );
        assert!(layers.any_needs_draw);
    }

    /// One `Text` leaf with declared style (CTX-1419 parity probes).
    fn styled_text(
        text: &str,
        fg: Option<&str>,
        bg: Option<&str>,
        bold: bool,
    ) -> bitty_lua::ui::UiNode {
        bitty_lua::ui::UiNode::Text {
            text: text.to_string(),
            fg: fg.map(str::to_string),
            bg: bg.map(str::to_string),
            bold: bold.then_some(true),
            on_click: None,
            on_scroll: None,
            anchor: None,
            on_hover: None,
        }
    }

    /// Overlay surface wrapping one styled node (CTX-1419 parity probes).
    fn surface_with(root: bitty_lua::ui::UiNode) -> BandContent {
        BandContent {
            plugin_id: String::from("bitty-terminal.composer"),
            slot: bitty_lua::ui::UiSlot::Overlay,
            root,
            version: 1,
        }
    }

    #[test]
    fn styled_overlay_text_paints_declared_tokens() {
        // CTX-1419 (issue #2191): `fg`/`bg` are accepted on every `UiNode`, so
        // the overlay host must paint them instead of silently dropping them.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(styled_text(
            "hello",
            Some("accent"),
            Some("selection"),
            false,
        ))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(layers.combined_overlay.len(), 1, "one span paints one fill");
        let accent = rt
            .config
            .theme
            .active_index(crate::runtime::band_host::BAND_ACCENT_ANSI_INDEX);
        assert_eq!(
            layers.combined_overlay[0].color, rt.config.theme.selection,
            "bg resolves through the same closed token table as a band"
        );
        assert_eq!(
            layers.combined_glyphs[0].color,
            [accent[0], accent[1], accent[2], 0xFF],
            "fg resolves through the same closed token table as a band"
        );
        assert_eq!(rt.band_stats.unknown_tokens, 0);
    }

    #[test]
    fn bold_overlay_text_double_strikes_glyphs() {
        // `bold` follows the band renderer's synthetic double-strike; the
        // unstyled control proves the extra instances come from `bold`, not
        // from splitting the row into runs.
        let plain_glyphs = {
            let mut rt = runtime();
            rt.set_plugin_overlay(Some(surface_with(styled_text("hello", None, None, false))));
            let mut layers = FrameLayers::default();
            rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
            layers.combined_glyphs.len()
        };
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(styled_text("hello", None, None, true))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_glyphs.len(),
            plain_glyphs * 2,
            "bold paints a one-pixel-offset duplicate of every glyph"
        );
    }

    #[test]
    fn unknown_overlay_token_falls_back_and_counts() {
        // Fail-soft parity with the band path: an unknown but well-formed
        // token substitutes the span default and counts a diagnostic, so the
        // user still sees the modal (never a silently blank pill).
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(styled_text(
            "hi",
            Some("accent"),
            Some("mystery"),
            false,
        ))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay[0].color, rt.config.theme.background,
            "unknown bg falls back to the span default"
        );
        assert_eq!(rt.band_stats.unknown_tokens, 1);
    }

    #[test]
    fn unstyled_multirun_overlay_row_keeps_one_pill() {
        // Backward compatibility: a pre-parity tree (no style declarations)
        // keeps the single-pill path byte-for-byte, even when it is composed
        // from several leaves. Splitting the glyph call would change shaping
        // at run boundaries, so the fast path is part of the contract.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(bitty_lua::ui::UiNode::row(vec![
            styled_text("hi ", None, None, false),
            styled_text("there", None, None, false),
        ]))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            1,
            "unstyled rows keep one centered pill fill"
        );
        assert!(!layers.combined_glyphs.is_empty());
    }

    #[test]
    fn styled_multirun_overlay_row_paints_per_run() {
        // Styled rows paint each run with its own declared background, so a
        // plugin-composed pill (for example an active and an inactive
        // workspace capsule) renders as declared.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(bitty_lua::ui::UiNode::row(vec![
            styled_text("act ", Some("foreground"), Some("accent"), false),
            styled_text("idle", Some("accent"), Some("selection"), false),
        ]))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            2,
            "one fill per declared run"
        );
        let accent = rt
            .config
            .theme
            .active_index(crate::runtime::band_host::BAND_ACCENT_ANSI_INDEX);
        assert_eq!(
            layers.combined_overlay[0].color,
            [accent[0], accent[1], accent[2], 0xFF]
        );
        assert_eq!(layers.combined_overlay[1].color, rt.config.theme.selection);
    }

    #[test]
    fn styled_empty_overlay_row_keeps_pill_chrome() {
        // CTX-1143 fail-open guarantee, styled path (CTX-1419 review): a row
        // that declares style but paints no cell (an empty composer buffer
        // under `bold`) must still paint the default one-cell pill.
        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(bitty_lua::ui::UiNode::row(vec![
            styled_text("", None, None, true),
        ]))));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        assert_eq!(
            layers.combined_overlay.len(),
            1,
            "a styled empty row falls back to one default pill fill"
        );
        assert_eq!(layers.combined_overlay[0].color, rt.config.theme.background);
        // One-cell chrome is a pixel claim, not just a draw-layer count: a
        // zero-width fallback rect would satisfy the fill/flag assertions while
        // still rendering nothing (CTX-1419 review, issue #2191).
        let cell_w = rt.live_cell_metrics().width;
        assert!(cell_w > 0, "the deterministic cell width is nonzero");
        assert_eq!(
            layers.combined_overlay[0].rect.width, cell_w,
            "the empty pill clamps to exactly one cell instead of collapsing to zero"
        );
        assert_eq!(
            layers.combined_overlay[0].rect.height,
            rt.live_cell_metrics().height,
            "the one-cell chrome is one row tall"
        );
        assert!(
            layers.combined_glyphs.is_empty(),
            "empty text paints no glyphs"
        );
        assert!(layers.any_needs_draw);
    }

    #[test]
    fn overlay_root_declaration_reaches_rows() {
        // CTX-1419 review: the overlay stacks a `Column`/`List` root's direct
        // children as rows, but the root's own declaration must still reach
        // them, exactly like a band container's declaration reaches its spans.
        let root = bitty_lua::ui::UiNode::Column {
            children: vec![bitty_lua::ui::UiNode::text("hi")],
            fg: Some("accent".to_string()),
            bg: Some("selection".to_string()),
            bold: Some(true),
            on_click: Some(bitty_lua::ui::ClickCommand {
                command: "test-bar:focus".to_string(),
                args: Vec::new(),
                region: Some(bitty_lua::ui::UiRegion {
                    id: "ws:1".to_string(),
                    kind: bitty_lua::ui::UiRegionKind::Item,
                }),
            }),
            on_scroll: None,
            anchor: None,
            on_hover: None,
        };
        let rows = Runtime::overlay_scene_rows(&root);
        assert_eq!(rows.len(), 1, "one row per direct child");
        assert_eq!(rows[0].runs[0].fg.as_deref(), Some("accent"));
        assert_eq!(rows[0].runs[0].bg.as_deref(), Some("selection"));
        assert!(rows[0].runs[0].bold, "root bold reaches the row run");
        let click = rows[0].runs[0]
            .on_click
            .as_ref()
            .expect("root click binding reaches the row run");
        assert_eq!(click.command, "test-bar:focus");
        assert_eq!(
            rows[0].runs[0]
                .region
                .as_ref()
                .map(|region| region.id.as_str()),
            Some("ws:1"),
            "root region identity reaches the row run (data only; overlay routing stays band-only)"
        );

        let mut rt = runtime();
        rt.set_plugin_overlay(Some(surface_with(root)));
        let mut layers = FrameLayers::default();
        rt.paint_plugin_overlay(&[one_frame()], 8, &mut layers);
        let accent = rt
            .config
            .theme
            .active_index(crate::runtime::band_host::BAND_ACCENT_ANSI_INDEX);
        assert_eq!(
            layers.combined_overlay[0].color, rt.config.theme.selection,
            "root bg resolves through the closed token table"
        );
        assert_eq!(
            layers.combined_glyphs[0].color,
            [accent[0], accent[1], accent[2], 0xFF],
            "root fg resolves through the closed token table"
        );
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
mod scroll_region_copy_tests {
    //! CTX-1207 (#2077): scroll region-copy + damage-only repaint.
    //!
    //! A scrolled leaf shifts its retained primitives by the motion and
    //! repaints only the exposed band instead of re-rendering the full
    //! leaf. Byte equality of the headless frame against a forced
    //! full-render tick proves no tearing; the `cells_examined` delta
    //! proves the saved placement work.
    use super::*;
    use crate::runtime::AnimationKind;

    /// Deterministic single-pane runtime: 80x24 leaf, animations instant
    /// (no time-varying alpha between the two compared ticks).
    fn runtime() -> Runtime {
        let mut config = RuntimeConfig::default();
        config.animations.duration_ms = [0; AnimationKind::COUNT];
        let mut rt = Runtime::with_deterministic_rasterizer(config)
            .expect("deterministic runtime must build");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.handle_resize(PhysicalSize::new(736, 472))
            .expect("resize applies");
        rt
    }

    /// Identical workload on both runtimes: fill the grid with distinct
    /// rows, settle the retained leaf, hide the cursor (no blink-phase
    /// overlay divergence), then scroll with trailing prints — the
    /// scroll-storm shape of motion plus companion print damage plus
    /// scrollback ranges. The scrolls run first as bare line feeds and
    /// the prints land after them via cursor-relative moves, so every
    /// companion addresses the exposed band directly (suffix zero) and
    /// the fast path stays engaged: print-then-scroll companions ride
    /// upward out of the band and fail closed (pinned by the decline
    /// test below). No grid height is assumed (the bottom address
    /// clamps, the rest is relative), and the tick stays inside the
    /// damage-history window.
    fn drive(rt: &mut Runtime) {
        for row in 0..24u8 {
            let line = format!("row-{row:02} ABCDEFGH\r\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        rt.tick();
        rt.handle_pty_bytes(b"\x1b[?25l");
        rt.tick();
        rt.handle_pty_bytes(b"\x1b[99;1H");
        rt.handle_pty_bytes(b"\n\n\n");
        rt.handle_pty_bytes(b"new-00 01234567");
        rt.handle_pty_bytes(b"\x1b[1A");
        rt.handle_pty_bytes(b"new-01 01234567");
        rt.handle_pty_bytes(b"\x1b[1A");
        rt.handle_pty_bytes(b"new-02 01234567");
    }

    #[test]
    fn scroll_fast_path_matches_full_render_and_saves_work() {
        let mut fast = runtime();
        drive(&mut fast);
        let mut full = runtime();
        drive(&mut full);
        // Force the full path on the reference tick: full-frame
        // invalidation declines the fast path and re-renders the leaf.
        full.pending_full_redraw = true;
        let fast_stats = fast.tick().expect("scroll tick must present");
        let full_stats = full.tick().expect("scroll tick must present");
        let (fast_rgba, full_rgba) = (fast.headless_rgba(), full.headless_rgba());
        if fast_rgba != full_rgba {
            // Compact locator (not a dump): first differing pixel in
            // window coordinates plus both frames' work counters.
            if let (Some(a), Some(b)) = (fast_rgba.as_ref(), full_rgba.as_ref()) {
                let w = fast.present_plan_extent().width as usize;
                let first = a
                    .iter()
                    .zip(b.iter())
                    .position(|(x, y)| x != y)
                    .unwrap_or(usize::MAX);
                panic!(
                    "first diff byte {} (px {},{}); fast cells {} full cells {}; fast={:?} full={:?}",
                    first,
                    (first / 4) % w,
                    (first / 4) / w,
                    fast_stats.cells_examined,
                    full_stats.cells_examined,
                    &a[first..first + 4],
                    &b[first..first + 4],
                );
            }
        }
        assert_eq!(
            fast_rgba, full_rgba,
            "region-copy + exposed repaint must equal a full repaint"
        );
        assert!(
            full_stats.cells_examined > 1000,
            "reference must place a leaf-sized frame, got {}",
            full_stats.cells_examined
        );
        assert!(
            fast_stats.cells_examined < full_stats.cells_examined,
            "fast path must examine fewer cells (fast {} vs full {})",
            fast_stats.cells_examined,
            full_stats.cells_examined
        );
    }

    /// Identical settle, then a mid-screen rewrite in the same tick as
    /// the scroll: the rewritten row rides the scroll to a post-tick
    /// row outside the exposed band and the margin row. Deliberately
    /// small (a 4-cell overwrite, bare-LF scrolls): the tick must stay
    /// inside the damage-history window or `damage_since` falls back
    /// to a full-grid rectangle and the fast path never engages.
    fn drive_with_mid_screen_rewrite(rt: &mut Runtime) {
        for row in 0..24u8 {
            let line = format!("row-{row:02} ABCDEFGH\r\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        rt.tick();
        rt.handle_pty_bytes(b"\x1b[?25l");
        rt.tick();
        // Overwrite four mid-screen cells (pre-shift row 10), then
        // scroll the grid three lines in the same tick. The bottom
        // address clamps, so no grid height is assumed.
        rt.handle_pty_bytes(b"\x1b[11;1H");
        rt.handle_pty_bytes(b"ZZZZ");
        rt.handle_pty_bytes(b"\x1b[99;1H");
        rt.handle_pty_bytes(b"\n\n\n");
    }

    #[test]
    fn scroll_fast_path_declines_out_of_band_companion_rewrite() {
        // PX-6182 (CodeRabbit PR #2080 inline Major): a translated
        // companion rect outside band+margin repaints fresh but never
        // drops the retained pre-print glyph, which paints over the
        // fresh fill. Fail-closed: decline to a full render, so the
        // frame still matches a forced full render with no work saved.
        let mut fast = runtime();
        drive_with_mid_screen_rewrite(&mut fast);
        let mut full = runtime();
        drive_with_mid_screen_rewrite(&mut full);
        full.pending_full_redraw = true;
        let fast_stats = fast.tick().expect("scroll tick must present");
        let full_stats = full.tick().expect("scroll tick must present");
        let (fast_rgba, full_rgba) = (fast.headless_rgba(), full.headless_rgba());
        if fast_rgba != full_rgba {
            if let (Some(a), Some(b)) = (fast_rgba.as_ref(), full_rgba.as_ref()) {
                let w = fast.present_plan_extent().width as usize;
                let first = a
                    .iter()
                    .zip(b.iter())
                    .position(|(x, y)| x != y)
                    .unwrap_or(usize::MAX);
                panic!(
                    "first diff byte {} (px {},{}); fast cells {} full cells {}",
                    first,
                    (first / 4) % w,
                    (first / 4) / w,
                    fast_stats.cells_examined,
                    full_stats.cells_examined,
                );
            }
            panic!("frame mismatch with no rgba on one side");
        }
        assert_eq!(
            fast_stats.cells_examined, full_stats.cells_examined,
            "out-of-band companion must decline the fast path (fail-closed full render)"
        );
    }
}

#[cfg(test)]
mod present_coalescing_tests {
    //! CTX-1231 (#2093): present-latest coalescing — drop intermediate
    //! frames when behind.
    //!
    //! When dirty ticks arrive faster than the present cadence
    //! (`min_present_interval`), intermediate presents are skipped with
    //! damage retained and only the latest state presents. The burst test
    //! below proves the final headless frame is byte-identical to an
    //! uncoalesced run at the same virtual clock (no torn intermediate is
    //! ever presented — intermediates are never presented at all) while
    //! present count and renderer work drop. The scroll-storm shape also
    //! exercises the CTX-1207 cross-tick frame-merge: deferred ticks
    //! accumulate same-region motions in the damage ring and the deadline
    //! tick sums them in one region-copy.
    use super::*;
    use crate::runtime::AnimationKind;
    use std::time::{Duration, Instant};

    /// Present cadence for the tests: dirty ticks 1 ms apart coalesce, the
    /// tick at +16 ms presents (mirrors `ANIMATION_FRAME_INTERVAL`).
    const INTERVAL: Duration = Duration::from_millis(16);

    /// Deterministic single-pane runtime: 80x24 leaf, animations instant
    /// (no time-varying alpha between compared ticks), cursor hidden at
    /// settle so no blink-phase overlay can diverge.
    fn runtime() -> Runtime {
        let mut config = RuntimeConfig::default();
        config.animations.duration_ms = [0; AnimationKind::COUNT];
        let mut rt = Runtime::with_deterministic_rasterizer(config)
            .expect("deterministic runtime must build");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.handle_resize(PhysicalSize::new(736, 472))
            .expect("resize applies");
        rt
    }

    /// Fill the grid with distinct rows, hide the cursor, and present once
    /// at `t0` (stamps the present cadence clock).
    fn settle(rt: &mut Runtime, t0: Instant) {
        for row in 0..24u8 {
            let line = format!("row-{row:02} ABCDEFGH\r\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        rt.handle_pty_bytes(b"\x1b[?25l");
        assert!(
            rt.tick_at(t0).is_some(),
            "settle tick must present the first frame"
        );
    }

    /// One scroll-storm step: scroll a line at the bottom, print a marker
    /// on the exposed row (motion plus companion print damage per tick).
    fn burst_step(rt: &mut Runtime, step: u8) {
        rt.handle_pty_bytes(b"\x1b[99;1H");
        rt.handle_pty_bytes(b"\n");
        let marker = format!("tick-{step:02}");
        rt.handle_pty_bytes(marker.as_bytes());
    }

    /// First differing byte locator shared by the equality asserts (not a
    /// dump: pixel coordinates plus both frames' work counters).
    fn assert_rgba_equal(a: &Option<Vec<u8>>, b: &Option<Vec<u8>>, context: &str, w: usize) {
        if a != b {
            if let (Some(x), Some(y)) = (a.as_ref(), b.as_ref()) {
                let first = x
                    .iter()
                    .zip(y.iter())
                    .position(|(p, q)| p != q)
                    .unwrap_or(usize::MAX);
                panic!(
                    "{context}: first diff byte {first} (px {},{}); left={:?} right={:?}",
                    (first / 4) % w,
                    (first / 4) / w,
                    &x[first..first + 4],
                    &y[first..first + 4],
                );
            }
            panic!("{context}: frame mismatch with rgba on only one side");
        }
    }

    #[test]
    fn burst_coalesces_to_latest_and_matches_uncoalesced() {
        let t0 = Instant::now();
        let mut coalesced = runtime();
        coalesced.set_min_present_interval(INTERVAL);
        let mut reference = runtime();
        settle(&mut coalesced, t0);
        settle(&mut reference, t0);
        let mut reference_presents = 1u32;
        let mut reference_cells = 0u64;
        let mut coalesced_cells = 0u64;
        // Twelve dirty ticks 1 ms apart: the reference presents every one,
        // the gated runtime defers every one with damage retained. The
        // burst exceeds the 64-batch damage-history window on purpose, so
        // the deadline tick exercises the fail-closed full-grid fallback —
        // still byte-identical, still fewer presents (the in-window
        // region-copy regime is pinned by the next test).
        for step in 0..12u8 {
            let at = t0 + Duration::from_millis(u64::from(step) + 1);
            burst_step(&mut coalesced, step);
            burst_step(&mut reference, step);
            assert!(
                coalesced.tick_at(at).is_none(),
                "burst tick {step} inside the interval must defer"
            );
            let stats = reference
                .tick_at(at)
                .expect("uncoalesced burst tick must present");
            reference_presents += 1;
            reference_cells += stats.cells_examined;
        }
        assert_eq!(
            coalesced.presents_coalesced(),
            12,
            "every intermediate burst tick must count as coalesced"
        );
        assert_eq!(
            reference_presents, 13,
            "reference must present settle + 12 burst ticks"
        );
        // One more step, then the deadline tick presents the accumulated
        // latest state on both runtimes at the same virtual clock.
        let end = t0 + INTERVAL;
        burst_step(&mut coalesced, 12);
        burst_step(&mut reference, 12);
        let final_stats = coalesced
            .tick_at(end)
            .expect("deadline tick must present the coalesced frame");
        coalesced_cells += final_stats.cells_examined;
        let reference_final = reference
            .tick_at(end)
            .expect("reference deadline tick must present");
        reference_presents += 1;
        reference_cells += reference_final.cells_examined;
        let w = coalesced.present_plan_extent().width as usize;
        assert_rgba_equal(
            &coalesced.headless_rgba(),
            &reference.headless_rgba(),
            "coalesced final frame must equal the uncoalesced final frame",
            w,
        );
        assert_eq!(
            reference_presents, 14,
            "reference must present every dirty tick"
        );
        assert!(
            coalesced_cells < reference_cells,
            "coalescing must skip renderer work (coalesced {coalesced_cells} vs reference {reference_cells} cells)"
        );
    }

    #[test]
    fn cross_tick_scroll_net_motion_matches_forced_full_render() {
        // The CTX-1207 interaction: deferred ticks accumulate same-region
        // scroll motions across ticks in the damage ring; the deadline tick
        // must region-copy the net motion and match a forced full render
        // byte-for-byte (the frame-merge follow-up the scroll path left
        // open), examining fewer cells. Five steps stay inside the 64-batch
        // damage-history window (~9 generations per step); wider bursts
        // degrade to the fail-closed full-grid fallback pinned above.
        let t0 = Instant::now();
        let mut coalesced = runtime();
        coalesced.set_min_present_interval(INTERVAL);
        let mut full = runtime();
        settle(&mut coalesced, t0);
        settle(&mut full, t0);
        for step in 0..4u8 {
            let at = t0 + Duration::from_millis(u64::from(step) + 1);
            burst_step(&mut coalesced, step);
            burst_step(&mut full, step);
            assert!(coalesced.tick_at(at).is_none());
            // The reference presents every mid-burst step (uncoalesced
            // cadence); its deadline tick below is forced to a full render
            // of the same final grid state.
            assert!(full.tick_at(at).is_some());
        }
        burst_step(&mut coalesced, 4);
        burst_step(&mut full, 4);
        // Force the full path on the reference deadline tick: full-frame
        // invalidation declines the region-copy and re-renders the leaf.
        full.pending_full_redraw = true;
        let end = t0 + INTERVAL;
        let fast_stats = coalesced.tick_at(end).expect("deadline tick must present");
        let full_stats = full.tick_at(end).expect("full tick must present");
        let w = coalesced.present_plan_extent().width as usize;
        assert_rgba_equal(
            &coalesced.headless_rgba(),
            &full.headless_rgba(),
            "cross-tick net-motion region-copy must equal a full repaint",
            w,
        );
        assert!(
            fast_stats.cells_examined < full_stats.cells_examined,
            "net-motion copy must examine fewer cells (fast {} vs full {})",
            fast_stats.cells_examined,
            full_stats.cells_examined,
        );
    }

    #[test]
    fn deferral_arms_deadline_and_presents_at_cadence() {
        let t0 = Instant::now();
        let mut rt = runtime();
        rt.set_min_present_interval(INTERVAL);
        settle(&mut rt, t0);
        assert_eq!(
            rt.present_coalescing_deadline(),
            None,
            "no deadline while nothing is deferred"
        );
        rt.handle_pty_bytes(b"hello");
        assert!(
            rt.tick_at(t0 + Duration::from_millis(1)).is_none(),
            "dirty tick inside the interval must defer"
        );
        assert_eq!(
            rt.present_coalescing_deadline(),
            Some(t0 + INTERVAL),
            "a deferred frame must arm the app wake at the cadence deadline"
        );
        let stats = rt
            .tick_at(t0 + INTERVAL)
            .expect("deadline tick must present the held frame");
        assert_eq!(stats.generation, rt.state.generation());
        assert_eq!(
            rt.present_coalescing_deadline(),
            None,
            "deadline clears once the held frame presents"
        );
        assert_eq!(rt.presents_coalesced(), 1);
        assert!(
            rt.tick_at(t0 + INTERVAL + Duration::from_millis(1))
                .is_none(),
            "quiet tick after the present must idle, not defer"
        );
        assert_eq!(
            rt.presents_coalesced(),
            1,
            "idle ticks must not count as coalesced"
        );
    }

    #[test]
    fn first_frame_always_presents() {
        let mut rt = runtime();
        rt.set_min_present_interval(INTERVAL);
        rt.handle_pty_bytes(b"first");
        assert!(
            rt.tick_at(Instant::now()).is_some(),
            "the first frame must present even with coalescing armed"
        );
    }

    #[test]
    fn unrepresentable_deadline_presents_immediately() {
        // CodeRabbit PR #2094 Minor: an interval past `Instant::MAX` makes
        // the wake deadline unrepresentable; the gate must present
        // immediately (fail-closed liveness) instead of deferring a frame
        // no wake can ever release.
        let t0 = Instant::now();
        let mut rt = runtime();
        rt.set_min_present_interval(Duration::MAX);
        settle(&mut rt, t0);
        rt.handle_pty_bytes(b"hello");
        assert!(
            rt.tick_at(t0 + Duration::from_millis(1)).is_some(),
            "an unrepresentable deadline must not defer without a wake"
        );
        assert_eq!(rt.presents_coalesced(), 0);
        assert_eq!(rt.present_coalescing_deadline(), None);
    }

    /// 2x2 opaque red RGBA payload (`f=32`), mirroring the
    /// `kitty_images_present` wiring suite.
    fn red_2x2() -> Vec<u8> {
        [0xFF, 0x00, 0x00, 0xFF].repeat(4)
    }

    #[test]
    fn alt_enter_leave_across_deferred_ticks_retains_placements() {
        // #2132: main-screen kitty placements survive an alternate-screen
        // cycle. Terminal truth scopes its alt switch to the alt screen,
        // so the rich placements stay valid underneath; the paint path
        // skips blits while alt is active and repaints them on leave.
        // With entry observed on one deferred tick and the leave on the
        // next, no tick ever paints while alt is active, so the deadline
        // frame must show the retained image, not a cleared plane.
        //
        // (Enter+leave inside a single PTY batch is latch-invisible with
        // or without coalescing — pre-existing parity, out of scope here.)
        let t0 = Instant::now();
        let mut rt = runtime();
        rt.set_min_present_interval(INTERVAL);
        settle(&mut rt, t0);
        rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
            .expect("admissible payload must place");
        assert_eq!(rt.kitty_placement_count(), 1);
        rt.handle_pty_bytes(b"\x1b[?1049h");
        assert!(
            rt.tick_at(t0 + Duration::from_millis(1)).is_none(),
            "alt-entry tick inside the interval must defer"
        );
        assert_eq!(
            rt.kitty_placement_count(),
            1,
            "alt entry must retain main-screen placements"
        );
        rt.handle_pty_bytes(b"\x1b[?1049l");
        assert!(
            rt.tick_at(t0 + Duration::from_millis(2)).is_none(),
            "alt-leave tick inside the interval must defer"
        );
        let stats = rt
            .tick_at(t0 + INTERVAL)
            .expect("deadline tick must present");
        assert_eq!(stats.images, 1, "deadline frame must repaint the image");
        assert_eq!(rt.kitty_placement_count(), 1);
    }

    #[test]
    fn alt_cycle_hides_while_active_and_restores_on_leave() {
        // #2132 repro shape (`chafa -f kitty` image, then a TUI app runs
        // and exits): the image paints before, hides while the fullscreen
        // app owns the pixels, and repaints after — never silently lost.
        let t0 = Instant::now();
        let mut rt = runtime();
        settle(&mut rt, t0);
        rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
            .expect("admissible payload must place");
        let stats = rt
            .tick_at(t0 + Duration::from_millis(1))
            .expect("image tick must present");
        assert_eq!(stats.images, 1, "main-screen image must paint");
        rt.handle_pty_bytes(b"\x1b[?1049h");
        let stats = rt
            .tick_at(t0 + Duration::from_millis(2))
            .expect("alt-entry tick must present");
        assert_eq!(stats.images, 0, "no blit while alt is active");
        assert_eq!(rt.kitty_placement_count(), 1, "placements retained");
        rt.handle_pty_bytes(b"\x1b[?1049l");
        let stats = rt
            .tick_at(t0 + Duration::from_millis(3))
            .expect("alt-leave tick must present");
        assert_eq!(stats.images, 1, "image must survive the TUI cycle");
        assert_eq!(rt.kitty_placement_count(), 1);
    }

    #[test]
    fn zero_interval_presents_every_dirty_tick() {
        let t0 = Instant::now();
        let mut rt = runtime();
        assert_eq!(rt.min_present_interval(), Duration::ZERO);
        settle(&mut rt, t0);
        for step in 0..4u8 {
            burst_step(&mut rt, step);
            assert!(
                rt.tick_at(t0 + Duration::from_millis(u64::from(step) + 1))
                    .is_some(),
                "default (zero interval) must preserve every-tick presents"
            );
        }
        assert_eq!(rt.presents_coalesced(), 0);
        assert_eq!(rt.present_coalescing_deadline(), None);
    }
}

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

/// Clip a scene row to a frame width in display cells (wide glyphs count 2).
///
/// Returns the clipped string plus its display width (minimum 1, matching the
/// pill-width floor). A wide glyph straddling the limit is dropped whole so a
/// pill never claims a half cell.
/// Horizontal start column for a hover-anchored tooltip (issue #2530).
///
/// Centers the tooltip row stack on the hovered region's midpoint and clamps
/// it so the whole stack stays inside the window whenever it fits. A stack
/// wider than the window starts at column 0 and clips like any other
/// over-wide overlay row; a zero-width stack keeps the region's own start
/// column (the caller then paints nothing for empty anchored rows).
fn centered_tooltip_x(start_col: usize, end_col: usize, stack_cells: u16, width: u16) -> u16 {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }
    let stack = usize::from(stack_cells);
    if stack == 0 {
        return u16::try_from(start_col.min(width - 1)).unwrap_or(0);
    }
    if stack >= width {
        return 0;
    }
    let center = start_col.saturating_add(end_col) / 2;
    let left = center.saturating_sub(stack / 2);
    u16::try_from(left.min(width - stack)).unwrap_or(u16::MAX)
}

fn clip_to_width_cells(text: &str, cols: u16) -> (String, usize) {
    let mut width = 0usize;
    let mut clipped = String::new();
    for c in text.chars() {
        let w = usize::from(bitty_term_state::char_cell_width(c));
        if width + w > usize::from(cols) {
            break;
        }
        width += w;
        clipped.push(c);
    }
    (clipped, width.max(1))
}

#[cfg(test)]
mod clip_width_cells_tests {
    use super::clip_to_width_cells;

    #[test]
    fn ascii_counts_one_cell_each() {
        assert_eq!(clip_to_width_cells("hi", 10), ("hi".to_string(), 2));
        assert_eq!(clip_to_width_cells("abcdef", 4), ("abcd".to_string(), 4));
    }

    #[test]
    fn cjk_counts_two_cells_each() {
        assert_eq!(clip_to_width_cells("界界", 10), ("界界".to_string(), 4));
    }

    #[test]
    fn straddling_wide_glyph_drops_whole() {
        assert_eq!(clip_to_width_cells("a界", 2), ("a".to_string(), 1));
    }

    #[test]
    fn empty_keeps_unit_floor() {
        assert_eq!(clip_to_width_cells("", 10), (String::new(), 1));
    }
}

#[cfg(test)]
mod unicode_run_union_tests {
    use super::*;
    use bitty_rich::geometry::RectPx;

    #[test]
    fn union_is_the_bounding_box() {
        let rects = [RectPx::new(0, 0, 10, 10), RectPx::new(5, 8, 10, 10)];
        assert_eq!(unicode_rect_union(&rects), Some(RectPx::new(0, 0, 15, 18)));
        assert_eq!(unicode_rect_union(&[]), None);
    }

    #[test]
    fn full_width_contiguous_rows_cover_the_union() {
        let union = RectPx::new(0, 0, 8, 4);
        assert!(unicode_rects_cover_union(
            union,
            &[RectPx::new(0, 0, 8, 2), RectPx::new(0, 2, 8, 2)]
        ));
        // A vertical gap leaves the union uncovered.
        assert!(!unicode_rects_cover_union(
            union,
            &[RectPx::new(0, 0, 8, 1), RectPx::new(0, 3, 8, 1)]
        ));
        // A narrower row does not span the union.
        assert!(!unicode_rects_cover_union(
            union,
            &[RectPx::new(0, 0, 4, 4)]
        ));
        assert!(!unicode_rects_cover_union(union, &[]));
    }

    #[test]
    fn selection_row_rects_are_rectangular_for_block() {
        // Issue #2523: block selections used the text-flow shape (first row
        // to the right edge, last row from the left edge) and looked like a
        // zigzag. Every block row must cover the same column window.
        use bitty_term_state::Style;
        let width = 6usize;
        let cells = vec![Cell::erased(Style::default()); width * 3];
        let metrics = CellMetrics {
            width: 9,
            height: 19,
        };
        let rects = selection_row_rects(
            &bitty_render::grid::ThemePalette::bitty_dark(),
            SelectionKind::Block,
            CellPos::new(0, 1),
            CellPos::new(2, 3),
            width as u16,
            3,
            &cells,
            width,
            3,
            0,
            metrics,
        );
        assert_eq!(rects.len(), 3, "one rect per spanned row");
        for (row, fill) in rects.iter().enumerate() {
            assert_eq!(fill.rect.x, 9, "block row {row} starts at column 1");
            assert_eq!(fill.rect.width, 27, "block row {row} covers columns 1..=3");
            assert_eq!(fill.rect.y, row as i32 * 19);
        }
    }

    #[test]
    fn selection_row_rects_follow_text_flow_for_simple() {
        // First row from the anchor column, middle rows full width, last row
        // through the focus column.
        use bitty_term_state::Style;
        let width = 6usize;
        let cells = vec![Cell::erased(Style::default()); width * 3];
        let metrics = CellMetrics {
            width: 9,
            height: 19,
        };
        let rects = selection_row_rects(
            &bitty_render::grid::ThemePalette::bitty_dark(),
            SelectionKind::Simple,
            CellPos::new(0, 1),
            CellPos::new(2, 3),
            width as u16,
            3,
            &cells,
            width,
            3,
            0,
            metrics,
        );
        assert_eq!(rects.len(), 3);
        assert_eq!((rects[0].rect.x, rects[0].rect.width), (9, 45));
        assert_eq!((rects[1].rect.x, rects[1].rect.width), (0, 54));
        assert_eq!((rects[2].rect.x, rects[2].rect.width), (0, 36));
    }

    #[test]
    fn selection_row_rects_cover_wide_glyph_trailing_cell() {
        // Issue #2523: ending on a wide leader must tint its spacer too, and
        // a block row ending there must extend by one cell.
        use bitty_term_state::{Style, Zerowidth};
        let width = 6usize;
        let mut cells = vec![Cell::erased(Style::default()); width * 2];
        cells[1] = Cell {
            glyph: '中',
            style: Style::default(),
            width: 2,
            spacer: false,
            padding: false,
            hyperlink: None,
            zerowidth: Zerowidth::new(),
        };
        cells[2] = Cell::wide_spacer(Style::default());
        let metrics = CellMetrics {
            width: 9,
            height: 19,
        };
        let rects = selection_row_rects(
            &bitty_render::grid::ThemePalette::bitty_dark(),
            SelectionKind::Simple,
            CellPos::new(0, 1),
            CellPos::new(0, 1),
            width as u16,
            2,
            &cells,
            width,
            2,
            0,
            metrics,
        );
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].rect.x, 9);
        assert_eq!(
            rects[0].rect.width, 18,
            "the wide leader and its spacer are one glyph"
        );
    }

    #[test]
    fn mask_zeroes_uncovered_pixels_only() {
        let union = RectPx::new(0, 0, 4, 2);
        let mut rgba = vec![255u8; 4 * 2 * 4];
        mask_uncovered_unicode_pixels(
            &mut rgba,
            union,
            &[RectPx::new(0, 0, 2, 1), RectPx::new(3, 1, 1, 1)],
        );
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let base = (y * 4 + x) * 4;
            [rgba[base], rgba[base + 1], rgba[base + 2], rgba[base + 3]]
        };
        assert_eq!(pixel(0, 0), [255; 4], "covered pixel untouched");
        assert_eq!(pixel(1, 0), [255; 4]);
        assert_eq!(pixel(2, 0), [0; 4], "uncovered column masked");
        assert_eq!(pixel(3, 0), [0; 4]);
        assert_eq!(pixel(3, 1), [255; 4], "covered cell on the second row");
        assert_eq!(pixel(0, 1), [0; 4], "uncovered cell stays text-visible");
    }
}
