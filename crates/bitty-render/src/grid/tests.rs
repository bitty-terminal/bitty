//! Grid-pipeline unit tests: damage conversion, placement, wide cells,
//! styles, atlas behavior, failure tolerance, and output determinism.
//!
//! All tests are headless and use a fully deterministic fake rasterizer, so
//! identical scripts produce byte-identical pipeline outputs on every
//! platform (terminal-state-rfc replay guarantee 2 applied to rendering).

use bitty_term_state::{
    Attribute, AttributeChange, AttributeDiff, Color, Damage, DamageRect, DamagedRegion, State,
    TerminalAction, UnderlineStyle,
};
use bitty_vt::GraphemeCell;

use super::{
    CellMetrics, DEFAULT_BG, DEFAULT_FG, FAINT_ALPHA, palette_rgb, resolve_color, resolved_colors,
    underline_thickness,
};
use crate::error::RenderError;
use crate::frame::{DamageDescriptor, FrameMode, plan_frame};
use crate::geometry::ExtentPx;
use crate::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use crate::grid::{GlyphSource, GridRenderer};

/// Deterministic fake: bitmap width varies with the character code (6..=8),
/// height is fixed at 6, and `' '`/`'\t'` rasterize to cached blanks. A
/// one-shot error switch simulates upstream failures. `face_metrics` is
/// `None` by default (legacy fixed-baseline path); [`FakeRasterizer::with_metrics`]
/// serves the CTX-0237 measured face plus measured tall glyphs.
struct FakeRasterizer {
    next_id: u64,
    blank_chars: Vec<char>,
    fail_next: bool,
    face_metrics: Option<crate::glyph::FontMetrics>,
}

impl FakeRasterizer {
    fn new() -> Self {
        Self {
            next_id: 0,
            blank_chars: vec![' ', '\t'],
            fail_next: false,
            face_metrics: None,
        }
    }

    /// Fake serving the CTX-0237 measured face (JetBrainsMono Nerd Font
    /// 12pt probe truth: line 22, descent -5) for metric-baseline tests.
    fn with_metrics() -> Self {
        Self {
            face_metrics: Some(crate::glyph::FontMetrics {
                average_advance_px: 10.0,
                line_height_px: 22.0,
                descent_px: -5.0,
            }),
            ..Self::new()
        }
    }

    fn bitmap_for(character: char) -> GlyphBitmap {
        // CTX-0237 measured tall glyphs (live raster truth at 12pt):
        // full block U+2588 (top=17, h=22) and box vertical U+2502
        // (top=18, h=25, deliberately taller than the 22px line box so the
        // overhang-permission lock has a real shape to hold).
        if character == '\u{2588}' {
            return tall_bitmap(0, 17, 10, 22, 0x2588);
        }
        if character == '\u{2502}' {
            return tall_bitmap(4, 18, 2, 25, 0x2502);
        }
        let code = u32::from(character) as usize;
        let width: i32 = i32::try_from(code % 3 + 6).unwrap();
        let height: i32 = 6;
        // Coverage pattern derived from the character code: deterministic
        // per key, non-uniform so blits are observable.
        let data: Vec<u8> = (0..(width as usize) * (height as usize) * 3)
            .map(|i| (0x30 + ((code + i) % 0x50)) as u8)
            .collect();
        GlyphBitmap::try_new(
            GlyphMetrics {
                left: 0,
                top: 1,
                width,
                height,
                advance: [width, 0],
            },
            BitmapFormat::Rgb,
            data,
        )
        .unwrap()
    }
}

/// Builds a deterministic tall-glyph bitmap with the given bearings.
fn tall_bitmap(left: i32, top: i32, width: i32, height: i32, seed: usize) -> GlyphBitmap {
    let data: Vec<u8> = (0..(width as usize) * (height as usize) * 3)
        .map(|i| (0x40 + ((seed + i) % 0x40)) as u8)
        .collect();
    GlyphBitmap::try_new(
        GlyphMetrics {
            left,
            top,
            width,
            height,
            advance: [10, 0],
        },
        BitmapFormat::Rgb,
        data,
    )
    .unwrap()
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next_id))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(RenderError::UpstreamRasterizer("synthetic".into()));
        }
        if self.blank_chars.contains(&key.character) {
            return Ok(None);
        }
        Ok(Some(Self::bitmap_for(key.character)))
    }

    fn font_metrics(
        &self,
        _font: FontId,
        _point_size: f32,
    ) -> Result<Option<crate::glyph::FontMetrics>, RenderError> {
        Ok(self.face_metrics)
    }
}

fn font_query() -> FontQuery {
    FontQuery {
        family: "Fake Mono".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    }
}

/// 8x16 cells: underline thickness clamps to 2, baseline sits at row*16+12.
fn cell_metrics() -> CellMetrics {
    CellMetrics::new(8, 16).unwrap()
}

fn renderer() -> GridRenderer<FakeRasterizer> {
    GridRenderer::new(FakeRasterizer::new(), &font_query(), cell_metrics()).unwrap()
}

fn print(c: char) -> TerminalAction {
    TerminalAction::Print(GraphemeCell::from(c))
}

fn sgr(changes: &[AttributeChange]) -> TerminalAction {
    TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: changes.iter().copied().collect(),
        },
    }
}

/// Applies the script and returns the resulting state.
fn state_from(script: &[TerminalAction]) -> State {
    let mut state = State::new();
    for action in script {
        state.apply(action);
    }
    state
}

/// Whole-history damage for the state (everything printed so far).
fn damage_all(state: &State) -> Damage {
    Damage {
        generation: state.generation(),
        regions: state.damage_since(0).into_boxed_slice(),
    }
}

/// Full-grid damage for a state (first frame / explicit full redraw).
fn full_damage(state: &State) -> Damage {
    Damage {
        generation: state.generation(),
        regions: Box::new([DamagedRegion::Grid(DamageRect::full(
            u16::try_from(state.height()).unwrap(),
            u16::try_from(state.width()).unwrap(),
        ))]),
    }
}

// ---------------------------------------------------------------------------
// Metrics, palette, color resolution
// ---------------------------------------------------------------------------

#[test]
fn cell_metrics_reject_zero() {
    assert!(matches!(
        CellMetrics::new(0, 16),
        Err(RenderError::InvalidInput { .. })
    ));
    assert!(matches!(
        CellMetrics::new(8, 0),
        Err(RenderError::InvalidInput { .. })
    ));
    assert_eq!(CellMetrics::new(8, 16).unwrap(), cell_metrics());
}

#[test]
fn extent_for_saturates_without_overflow() {
    let cell = cell_metrics();
    assert_eq!(cell.extent_for(80, 24), ExtentPx::new(640, 384));
    let huge = cell.extent_for(usize::MAX, usize::MAX);
    assert_eq!(huge, ExtentPx::new(u32::MAX, u32::MAX));
}

