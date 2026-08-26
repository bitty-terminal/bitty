//! The grid pipeline: terminal-state snapshots in, owned draw records out.
//!
//! [`GridRenderer`] consumes the public render surface of
//! `bitty-term-state` — a versioned [`Snapshot`] plus its [`Damage`] — and
//! produces an owned [`DrawList`] through three stages:
//!
//! 1. **Plan**: grid-coordinate damage is converted to pixel rectangles
//!    against the configured [`CellMetrics`] and fed to
//!    [`frame::plan_frame`] through [`SnapshotDamage`], preserving the
//!    damage-driven partial-redraw semantics of the terminal-state-rfc
//!    damage model.
//! 2. **Place**: every cell covered by a planned dirty rectangle is
//!    examined. Background fills and text decorations become
//!    [`FillRect`]s; printable characters become [`GlyphInstance`]s.
//!    Trailing halves of wide characters (`spacer` cells) are skipped — the
//!    leading half already paints across both columns. Invisible cells
//!    suppress their glyph but keep their background.
//! 3. **Cache**: glyph lookups go through [`GlyphCache`] keyed by
//!    [`RasterKey`]; bitmaps are placed into [`GlyphAtlas`], which pairs
//!    the shelf-packed [`AtlasLayout`] with a CPU-side coverage texture and
//!    a drainable upload queue. A backend drains
//!    [`GridRenderer::take_atlas_uploads`] and copies the queued bytes into
//!    its texture; nothing here touches GPU objects (the GpuContext seam
//!    stays behind `gpu`). Actual vertex buffers are intentionally **not**
//!    part of this slice: the [`DrawList`] describes instances in owned
//!    types only, and the platform-seam slice decides the upload format.
//!
//! # Reading rule (ADR-0003 dependency rule 3)
//!
//! This module depends on `bitty-term-state` exclusively through its public
//! `Snapshot`/`Damage`/`Cell` types. No private structure is reached into,
//! and nothing here ever mutates terminal state: the renderer only reads.
//!
//! # Damage semantics
//!
//! Every visited cell repaints its full background (resolved, including the
//! default background), so drawing the union of incremental frames equals a
//! full redraw of the same final state — over-damage stays safe and
//! under-damage is impossible, mirroring the state layer's contract.
//! Scrollback damage ranges concern lines above the visible grid; they add
//! no pixels on this surface (the active screen) and contribute no regions.
//! Stale or oversized damage cannot under-damage either: planning clips
//! regions to the snapshot-derived extent.
//!
//! # Deferred deliberately
//!
//! - **Cursor visuals**: the cursor flows through the snapshot, but cursor
//!   shape/color policy belongs to the configuration/theme layer and is not
//!   invented here.
//! - **Scrollback viewport rendering**: this surface is the active screen.
//! - **Shaped clusters, color fonts, synthetic bold**: await the text RFC
//!   named by ADR-0004. One glyph per cell (the leading Unicode scalar),
//!   placed on a fixed baseline (see [`BASELINE_NUMERATOR`]).
//! - **Subpixel RGB antialiasing policy**: upstream coverage is averaged to
//!   luminance exactly like [`crate::software::SurfaceRgba::blend_glyph`].
//!
//! # Performance instrumentation vs budgets
//!
//! [`GridRenderer::counters`], [`GridRenderer::cache_stats`], and
//! [`GridRenderer::atlas_stats`] expose monotone counters (frames planned,
//! cells examined/drawn, glyphs emitted/rasterized, cache and atlas
//! hits/misses, evictions, inline fallbacks). They exist to make the cost
//! drivers of performance-budget-rfc **measurable**: PB-4 input latency
//! (≤ 8 ms p50 key-to-screen) and PB-6 throughput floor (≥ 40 MB/s
//! parse-and-render) are dominated by exactly these work items, and PB-7
//! idle usage depends on this pipeline staying frame-on-demand (a clean
//! plan emits no work at all). These counters do **not** measure wall-clock
//! time, and this slice claims **no budget compliance**: per the RFC's
//! cross-cutting rules, budgets become acceptance criteria only after the
//! measurement harness, corpora, and reference machines are defined by an
//! implementing task.

use std::collections::HashMap;

use bitty_term_state::{Color, Damage, DamageRect, DamagedRegion, Rgb, Snapshot, Style};

use crate::atlas::{AtlasDims, AtlasLayout, AtlasSlot, DEFAULT_ATLAS_DIMENSION};
use crate::cache::{CachedGlyph, GlyphCache};
use crate::error::RenderError;
use crate::frame::{DamageDescriptor, FramePlan, plan_frame};
use crate::geometry::{ExtentPx, RectPx};
use crate::glyph::{
    BitmapFormat, FontId, FontQuery, GlyphBitmap, GlyphMetrics, GlyphRasterizer, RasterKey,
};

