//! CPU batch translation for GPU presentation.
//!
//! This module turns an owned [`DrawList`](crate::grid::DrawList) into
//! bounded, pre-serialized vertex batches plus atlas-upload bookkeeping. It
//! is pure CPU math with no `wgpu` dependency, so every item here runs on
//! headless CI without a GPU, window, or filesystem. The `wgpu`-dependent
//! half lives in [`crate::pipeline`] and consumes exactly these batches.
//!
//! # Coordinate systems
//!
//! [`DrawList`](crate::grid::DrawList) destinations are in render pixels at
//! the grid's cell metrics. The swap-chain surface is in physical pixels.
//! On a HiDPI output the surface is larger than the plan extent by the scale
//! factor; [`derive_scale`] recovers that factor per frame from
//! `surface_extent / plan_extent` so no API change is needed in the runtime
//! (which owns the scale factor but does not pass it to `present`). Vertices
//! are emitted directly in normalized device coordinates (NDC), so the
//! shaders need no uniform for resolution.
//!
//! # Bounds
//!
//! All outputs are chunked: [`MAX_FILL_QUADS_PER_BATCH`] and
//! [`MAX_GLYPH_QUADS_PER_BATCH`] bound every vertex buffer upload, and
//! [`MAX_ATLAS_DIMENSION`] bounds atlas textures. Functions that would
//! exceed a cap return `None`/`Err` (fail-closed) instead of growing
//! without limit; callers draw chunk after chunk reusing the same bounded
//! buffers (see [`chunk_fills`], [`chunk_atlas_glyphs`]).
//!
//! # Atlas uploads
//!
//! The CPU atlas (`GlyphAtlas` texels + [`AtlasDims`]) is the source of
//! truth. [`compute_atlas_dirty`] diffs the last uploaded texels against the
//! new ones and reports [`AtlasDirty::Clean`] (skip the upload),
//! [`AtlasDirty::Full`] (first upload, dimension change, or length
//! mismatch), or a full-width [`AtlasDirty::Strip`] covering the first to
//! the last differing row. Strips stay full-width so
//! `bytes_per_row` keeps the 256-byte `COPY_BYTES_PER_ROW_ALIGNMENT`
//! required by `queue.write_texture` (see [`padded_bytes_per_row`]).
//! Inline glyphs (atlas overflow fallback) are packed per frame into a
//! fixed [`INLINE_TEXTURE_SIZE`] texture by [`pack_inline_glyphs`].

use crate::atlas::AtlasDims;
use crate::error::RenderError;
use crate::geometry::{ExtentPx, RectPx};
use crate::grid::{FillRect, GlyphInstance, GlyphSource, Rgba8};

/// Maximum fill quads per vertex-buffer upload chunk.
///
/// 4096 quads x 4 vertices x 24 bytes = 384 KiB per chunk, reused across
/// chunks within one frame so arbitrarily large `fills` lists (for example
/// a fullscreen 29k-cell window) draw correctly without unbounded buffers.
pub const MAX_FILL_QUADS_PER_BATCH: usize = 4096;

/// Maximum glyph quads per vertex-buffer upload chunk.
///
/// 4096 quads x 4 vertices x 32 bytes = 512 KiB per chunk, reused across
/// chunks the same way as fills.
pub const MAX_GLYPH_QUADS_PER_BATCH: usize = 4096;

/// Maximum inline glyphs packed per frame into the transient texture.
///
/// Bounds CPU packing work and the transient texture content. Excess inline
/// glyphs are skipped and counted ([`InlinePlan::skipped`]), never grown
/// past this cap.
pub const MAX_INLINE_GLYPHS_PER_FRAME: usize = 2048;

/// Largest atlas side accepted for GPU upload (both dimensions).
///
/// The default atlas is 2048; 4096 keeps a single R8 texture at 16 MiB.
/// Larger dimensions are rejected with [`RenderError::InvalidInput`]
/// (fail-closed) rather than attempting an oversized texture.
pub const MAX_ATLAS_DIMENSION: u16 = 4096;

/// Side length of the fixed transient inline texture (R8, square).
///
/// 1024 x 1024 x 1 byte = 1 MiB, uploaded only on frames that actually
/// contain inline glyphs (rare: only when the persistent atlas overflowed).
pub const INLINE_TEXTURE_SIZE: u32 = 1024;

/// Required `bytes_per_row` alignment for texture uploads.
pub const COPY_BYTES_PER_ROW_ALIGNMENT: u32 = 256;