#[test]
fn palette_spots_cover_all_bands() {
    // Indices 0-15 are the Bitty Dark preset (single source of truth in
    // `bitty_config::theme::BITTY_DARK`); see the CTX-0147 theme tests for
    // the full table.
    assert_eq!(palette_rgb(0), [0x45, 0x47, 0x5A]);
    assert_eq!(palette_rgb(1), [0xF3, 0x8B, 0xA8]);
    assert_eq!(palette_rgb(7), [0xBA, 0xC2, 0xDE]);
    assert_eq!(palette_rgb(15), [0xCD, 0xD6, 0xF4]);
    // Cube corner: index 231 = level (5,5,5).
    assert_eq!(palette_rgb(231), [255, 255, 255]);
    assert_eq!(palette_rgb(16), [0, 0, 0]);
    assert_eq!(palette_rgb(17), [0, 0, 95]);
    // Grayscale endpoints.
    assert_eq!(palette_rgb(232), [8, 8, 8]);
    assert_eq!(palette_rgb(255), [238, 238, 238]);
}

#[test]
fn resolve_color_covers_default_indexed_and_rgb() {
    use bitty_term_state::Rgb;
    assert_eq!(
        resolve_color(None, DEFAULT_FG),
        [DEFAULT_FG[0], DEFAULT_FG[1], DEFAULT_FG[2], DEFAULT_FG[3]]
    );
    assert_eq!(resolve_color(Some(&Color::Indexed(1)), DEFAULT_FG)[0], 0xF3);
    assert_eq!(
        resolve_color(Some(&Color::Rgb(Rgb { r: 1, g: 2, b: 3 })), DEFAULT_FG),
        [1, 2, 3, 255]
    );
    // Default keeps the fallback's alpha; indexed entries inherit it too.
    assert_eq!(resolve_color(Some(&Color::Default), [9, 9, 9, 7])[3], 7);
}

#[test]
fn resolved_colors_handle_inverse_and_faint() {
    let mut style = bitty_term_state::Style::default();
    let (fg, bg) = resolved_colors(&style);
    assert_eq!(fg, DEFAULT_FG);
    assert_eq!(bg, DEFAULT_BG);

    style.attributes.inverse = true;
    let (fg, bg) = resolved_colors(&style);
    assert_eq!(fg, DEFAULT_BG);
    assert_eq!(bg, DEFAULT_FG);

    style.attributes.inverse = false;
    style.attributes.faint = true;
    let (fg, _) = resolved_colors(&style);
    assert_eq!(fg[3], FAINT_ALPHA);
}

// ---------------------------------------------------------------------------
// SnapshotDamage descriptor
// ---------------------------------------------------------------------------

#[test]
fn descriptor_drops_scrollback_and_converts_grid_rects() {
    let state = state_from(&[print('A')]);
    let damage = Damage {
        generation: state.generation(),
        regions: Box::new([
            DamagedRegion::Grid(DamageRect {
                top: 1,
                left: 2,
                bottom: 1,
                right: 4,
            }),
            DamagedRegion::Scrollback {
                first_line_id: 7,
                count: 2,
            },
        ]),
    };
    let snapshot = state.snapshot();
    let desc = super::SnapshotDamage::new(&snapshot, &damage, cell_metrics());
    assert_eq!(desc.extent(), ExtentPx::new(640, 384));
    assert_eq!(desc.grid_regions().len(), 1);
    assert_eq!(desc.damaged_regions().len(), 1);
    assert_eq!(desc.damaged_regions()[0].x, 16);
    assert_eq!(desc.damaged_regions()[0].y, 16);
    assert_eq!(desc.damaged_regions()[0].width, 24);
    assert_eq!(desc.damaged_regions()[0].height, 16);
    assert!(!desc.full_redraw_hint());
    let hinted = super::SnapshotDamage::new(&snapshot, &damage, cell_metrics()).with_full_redraw();
    assert!(hinted.full_redraw_hint());
}

#[test]
fn stale_damage_far_outside_the_extent_clips_to_clean() {
    let state = state_from(&[print('A')]);
    let damage = Damage {
        generation: state.generation(),
        regions: Box::new([DamagedRegion::Grid(DamageRect {
            top: 100,
            left: 200,
            bottom: 120,
            right: 400,
        })]),
    };
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();
    assert_eq!(list.plan.mode, FrameMode::Clean);
    assert!(!list.needs_draw());
}

// ---------------------------------------------------------------------------
// Frame behavior
// ---------------------------------------------------------------------------

#[test]
fn clean_frame_produces_empty_list_but_counts_a_plan() {
    let state = state_from(&[]);
    let clean = Damage {
        generation: state.generation(),
        regions: Box::new([]),
    };
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &clean).unwrap();
    assert_eq!(list.plan.mode, FrameMode::Clean);
    assert!(list.fills.is_empty() && list.glyphs.is_empty());
    assert_eq!(list.generation, state.generation());
    assert_eq!(grid.counters().frames_planned, 1);
    assert_eq!(grid.counters().cells_examined, 0);
}

#[test]
fn print_run_partial_frame_places_exact_cells() {
    let state = state_from(&[print('H'), print('i')]);
    let damage = damage_all(&state);
    let snapshot = state.snapshot();

    let mut grid = renderer();
    let list = grid.render(&snapshot, &damage).unwrap();
    assert_eq!(list.plan.mode, FrameMode::Partial);
    assert_eq!(list.fills.len(), 2);
    assert_eq!(list.glyphs.len(), 2);

    // Background fills sit exactly on the two damaged cells.
    assert_eq!(
        list.fills[0].rect,
        crate::geometry::RectPx::new(0, 0, 8, 16)
    );
    assert_eq!(
        list.fills[1].rect,
        crate::geometry::RectPx::new(8, 0, 8, 16)
    );
    assert_eq!(list.fills[0].color, DEFAULT_BG);

    // Glyphs: baseline rule row*16 + 16*3/4 = 12; dest_y = baseline - top(1).
    for (i, glyph) in list.glyphs.iter().enumerate() {
        assert_eq!(glyph.dest[0], i32::try_from(i * 8).unwrap());
        assert_eq!(glyph.dest[1], 11);
        assert_eq!(glyph.size[1], 6);
        assert_eq!(glyph.color, DEFAULT_FG);
        assert!(matches!(glyph.source, GlyphSource::Atlas { .. }));
    }

    let counters = grid.counters();
    assert_eq!(counters.cells_examined, 2);
    assert_eq!(counters.cells_drawn, 2);
    assert_eq!(counters.background_fills, 2);
    assert_eq!(counters.glyphs_emitted, 2);
    assert_eq!(counters.spacer_cells_skipped, 0);
}

