//! GPU presentation batch translation: headless integration coverage.
//!
//! These tests drive the public [`bitty_render::batch`] API end to end from
//! a real [`GridRenderer`](bitty_render::grid::GridRenderer) `DrawList`
//! (deterministic fake rasterizer, no font stack, GPU, display server,
//! filesystem, or network) and verify the bounded-presentation contract:
//! chunk caps, byte layouts, atlas dirty-region bookkeeping, inline packing,
//! and determinism. The `wgpu`-dependent draw itself stays env-gated in
//! `gpu_integration.rs`; everything here runs on plain CI, including the
//! `x86_64-pc-windows-gnu` target.

use bitty_render::atlas::AtlasDims;
use bitty_render::batch::AtlasDirty;
use bitty_render::batch::{
    FILL_VERTEX_SIZE_BYTES, GLYPH_VERTEX_SIZE_BYTES, INLINE_TEXTURE_SIZE, MAX_FILL_QUADS_PER_BATCH,
    MAX_GLYPH_QUADS_PER_BATCH, chunk_atlas_glyphs, chunk_fills, chunk_inline_glyphs,
    compute_atlas_dirty, derive_scale, pack_inline_glyphs, padded_bytes_per_row, quad_indices_for,
    rgba8_to_float4, validate_atlas_dims,
};
use bitty_render::error::RenderError;
use bitty_render::geometry::{ExtentPx, RectPx};
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, FillRect, GlyphInstance, GlyphSource, GridRenderer};

#[derive(Debug)]
struct FakeR {
    next: u64,
}