/// Bytes per fill vertex: `pos: vec2<f32>` + `color: vec4<f32>`.
pub const FILL_VERTEX_SIZE_BYTES: usize = 24;

/// Bytes per glyph vertex: `pos: vec2<f32>` + `uv: vec2<f32>` +
/// `color: vec4<f32>`.
pub const GLYPH_VERTEX_SIZE_BYTES: usize = 32;

/// Vertices per quad and indices per quad (two triangles).
pub const VERTICES_PER_QUAD: usize = 4;
/// Indices per quad (two triangles).
pub const INDICES_PER_QUAD: usize = 6;

/// Converts straight-alpha `Rgba8` bytes to normalized shader floats.
#[must_use]
pub fn rgba8_to_float4(color: Rgba8) -> [f32; 4] {
    [
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        f32::from(color[3]) / 255.0,
    ]
}

/// Maps a pixel position to normalized device coordinates.
///
/// `w`/`h` are the physical surface dimensions (non-zero; callers skip
/// drawing when the surface is zero-sized). The y axis points down in
/// pixels and up in NDC, hence the flip.
#[must_use]
pub fn pixel_to_ndc(x: f32, y: f32, w: f32, h: f32) -> [f32; 2] {
    if w <= 0.0 || h <= 0.0 {
        return [-1.0, 1.0];
    }
    [(x / w) * 2.0 - 1.0, 1.0 - (y / h) * 2.0]
}

/// Recovers the DPI scale factor for one frame.
///
/// The runtime builds the [`DrawList`](crate::grid::DrawList) at cell
/// metrics while the surface extent is physical pixels, so
/// `surface / plan` is the effective scale (1.0 on standard displays).
/// Returns 1.0 when the plan extent is zero (no information) and clamps to
/// `[0.25, 4.0]` so stale or hostile extents cannot produce degenerate or
/// gigantic vertices.
#[must_use]
pub fn derive_scale(surface_w: u32, surface_h: u32, plan: ExtentPx) -> f32 {
    if plan.width == 0 || plan.height == 0 {
        return 1.0;
    }
    let sx = surface_w as f32 / plan.width as f32;
    let sy = surface_h as f32 / plan.height as f32;
    ((sx + sy) * 0.5).clamp(0.25, 4.0)
}

/// Validates atlas dimensions for GPU upload.
///
/// # Errors
///
/// [`RenderError::InvalidInput`] when either dimension is zero or exceeds
/// [`MAX_ATLAS_DIMENSION`].
pub fn validate_atlas_dims(dims: AtlasDims) -> Result<(), RenderError> {
    if dims.width == 0 || dims.height == 0 {
        return Err(RenderError::InvalidInput {
            reason: "atlas dimensions must be non-zero",
        });
    }
    if dims.width > MAX_ATLAS_DIMENSION || dims.height > MAX_ATLAS_DIMENSION {
        return Err(RenderError::InvalidInput {
            reason: "atlas dimensions exceed the GPU upload cap",
        });
    }
    Ok(())
}

/// One chunk of fill quads: pre-serialized little-endian vertex bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillChunk {
    /// Number of quads in this chunk (`<= MAX_FILL_QUADS_PER_BATCH`).
    pub quad_count: usize,
    /// `quad_count * 4 * FILL_VERTEX_SIZE_BYTES` vertex bytes.
    pub bytes: Vec<u8>,
}

/// One chunk of atlas-textured glyph quads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphChunk {
    /// Number of quads in this chunk (`<= MAX_GLYPH_QUADS_PER_BATCH`).
    pub quad_count: usize,
    /// `quad_count * 4 * GLYPH_VERTEX_SIZE_BYTES` vertex bytes.
    pub bytes: Vec<u8>,
}