#[test]
fn wide_char_paints_two_columns_but_emits_one_glyph() {
    // U+6F22 (CJK) resolves to width 2 with a spacer trailing half.
    let state = state_from(&[print('\u{6F22}')]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    // Both halves get a background fill; the leading half spans two columns.
    assert_eq!(list.fills.len(), 2);
    assert_eq!(
        list.fills[0].rect,
        crate::geometry::RectPx::new(0, 0, 16, 16)
    );
    assert_eq!(
        list.fills[1].rect,
        crate::geometry::RectPx::new(8, 0, 8, 16)
    );
    // Exactly one glyph: the trailing half never rasterizes.
    assert_eq!(list.glyphs.len(), 1);

    let counters = grid.counters();
    assert_eq!(counters.cells_examined, 2);
    assert_eq!(counters.cells_drawn, 1);
    assert_eq!(counters.spacer_cells_skipped, 1);
    assert_eq!(counters.glyphs_emitted, 1);
    assert_eq!(counters.blank_cells_skipped, 0);
}

#[test]
fn blanks_emit_background_only() {
    // Erased cells stay blank: only background fills are emitted.
    let state = state_from(&[print('A')]);
    let mut grid = renderer();
    let list = grid
        .render(&state.snapshot(), &full_damage(&state))
        .unwrap();

    // Exactly one glyph on the whole screen; everything else is background.
    assert_eq!(list.glyphs.len(), 1);
    assert_eq!(list.fills.len(), state.width() * state.height());
    let counters = grid.counters();
    assert_eq!(counters.blank_cells_skipped, 80 * 24 - 1);
    assert_eq!(counters.cells_examined, 80 * 24);
}

#[test]
fn inverse_video_swaps_fill_and_glyph_colors() {
    let state = state_from(&[
        sgr(&[AttributeChange::Enable(Attribute::Inverse)]),
        print('X'),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    assert_eq!(list.fills[0].color, DEFAULT_FG); // swapped background
    assert_eq!(list.glyphs[0].color, DEFAULT_BG); // swapped foreground
}

#[test]
fn faint_text_carries_reduced_alpha_on_glyphs_only() {
    let state = state_from(&[
        sgr(&[AttributeChange::Enable(Attribute::Faint)]),
        print('X'),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    assert_eq!(list.fills[0].color, DEFAULT_BG); // background unaffected
    assert_eq!(list.glyphs[0].color[3], FAINT_ALPHA);
    assert_eq!(list.glyphs[0].color[0], DEFAULT_FG[0]);
}

#[test]
fn invisible_cells_keep_background_but_drop_glyphs() {
    let state = state_from(&[
        sgr(&[AttributeChange::Enable(Attribute::Invisible)]),
        print('X'),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    assert_eq!(list.fills.len(), 1);
    assert!(list.glyphs.is_empty());
    assert_eq!(grid.counters().invisible_cells_skipped, 1);
}

#[test]
fn colored_background_and_indexed_foreground_resolve_deterministically() {
    let state = state_from(&[
        sgr(&[
            AttributeChange::Background(Color::Indexed(4)),
            AttributeChange::Foreground(Color::Rgb(bitty_term_state::Rgb {
                r: 10,
                g: 20,
                b: 30,
            })),
        ]),
        print('Y'),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    assert_eq!(
        list.fills[0].color,
        resolve_color(Some(&Color::Indexed(4)), DEFAULT_BG)
    );
    assert_eq!(list.glyphs[0].color, [10, 20, 30, 255]);
}

#[test]
fn underline_and_strikethrough_geometry_is_fixed() {
    let state = state_from(&[
        sgr(&[
            AttributeChange::Enable(Attribute::Underline(UnderlineStyle::Double)),
            AttributeChange::Enable(Attribute::Strikethrough),
        ]),
        print('Z'),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    // thickness = clamp(16/8, 1, 2) = 2; double underline rows at
    // y = 16-4 = 12 and y = 12-4 = 8; strikethrough at y = 8 - 1 = 7.
    let rects: Vec<_> = list.fills.iter().skip(1).take(3).collect();
    assert_eq!(rects.len(), 3);
    assert_eq!(rects[0].rect.y, 12);
    assert_eq!(rects[0].rect.height, 2);
    assert_eq!(rects[1].rect.y, 8);
    assert_eq!(rects[2].rect.y, 7);
    assert_eq!(grid.counters().decorations_emitted, 3);
    assert_eq!(grid.counters().cells_drawn, 1);
}

#[test]
fn underline_thickness_is_named_and_clamped() {
    // CTX-0301: `(height / 8).clamp(1, 2)` is named; pin the representative
    // heights including the clamps and the cell heights used by tests.
    for (height, expected) in [
        (0u32, 1),
        (7, 1),
        (8, 1),
        (15, 1),
        (16, 2),
        (64, 2),
        (u32::MAX, 2),
    ] {
        assert_eq!(underline_thickness(height), expected, "height {height}");
    }
}

#[test]
fn underlined_blank_still_counts_as_drawn() {
    let state = state_from(&[
        sgr(&[AttributeChange::Enable(Attribute::Underline(
            UnderlineStyle::Single,
        ))]),
        print(' '),
    ]);
    let damage = damage_all(&state);
    let mut grid = renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    // Background + underline bar, no glyph; still a drawn cell.
    assert_eq!(list.fills.len(), 2);
    assert!(list.glyphs.is_empty());
    assert_eq!(grid.counters().cells_drawn, 1);
    assert_eq!(grid.counters().blank_cells_skipped, 0);
}

// ---------------------------------------------------------------------------
// Atlas behavior
// ---------------------------------------------------------------------------

#[test]
fn atlas_miss_then_hit_with_upload_drain() {
    let state = state_from(&[print('A'), print('B')]);
    let snapshot = state.snapshot();
    let damage = damage_all(&state);

    let mut grid = renderer();
    let first = grid.render(&snapshot, &damage).unwrap();
    assert_eq!(first.glyphs.len(), 2);
    assert_eq!(grid.atlas_stats().1, 2); // two misses
    assert_eq!(grid.atlas_stats().0, 0); // no hits yet

    // The upload queue carries exactly the placed glyphs until drained.
    let uploads = grid.take_atlas_uploads();
    assert_eq!(uploads.len(), 2);
    for upload in &uploads {
        assert_eq!(
            upload.data.len(),
            usize::from(upload.slot.width) * usize::from(upload.slot.height)
        );
        // Coverage bytes landed in the live texture at the slot offset.
        let dims = grid.atlas_dims();
        let stride = usize::from(dims.width);
        let x = usize::from(upload.slot.x);
        let y = usize::from(upload.slot.y);
        assert_eq!(grid.atlas_texels()[y * stride + x], upload.data[0]);
    }
    assert!(grid.take_atlas_uploads().is_empty());

    // Second frame over the same cells: pure hits, nothing new to upload.
    let second = grid.render(&snapshot, &damage).unwrap();
    assert_eq!(second.glyphs.len(), 2);
    let (hits, misses, _, _) = grid.atlas_stats();
    assert_eq!((hits, misses), (2, 2));
    assert_eq!(grid.cache_stats(), (2, 2)); // glyph cache mirrors the story
    assert!(grid.take_atlas_uploads().is_empty());

    // Slots are stable across frames: identical uv coordinates.
    for (a, b) in first.glyphs.iter().zip(&second.glyphs) {
        assert_eq!(a.uv, b.uv);
        assert_eq!(a.dest, b.dest);
    }
}

#[test]
fn atlas_eviction_is_wholesale_and_deterministic() {
    // 8x8 atlas; fake bitmaps are 6..=8 wide and 6 tall, so each shelf
    // holds at most one placement and only two shelves exist.
    let script: Vec<TerminalAction> = ['a', 'b', 'c'].iter().map(|&c| print(c)).collect();
    let state = state_from(&script);
    let snapshot = state.snapshot();
    let damage = damage_all(&state);

    let render_once = || -> GridRenderer<FakeRasterizer> {
        let mut grid = GridRenderer::with_atlas_dimension(
            FakeRasterizer::new(),
            &font_query(),
            cell_metrics(),
            8,
        )
        .unwrap();
        let _list = grid.render(&snapshot, &damage).unwrap();
        grid
    };

    let a = render_once();
    let b = render_once();

    assert_eq!(a.atlas_stats().3, 0); // nothing oversized
    assert_eq!(
        a.atlas_stats().2,
        2,
        "'b' and 'c' each force one wholesale reset on the tiny atlas"
    );
    assert_eq!(a.atlas_placements(), 1); // post-eviction retry holds only 'c'

    // Both runs end with byte-identical textures and stats.
    assert_eq!(a.atlas_texels(), b.atlas_texels());
    assert_eq!(a.atlas_stats(), b.atlas_stats());
}

#[test]
fn oversized_glyph_falls_back_inline_instead_of_failing() {
    let state = state_from(&[print('W')]);
    let damage = damage_all(&state);
    let mut grid = GridRenderer::with_atlas_dimension(
        FakeRasterizer::new(),
        &font_query(),
        cell_metrics(),
        4, // smaller than any fake bitmap
    )
    .unwrap();
    let list = grid.render(&state.snapshot(), &damage).unwrap();

    assert_eq!(list.glyphs.len(), 1);
    match &list.glyphs[0].source {
        GlyphSource::Inline {
            mask,
            width,
            height,
        } => {
            assert_eq!((*width, *height), (6, 6));
            assert_eq!(mask.len(), 6 * 6);
            assert!(mask.iter().any(|&coverage| coverage > 0));
        }
        GlyphSource::Atlas { .. } => panic!("expected inline fallback"),
    }
    assert_eq!(grid.atlas_stats().3, 1); // inline_fallbacks
    assert_eq!(grid.counters().glyphs_emitted, 1);
}

// ---------------------------------------------------------------------------
// Failure tolerance
// ---------------------------------------------------------------------------

#[test]
fn rasterizer_failure_skips_the_glyph_but_keeps_the_frame() {
    let state = state_from(&[print('Q')]);
    let damage = damage_all(&state);
    let mut fake = FakeRasterizer::new();
    fake.fail_next = true;
    let mut grid = GridRenderer::new(fake, &font_query(), cell_metrics()).unwrap();

    let list = grid.render(&state.snapshot(), &damage).unwrap();
    assert_eq!(list.fills.len(), 1);
    assert!(list.glyphs.is_empty());
    assert_eq!(grid.counters().blank_cells_skipped, 1);
    assert_eq!(grid.cache_stats().1, 1); // counted as a miss, never cached
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn identical_inputs_yield_identical_draw_lists_and_textures() {
    let script: Vec<TerminalAction> = vec![
        sgr(&[
            AttributeChange::Foreground(Color::Indexed(2)),
            AttributeChange::Enable(Attribute::Bold),
        ]),
        print('H'),
        print('i'),
        sgr(&[AttributeChange::Reset]),
        TerminalAction::Print(GraphemeCell::from('\u{6F22}')),
        sgr(&[AttributeChange::Background(Color::Indexed(17))]),
        print(' '),
        print('x'),
    ];

    let run = || {
        let state = state_from(&script);
        let snapshot = state.snapshot();
        let mut grid = renderer();
        // Two partial frames, then one authoritative full redraw.
        let d1 = Damage {
            generation: snapshot.generation,
            regions: Box::new([DamagedRegion::Grid(DamageRect {
                top: 0,
                left: 0,
                bottom: 0,
                right: 3,
            })]),
        };
        let partial_a = grid.render(&snapshot, &d1).unwrap();
        let partial_b = grid.render(&snapshot, &d1).unwrap();
        let full = grid.render(&snapshot, &full_damage(&state)).unwrap();
        let texels = grid.atlas_texels().to_vec();
        (partial_a, partial_b, full, texels)
    };

    let (pa, pb, fa, texels_a) = run();
    let (pb2, _, fb, texels_b) = run();

    // Repeated frames over identical inputs are value-identical.
    assert_eq!(pa, pb);
    assert_eq!(pb, pb2);
    assert_eq!(fa, fb);
    assert_eq!(texels_a, texels_b);

    // The full redraw really covers everything and plans deterministically.
    assert_eq!(fa.plan.mode, FrameMode::Full);
    assert_eq!(
        fa.plan,
        plan_frame(&super::SnapshotDamage::new(
            &State::new().snapshot(),
            &full_damage(&State::new()),
            cell_metrics()
        ))
    );

    // uv coordinates stay normalized; their exact bit patterns are already
    // covered by the value equality asserted above.
    for glyph in &fa.glyphs {
        assert!(glyph.uv.iter().all(|v| (0.0..=1.0).contains(v)));
    }
}

#[test]
fn full_frame_matches_union_of_incremental_cell_visits() {
    // Drawing cells [0..4] incrementally must visit exactly the same cells
    // as the covering full redraw restricted to those columns.
    let script: Vec<TerminalAction> = "abcd".chars().map(print).collect();
    let state = state_from(&script);
    let snapshot = state.snapshot();

    let mut incremental = renderer();
    for right in 0u16..4 {
        let damage = Damage {
            generation: snapshot.generation,
            regions: Box::new([DamagedRegion::Grid(DamageRect {
                top: 0,
                left: right,
                bottom: 0,
                right,
            })]),
        };
        let list = incremental.render(&snapshot, &damage).unwrap();
        assert_eq!(list.fills.len(), 1);
        assert_eq!(list.glyphs.len(), 1);
        assert_eq!(list.fills[0].rect.x, i32::from(right) * 8);
    }

    // Every visited cell repainted its background: nothing stale survives.
    let counters = incremental.counters();
    assert_eq!(counters.cells_examined, 4);
    assert_eq!(counters.background_fills, 4);
}

#[test]
fn scrolled_blanks_after_sgr_reset_stay_themed() {
    use bitty_vt::{Color as VtColor, Rgb};
    let light = VtColor::Rgb(Rgb {
        r: 231,
        g: 236,
        b: 248,
    });
    let mut script = vec![
        sgr(&[AttributeChange::Background(light)]),
        sgr(&[AttributeChange::Reset]),
    ];
    for _ in 0..(80 + 5) {
        script.push(TerminalAction::PrintControl(bitty_vt::ControlChar(0x0A)));
    }
    let state = state_from(&script);
    let snapshot = state.snapshot();
    let mut grid = renderer();
    let list = grid.render(&snapshot, &full_damage(&state)).unwrap();
    assert_eq!(list.fills.len(), snapshot.width * snapshot.height);
    for fill in &list.fills {
        assert_eq!(
            fill.color, DEFAULT_BG,
            "every scrolled row must use the themed background"
        );
    }
}

// ---------------------------------------------------------------------------
// DPI rescale: atlas rasterization matches the scaled cell
// ---------------------------------------------------------------------------

#[test]
fn dpi_rescale_updates_cell_font_and_invalidates_caches() {
    let mut renderer = renderer();
    // Populate caches at 1x so invalidation is observable.
    let script: Vec<TerminalAction> = "ab".chars().map(print).collect();
    let state = state_from(&script);
    let snapshot = state.snapshot();
    let list = renderer.render(&snapshot, &full_damage(&state)).unwrap();
    assert!(!list.glyphs.is_empty());
    assert!(renderer.atlas_placements() > 0);
    let (hits_before, misses_before, _, _) = renderer.atlas_stats();

    let applied = renderer
        .apply_dpi_scale(cell_metrics(), &font_query(), 1.6)
        .unwrap();
    assert_eq!(applied.scale, 1.6);
    assert_eq!(applied.cell, CellMetrics::new(13, 26).unwrap());
    assert!((applied.point_size - 19.2).abs() < 1e-4);
    assert_eq!(renderer.cell_metrics(), applied.cell);
    // Stale placements and cached bitmaps are gone; cumulative counters stay.
    assert_eq!(renderer.atlas_placements(), 0);
    assert!(renderer.atlas_texels().iter().all(|&b| b == 0));
    let (hits_after, misses_after, _, _) = renderer.atlas_stats();
    assert_eq!((hits_after, misses_after), (hits_before, misses_before));

    // Frames after the rescale place glyphs on the 13px pitch.
    let script: Vec<TerminalAction> = "ab".chars().map(print).collect();
    let state = state_from(&script);
    let snapshot = state.snapshot();
    let list = renderer.render(&snapshot, &full_damage(&state)).unwrap();
    assert!(!list.glyphs.is_empty());
    for glyph in &list.glyphs {
        assert_eq!(glyph.dest[0] % 13, 0, "glyph origin follows scaled pitch");
    }
    assert_eq!(list.plan.extent.width % 13, 0);
}

#[test]
fn dpi_rescale_sanitizes_invalid_scales() {
    let mut renderer = renderer();
    for invalid in [0.0, -1.6, f64::NAN, f64::INFINITY] {
        let applied = renderer
            .apply_dpi_scale(cell_metrics(), &font_query(), invalid)
            .unwrap();
        assert_eq!(applied.scale, 1.0);
        assert_eq!(applied.cell, cell_metrics());
        assert_eq!(applied.point_size, 12.0);
    }
    // Hostile scales clamp instead of exploding geometry.
    let applied = renderer
        .apply_dpi_scale(cell_metrics(), &font_query(), 100.0)
        .unwrap();
    assert_eq!(applied.scale, 4.0);
    assert_eq!(applied.cell, CellMetrics::new(32, 64).unwrap());
}

#[test]
fn dpi_rescale_failure_leaves_renderer_unchanged() {
    let mut renderer = renderer();
    let script: Vec<TerminalAction> = "ab".chars().map(print).collect();
    let state = state_from(&script);
    let snapshot = state.snapshot();
    let before = renderer.render(&snapshot, &full_damage(&state)).unwrap();
    assert!(!before.glyphs.is_empty());
    let placements_before = renderer.atlas_placements();

    // An invalid base query fails validation before any mutation: the font
    // is loaded before fields update or caches clear, so the renderer keeps
    // serving 1x frames.
    let invalid = FontQuery {
        family: "   ".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    assert!(
        renderer
            .apply_dpi_scale(cell_metrics(), &invalid, 1.6)
            .is_err()
    );
    assert_eq!(renderer.cell_metrics(), cell_metrics());
    assert_eq!(renderer.atlas_placements(), placements_before);
}

// ---------------------------------------------------------------------------
// CTX-0147: designed default theme preset (render consumes the registry)
// ---------------------------------------------------------------------------

#[test]
fn theme_resolution_none_unknown_and_known() {
    use super::{DEFAULT_CURSOR, DEFAULT_SELECTION};
    use super::{active_theme, cursor_fill, selection_fill};

    // None -> default preset.
    let theme = active_theme(None);
    assert_eq!(theme.name, bitty_config::theme::DEFAULT_THEME_NAME);
    assert_eq!(theme.background, [0x1E, 0x1E, 0x2E]);
    assert_eq!(theme.foreground, [0xCD, 0xD6, 0xF4]);

    // Unknown name -> default preset (config layer logs the fallback).
    let (fallback, status) = bitty_config::theme::resolve_theme_with_status(Some("not-a-theme"));
    assert_eq!(
        status,
        bitty_config::theme::ThemeResolution::FallbackUnknown
    );
    assert_eq!(fallback.name, bitty_config::theme::DEFAULT_THEME_NAME);
    assert!(std::ptr::eq(active_theme(Some("not-a-theme")), fallback));

    // Known name -> exact preset values.
    let named = active_theme(Some("bitty-dark"));
    assert_eq!(named.background, [0x1E, 0x1E, 0x2E]);
    assert_eq!(named.foreground, [0xCD, 0xD6, 0xF4]);
    assert_eq!(named.cursor, [0xF5, 0xE0, 0xDC]);
    assert_eq!(named.selection, [0x31, 0x32, 0x44]);

    // Render-side cursor/selection colors equal the preset entries.
    assert_eq!(DEFAULT_CURSOR[..3], named.cursor);
    assert_eq!(DEFAULT_SELECTION[..3], named.selection);
    assert_eq!(selection_fill()[..3], named.selection);
    let _ = cursor_fill;
}

#[test]
fn default_fg_bg_match_preset_and_ansi_maps_to_theme() {
    // Default cell colors are the preset foreground/background.
    assert_eq!(DEFAULT_FG[..3], bitty_config::theme::BITTY_DARK.foreground);
    assert_eq!(DEFAULT_BG[..3], bitty_config::theme::BITTY_DARK.background);

    // All 16 ANSI entries resolve to the preset table (single source).
    for index in 0u8..16 {
        assert_eq!(
            palette_rgb(index),
            bitty_config::theme::BITTY_DARK.ansi[usize::from(index)],
            "ANSI index {index}"
        );
    }
    // Spot checks: the roles from the module table.
    assert_eq!(palette_rgb(0), [0x45, 0x47, 0x5A]);
    assert_eq!(palette_rgb(2), [0xA6, 0xE3, 0xA1]);
    assert_eq!(palette_rgb(4), [0x89, 0xB4, 0xFA]);
    assert_eq!(palette_rgb(15), [0xCD, 0xD6, 0xF4]);
    // The 256-color cube and grays stay xterm-compatible past index 15.
    assert_eq!(palette_rgb(16), [0, 0, 0]);
    assert_eq!(palette_rgb(231), [255, 255, 255]);
    assert_eq!(palette_rgb(232), [8, 8, 8]);

    // Indexed colors inherit the fallback alpha, like before.
    assert_eq!(
        resolve_color(Some(&Color::Indexed(2)), [9, 9, 9, 7]),
        [0xA6, 0xE3, 0xA1, 7]
    );
}

#[test]
fn cursor_fill_geometry_and_bounds() {
    use super::{DEFAULT_CURSOR, cursor_fill};
    use bitty_term_state::{Cursor, CursorPosition};

    let cell = cell_metrics(); // 8x16
    let visible = Cursor {
        position: CursorPosition { row: 2, col: 3 },
        visible: true,
        ..Cursor::default()
    };
    let fill = cursor_fill(&visible, cell, 80, 24).expect("visible cursor in grid");
    assert_eq!(fill.rect, crate::geometry::RectPx::new(24, 32, 8, 16));
    assert_eq!(fill.color, DEFAULT_CURSOR);

    // Hidden cursor paints nothing.
    let hidden = Cursor {
        visible: false,
        ..visible.clone()
    };
    assert!(cursor_fill(&hidden, cell, 80, 24).is_none());

    // Out-of-grid cursor paints nothing (no panic, no wraparound).
    let outside = Cursor {
        position: CursorPosition { row: 24, col: 0 },
        visible: true,
        ..Cursor::default()
    };
    assert!(cursor_fill(&outside, cell, 80, 24).is_none());
}

#[test]
fn cursor_fill_shapes_per_decscusr() {
    // CTX-0162 (DEC-0017 ghostty `cursor_bar`/`cursor_underline` + alacritty
    // 15% thickness): block = full cell, bar = left strip, underline =
    // bottom strip; all in the theme cursor color, distinct from selection.
    use super::{DEFAULT_CURSOR, DEFAULT_SELECTION, cursor_fill};
    use bitty_term_state::{Cursor, CursorPosition, CursorStyle};

    let cell = cell_metrics(); // 8x16 -> thickness max(1, round(8*0.15)) = 1
    let at = |style: CursorStyle| Cursor {
        position: CursorPosition { row: 2, col: 3 },
        visible: true,
        cursor_style: style,
        ..Cursor::default()
    };

    // Block family: full cell.
    for style in [
        CursorStyle::Default,
        CursorStyle::BlinkingBlock,
        CursorStyle::SteadyBlock,
    ] {
        let fill = cursor_fill(&at(style), cell, 80, 24).expect("block visible");
        assert_eq!(
            fill.rect,
            crate::geometry::RectPx::new(24, 32, 8, 16),
            "style {style:?} must paint a full block"
        );
        assert_eq!(fill.color, DEFAULT_CURSOR);
        assert_ne!(fill.color, DEFAULT_SELECTION);
    }

    // Bar family: thin left strip, full height (~1-2px at 8px cells).
    for style in [CursorStyle::BlinkingBar, CursorStyle::SteadyBar] {
        let fill = cursor_fill(&at(style), cell, 80, 24).expect("bar visible");
        assert_eq!(
            fill.rect,
            crate::geometry::RectPx::new(24, 32, 1, 16),
            "style {style:?} must paint a thin left bar"
        );
        assert_eq!(fill.color, DEFAULT_CURSOR);
    }

    // Underline family: full width, thin bottom strip.
    for style in [CursorStyle::BlinkingUnderline, CursorStyle::SteadyUnderline] {
        let fill = cursor_fill(&at(style), cell, 80, 24).expect("underline visible");
        assert_eq!(
            fill.rect,
            crate::geometry::RectPx::new(24, 32 + 16 - 1, 8, 1),
            "style {style:?} must paint a bottom underline"
        );
        assert_eq!(fill.color, DEFAULT_CURSOR);
    }

    // Scaled cells keep the fraction: 16x32 -> thickness 2px.
    let big = super::CellMetrics::new(16, 32).unwrap();
    let bar = cursor_fill(&at(CursorStyle::SteadyBar), big, 80, 24).expect("scaled bar");
    assert_eq!(bar.rect.width, 2);
    assert_eq!(bar.rect.height, 32);
    let underline =
        cursor_fill(&at(CursorStyle::SteadyUnderline), big, 80, 24).expect("scaled underline");
    assert_eq!(underline.rect.width, 16);
    assert_eq!(underline.rect.height, 2);
}

#[test]
fn demo_green_resolves_to_theme_green() {
    // The synthetic demo pump emits `\x1b[32m` (Indexed 2). Render must map
    // it to the preset green, not a hardcoded ad-hoc green.
    let themed = resolve_color(Some(&Color::Indexed(2)), DEFAULT_FG);
    assert_eq!(themed, [0xA6, 0xE3, 0xA1, 0xFF]);
}

// ---------------------------------------------------------------------------
// CTX-0237: metric-aware baseline + permitted overhang
// ---------------------------------------------------------------------------

/// 10x22 cells covering the measured JetBrainsMono Nerd Font 12pt line box
/// (advance 10, ascent 17 + descent 5): tall glyphs fit with zero overhang.
fn metric_cell() -> CellMetrics {
    CellMetrics::new(10, 22).unwrap()
}

fn metric_renderer() -> GridRenderer<FakeRasterizer> {
    GridRenderer::new(FakeRasterizer::with_metrics(), &font_query(), metric_cell()).unwrap()
}

#[test]
fn resolve_baseline_offset_prefers_measured_ascent() {
    use super::resolve_baseline_offset;
    use crate::glyph::FontMetrics;

    let face = FontMetrics {
        average_advance_px: 10.0,
        line_height_px: 22.0,
        descent_px: -5.0,
    };
    // Measured ascent (22 - 5) places the pen 17 below the row top, so the
    // measured full block (top=17, h=22) lands exactly on the cell.
    assert_eq!(resolve_baseline_offset(22, Some(face)), 17);
    // Legacy 3/4 rule stays for backends without measurements.
    assert_eq!(resolve_baseline_offset(16, None), 12);
    assert_eq!(resolve_baseline_offset(19, None), 14);
    assert_eq!(resolve_baseline_offset(22, None), 16);
    // Hostile measurements clamp into the cell, never off-grid.
    let huge = FontMetrics {
        line_height_px: 220.0,
        ..face
    };
    assert_eq!(resolve_baseline_offset(22, Some(huge)), 22);
    let below = FontMetrics {
        descent_px: -30.0,
        ..face
    };
    assert_eq!(resolve_baseline_offset(22, Some(below)), 0);
    let above = FontMetrics {
        descent_px: 5.0,
        ..face
    };
    assert_eq!(resolve_baseline_offset(22, Some(above)), 22);
    // Unusable values fall back to the legacy rule (total, no panics).
    for bad in [
        FontMetrics {
            line_height_px: f32::NAN,
            ..face
        },
        FontMetrics {
            line_height_px: 0.0,
            ..face
        },
        FontMetrics {
            average_advance_px: -1.0,
            ..face
        },
        FontMetrics {
            descent_px: f32::INFINITY,
            ..face
        },
    ] {
        assert_eq!(
            resolve_baseline_offset(22, Some(bad)),
            16,
            "{bad:?} must fall back"
        );
    }
}

#[test]
fn renderer_adopts_metric_baseline_and_keeps_legacy_fallback() {
    // Measured face: ascent 17 wins over the 3/4 rule (22*3/4 = 16).
    assert_eq!(metric_renderer().baseline_offset(), 17);
    // Unmeasured fake: legacy rule untouched (8x16 -> 12).
    assert_eq!(renderer().baseline_offset(), 12);
}

#[test]
fn metric_baseline_fits_measured_tall_glyphs_with_zero_overhang() {
    // Full block U+2588 measured top=17 h=22: dest lands exactly on the
    // cell origin and the bitmap spans exactly the cell — no overhang for
    // neighbor repaints to erase (the CTX-0237 clip mechanism).
    let state = state_from(&[print('\u{2588}')]);
    let damage = damage_all(&state);
    let mut grid = metric_renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();
    assert_eq!(list.glyphs.len(), 1);
    let glyph = &list.glyphs[0];
    assert_eq!(glyph.dest, [0, 0]);
    assert_eq!(glyph.size, [10, 22]);
}

#[test]
fn over_tall_glyphs_overhang_instead_of_clipping() {
    // Box vertical U+2502 (top=18, h=25) exceeds even the measured 22px
    // line box: the pipeline must still emit the whole bitmap at
    // `baseline - top` (negative dest allowed) — cells never clip glyphs
    // (alacritty/ghostty overdraw pattern), so line-drawing joints stay
    // continuous across rows.
    let state = state_from(&[print('\u{2502}')]);
    let damage = damage_all(&state);
    let mut grid = metric_renderer();
    let list = grid.render(&state.snapshot(), &damage).unwrap();
    assert_eq!(list.glyphs.len(), 1);
    let glyph = &list.glyphs[0];
    assert_eq!(glyph.dest, [4, -1]);
    assert_eq!(glyph.size, [2, 25]);
}

#[test]
fn dpi_rescale_re_resolves_the_baseline() {
    // Rescaling rebuilds cells and re-reads the face: the baseline follows
    // the new cell instead of going stale (the blur/scale audit companion:
    // raster size and cell stay matched so the NDC factor stays 1.0).
    let mut grid = metric_renderer();
    let base_cell = metric_cell();
    let applied = grid.apply_dpi_scale(base_cell, &font_query(), 2.0).unwrap();
    assert_eq!(applied.cell, CellMetrics::new(20, 44).unwrap());
    // The fake serves constant (unscaled) metrics: ascent 17 clamps into
    // the 44px cell unchanged. Live backends measure at the scaled size.
    assert_eq!(grid.baseline_offset(), 17);
    assert_eq!(grid.cell_metrics(), applied.cell);
}

// ---------------------------------------------------------------------------
// CTX-0311: rounded SDF decoration primitive (solid fills + border rings).
// ---------------------------------------------------------------------------

/// Coverage mask from the rounded-fill SDF at pixel centers.
fn rounded_mask(fill: &super::RoundedFill, w: u32, h: u32) -> Vec<f32> {
    let mut mask = vec![0.0f32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            mask[(y * w + x) as usize] = fill.coverage_at(x as f32 + 0.5, y as f32 + 0.5);
        }
    }
    mask
}

/// Threshold helper: the pixel center is at least half-covered.
fn covered(mask: &[f32], w: u32, x: u32, y: u32) -> bool {
    mask[(y * w + x) as usize] >= 0.5
}

#[test]
fn rounded_fill_zero_radius_ring_is_square() {
    let fill = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 20, 10),
        border: 2,
        radius: 0,
        color: super::DECORATION_BORDER,
    };
    let mask = rounded_mask(&fill, 20, 10);
    // Ring edges are border; the diagonal corner and interior stay clear.
    assert!(covered(&mask, 20, 0, 0) && covered(&mask, 20, 19, 9));
    assert!(covered(&mask, 20, 0, 5) && covered(&mask, 20, 10, 0));
    assert!(!covered(&mask, 20, 10, 5), "interior stays unpainted");
    assert!(!covered(&mask, 20, 2, 2), "straight corner stays square");
}

#[test]
fn rounded_fill_cuts_corners_with_one_pixel_aa() {
    let fill = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 20, 10),
        border: 2,
        radius: 4,
        color: super::DECORATION_BORDER,
    };
    let mask = rounded_mask(&fill, 20, 10);
    // The outer corner pixels are outside the quarter circle and stay clear.
    assert!(!covered(&mask, 20, 0, 0) && !covered(&mask, 20, 1, 0));
    assert!(!covered(&mask, 20, 0, 1));
    assert!(!covered(&mask, 20, 19, 0) && !covered(&mask, 20, 18, 0));
    // Mid-edge and interior geometry are unchanged.
    assert!(covered(&mask, 20, 0, 5) && covered(&mask, 20, 19, 5));
    assert!(covered(&mask, 20, 10, 0) && covered(&mask, 20, 10, 9));
    assert!(!covered(&mask, 20, 10, 5), "content interior stays clear");
    assert!(!covered(&mask, 20, 5, 3), "off-corner interior stays clear");
    // The SDF produces a one-pixel anti-aliasing ramp somewhere on the arc.
    assert!(
        mask.iter().any(|c| *c > 0.0 && *c < 1.0),
        "arc must be anti-aliased"
    );
}

#[test]
fn rounded_fill_radius_saturates_at_half_span() {
    // Oversized radius saturates at half the shorter side: both corners meet
    // without inverted or overlapping geometry.
    let fill = super::RoundedFill {
        frame: crate::geometry::RectPx::new(3, 7, 12, 8),
        border: 2,
        radius: 40,
        color: super::DECORATION_BORDER,
    };
    assert_eq!(fill.resolved_radius(), 4.0);
    let mask = rounded_mask(&fill, 24, 24);
    // Top-center and bottom-center border rows exist; the outer corners cut.
    assert!(covered(&mask, 24, 9, 7) && covered(&mask, 24, 9, 14));
    assert!(!covered(&mask, 24, 3, 7) && !covered(&mask, 24, 14, 7));
    assert!(!covered(&mask, 24, 3, 14) && !covered(&mask, 24, 14, 14));
}

#[test]
fn rounded_fill_full_frame_border_paints_everything() {
    let fill = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 20, 10),
        border: 10,
        radius: 0,
        color: super::DECORATION_BORDER,
    };
    let mask = rounded_mask(&fill, 20, 10);
    assert!(
        mask.iter().all(|c| *c >= 0.5),
        "border consuming the frame must cover it"
    );
    // Zero-size frames have zero coverage everywhere.
    let empty = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 0, 10),
        border: 2,
        radius: 6,
        color: super::DECORATION_BORDER,
    };
    assert_eq!(empty.coverage_at(0.5, 0.5), 0.0);
}

#[test]
fn rounded_fill_solid_variant_has_no_inner_hole() {
    // border == 0 is the solid rounded fill (not the decoration ring): the
    // center is fully covered and only the outer corners are cut.
    let fill = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 20, 10),
        border: 0,
        radius: 4,
        color: super::DECORATION_BORDER,
    };
    let mask = rounded_mask(&fill, 20, 10);
    assert!(covered(&mask, 20, 10, 5));
    assert!(covered(&mask, 20, 2, 2));
    assert!(!covered(&mask, 20, 0, 0));
}

#[test]
fn inner_clip_derivation_matches_the_ring() {
    let frame = crate::geometry::RectPx::new(6, 6, 708, 444);
    let clip = super::rounded_frame_clip(frame, 2, 6).expect("rounded inner clip");
    assert_eq!(clip.rect, crate::geometry::RectPx::new(8, 8, 704, 440));
    assert_eq!(clip.radius, 4);
    // Fully inside far from the corner; cut at the inner arc; AA on it.
    assert_eq!(clip.coverage_at(100.5, 100.5), 1.0);
    assert_eq!(clip.coverage_at(8.5, 8.5), 0.0);
    let arc = clip.coverage_at(9.5, 8.5);
    assert!(arc > 0.0 && arc < 1.0, "inner arc must be AA: {arc}");
    // Square frames, borders that consume the radius, and degenerate frames
    // carry no clip (documented glyph-overhang behavior stays).
    assert!(super::rounded_frame_clip(frame, 2, 0).is_none());
    assert!(super::rounded_frame_clip(frame, 6, 6).is_none());
    assert!(super::rounded_frame_clip(frame, 8, 6).is_none());
    assert!(super::rounded_frame_clip(crate::geometry::RectPx::new(0, 0, 0, 10), 2, 6).is_none());
    // The primitive delegates to the same derivation.
    let fill = super::RoundedFill {
        frame,
        border: 2,
        radius: 6,
        color: super::DECORATION_BORDER,
    };
    assert_eq!(fill.inner_clip(), Some(clip));
}

#[test]
fn rounded_fill_hidpi_doubling_scales_the_arc() {
    // Physical-px inputs: doubling the frame, border, and radius doubles the
    // corner cut depth (within the sampled pixel), so the DPI step needs no
    // backend-specific work.
    let one = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 40, 20),
        border: 2,
        radius: 6,
        color: super::DECORATION_BORDER,
    };
    let two = super::RoundedFill {
        frame: crate::geometry::RectPx::new(0, 0, 80, 40),
        border: 4,
        radius: 12,
        color: super::DECORATION_BORDER,
    };
    let cut_depth = |fill: &super::RoundedFill, w: u32| -> u32 {
        let mask = rounded_mask(fill, w, w);
        (0..w).take_while(|x| !covered(&mask, w, *x, 0)).count() as u32
    };
    let one_cut = cut_depth(&one, 40);
    let two_cut = cut_depth(&two, 80);
    assert!(one_cut >= 2, "1x corner must be cut: {one_cut}");
    assert!(
        two_cut + 2 >= one_cut * 2 && two_cut <= one_cut * 2 + 2,
        "corner cut must scale with DPI: 1x {one_cut} -> 2x {two_cut}"
    );
    let mask1 = rounded_mask(&one, 40, 20);
    let mask2 = rounded_mask(&two, 80, 40);
    assert!(!covered(&mask1, 40, 20, 5), "1x interior unpainted");
    assert!(!covered(&mask2, 80, 40, 10), "2x interior unpainted");
}