/// Straight-alpha RGBA color, `[r, g, b, a]` bytes.
pub type Rgba8 = [u8; 4];

/// Default foreground: light gray, fully opaque.
pub const DEFAULT_FG: Rgba8 = [0xE5, 0xE5, 0xE5, 0xFF];
/// Default background: near-black, fully opaque.
pub const DEFAULT_BG: Rgba8 = [0x10, 0x10, 0x10, 0xFF];
/// Foreground alpha substituted for faint (`SGR 2`) text.
pub const FAINT_ALPHA: u8 = 0x7F;

/// Numerator of the fixed baseline rule: the pen baseline sits at
/// `row_top + cell_height * BASELINE_NUMERATOR / 4`. Metric-aware baselines
/// arrive with the text RFC; this constant rule keeps placement
/// deterministic today.
pub const BASELINE_NUMERATOR: u32 = 3;

/// Pixel size of one grid cell.
///
/// Both dimensions must be non-zero. A monospace face at a given point size
/// yields one fixed cell size, supplied by the embedder; the rasterizer
/// contract deliberately does not aggregate font-wide metrics yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub width: u32,
    /// Cell height in pixels.
    pub height: u32,
}

impl CellMetrics {
    /// Validates and builds cell metrics.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero.
    pub fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidInput {
                reason: "cell metrics must be non-zero",
            });
        }
        Ok(Self { width, height })
    }

    /// Pixel extent of a `cols x rows` grid (saturating arithmetic: every
    /// intermediate multiply saturates, so hostile dimensions cannot panic
    /// or wrap under any build profile).
    #[must_use]
    pub fn extent_for(&self, cols: usize, rows: usize) -> ExtentPx {
        ExtentPx::new(
            saturating_u32(u64::from(self.width).saturating_mul(cols as u64)),
            saturating_u32(u64::from(self.height).saturating_mul(rows as u64)),
        )
    }
}

/// Clamps a `u64` product back into `u32` range (value-preserving for every
/// realistic grid; saturation keeps hostile dimensions overflow-free).
const fn saturating_u32(value: u64) -> u32 {
    const MAX: u64 = u32::MAX as u64;
    if value > MAX { u32::MAX } else { value as u32 }
}

/// Clamps a `u64` product back into positive-`i32` range.
const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

/// Resolves one palette/indexed/direct color to straight-alpha RGBA.
///
/// `Color::Default` maps to `fallback`; indexed entries go through the
/// built-in deterministic 256-color palette (16 ANSI entries, the 6×6×6
/// cube, 24 grayscale steps — xterm-compatible values). Fully deterministic
/// on every platform; palette customization awaits the configuration model.
#[must_use]
pub fn resolve_color(color: Option<&Color>, fallback: Rgba8) -> Rgba8 {
    let rgb = match color {
        None | Some(Color::Default) => [fallback[0], fallback[1], fallback[2]],
        Some(Color::Rgb(Rgb { r, g, b })) => [*r, *g, *b],
        Some(Color::Indexed(i)) => palette_rgb(*i),
    };
    [rgb[0], rgb[1], rgb[2], fallback[3]]
}

/// Built-in 256-color palette entry (`xterm`-compatible constants).
#[must_use]
pub fn palette_rgb(index: u8) -> [u8; 3] {
    const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0 => [0, 0, 0],
        1 => [205, 0, 0],
        2 => [0, 205, 0],
        3 => [205, 205, 0],
        4 => [0, 0, 238],
        5 => [205, 0, 205],
        6 => [0, 205, 205],
        7 => [229, 229, 229],
        8 => [127, 127, 127],
        9 => [255, 0, 0],
        10 => [0, 255, 0],
        11 => [255, 255, 0],
        12 => [92, 92, 255],
        13 => [255, 0, 255],
        14 => [0, 255, 255],
        15 => [255, 255, 255],
        16..=231 => {
            let n = u32::from(index) - 16;
            [
                CUBE_LEVELS[(n / 36) as usize],
                CUBE_LEVELS[((n / 6) % 6) as usize],
                CUBE_LEVELS[(n % 6) as usize],
            ]
        }
        232..=255 => {
            let gray = 8 + 10 * (u32::from(index) - 232);
            [gray as u8; 3]
        }
    }
}

/// Effective (foreground, background) pair for a styled cell.
///
/// Inverse video swaps the pair; faint reduces foreground alpha. The pair
/// is resolved eagerly so downstream code never sees symbolic colors.
#[must_use]
pub fn resolved_colors(style: &Style) -> (Rgba8, Rgba8) {
    let fg = resolve_color(style.foreground.as_ref(), DEFAULT_FG);
    let bg = resolve_color(style.background.as_ref(), DEFAULT_BG);
    let (fg, bg) = if style.attributes.inverse {
        (bg, fg)
    } else {
        (fg, bg)
    };
    let fg = if style.attributes.faint {
        [fg[0], fg[1], fg[2], FAINT_ALPHA]
    } else {
        fg
    };
    (fg, bg)
}