/// Serializes one fill quad (4 vertices, 96 bytes) or `None` when the rect
/// is empty. Off-surface quads are still emitted (the GPU clips them); only
/// empty rects are skipped.
fn fill_quad_bytes(
    rect: RectPx,
    color: Rgba8,
    surface_w: u32,
    surface_h: u32,
    scale: f32,
) -> Option<[u8; 96]> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let w = surface_w as f32;
    let h = surface_h as f32;
    let c = rgba8_to_float4(color);
    let x0 = rect.x as f32 * scale;
    let y0 = rect.y as f32 * scale;
    let x1 = (rect.x as f32 + rect.width as f32) * scale;
    let y1 = (rect.y as f32 + rect.height as f32) * scale;
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    let mut out = [0u8; 96];
    for (i, (px, py)) in corners.iter().enumerate() {
        let ndc = pixel_to_ndc(*px, *py, w, h);
        let base = i * FILL_VERTEX_SIZE_BYTES;
        out[base..base + 4].copy_from_slice(&ndc[0].to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&ndc[1].to_le_bytes());
        for (k, v) in c.iter().enumerate() {
            out[base + 8 + k * 4..base + 12 + k * 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    Some(out)
}

/// Serializes one glyph quad (4 vertices, 128 bytes) or `None` when the
/// glyph has a zero size.
fn glyph_quad_bytes(
    dest: [i32; 2],
    size: [u32; 2],
    uv: [f32; 4],
    color: Rgba8,
    surface_w: u32,
    surface_h: u32,
    scale: f32,
) -> Option<[u8; 128]> {
    if size[0] == 0 || size[1] == 0 {
        return None;
    }
    let w = surface_w as f32;
    let h = surface_h as f32;
    let c = rgba8_to_float4(color);
    let x0 = dest[0] as f32 * scale;
    let y0 = dest[1] as f32 * scale;
    let x1 = (dest[0] as f32 + size[0] as f32) * scale;
    let y1 = (dest[1] as f32 + size[1] as f32) * scale;
    // uv is [u0, v0, u1, v1]; corners match the fill winding.
    let corners = [
        (x0, y0, uv[0], uv[1]),
        (x1, y0, uv[2], uv[1]),
        (x1, y1, uv[2], uv[3]),
        (x0, y1, uv[0], uv[3]),
    ];
    let mut out = [0u8; 128];
    for (i, (px, py, u, v)) in corners.iter().enumerate() {
        let ndc = pixel_to_ndc(*px, *py, w, h);
        let base = i * GLYPH_VERTEX_SIZE_BYTES;
        out[base..base + 4].copy_from_slice(&ndc[0].to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&ndc[1].to_le_bytes());
        out[base + 8..base + 12].copy_from_slice(&u.to_le_bytes());
        out[base + 12..base + 16].copy_from_slice(&v.to_le_bytes());
        for (k, ch) in c.iter().enumerate() {
            out[base + 16 + k * 4..base + 20 + k * 4].copy_from_slice(&ch.to_le_bytes());
        }
    }
    Some(out)
}

/// Translates fills into bounded serialized chunks.
///
/// Empty rects are skipped. An empty input (or a zero-sized surface) yields
/// no chunks. Chunk order preserves input order, so frames stay
/// deterministic.
#[must_use]
pub fn chunk_fills(
    fills: &[FillRect],
    surface_w: u32,
    surface_h: u32,
    scale: f32,
) -> Vec<FillChunk> {
    if surface_w == 0 || surface_h == 0 || fills.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut count = 0usize;
    for fill in fills {
        let Some(quad) = fill_quad_bytes(fill.rect, fill.color, surface_w, surface_h, scale) else {
            continue;
        };
        current.extend_from_slice(&quad);
        count += 1;
        if count >= MAX_FILL_QUADS_PER_BATCH {
            chunks.push(FillChunk {
                quad_count: count,
                bytes: std::mem::take(&mut current),
            });
            count = 0;
        }
    }
    if count > 0 {
        chunks.push(FillChunk {
            quad_count: count,
            bytes: current,
        });
    }
    chunks
}

/// Translates atlas-sourced glyphs into bounded serialized chunks.
///
/// Inline-sourced glyphs are skipped here (see [`pack_inline_glyphs`]);
/// zero-sized glyphs are skipped. Order preserves input order.
#[must_use]
pub fn chunk_atlas_glyphs(
    glyphs: &[GlyphInstance],
    surface_w: u32,
    surface_h: u32,
    scale: f32,
) -> Vec<GlyphChunk> {
    if surface_w == 0 || surface_h == 0 || glyphs.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut count = 0usize;
    for glyph in glyphs {
        if !matches!(glyph.source, GlyphSource::Atlas { .. }) {
            continue;
        }
        let Some(quad) = glyph_quad_bytes(
            glyph.dest,
            glyph.size,
            glyph.uv,
            glyph.color,
            surface_w,
            surface_h,
            scale,
        ) else {
            continue;
        };
        current.extend_from_slice(&quad);
        count += 1;
        if count >= MAX_GLYPH_QUADS_PER_BATCH {
            chunks.push(GlyphChunk {
                quad_count: count,
                bytes: std::mem::take(&mut current),
            });
            count = 0;
        }
    }
    if count > 0 {
        chunks.push(GlyphChunk {
            quad_count: count,
            bytes: current,
        });
    }
    chunks
}

/// One inline glyph placed in the transient texture.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineGlyph {
    /// Destination top-left in render pixels (unscaled; scaled at upload).
    pub dest: [i32; 2],
    /// Size in texels.
    pub size: [u32; 2],
    /// Normalized UVs into the transient texture (`[u0, v0, u1, v1]`).
    pub uv: [f32; 4],
    /// Tint color (straight alpha).
    pub color: Rgba8,
}

/// Per-frame packing of inline glyph masks into a fixed transient texture.
#[derive(Debug, Clone, PartialEq)]
pub struct InlinePlan {
    /// Transient texture width ([`INLINE_TEXTURE_SIZE`]).
    pub width: u32,
    /// Transient texture height ([`INLINE_TEXTURE_SIZE`]).
    pub height: u32,
    /// Coverage texels, row-major, `width * height` bytes.
    pub texels: Vec<u8>,
    /// Placed glyphs with UVs into `texels`.
    pub placements: Vec<InlineGlyph>,
    /// Glyphs that could not be packed (too large or texture exhausted).
    /// They are skipped for this frame (fail-closed, counted honestly).
    pub skipped: usize,
}

/// Packs inline glyph masks into a fixed transient texture.
///
/// Deterministic shelf packing in input order, capped at
/// [`MAX_INLINE_GLYPHS_PER_FRAME`] examined glyphs. Malformed masks
/// (length mismatch), zero sizes, glyphs larger than the whole texture, and
/// placements past exhaustion are skipped and counted in
/// [`InlinePlan::skipped`], never grown past the fixed texture.
#[must_use]
pub fn pack_inline_glyphs(glyphs: &[GlyphInstance]) -> InlinePlan {
    let side = INLINE_TEXTURE_SIZE;
    let mut texels = vec![0u8; (side * side) as usize];
    let mut placements = Vec::new();
    let mut skipped = 0usize;
    let mut cursor_x: u32 = 0;
    let mut shelf_top: u32 = 0;
    let mut shelf_h: u32 = 0;
    let mut examined = 0usize;

    for glyph in glyphs {
        let GlyphSource::Inline {
            mask,
            width,
            height,
        } = &glyph.source
        else {
            continue;
        };
        if examined >= MAX_INLINE_GLYPHS_PER_FRAME {
            skipped += 1;
            continue;
        }
        examined += 1;
        let w = *width;
        let h = *height;
        if w == 0 || h == 0 || w > side || h > side {
            skipped += 1;
            continue;
        }
        if mask.len() != w as usize * h as usize {
            skipped += 1;
            continue;
        }
        if glyph.size[0] == 0 || glyph.size[1] == 0 {
            skipped += 1;
            continue;
        }
        // Shelf advance: new shelf when the glyph does not fit beside the
        // cursor; skip when no shelf remains.
        if cursor_x + w > side {
            let next_top = shelf_top.saturating_add(shelf_h);
            if next_top.saturating_add(h) > side {
                skipped += 1;
                continue;
            }
            shelf_top = next_top;
            shelf_h = 0;
            cursor_x = 0;
        } else if shelf_top.saturating_add(h) > side {
            // First glyph on a fresh row that still does not fit vertically.
            if cursor_x != 0 {
                let next_top = shelf_top.saturating_add(shelf_h);
                if next_top.saturating_add(h) > side {
                    skipped += 1;
                    continue;
                }
                shelf_top = next_top;
                shelf_h = 0;
                cursor_x = 0;
            } else {
                skipped += 1;
                continue;
            }
        }
        let slot_x = cursor_x;
        let slot_y = shelf_top;
        for row in 0..h {
            let src = row as usize * w as usize;
            let dst = (slot_y + row) as usize * side as usize + slot_x as usize;
            texels[dst..dst + w as usize].copy_from_slice(&mask[src..src + w as usize]);
        }
        cursor_x += w;
        shelf_h = shelf_h.max(h);
        let dim = side as f32;
        placements.push(InlineGlyph {
            dest: glyph.dest,
            size: glyph.size,
            uv: [
                slot_x as f32 / dim,
                slot_y as f32 / dim,
                (slot_x + w) as f32 / dim,
                (slot_y + h) as f32 / dim,
            ],
            color: glyph.color,
        });
    }

    InlinePlan {
        width: side,
        height: side,
        texels,
        placements,
        skipped,
    }
}

/// Serializes packed inline placements into a bounded chunk (same layout as
/// atlas glyphs, but UVs address the transient texture).
#[must_use]
pub fn chunk_inline_glyphs(
    plan: &InlinePlan,
    surface_w: u32,
    surface_h: u32,
    scale: f32,
) -> Vec<GlyphChunk> {
    if surface_w == 0 || surface_h == 0 || plan.placements.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut count = 0usize;
    for placement in &plan.placements {
        let Some(quad) = glyph_quad_bytes(
            placement.dest,
            placement.size,
            placement.uv,
            placement.color,
            surface_w,
            surface_h,
            scale,
        ) else {
            continue;
        };
        current.extend_from_slice(&quad);
        count += 1;
        if count >= MAX_GLYPH_QUADS_PER_BATCH {
            chunks.push(GlyphChunk {
                quad_count: count,
                bytes: std::mem::take(&mut current),
            });
            count = 0;
        }
    }
    if count > 0 {
        chunks.push(GlyphChunk {
            quad_count: count,
            bytes: current,
        });
    }
    chunks
}

/// What changed in the atlas since the last GPU upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasDirty {
    /// Texels are identical; skip the upload.
    Clean,
    /// First upload, dimension change, or length mismatch; upload all texels.
    Full,
    /// Rows `y..y+height` (full width) differ; upload that strip only.
    Strip {
        /// First differing row.
        y: u32,
        /// Number of differing rows.
        height: u32,
    },
}

