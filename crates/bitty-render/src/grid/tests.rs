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
/// one-shot error switch simulates upstream failures.
struct FakeRasterizer {
    next_id: u64,
    blank_chars: Vec<char>,
    fail_next: bool,
}

impl FakeRasterizer {
    fn new() -> Self {
        Self {
            next_id: 0,
            blank_chars: vec![' ', '\t'],
            fail_next: false,
        }
    }

    fn bitmap_for(character: char) -> GlyphBitmap {
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