/// Converts grid-coordinate damage into the pixel-domain descriptor
/// consumed by [`plan_frame`].
///
/// Construction is infallible: conversion uses saturating `u64`/`i64`
/// intermediates, and out-of-range regions are later clipped by the planner
/// rather than rejected (over-damage is safe). Scrollback regions
/// contribute nothing on this surface (see module docs).
#[derive(Debug, Clone)]
pub struct SnapshotDamage {
    extent: ExtentPx,
    pixel_regions: Vec<RectPx>,
    grid_regions: Vec<DamageRect>,
    full_hint: bool,
}

impl SnapshotDamage {
    /// Builds the descriptor for `snapshot` damaged by `damage`.
    #[must_use]
    pub fn new(snapshot: &Snapshot, damage: &Damage, cell: CellMetrics) -> Self {
        let extent = cell.extent_for(snapshot.width, snapshot.height);
        let mut grid_regions = Vec::new();
        let mut pixel_regions = Vec::new();
        for region in &damage.regions {
            let DamageRect {
                top,
                left,
                bottom,
                right,
            } = match region {
                DamagedRegion::Grid(rect) => *rect,
                DamagedRegion::Scrollback { .. } => continue,
            };
            grid_regions.push(DamageRect {
                top,
                left,
                bottom,
                right,
            });
            pixel_regions.push(grid_rect_to_px(top, left, bottom, right, cell));
        }
        Self {
            extent,
            pixel_regions,
            grid_regions,
            full_hint: false,
        }
    }

    /// Forces a full-redraw hint (first frame after realization, resize,
    /// device-loss recovery).
    #[must_use]
    pub fn with_full_redraw(mut self) -> Self {
        self.full_hint = true;
        self
    }

    /// The grid-coordinate regions behind this descriptor (same order;
    /// scrollback entries dropped).
    #[must_use]
    pub fn grid_regions(&self) -> &[DamageRect] {
        &self.grid_regions
    }
}

impl DamageDescriptor for SnapshotDamage {
    fn extent(&self) -> ExtentPx {
        self.extent
    }

    fn damaged_regions(&self) -> &[RectPx] {
        &self.pixel_regions
    }

    fn full_redraw_hint(&self) -> bool {
        self.full_hint
    }
}

/// Inclusive grid rectangle to inclusive-cell pixel rectangle; saturating.
fn grid_rect_to_px(top: u16, left: u16, bottom: u16, right: u16, cell: CellMetrics) -> RectPx {
    let cols = u64::from(right) - u64::from(left) + 1;
    let rows = u64::from(bottom) - u64::from(top) + 1;
    RectPx::new(
        saturating_i32(u64::from(left) * u64::from(cell.width)),
        saturating_i32(u64::from(top) * u64::from(cell.height)),
        saturating_u32(cols * u64::from(cell.width)),
        saturating_u32(rows * u64::from(cell.height)),
    )
}

/// One opaque background/decoration rectangle to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRect {
    /// Rectangle in logical pixels (cell-aligned by construction).
    pub rect: RectPx,
    /// Straight-alpha fill color.
    pub color: Rgba8,
}

/// Where a glyph instance's texels live.
#[derive(Debug, Clone, PartialEq)]
pub enum GlyphSource {
    /// Coverage texels inside the renderer's atlas texture.
    Atlas {
        /// Placed region; `uv` on the instance carries the normalized form.
        slot: AtlasSlot,
    },
    /// Texels carried inline because the bitmap could not fit the atlas
    /// even after a deterministic wholesale eviction. Coverage bytes,
    /// row-major, length `width * height`.
    Inline {
        /// Coverage bytes (one per texel).
        mask: Vec<u8>,
        /// Mask width in texels.
        width: u32,
        /// Mask height in texels.
        height: u32,
    },
}

/// One glyph to composite, fully described in owned data.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphInstance {
    /// Destination top-left in logical pixels (may be negative; bearings).
    pub dest: [i32; 2],
    /// Source size in texels (equals the bitmap dimensions).
    pub size: [u32; 2],
    /// Normalized atlas coordinates `[u0, v0, u1, v1]`; zeros when inline.
    pub uv: [f32; 4],
    /// Tint color (straight alpha).
    pub color: Rgba8,
    /// Texel source.
    pub source: GlyphSource,
}

/// Owned record of everything one frame needs to draw.
///
/// Produced by [`GridRenderer::render`]; consumed by a GPU backend seam or,
/// under the `sw-fallback` feature, by
/// [`crate::software::draw_list_onto`]. Paint order is fills first, then
/// glyphs; each vector preserves cell scan order so identical inputs give
/// byte-identical records.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawList {
    /// Snapshot generation this list was built from.
    pub generation: u64,
    /// The frame plan that drove cell selection.
    pub plan: FramePlan,
    /// Background and decoration rectangles.
    pub fills: Vec<FillRect>,
    /// Glyph instances.
    pub glyphs: Vec<GlyphInstance>,
}