/// Diffs atlas texels to find the upload region.
///
/// `prev` is the last uploaded `(texels, dims)` (`None` on the first
/// frame); `next` is the current CPU atlas. Dimension changes, missing
/// history, and length mismatches report [`AtlasDirty::Full`]; identical
/// texels report [`AtlasDirty::Clean`]; otherwise the strip from the first
/// to the last differing row is reported (full width, keeping
/// `bytes_per_row` alignment-friendly).
#[must_use]
pub fn compute_atlas_dirty(
    prev: Option<(&[u8], AtlasDims)>,
    next_texels: &[u8],
    next_dims: AtlasDims,
) -> AtlasDirty {
    let stride = next_dims.width as usize;
    let height = next_dims.height as usize;
    let Some((prev_texels, prev_dims)) = prev else {
        return AtlasDirty::Full;
    };
    if prev_dims != next_dims {
        return AtlasDirty::Full;
    }
    if prev_texels.len() != stride * height || next_texels.len() != stride * height {
        return AtlasDirty::Full;
    }
    if stride == 0 || height == 0 {
        return AtlasDirty::Full;
    }
    let mut first: Option<usize> = None;
    let mut last: usize = 0;
    for row in 0..height {
        let start = row * stride;
        if prev_texels[start..start + stride] != next_texels[start..start + stride] {
            if first.is_none() {
                first = Some(row);
            }
            last = row;
        }
    }
    match first {
        None => AtlasDirty::Clean,
        Some(top) => AtlasDirty::Strip {
            y: top as u32,
            height: (last - top + 1) as u32,
        },
    }
}