impl GlyphRasterizer for FakeR {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, k: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if k.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(k.character) % 3 + 6) as i32;
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side,
                    height: side,
                    advance: [side, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn fake_renderer() -> GridRenderer<FakeR> {
    let q = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(FakeR { next: 0 }, &q, CellMetrics::new(8, 16).unwrap()).unwrap()
}

fn render_hello() -> (
    GridRenderer<FakeR>,
    bitty_render::grid::DrawList,
    Vec<u8>,
    AtlasDims,
) {
    let mut renderer = fake_renderer();
    let mut st = bitty_term_state::State::new();
    for ch in "hello".chars() {
        st.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    let snap = st.snapshot();
    let dmg = bitty_term_state::Damage {
        generation: snap.generation,
        regions: st.damage_since(0).into_boxed_slice(),
    };
    let list = renderer.render(&snap, &dmg).expect("render");
    assert!(list.needs_draw());
    let texels = renderer.atlas_texels().to_vec();
    let dims = renderer.atlas_dims();
    (renderer, list, texels, dims)
}

#[test]
fn draw_list_translates_to_bounded_batches_deterministically() {
    let (_renderer, list, _texels, _dims) = render_hello();
    let (surface_w, surface_h) = (640, 384);
    let scale = derive_scale(surface_w, surface_h, list.plan.extent);

    let fills = chunk_fills(&list.fills, surface_w, surface_h, scale);
    assert!(!fills.is_empty(), "hello frame must emit fills");
    for chunk in &fills {
        assert!(chunk.quad_count <= MAX_FILL_QUADS_PER_BATCH);
        assert_eq!(
            chunk.bytes.len(),
            chunk.quad_count * 4 * FILL_VERTEX_SIZE_BYTES
        );
    }
    let total_fill_quads: usize = fills.iter().map(|c| c.quad_count).sum();
    assert_eq!(total_fill_quads, list.fills.len());

    let atlas_glyph_count = list
        .glyphs
        .iter()
        .filter(|g| matches!(g.source, GlyphSource::Atlas { .. }))
        .count();
    let glyphs = chunk_atlas_glyphs(&list.glyphs, surface_w, surface_h, scale);
    let total_glyph_quads: usize = glyphs.iter().map(|c| c.quad_count).sum();
    assert_eq!(total_glyph_quads, atlas_glyph_count);
    for chunk in &glyphs {
        assert!(chunk.quad_count <= MAX_GLYPH_QUADS_PER_BATCH);
        assert_eq!(
            chunk.bytes.len(),
            chunk.quad_count * 4 * GLYPH_VERTEX_SIZE_BYTES
        );
    }

    // Determinism: identical inputs give byte-identical batches.
    let fills2 = chunk_fills(&list.fills, surface_w, surface_h, scale);
    assert_eq!(fills, fills2);

    // HiDPI scale keeps every vertex inside clip space.
    let hidpi = chunk_fills(&list.fills, surface_w * 2, surface_h * 2, 2.0);
    assert_eq!(hidpi.len(), fills.len());
    for chunk in &hidpi {
        for vertex in chunk.bytes.chunks_exact(FILL_VERTEX_SIZE_BYTES) {
            let x = f32::from_le_bytes(vertex[0..4].try_into().unwrap());
            let y = f32::from_le_bytes(vertex[4..8].try_into().unwrap());
            assert!(
                (-1.0..=1.0).contains(&x) && (-1.0..=1.0).contains(&y),
                "vertex ({x}, {y}) escapes clip space at 2x scale"
            );
        }
    }
}

#[test]
fn atlas_upload_bookkeeping_goes_full_clean_strip() {
    let (_renderer, _list, texels, dims) = render_hello();

    // First frame has no history: full upload.
    assert_eq!(compute_atlas_dirty(None, &texels, dims), AtlasDirty::Full);
    // Identical texels: skip the upload.
    assert_eq!(
        compute_atlas_dirty(Some((&texels, dims)), &texels, dims),
        AtlasDirty::Clean
    );
    // One dirty row: strip covering exactly that row.
    let mut changed = texels.clone();
    let stride = dims.width as usize;
    changed[stride * 3] ^= 0xFF;
    assert_eq!(
        compute_atlas_dirty(Some((&texels, dims)), &changed, dims),
        AtlasDirty::Strip { y: 3, height: 1 }
    );
    // Dimension change forces a full upload.
    let bigger = AtlasDims {
        width: dims.width,
        height: dims.height.saturating_add(8),
    };
    let bigger_texels = vec![0u8; bigger.width as usize * bigger.height as usize];
    assert_eq!(
        compute_atlas_dirty(Some((&texels, dims)), &bigger_texels, bigger),
        AtlasDirty::Full
    );

    // Row stride stays upload-aligned for real atlas sizes.
    assert_eq!(padded_bytes_per_row(u32::from(dims.width)), 2048);
    assert_eq!(padded_bytes_per_row(100), 256);
}

#[test]
fn oversized_frames_chunk_without_unbounded_buffers() {
    // Fullscreen-scale fill list (29k cells, as observed live on Hyprland).
    let fills: Vec<FillRect> = (0..29_109usize)
        .map(|i| FillRect {
            rect: RectPx::new((i % 170) as i32 * 8, (i / 170) as i32 * 16, 8, 16),
            color: [0x10, 0x10, 0x10, 0xFF],
        })
        .collect();
    let chunks = chunk_fills(&fills, 1440, 900, 1.0);
    assert!(chunks.len() > 1, "29k fills must split into chunks");
    for chunk in &chunks {
        assert!(chunk.quad_count <= MAX_FILL_QUADS_PER_BATCH);
    }
    let total: usize = chunks.iter().map(|c| c.quad_count).sum();
    assert_eq!(total, fills.len());

    // Oversized index builds fail closed instead of allocating.
    assert!(quad_indices_for(usize::MAX).is_none());
    // Oversized atlas dims fail closed.
    assert!(
        validate_atlas_dims(AtlasDims {
            width: 8192,
            height: 8192
        })
        .is_err()
    );
}

#[test]
fn inline_glyphs_pack_into_the_fixed_transient_texture() {
    let (_renderer, list, _texels, _dims) = render_hello();
    // The default-size atlas holds "hello" glyphs, so no inline glyphs exist;
    // synthesize inline instances directly for the packing path.
    let inline_glyphs: Vec<GlyphInstance> = (0..8)
        .map(|i| GlyphInstance {
            dest: [i * 10, 0],
            size: [8, 8],
            uv: [0.0; 4],
            color: rgba8_to_float4([0xE5, 0xE5, 0xE5, 0xFF]).map(|v| (v * 255.0) as u8),
            source: GlyphSource::Inline {
                mask: vec![128; 64],
                width: 8,
                height: 8,
            },
        })
        .collect();
    let plan = pack_inline_glyphs(&inline_glyphs);
    assert_eq!(plan.width, INLINE_TEXTURE_SIZE);
    assert_eq!(plan.height, INLINE_TEXTURE_SIZE);
    assert_eq!(
        plan.texels.len(),
        (INLINE_TEXTURE_SIZE * INLINE_TEXTURE_SIZE) as usize
    );
    assert_eq!(plan.placements.len(), 8);
    assert_eq!(plan.skipped, 0);

    let chunks = chunk_inline_glyphs(&plan, 640, 384, 1.0);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].quad_count, 8);

    // Malformed masks are skipped and counted, never trusted.
    let bad = vec![GlyphInstance {
        dest: [0, 0],
        size: [8, 8],
        uv: [0.0; 4],
        color: [255, 255, 255, 255],
        source: GlyphSource::Inline {
            mask: vec![1, 2, 3],
            width: 8,
            height: 8,
        },
    }];
    let plan = pack_inline_glyphs(&bad);
    assert_eq!(plan.placements.len(), 0);
    assert_eq!(plan.skipped, 1);

    // The untouched hello frame packs to an empty plan (no inline glyphs).
    let empty = pack_inline_glyphs(&list.glyphs);
    assert_eq!(empty.skipped, 0);
    assert!(chunk_inline_glyphs(&empty, 640, 384, 1.0).is_empty());
}

#[test]
fn batch_helpers_stay_total_on_degenerate_inputs() {
    // Zero surfaces draw nothing; zero plan extents recover scale 1.0.
    assert!(chunk_fills(&[], 640, 384, 1.0).is_empty());
    assert_eq!(derive_scale(640, 384, ExtentPx::new(0, 0)), 1.0);
    // Empty rects never become quads.
    let fills = vec![FillRect {
        rect: RectPx::new(0, 0, 0, 16),
        color: [255, 0, 0, 255],
    }];
    assert!(chunk_fills(&fills, 640, 384, 1.0).is_empty());
}