impl DrawList {
    /// True when the frame carries any drawing work.
    #[must_use]
    pub fn needs_draw(&self) -> bool {
        !self.fills.is_empty() || !self.glyphs.is_empty()
    }
}

/// Monotone pipeline counters (see the module-level budget discussion).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderCounters {
    /// Frames planned, including clean ones.
    pub frames_planned: u64,
    /// Cells visited because a dirty rectangle covered them.
    pub cells_examined: u64,
    /// Visited cells that emitted a glyph or decoration.
    pub cells_drawn: u64,
    /// Spacer (trailing wide-half) cells skipped.
    pub spacer_cells_skipped: u64,
    /// Cells whose character produced no drawable bitmap.
    pub blank_cells_skipped: u64,
    /// Cells whose glyph was suppressed by the invisible attribute.
    pub invisible_cells_skipped: u64,
    /// Background rectangles emitted (exactly one per visited cell).
    pub background_fills: u64,
    /// Decoration rectangles emitted (underline/strikethrough bars).
    pub decorations_emitted: u64,
    /// Glyph instances emitted.
    pub glyphs_emitted: u64,
}

/// One queued texture upload: coverage bytes for a freshly allocated slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasUpload {
    /// Destination region inside the atlas texture.
    pub slot: AtlasSlot,
    /// Coverage bytes, row-major, length `slot.width * slot.height`.
    pub data: Vec<u8>,
}

/// Shelf-packed glyph atlas plus its CPU-side coverage texture and upload
/// queue.
///
/// Eviction policy mirrors [`GlyphCache`]: when a fresh allocation does not
/// fit, the layout, slot table, texture, and pending queue are reset
/// wholesale (one counted eviction) and the allocation retries once.
/// Bitmaps larger than the whole atlas fall back to [`GlyphSource::Inline`]
/// instead of failing the frame. All outcomes are deterministic functions
/// of the insertion sequence.
#[derive(Debug)]
pub struct GlyphAtlas {
    layout: AtlasLayout,
    slots: HashMap<RasterKey, AtlasSlot>,
    texels: Vec<u8>,
    pending: Vec<AtlasUpload>,
    hits: u64,
    misses: u64,
    evictions: u64,
    inline_fallbacks: u64,
}

impl GlyphAtlas {
    /// Creates an empty square atlas of side `dimension`.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when `dimension` is zero.
    pub fn new(dimension: u16) -> Result<Self, RenderError> {
        Self::with_dims(dimension, dimension)
    }

    /// Creates an empty atlas with explicit dimensions.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero.
    pub fn with_dims(width: u16, height: u16) -> Result<Self, RenderError> {
        let layout = AtlasLayout::new(width, height)?;
        let len = usize::from(width) * usize::from(height);
        Ok(Self {
            layout,
            slots: HashMap::new(),
            texels: vec![0; len],
            pending: Vec::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
            inline_fallbacks: 0,
        })
    }

    /// Atlas texture dimensions.
    #[must_use]
    pub const fn dims(&self) -> AtlasDims {
        self.layout.dimensions()
    }

    /// Read-only view of the coverage texture (one byte per texel).
    #[must_use]
    pub fn texels(&self) -> &[u8] {
        &self.texels
    }

    /// Number of placements currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True when no placement is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Fraction of atlas texels covered by issued slots.
    #[must_use]
    pub fn occupancy(&self) -> f64 {
        self.layout.occupancy()
    }

    /// Cumulative lookups served by an existing placement.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// Cumulative lookups that had to place (and queue an upload for) a new
    /// bitmap.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// Cumulative wholesale resets triggered by exhaustion.
    #[must_use]
    pub const fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Cumulative bitmaps that could not fit even after an eviction.
    #[must_use]
    pub const fn inline_fallbacks(&self) -> u64 {
        self.inline_fallbacks
    }

    /// Returns the slot previously issued for `key`, if any.
    #[must_use]
    pub fn slot(&self, key: &RasterKey) -> Option<AtlasSlot> {
        self.slots.get(key).copied()
    }

    /// Drains the pending upload queue (a backend copies these into its own
    /// texture, then drops them).
    pub fn take_pending_uploads(&mut self) -> Vec<AtlasUpload> {
        std::mem::take(&mut self.pending)
    }

