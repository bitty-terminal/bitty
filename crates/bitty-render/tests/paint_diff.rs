//! Differential headless paint check: incremental damage paint vs full redraw.
//!
//! Terminal truth is exonerated separately (replay plus a per-action damage
//! audit); this harness covers the paint side of the "previous frame survives
//! under the new rows" class. It drives the production plan/place/cache
//! pipeline (`GridRenderer::render`) and the headless compositor
//! (`Surface::headless_present_retained`) with the same deterministic fake
//! rasterizer the other headless render tests use:
//!
//! 1. feed a scripted chunk sequence into one `State` (100x30 by default);
//! 2. after each chunk, paint **only that chunk's damage** onto one retained
//!    headless surface (the `headless_present_retained` seam);
//! 3. after each chunk, paint the same snapshot **from scratch** with a full
//!    extent damage rectangle onto a fresh headless surface;
//! 4. compare the RGBA buffers byte for byte.
//!
//! The invariant is documented in `GridRenderer::place_cell`: every visited
//! cell repaints its background, so the union of incremental damage frames
//! must equal one full redraw. A mismatch therefore names either a damage
//! rectangle the paint path never visited or a region whose paint was
//! dropped; the report carries the chunk index, the first differing pixel,
//! and its cell.
//!
//! Determinism: one fake rasterizer with character-derived bitmaps, the
//! default theme palette on both sides, fixed cell metrics, no cursor paint
//! (the grid renderer emits no cursor overlay), no animation, and no
//! time-dependent state. Chunks are fixed at construction; the capture path
//! is split at a fixed byte stride.
//!
//! Deliberately not covered by this harness: compositor-level occlusion,
//! window-level swap-chain reuse, and plugin band repaint (overlay layers
//! composed by the runtime, not by `GridRenderer::render`), plus glyph
//! overhang outside a damaged cell (the fake rasterizer stays inside its
//! cell box).

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::gpu::Surface;
use bitty_render::grid::{CellMetrics, GridRenderer};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, State};
use bitty_vt::Parser;

const COLS: usize = 100;
const ROWS: usize = 30;
const CELL_WIDTH: u32 = 8;
const CELL_HEIGHT: u32 = 16;
/// Bound on chunk count so a wrong fixture path or an edited script cannot
/// turn the harness into an unbounded frame loop.
const MAX_CHUNKS: usize = 64;
/// Fixed split stride for the recorded capture.
const CAPTURE_CHUNK_BYTES: usize = 256;

/// The committed Antigravity help-page capture (same bytes as the recording
/// source; committed by the VT replay task so both sides of the presentation
/// boundary can replay it).
const AGY_HELP: &[u8] = include_bytes!("../../bitty-term-state/tests/data/agy-help-100x30.raw");

// ---------------------------------------------------------------------------
// Deterministic rasterizer (same contract shape as `wgpu_surface.rs`)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeRasterizer {
    next: u64,
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(key.character) % 3 + 6) as i32;
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
            .expect("fixture bitmap matches its metrics"),
        ))
    }
}

fn cell() -> CellMetrics {
    CellMetrics::new(CELL_WIDTH, CELL_HEIGHT).expect("fixture cell metrics are non-zero")
}

fn physical(extent: bitty_render::ExtentPx) -> PhysicalSize {
    PhysicalSize::new(extent.width, extent.height)
}

fn renderer() -> GridRenderer<FakeRasterizer> {
    let query = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(FakeRasterizer { next: 0 }, &query, cell())
        .expect("fixture renderer fits the atlas")
}

// ---------------------------------------------------------------------------
// Chunks and the differential run
// ---------------------------------------------------------------------------

/// One scripted step: an optional grid resize followed by a byte chunk.
struct Chunk {
    label: String,
    resize: Option<(usize, usize)>,
    bytes: Vec<u8>,
}

impl Chunk {
    fn new(label: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            label: label.into(),
            resize: None,
            bytes: bytes.into(),
        }
    }

    fn resized(mut self, cols: usize, rows: usize) -> Self {
        self.resize = Some((cols, rows));
        self
    }
}

/// Where the incremental paint first stops matching the full redraw.
#[derive(Debug)]
struct Divergence {
    chunk: usize,
    label: String,
    pixel: usize,
    x: u32,
    y: u32,
    row: u32,
    col: u32,
    incremental: [u8; 4],
    reference: [u8; 4],
}