/// Padded `bytes_per_row` for a tightly packed R8 row of `width` texels.
#[must_use]
pub fn padded_bytes_per_row(width: u32) -> u32 {
    width.next_multiple_of(COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// Builds a padded staging buffer holding all atlas texels.
///
/// Returns `None` when `texels` does not match `dims` (fail-closed; the
/// caller maps this to [`RenderError::InvalidInput`]).
#[must_use]
pub fn build_padded_full(texels: &[u8], dims: AtlasDims) -> Option<Vec<u8>> {
    let w = dims.width as usize;
    let h = dims.height as usize;
    if texels.len() != w * h {
        return None;
    }
    let padded = padded_bytes_per_row(u32::from(dims.width)) as usize;
    let mut out = vec![0u8; padded * h];
    for row in 0..h {
        out[row * padded..row * padded + w].copy_from_slice(&texels[row * w..row * w + w]);
    }
    Some(out)
}

/// Builds a padded staging buffer for rows `y..y+height` of the atlas.
///
/// Returns `None` on any out-of-range or length mismatch (fail-closed).
#[must_use]
pub fn build_padded_strip(texels: &[u8], dims: AtlasDims, y: u32, height: u32) -> Option<Vec<u8>> {
    let w = dims.width as usize;
    let h = dims.height as usize;
    let y0 = y as usize;
    let strip = height as usize;
    if w == 0 || h == 0 || strip == 0 || y0.saturating_add(strip) > h {
        return None;
    }
    if texels.len() != w * h {
        return None;
    }
    let padded = padded_bytes_per_row(u32::from(dims.width)) as usize;
    let mut out = vec![0u8; padded * strip];
    for row in 0..strip {
        let src = (y0 + row) * w;
        out[row * padded..row * padded + w].copy_from_slice(&texels[src..src + w]);
    }
    Some(out)
}

/// Builds the index buffer contents for `quad_count` quads.
///
/// Each quad contributes 6 `u16` indices (`0,1,2, 0,2,3` offset by
/// `quad * 4`). Returns `None` when `quad_count` exceeds
/// [`MAX_FILL_QUADS_PER_BATCH`] (fail-closed: callers chunk first, so this
/// is unreachable in normal draws and guards against unbounded
/// allocation).
#[must_use]
pub fn quad_indices_for(quad_count: usize) -> Option<Vec<u16>> {
    if quad_count > MAX_FILL_QUADS_PER_BATCH.max(MAX_GLYPH_QUADS_PER_BATCH) {
        return None;
    }
    let mut out = Vec::with_capacity(quad_count * INDICES_PER_QUAD);
    for quad in 0..quad_count {
        let base = (quad * VERTICES_PER_QUAD) as u16;
        out.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Some(out)
}

/// Serializes `u16` indices to little-endian bytes without `bytemuck`.
#[must_use]
pub fn indices_to_le_bytes(indices: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(indices.len() * 2);
    for index in indices {
        out.extend_from_slice(&index.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::RectPx;

    fn fill_rect(x: i32, y: i32, w: u32, h: u32) -> FillRect {
        FillRect {
            rect: RectPx::new(x, y, w, h),
            color: [0xFF, 0x00, 0x00, 0xFF],
        }
    }

    fn atlas_glyph(dest: [i32; 2], size: [u32; 2]) -> GlyphInstance {
        GlyphInstance {
            dest,
            size,
            uv: [0.0, 0.0, 0.5, 0.5],
            color: [0xE5, 0xE5, 0xE5, 0xFF],
            source: GlyphSource::Atlas {
                slot: crate::atlas::AtlasSlot {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                },
            },
        }
    }

    #[test]
    fn rgba_bytes_normalize_to_unit_floats() {
        assert_eq!(rgba8_to_float4([0, 0, 0, 0]), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(rgba8_to_float4([255, 255, 255, 255]), [1.0, 1.0, 1.0, 1.0]);
        let half = rgba8_to_float4([128, 0, 0, 255]);
        assert!((half[0] - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn ndc_corners_map_surface_to_clip() {
        // Top-left pixel maps to (-1, 1), bottom-right to (1, -1).
        assert_eq!(pixel_to_ndc(0.0, 0.0, 800.0, 600.0), [-1.0, 1.0]);
        assert_eq!(pixel_to_ndc(800.0, 600.0, 800.0, 600.0), [1.0, -1.0]);
        assert_eq!(pixel_to_ndc(400.0, 300.0, 800.0, 600.0), [0.0, 0.0]);
    }

    #[test]
    fn scale_derivation_handles_zero_plan_and_clamps() {
        assert_eq!(
            derive_scale(800, 600, ExtentPx::new(0, 0)),
            1.0,
            "zero plan carries no information"
        );
        assert_eq!(derive_scale(800, 600, ExtentPx::new(800, 600)), 1.0);
        assert_eq!(derive_scale(1600, 1200, ExtentPx::new(800, 600)), 2.0);
        // Hostile ratios clamp instead of exploding vertices.
        assert_eq!(derive_scale(8000, 8000, ExtentPx::new(8, 8)), 4.0);
        assert_eq!(derive_scale(8, 8, ExtentPx::new(8000, 8000)), 0.25);
    }

    #[test]
    fn scale_derivation_recovers_fractional_hidpi_factor() {
        // Matched scale-aware plan: 192x57 cells at 13x26 physical pixels on
        // a 2496x1482 surface derives ~1.0 (no magnification).
        let matched = derive_scale(2496, 1482, ExtentPx::new(2496, 1482));
        assert!((matched - 1.0).abs() < 1e-6);
        // Stale 1x plan (192x57 cells at 8x16) against the same physical
        // surface derives ~1.625: every 1x texel magnifies through linear
        // filtering. The hidpi rescale path exists to prevent this state.
        let stale = derive_scale(2496, 1482, ExtentPx::new(1536, 912));
        assert!((stale - 1.625).abs() < 1e-3, "stale scale {stale}");
    }

    #[test]
    fn fill_chunking_preserves_order_and_bounds_chunks() {
        let fills: Vec<FillRect> = (0..10).map(|i| fill_rect(i * 8, 0, 8, 16)).collect();
        let chunks = chunk_fills(&fills, 800, 600, 1.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].quad_count, 10);
        assert_eq!(chunks[0].bytes.len(), 10 * 4 * FILL_VERTEX_SIZE_BYTES);
        // Empty rects are skipped, not emitted.
        let with_empty = vec![fill_rect(0, 0, 0, 16), fill_rect(0, 0, 8, 16)];
        let chunks = chunk_fills(&with_empty, 800, 600, 1.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].quad_count, 1);
        // Zero surface draws nothing.
        assert!(chunk_fills(&fills, 0, 600, 1.0).is_empty());
    }

    #[test]
    fn atlas_glyph_chunking_skips_inline_and_zero_sizes() {
        let glyphs = vec![
            atlas_glyph([0, 0], [8, 8]),
            GlyphInstance {
                dest: [8, 0],
                size: [8, 8],
                uv: [0.0; 4],
                color: [255, 255, 255, 255],
                source: GlyphSource::Inline {
                    mask: vec![255; 64],
                    width: 8,
                    height: 8,
                },
            },
            atlas_glyph([16, 0], [0, 8]),
        ];
        let chunks = chunk_atlas_glyphs(&glyphs, 800, 600, 1.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].quad_count, 1,
            "only the valid atlas glyph is emitted"
        );
    }

    #[test]
    fn large_fill_lists_split_into_bounded_chunks() {
        let fills: Vec<FillRect> = (0..(MAX_FILL_QUADS_PER_BATCH + 7))
            .map(|i| fill_rect((i % 100) as i32 * 8, (i / 100) as i32 * 16, 8, 16))
            .collect();
        let chunks = chunk_fills(&fills, 4096, 4096, 1.0);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].quad_count, MAX_FILL_QUADS_PER_BATCH);
        assert_eq!(chunks[1].quad_count, 7);
        for chunk in &chunks {
            assert!(chunk.quad_count <= MAX_FILL_QUADS_PER_BATCH);
        }
    }

    #[test]
    fn ctx_0182_fullscreen_multi_chunk_requires_all_submits() {
        // nmtui live repro (CTX-0182, issue #282): 200x62 fullscreen alt-screen
        // = 12400 background fills -> 4 bounded chunks (4096*3 + 112).
        // The GPU present path must submit every chunk (first Clear, rest
        // Load). Reusing one encoder+submit with vertex-buffer offset 0 lets
        // later writes overwrite earlier chunks before the draw, so only the
        // last (bottom) chunk painted: top went dark stale + bottom blue with
        // dialog glyphs (single glyph chunk) visible but wrong bg on top.
        let fills: Vec<FillRect> = (0..12400usize)
            .map(|i| fill_rect((i % 200) as i32 * 8, (i / 200) as i32 * 16, 8, 16))
            .collect();
        let chunks = chunk_fills(&fills, 2506, 1496, 1.0);
        assert_eq!(chunks.len(), 4, "200x62 fullscreen must split");
        let total: usize = chunks.iter().map(|c| c.quad_count).sum();
        assert_eq!(total, 12400, "all fills must survive chunking");
        for chunk in &chunks {
            assert!(chunk.quad_count <= MAX_FILL_QUADS_PER_BATCH);
        }
        // Last chunk alone covers only the bottom (<1 row here), never the
        // full frame: drawing only it would leave the top dark stale.
        let last = chunks.last().expect("non-empty").quad_count;
        assert!(last < 1000, "last chunk {last} must not cover fullscreen");
        assert!(
            chunks[0].quad_count == MAX_FILL_QUADS_PER_BATCH,
            "first (top) chunk must be full or top rows go missing"
        );
    }

    #[test]
    fn inline_packing_places_masks_and_reports_skips() {
        let glyphs = vec![
            GlyphInstance {
                dest: [0, 0],
                size: [4, 4],
                uv: [0.0; 4],
                color: [255, 255, 255, 255],
                source: GlyphSource::Inline {
                    mask: vec![200; 16],
                    width: 4,
                    height: 4,
                },
            },
            GlyphInstance {
                dest: [8, 0],
                size: [0, 4],
                uv: [0.0; 4],
                color: [255, 255, 255, 255],
                source: GlyphSource::Inline {
                    mask: vec![],
                    width: 0,
                    height: 4,
                },
            },
        ];
        let plan = pack_inline_glyphs(&glyphs);
        assert_eq!(plan.width, INLINE_TEXTURE_SIZE);
        assert_eq!(plan.placements.len(), 1);
        assert_eq!(plan.skipped, 1, "zero-sized mask is skipped");
        assert_eq!(
            plan.texels.len(),
            (INLINE_TEXTURE_SIZE * INLINE_TEXTURE_SIZE) as usize
        );
        // First texel carries the mask coverage.
        assert_eq!(plan.texels[0], 200);
        let uv = plan.placements[0].uv;
        assert_eq!(uv[0], 0.0);
        assert!((uv[2] - 4.0 / INLINE_TEXTURE_SIZE as f32).abs() < 1e-6);
    }

    #[test]
    fn atlas_dirty_reports_clean_full_and_strip() {
        let dims = AtlasDims {
            width: 8,
            height: 4,
        };
        let texels = vec![0u8; 32];
        assert_eq!(compute_atlas_dirty(None, &texels, dims), AtlasDirty::Full);
        assert_eq!(
            compute_atlas_dirty(Some((&texels, dims)), &texels, dims),
            AtlasDirty::Clean
        );
        let other_dims = AtlasDims {
            width: 8,
            height: 8,
        };
        assert_eq!(
            compute_atlas_dirty(Some((&texels, dims)), &[0u8; 64], other_dims),
            AtlasDirty::Full,
            "dimension change forces a full upload"
        );
        let mut changed = texels.clone();
        changed[8] = 255;
        changed[23] = 128;
        assert_eq!(
            compute_atlas_dirty(Some((&texels, dims)), &changed, dims),
            AtlasDirty::Strip { y: 1, height: 2 },
            "strip spans first to last differing row"
        );
    }

    #[test]
    fn padded_upload_helpers_keep_row_alignment() {
        assert_eq!(padded_bytes_per_row(2048), 2048);
        assert_eq!(padded_bytes_per_row(1024), 1024);
        assert_eq!(padded_bytes_per_row(100), 256);
        assert_eq!(padded_bytes_per_row(0), 0);
        let dims = AtlasDims {
            width: 4,
            height: 2,
        };
        let texels = vec![7u8; 8];
        let full = build_padded_full(&texels, dims).expect("valid");
        assert_eq!(full.len(), 256 * 2);
        assert_eq!(&full[0..4], &[7; 4]);
        assert_eq!(&full[256..260], &[7; 4]);
        let strip = build_padded_strip(&texels, dims, 1, 1).expect("valid strip");
        assert_eq!(strip.len(), 256);
        assert_eq!(&strip[0..4], &[7; 4]);
        assert!(build_padded_full(&[1, 2, 3], dims).is_none());
        assert!(build_padded_strip(&texels, dims, 1, 2).is_none());
    }

    #[test]
    fn quad_indices_cover_two_triangles_per_quad() {
        let indices = quad_indices_for(2).expect("bounded");
        assert_eq!(indices.len(), 12);
        assert_eq!(&indices[0..6], &[0, 1, 2, 0, 2, 3]);
        assert_eq!(&indices[6..12], &[4, 5, 6, 4, 6, 7]);
        assert!(quad_indices_for(usize::MAX).is_none());
        let bytes = indices_to_le_bytes(&[1, 256]);
        assert_eq!(bytes, vec![1, 0, 0, 1]);
    }

    #[test]
    fn atlas_dim_validation_rejects_zero_and_oversize() {
        assert!(
            validate_atlas_dims(AtlasDims {
                width: 0,
                height: 16
            })
            .is_err()
        );
        assert!(
            validate_atlas_dims(AtlasDims {
                width: 8192,
                height: 16
            })
            .is_err()
        );
        assert!(
            validate_atlas_dims(AtlasDims {
                width: 2048,
                height: 2048
            })
            .is_ok()
        );
    }
}