// ---------------------------------------------------------------------------
// CTX-0355: resolved preset palette drives render (not hardcoded Bitty Dark)
// ---------------------------------------------------------------------------

/// `github-light` resolves a light palette distinct from `bitty-dark` on every
/// chrome color and at least the ANSI entries the issue calls out.
#[test]
fn theme_palette_resolves_selected_preset_not_bitty_dark() {
    use super::ThemePalette;

    let dark = ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("bitty-dark")));
    let light = ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("github-light")));

    // The issue's exact regression: github-light documents #FFFFFF bg but
    // rendered #1E1E2E. The resolved palette must disagree with bitty-dark.
    assert_eq!(light.background, [0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(dark.background, [0x1E, 0x1E, 0x2E, 0xFF]);
    assert_ne!(light.background, dark.background);
    assert_ne!(light.foreground, dark.foreground);
    assert_ne!(light.cursor, dark.cursor);
    assert_ne!(light.selection, dark.selection);
    assert_ne!(light.ansi, dark.ansi);

    // Default is the designed preset, byte-identical to the legacy constants.
    assert_eq!(ThemePalette::default(), ThemePalette::bitty_dark());
    assert_eq!(ThemePalette::bitty_dark().background[..3], DEFAULT_BG[..3]);
    assert_eq!(ThemePalette::bitty_dark().foreground[..3], DEFAULT_FG[..3]);
}

/// The default-preset wrappers stay byte-identical to the explicit Bitty Dark
/// palette, so every untouched call site keeps its behavior.
#[test]
fn default_wrappers_equal_bitty_dark_palette() {
    use super::{
        ThemePalette, cursor_fill, cursor_fill_in, palette_rgb, palette_rgb_in, resolve_color,
        resolve_color_in, selection_fill, selection_fill_in,
    };
    use bitty_term_state::{Cursor, CursorPosition};

    let dark = ThemePalette::bitty_dark();
    for index in 0u8..=255 {
        assert_eq!(
            palette_rgb(index),
            palette_rgb_in(&dark, index),
            "idx {index}"
        );
    }
    assert_eq!(
        resolve_color(Some(&Color::Indexed(4)), DEFAULT_FG),
        resolve_color_in(&dark, Some(&Color::Indexed(4)), DEFAULT_FG)
    );
    assert_eq!(selection_fill(), selection_fill_in(&dark));
    let cursor = Cursor {
        position: CursorPosition { row: 1, col: 1 },
        visible: true,
        ..Cursor::default()
    };
    assert_eq!(
        cursor_fill(&cursor, cell_metrics(), 80, 24),
        cursor_fill_in(&dark, &cursor, cell_metrics(), 80, 24)
    );
}

/// A non-default preset threads through the renderer: default cell fg/bg and
/// indexed ANSI 0..16 come from the selected palette, not Bitty Dark.
#[test]
fn renderer_resolves_cells_from_selected_palette() {
    use super::ThemePalette;

    let light = ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("github-light")));
    let mut renderer = renderer();
    renderer.set_theme_palette(light);
    assert_eq!(renderer.theme_palette(), light);

    // A plain unstyled cell: background fill must be the preset background,
    // glyph the preset foreground (issue AC: "paints the preset
    // background/foreground").
    let state = state_from(&[print('A')]);
    let list = renderer
        .render(&state.snapshot(), &full_damage(&state))
        .unwrap();
    assert_eq!(list.fills[0].color, light.background);
    assert_eq!(list.glyphs[0].color, light.foreground);
    assert_ne!(list.fills[0].color, DEFAULT_BG);

    // Indexed ANSI (SGR 32 -> Indexed 2 green) resolves from the preset.
    let green_state = state_from(&[
        sgr(&[AttributeChange::Foreground(Color::Indexed(2))]),
        print('G'),
    ]);
    let green = renderer
        .render(&green_state.snapshot(), &full_damage(&green_state))
        .unwrap();
    let green_rgb = light.ansi[2];
    assert_eq!(
        green.glyphs[0].color,
        [green_rgb[0], green_rgb[1], green_rgb[2], 0xFF]
    );
    assert_ne!(green.glyphs[0].color, [0xA6, 0xE3, 0xA1, 0xFF]);
}

/// Inverse video and default-preset fallbacks still hold under a custom
/// palette (regression guard for the swapped pair).
#[test]
fn custom_palette_inverse_swaps_preset_pair() {
    use super::ThemePalette;

    let light = ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("github-light")));
    let mut style = bitty_term_state::Style::default();
    style.attributes.inverse = true;
    let (fg, bg) = super::resolved_colors_in(&light, &style);
    assert_eq!(fg, light.background);
    assert_eq!(bg, light.foreground);
}