impl Divergence {
    fn first(
        chunk: usize,
        label: &str,
        incremental: &[u8],
        reference: &[u8],
        width: u32,
        cell: CellMetrics,
    ) -> Option<Self> {
        let mismatch = incremental
            .chunks_exact(4)
            .zip(reference.chunks_exact(4))
            .position(|(left, right)| left != right)?;
        let x = (mismatch as u32) % width;
        let y = (mismatch as u32) / width;
        let mut incremental_px = [0u8; 4];
        let mut reference_px = [0u8; 4];
        incremental_px.copy_from_slice(&incremental[mismatch * 4..mismatch * 4 + 4]);
        reference_px.copy_from_slice(&reference[mismatch * 4..mismatch * 4 + 4]);
        Some(Self {
            chunk,
            label: label.to_string(),
            pixel: mismatch,
            x,
            y,
            row: y / cell.height,
            col: x / cell.width,
            incremental: incremental_px,
            reference: reference_px,
        })
    }

    fn message(&self) -> String {
        format!(
            "incremental paint diverged from the full redraw at chunk {} ({}): \
             first differing pixel {} at ({}, {}) in cell row {}, col {} \
             (incremental {:02x?}, full {:02x?})",
            self.chunk,
            self.label,
            self.pixel,
            self.x,
            self.y,
            self.row,
            self.col,
            self.incremental,
            self.reference,
        )
    }
}

/// What one differential run exercised. `partial` counts damage-only
/// presents; `full` counts the production full-rebuild fallbacks (resize,
/// scroll) that cannot use retained pixels across an extent/region change.
#[derive(Debug, Default)]
struct Summary {
    chunks: usize,
    actions: usize,
    partial: usize,
    full: usize,
    glyphs: usize,
}

/// Runs the scripted chunks and checks the incremental buffer against a full
/// redraw after every chunk. `fault` injects one frame whose damage is dropped
/// (a negative control for the harness itself).
fn run(
    chunks: &[Chunk],
    initial: (usize, usize),
    fault: Option<usize>,
) -> Result<Summary, Divergence> {
    assert!(
        chunks.len() <= MAX_CHUNKS,
        "chunk count {} exceeds the harness bound {MAX_CHUNKS}",
        chunks.len()
    );
    let mut state = State::new();
    let mut parser = Parser::new();
    let mut incremental = renderer();
    let mut full = renderer();
    let cell = cell();
    let mut extent = cell.extent_for(initial.0, initial.1);
    state.resize(initial.0, initial.1);

    // Seed the retained surface with a full paint of the initial (blank)
    // state, exactly like the first frame after realization.
    let mut surface =
        Surface::headless(physical(extent)).expect("headless surface fits the extent cap");
    let snapshot = state.snapshot();
    let seed = incremental
        .render(&snapshot, &full_damage(&snapshot))
        .expect("seed render");
    surface
        .headless_present(
            &seed,
            Some((incremental.atlas_texels(), incremental.atlas_dims())),
        )
        .expect("seed present");

    let mut summary = Summary {
        chunks: chunks.len(),
        ..Summary::default()
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let generation_before = state.generation();
        if let Some((cols, rows)) = chunk.resize {
            state.resize(cols, rows);
        }
        parser.advance(&chunk.bytes, |action| {
            state.apply(&action);
            summary.actions += 1;
        });
        let generation_after = state.generation();
        if generation_after == generation_before {
            continue;
        }

        let snapshot = state.snapshot();
        let next_extent = cell.extent_for(snapshot.width, snapshot.height);
        let resized = next_extent != extent;
        if resized {
            // Production forces a full redraw after a resize; the retained
            // pixels describe a different extent and cannot be reused.
            surface = Surface::headless(physical(next_extent))
                .expect("resized headless surface fits the extent cap");
            extent = next_extent;
        }

        let regions = state.damage_since(generation_before);
        let has_scroll = regions
            .iter()
            .any(|region| matches!(region, DamagedRegion::Scroll { .. }));
        let full_rebuild = resized || has_scroll;
        if full_rebuild {
            summary.full += 1;
        } else {
            summary.partial += 1;
        }
        let damage = if full_rebuild || fault == Some(index) {
            // Resize and scroll frames are the production full-rebuild
            // fallbacks; the fault frame keeps the same full-damage shape but
            // drops it before rendering, so the harness can prove it notices.
            if fault == Some(index) {
                Damage {
                    generation: generation_after,
                    regions: Vec::new().into_boxed_slice(),
                }
            } else {
                full_damage(&snapshot)
            }
        } else {
            Damage {
                generation: generation_after,
                regions: regions.into_boxed_slice(),
            }
        };

        let painted = incremental
            .render(&snapshot, &damage)
            .expect("incremental render");
        let incremental_pixels = if resized {
            surface
                .headless_present(
                    &painted,
                    Some((incremental.atlas_texels(), incremental.atlas_dims())),
                )
                .expect("resized present")
        } else {
            surface
                .headless_present_retained(
                    &painted,
                    Some((incremental.atlas_texels(), incremental.atlas_dims())),
                )
                .expect("retained present")
        };
        debug_assert!(incremental_pixels.headless);
        summary.glyphs += incremental_pixels.glyphs;

        // Full-redraw reference: a fresh surface and a full extent damage
        // rectangle for the same snapshot.
        let reference_list = full
            .render(&snapshot, &full_damage(&snapshot))
            .expect("full redraw render");
        let reference_surface = Surface::headless(physical(extent))
            .expect("reference headless surface fits the extent cap");
        reference_surface
            .headless_present(
                &reference_list,
                Some((full.atlas_texels(), full.atlas_dims())),
            )
            .expect("reference present");
        let incremental_rgba = surface.headless_rgba().expect("incremental pixels");
        let reference_rgba = reference_surface.headless_rgba().expect("reference pixels");
        if let Some(divergence) = Divergence::first(
            index,
            &chunk.label,
            &incremental_rgba,
            &reference_rgba,
            extent.width,
            cell,
        ) {
            return Err(divergence);
        }
    }
    Ok(summary)
}