    /// Ensures `bitmap` is placed for `key`, queueing an upload when newly
    /// placed. Blank bitmaps are refused by callers before reaching the
    /// atlas.
    pub fn ensure(&mut self, key: RasterKey, bitmap: &GlyphBitmap) -> GlyphSource {
        if let Some(slot) = self.slots.get(&key) {
            self.hits += 1;
            return GlyphSource::Atlas { slot: *slot };
        }
        self.misses += 1;

        let width = u16::try_from(bitmap.metrics.width.max(0));
        let height = u16::try_from(bitmap.metrics.height.max(0));
        let (width, height) = match (width, height) {
            (Ok(w), Ok(h)) => (w, h),
            _ => return self.fallback_inline(bitmap),
        };

        let slot = match self.layout.allocate(width, height) {
            Some(slot) => slot,
            None => {
                // Wholesale eviction (deterministic), then retry once.
                self.evict_all();
                match self.layout.allocate(width, height) {
                    Some(slot) => slot,
                    None => return self.fallback_inline(bitmap),
                }
            }
        };

        let coverage = coverage_mask(bitmap);
        write_slot_texels(&mut self.texels, self.layout.dimensions(), slot, &coverage);
        self.pending.push(AtlasUpload {
            slot,
            data: coverage,
        });
        self.slots.insert(key, slot);
        GlyphSource::Atlas { slot }
    }

    fn evict_all(&mut self) {
        self.layout.reset();
        self.slots.clear();
        self.texels.fill(0);
        self.pending.clear();
        self.evictions += 1;
    }

    fn fallback_inline(&mut self, bitmap: &GlyphBitmap) -> GlyphSource {
        self.inline_fallbacks += 1;
        GlyphSource::Inline {
            width: saturating_u32(u64::try_from(bitmap.metrics.width.max(0)).unwrap_or(u64::MAX)),
            height: saturating_u32(u64::try_from(bitmap.metrics.height.max(0)).unwrap_or(u64::MAX)),
            mask: coverage_mask(bitmap),
        }
    }
}