/// A full-extent grid damage rectangle, which `plan_frame` promotes to
/// `FrameMode::Full`; every cell is visited and repaints its background.
fn full_damage(snapshot: &bitty_term_state::Snapshot) -> Damage {
    let rows = snapshot.height.min(usize::from(u16::MAX)) as u16;
    let cols = snapshot.width.min(usize::from(u16::MAX)) as u16;
    Damage {
        generation: snapshot.generation,
        regions: vec![DamagedRegion::Grid(DamageRect::full(rows, cols))].into_boxed_slice(),
    }
}

// ---------------------------------------------------------------------------
// Scripted sequence: two-frame overwrite, ECH/EL/ED/CUP, scroll, resize
// ---------------------------------------------------------------------------

/// Full-width marker row used to seed the "previous frame".
fn old_row(row: usize) -> String {
    let mut line = format!("\x1b[{row};1HOLD-{row:02}-");
    // The status prefix is seven columns, so the row fills all 100.
    line.extend(std::iter::repeat_n('=', COLS - 7));
    line
}

fn synthetic_chunks() -> Vec<Chunk> {
    // Frame 1: a complete "previous" screen, including the exact tab-row text
    // the Antigravity help page must replace.
    let mut old = String::from("\x1b[2J\x1b[H\x1b[?25l");
    for row in 1..=ROWS {
        old.push_str(&old_row(row));
    }
    old.push_str("\x1b[10;1Hgeneral commands shortcuts");

    // Frame 2a: overwrite the title with a shorter string (EL clears the old
    // tail), repaint the tab row, and write the left band then ECH its tail.
    let frame_2a = concat!(
        "\x1b[1;1HKeyboard Shortcuts\x1b[K",
        "\x1b[3;1Hgeneral commands shortcuts",
        "\x1b[10;1H> /\x1b[29X",
    );

    // Frame 2b: absolute CUP to the description column, two rows in the agy
    // shape. The ECH from 2a and these writes land in separate presents.
    let frame_2b = concat!(
        "\x1b[10;33H\x1b[m  Open slash commands",
        "\x1b[11;1H  alt+j\x1b[25X\x1b[11;33H\x1b[m  Manage subagent",
        "\x1b[12;1H\x1b[K",
    );

    // Frame 3: reverse/forward motion inside a DECSTBM region, then a repaint.
    // Any chunk carrying Scroll damage takes the full-rebuild fallback.
    let frame_3 = concat!(
        "\x1b[5;12r",
        "\x1b[12;1H\n\n\n",
        "\x1b[r",
        "\x1b[5;1Hscrolled region\x1b[K",
    );

    // Frame 4: shrink between frames, then repaint the new screen from ED.
    let frame_4 = concat!(
        "\x1b[2J\x1b[H",
        "\x1b[1;1Hresized frame\x1b[K",
        "\x1b[2;1Hnarrow helper\x1b[K",
        "\x1b[10;1H> /\x1b[29X\x1b[10;33H  Open slash commands",
    );

    // Frame 5: grow back to the original size and overwrite with ECH again.
    let frame_5 = concat!(
        "\x1b[2J\x1b[H",
        "\x1b[1;1HKeyboard Shortcuts\x1b[K",
        "\x1b[3;1Hgeneral commands shortcuts",
        "\x1b[10;1H> /\x1b[29X\x1b[10;33H  Open slash commands",
        "\x1b[11;1H  alt+j\x1b[25X\x1b[11;33H  Manage subagent",
    );

    vec![
        Chunk::new("old-screen", old),
        Chunk::new("help-left-band-ech", frame_2a),
        Chunk::new("help-description-cup", frame_2b),
        Chunk::new("decstbm-scroll", frame_3),
        Chunk::new("resize-88x24", frame_4).resized(88, 24),
        Chunk::new("resize-100x30", frame_5).resized(COLS, ROWS),
    ]
}

fn capture_chunks(bytes: &[u8]) -> Vec<Chunk> {
    bytes
        .chunks(CAPTURE_CHUNK_BYTES)
        .enumerate()
        .map(|(index, slice)| Chunk::new(format!("agy-help[{index}]"), slice))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn scripted_incremental_paint_matches_full_redraw() {
    let chunks = synthetic_chunks();
    let summary = run(&chunks, (COLS, ROWS), None)
        .unwrap_or_else(|divergence| panic!("{}", divergence.message()));
    // The ECH/CUP overwrite frames are the point of this script; a run that
    // only exercised full rebuilds would not test the damage paint path.
    assert!(
        summary.partial >= 3,
        "expected damage-only overwrite frames: {summary:?}"
    );
    // Scroll and the two resizes must take the full-rebuild fallback.
    assert!(
        summary.full >= 3,
        "expected scroll/resize full-rebuild frames: {summary:?}"
    );
    assert!(
        summary.glyphs > 0,
        "the script must paint text, not only backgrounds: {summary:?}"
    );
    eprintln!(
        "synthetic script: {} chunks, {} actions, {} damage-only, {} full-rebuild, {} glyphs",
        summary.chunks, summary.actions, summary.partial, summary.full, summary.glyphs
    );
}

#[test]
fn recorded_agy_help_incremental_paint_matches_full_redraw() {
    let chunks = capture_chunks(AGY_HELP);
    let summary = run(&chunks, (COLS, ROWS), None)
        .unwrap_or_else(|divergence| panic!("{}", divergence.message()));
    // The capture scrolls the bottom row, so some chunks take the full
    // rebuild fallback; record the split in the failure evidence.
    assert!(
        summary.partial > 0,
        "capture never exercised a damage-only frame: {summary:?}"
    );
    assert!(
        summary.glyphs > 0,
        "the capture must paint text, not only backgrounds: {summary:?}"
    );
    eprintln!(
        "agy capture: {} chunks, {} actions, {} damage-only, {} full-rebuild, {} glyphs",
        summary.chunks, summary.actions, summary.partial, summary.full, summary.glyphs
    );
}

/// Negative control: dropping one frame's damage must be reported at that
/// frame, with the first differing pixel attributed to a cell. Without this,
/// a green differential run would not prove the comparison has teeth.
#[test]
fn harness_reports_a_dropped_damage_rect() {
    let chunks = synthetic_chunks();
    let fault = 1; // the frame that overwrites the old screen
    let divergence = run(&chunks, (COLS, ROWS), Some(fault))
        .expect_err("dropping a frame's damage must diverge");
    assert_eq!(divergence.chunk, fault, "{divergence:?}");
    assert_eq!(
        (divergence.row, divergence.col),
        (0, 0),
        "first stale cell should be the overwritten title: {divergence:?}"
    );
    assert_ne!(divergence.incremental, divergence.reference);
}