/// Flattens any supported bitmap format to one coverage byte per texel.
///
/// RGB coverage is averaged to luminance (identical rule to
/// [`crate::software::SurfaceRgba::blend_glyph`]); RGBA sources contribute
/// their alpha channel (upstream RGBA is premultiplied, so alpha *is*
/// coverage).
fn coverage_mask(bitmap: &GlyphBitmap) -> Vec<u8> {
    let GlyphMetrics { width, height, .. } = bitmap.metrics;
    let mut mask = Vec::with_capacity(width.max(0) as usize * height.max(0) as usize);
    match bitmap.format {
        BitmapFormat::Rgb => {
            for px in bitmap.data.chunks_exact(3) {
                let lum = (u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3;
                mask.push(lum as u8);
            }
        }
        BitmapFormat::Rgba => {
            for px in bitmap.data.chunks_exact(4) {
                mask.push(px[3]);
            }
        }
    }
    mask
}

/// Writes a coverage mask into the atlas texture at `slot`.
fn write_slot_texels(texels: &mut [u8], dims: AtlasDims, slot: AtlasSlot, mask: &[u8]) {
    let stride = usize::from(dims.width);
    let slot_w = usize::from(slot.width);
    for row in 0..usize::from(slot.height) {
        let src = row * slot_w;
        let dst = (usize::from(slot.y) + row) * stride + usize::from(slot.x);
        texels[dst..dst + slot_w].copy_from_slice(&mask[src..src + slot_w]);
    }
}

/// Renders terminal snapshots through the shared pipeline: plan, place,
/// cache, emit.
///
/// Generic over the rasterizer so headless tests drive the identical path
/// the crossfont-backed renderer uses (terminal-state-rfc testing strategy:
/// the software fallback must exercise the production pipeline, not a
/// parallel one).
#[derive(Debug)]
pub struct GridRenderer<R: GlyphRasterizer> {
    cache: GlyphCache<R>,
    atlas: GlyphAtlas,
    font: FontId,
    point_size: f32,
    cell: CellMetrics,
    counters: RenderCounters,
}

impl<R: GlyphRasterizer> GridRenderer<R> {
    /// Builds a renderer around `rasterizer`, loading `query` and using the
    /// default atlas dimension.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] for invalid queries or zero cell
    /// metrics; whatever the rasterizer reports for font loading.
    pub fn new(rasterizer: R, query: &FontQuery, cell: CellMetrics) -> Result<Self, RenderError> {
        Self::with_atlas_dimension(rasterizer, query, cell, DEFAULT_ATLAS_DIMENSION)
    }

    /// As [`GridRenderer::new`] with an explicit atlas side length (tests
    /// use small values to exercise eviction deterministically).
    ///
    /// # Errors
    ///
    /// Same as [`GridRenderer::new`], plus invalid atlas dimensions.
    pub fn with_atlas_dimension(
        rasterizer: R,
        query: &FontQuery,
        cell: CellMetrics,
        dimension: u16,
    ) -> Result<Self, RenderError> {
        query.validate()?;
        let mut cache = GlyphCache::new(rasterizer, crate::cache::DEFAULT_GLYPH_CACHE_CAPACITY)?;
        let font = cache.load_font(query)?;
        Ok(Self {
            cache,
            atlas: GlyphAtlas::new(dimension)?,
            font,
            point_size: query.point_size,
            cell,
            counters: RenderCounters::default(),
        })
    }

    /// The configured cell metrics.
    #[must_use]
    pub const fn cell_metrics(&self) -> CellMetrics {
        self.cell
    }

    /// Point-in-time copy of the renderer-side counters.
    #[must_use]
    pub const fn counters(&self) -> RenderCounters {
        self.counters
    }

    /// Glyph-cache `(hits, misses)` totals ("glyphs rasterized" is exactly
    /// the miss count).
    #[must_use]
    pub const fn cache_stats(&self) -> (u64, u64) {
        (self.cache.hits(), self.cache.misses())
    }

    /// Atlas `(hits, misses, evictions, inline_fallbacks)` totals.
    #[must_use]
    pub const fn atlas_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.atlas.hits(),
            self.atlas.misses(),
            self.atlas.evictions(),
            self.atlas.inline_fallbacks(),
        )
    }

    /// Number of glyph placements currently held by the atlas.
    #[must_use]
    pub fn atlas_placements(&self) -> usize {
        self.atlas.len()
    }

    /// Drainable atlas upload queue for the backend seam. Draining does not
    /// invalidate the CPU-side texture ([`Self::atlas_texels`] stays
    /// complete), so software compositing never depends on the drain.
    pub fn take_atlas_uploads(&mut self) -> Vec<AtlasUpload> {
        self.atlas.take_pending_uploads()
    }

    /// Read-only atlas texture (coverage bytes) for software compositing.
    #[must_use]
    pub fn atlas_texels(&self) -> &[u8] {
        self.atlas.texels()
    }

    /// Atlas texture dimensions.
    #[must_use]
    pub const fn atlas_dims(&self) -> AtlasDims {
        self.atlas.dims()
    }

    /// Plans and records one frame from `snapshot` limited by `damage`.
    ///
    /// Deterministic: identical `(snapshot, damage, font, insertion order)`
    /// inputs yield identical [`DrawList`] values — ordering follows the
    /// plan's dirty rectangles then row-major cell scan, floats come only
    /// from integer slot divisions, and no timing or randomness
    /// participates. Errors mean the frame failed outright; partial frames
    /// are never returned.
    ///
    /// # Errors
    ///
    /// Propagates rasterizer failures (never cached by [`GlyphCache`]).
    pub fn render(
        &mut self,
        snapshot: &Snapshot,
        damage: &Damage,
    ) -> Result<DrawList, RenderError> {
        self.counters.frames_planned += 1;

        let descriptor = SnapshotDamage::new(snapshot, damage, self.cell);
        let plan = plan_frame(&descriptor);

        let mut list = DrawList {
            generation: snapshot.generation,
            fills: Vec::new(),
            glyphs: Vec::new(),
            plan,
        };
        if !list.plan.needs_draw() {
            return Ok(list);
        }

        // Coalesced dirty rectangles are pairwise disjoint (frame.rs
        // invariant), so their cell ranges never overlap and each cell is
        // visited at most once, in row-major order per rectangle.
        let cols = snapshot.width;
        // Clone the plan's rectangles so cells can be placed while the list
        // is being built (plan output is tiny and bounded).
        let dirty_rects = list.plan.dirty_rects.clone();
        for dirty in &dirty_rects {
            let col_range = pixel_span_to_cells(dirty.x, dirty.width, self.cell.width);
            let row_range = pixel_span_to_cells(dirty.y, dirty.height, self.cell.height);
            for row in row_range {
                for col in col_range.clone() {
                    let Some(term_cell) = snapshot.cells.get(row * cols + col) else {
                        // Defensive against malformed snapshots; skipping a
                        // nonexistent cell can only under-*draw* a region
                        // that cannot exist, never corrupt real cells.
                        continue;
                    };
                    self.place_cell(term_cell, row, col, cols, &mut list);
                }
            }
        }
        Ok(list)
    }

    /// Emits background, decorations, and (unless suppressed) one glyph for
    /// a single visited cell.
    fn place_cell(
        &mut self,
        term_cell: &bitty_term_state::Cell,
        row: usize,
        col: usize,
        grid_width: usize,
        list: &mut DrawList,
    ) {
        self.counters.cells_examined += 1;

        let (fg, bg) = resolved_colors(&term_cell.style);
        let left = u64::try_from(col)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.width));
        let top = u64::try_from(row)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.height));

        // Span in columns: wide cells cover their spacer column too, while
        // spacers paint only their own half (the leading half already
        // covers both). The span clamps at the grid edge so a trailing wide
        // cell cannot paint past the extent (defensive; term-state forbids
        // orphan spacers).
        let span_cols = if term_cell.spacer {
            1
        } else {
            usize::from(term_cell.width).max(1).min(grid_width - col)
        };

        // Every visited cell repaints its background: the union of
        // incremental frames equals a full redraw (module docs).
        list.fills.push(FillRect {
            rect: RectPx::new(
                saturating_i32(left),
                saturating_i32(top),
                saturating_u32(
                    u64::try_from(span_cols)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(u64::from(self.cell.width)),
                ),
                self.cell.height,
            ),
            color: bg,
        });
        self.counters.background_fills += 1;

        if term_cell.spacer {
            self.counters.spacer_cells_skipped += 1;
            return;
        }

        let decorations = self.emit_decorations(term_cell, row, col, span_cols, fg, list);
        if term_cell.style.attributes.invisible {
            self.counters.invisible_cells_skipped += 1;
        } else if term_cell.glyph != ' ' {
            self.emit_glyph(term_cell.glyph, row, col, fg, list);
        } else if decorations == 0 {
            self.counters.blank_cells_skipped += 1;
            return;
        }
        self.counters.cells_drawn += 1;
    }

    /// Emits underline/strikethrough bars; returns how many were pushed.
    fn emit_decorations(
        &mut self,
        term_cell: &bitty_term_state::Cell,
        row: usize,
        col: usize,
        span_cols: usize,
        fg: Rgba8,
        list: &mut DrawList,
    ) -> u64 {
        use bitty_term_state::UnderlineStyle;

        let thickness = (self.cell.height / 8).clamp(1, 2);
        let left = u64::try_from(col)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.width));
        let top = u64::try_from(row)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.height));
        let span_w = saturating_u32(
            u64::try_from(span_cols)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::from(self.cell.width)),
        );
        let underline_color = resolve_color(term_cell.style.underline_color.as_ref(), fg);

        // Curly/dotted/dashed shapes approximate to solid bars until the
        // text RFC defines decorated-glyph rendering; geometry stays
        // deterministic regardless of style.
        let underline_rows: &[u32] = match term_cell.style.attributes.underline {
            UnderlineStyle::None => &[],
            UnderlineStyle::Single
            | UnderlineStyle::Curly
            | UnderlineStyle::Dotted
            | UnderlineStyle::Dashed => &[self.cell.height.saturating_sub(thickness * 2)],
            UnderlineStyle::Double => &[
                self.cell.height.saturating_sub(thickness * 2),
                self.cell.height.saturating_sub(thickness * 4),
            ],
        };

        let mut pushed = 0u64;
        for offset in underline_rows {
            list.fills.push(FillRect {
                rect: RectPx::new(
                    saturating_i32(left),
                    saturating_i32(top + u64::from(*offset)),
                    span_w,
                    thickness,
                ),
                color: underline_color,
            });
            pushed += 1;
        }

        if term_cell.style.attributes.strikethrough {
            let mid = top + u64::from(self.cell.height) / 2;
            let y = mid.saturating_sub(u64::from(thickness) / 2);
            list.fills.push(FillRect {
                rect: RectPx::new(saturating_i32(left), saturating_i32(y), span_w, thickness),
                color: fg,
            });
            pushed += 1;
        }

        self.counters.decorations_emitted += pushed;
        pushed
    }

    /// Looks up, caches, atlas-uploads, and emits one glyph instance.
    fn emit_glyph(
        &mut self,
        character: char,
        row: usize,
        col: usize,
        color: Rgba8,
        list: &mut DrawList,
    ) {
        let Ok(key) = RasterKey::new(character, self.font, self.point_size) else {
            // The point size was validated at construction, so this is
            // unreachable; treating it as "nothing to draw" stays safe.
            self.counters.blank_cells_skipped += 1;
            return;
        };
        let bitmap = match self.cache.glyph(key) {
            Ok(CachedGlyph::Bitmap(bitmap)) => bitmap,
            Ok(CachedGlyph::Blank) => {
                // Cached negative (whitespace/missing glyph): background
                // and decorations already painted.
                self.counters.blank_cells_skipped += 1;
                return;
            }
            Err(_) => {
                // Rasterizer failure: skip this glyph instead of failing
                // the whole frame. The failure stays observable through
                // GlyphCache miss counters and upstream diagnostics.
                self.counters.blank_cells_skipped += 1;
                return;
            }
        };

        let metrics = bitmap.metrics;
        let source = self.atlas.ensure(key, bitmap);
        let dest_x = col as i64 * i64::from(self.cell.width) + i64::from(metrics.left);
        let baseline = row as i64 * i64::from(self.cell.height)
            + i64::from(self.cell.height) * i64::from(BASELINE_NUMERATOR) / 4;
        let dest_y = baseline - i64::from(metrics.top);

        let instance = match source {
            GlyphSource::Atlas { slot } => GlyphInstance {
                dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                size: [
                    saturating_u32(u64::try_from(metrics.width.max(0)).unwrap_or(u64::MAX)),
                    saturating_u32(u64::try_from(metrics.height.max(0)).unwrap_or(u64::MAX)),
                ],
                uv: slot.uv(self.atlas.dims()),
                color,
                source: GlyphSource::Atlas { slot },
            },
            GlyphSource::Inline {
                mask,
                width,
                height,
            } => GlyphInstance {
                dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                size: [width, height],
                uv: [0.0; 4],
                color,
                source: GlyphSource::Inline {
                    mask,
                    width,
                    height,
                },
            },
        };
        list.glyphs.push(instance);
        self.counters.glyphs_emitted += 1;
    }
}

/// Converts a pixel span back to the half-open range of cells it covers.
///
/// Dirty rectangles are cell-aligned by construction (grid damage
/// rectangles scale by the same cell metrics that built the extent), so
/// ranges are exact; the dividing ceiling tolerates foreign descriptors.
fn pixel_span_to_cells(offset: i32, span: u32, cell_side: u32) -> std::ops::Range<usize> {
    let side = i64::from(cell_side);
    let start = i64::from(offset).max(0) / side;
    let covered = i64::from(offset).max(0) + i64::from(span);
    let end = (covered + side - 1) / side;
    let to_usize = |v: i64| usize::try_from(v).unwrap_or(usize::MAX);
    to_usize(start)..to_usize(end)
}

#[cfg(feature = "sw-fallback")]
mod sw {
    //! Headless end-to-end entry point: the SAME plan/place/cache pipeline,
    //! composited onto a [`SurfaceRgba`] by the software backend.

    use bitty_term_state::{Damage, Snapshot};

    use super::{CellMetrics, DrawList, GridRenderer, Rgba8};
    use crate::error::RenderError;
    use crate::glyph::GlyphRasterizer;
    use crate::software::{SurfaceRgba, draw_list_onto};

    /// Pixel extent of the surface covering a whole snapshot.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the resulting extent exceeds the
    /// software surface byte cap or a dimension collapses to zero.
    pub fn surface_extent(
        snapshot: &Snapshot,
        cell: CellMetrics,
    ) -> Result<(u32, u32), RenderError> {
        let extent = cell.extent_for(snapshot.width, snapshot.height);
        if extent.is_zero() {
            return Err(RenderError::InvalidInput {
                reason: "snapshot grid collapses to an empty surface",
            });
        }
        let bytes = u64::from(extent.width) * u64::from(extent.height) * 4;
        if bytes > crate::software::MAX_SURFACE_BYTES as u64 {
            return Err(RenderError::InvalidInput {
                reason: "snapshot surface exceeds the configured byte cap",
            });
        }
        Ok((extent.width, extent.height))
    }

    /// Composites one frame's [`DrawList`] onto `surface` using the
    /// renderer's live atlas texture.
    ///
    /// # Errors
    ///
    /// Propagates compositing failures from [`draw_list_onto`].
    pub fn composite_frame<R: GlyphRasterizer>(
        renderer: &GridRenderer<R>,
        list: &DrawList,
        surface: &mut SurfaceRgba,
    ) -> Result<(), RenderError> {
        draw_list_onto(
            list,
            Some((renderer.atlas_texels(), renderer.atlas_dims())),
            surface,
        )
    }

    /// Convenience for tests and tools: renders `snapshot`/`damage` through
    /// the full pipeline and returns a freshly cleared surface holding only
    /// this frame's damage. Incremental consumers should keep one surface
    /// across frames and call [`composite_frame`] per frame instead.
    ///
    /// # Errors
    ///
    /// Propagates rendering and compositing failures.
    pub fn render_snapshot_to_surface<R: GlyphRasterizer>(
        renderer: &mut GridRenderer<R>,
        snapshot: &Snapshot,
        damage: &Damage,
        background: Rgba8,
    ) -> Result<SurfaceRgba, RenderError> {
        let (width, height) = {
            let cell = renderer.cell_metrics();
            // Borrow of renderer ends here; render below needs &mut.
            let (w, h) = surface_extent(snapshot, cell)?;
            (w, h)
        };
        let list = renderer.render(snapshot, damage)?;
        let mut surface = SurfaceRgba::try_new(width, height)?;
        surface.clear(background);
        composite_frame(renderer, &list, &mut surface)?;
        Ok(surface)
    }
}

#[cfg(feature = "sw-fallback")]
pub use sw::{composite_frame, render_snapshot_to_surface, surface_extent};

#[cfg(test)]
mod tests;

/// Clamps an `i64` coordinate back into `i32` range (destinations may be
/// negative from glyph bearings; extremes stay overflow-free).
const fn clamp_i32(value: i64) -> i32 {
    const MAX: i64 = i32::MAX as i64;
    const MIN: i64 = i32::MIN as i64;
    if value > MAX {
        i32::MAX
    } else if value < MIN {
        i32::MIN
    } else {
        value as i32
    }
}
