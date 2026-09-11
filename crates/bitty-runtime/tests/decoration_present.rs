#![forbid(unsafe_code)]
//! Core-owned live present wiring for workspace decoration (CTX-0294,
//! stage-2 radius CTX-0311).
//!
//! The accepted workspace-compositor contract (spec CTX-0118) owns
//! `decoration.gaps_in/gaps_out/border/radius` in logical pixels. CTX-0292
//! landed the config + solver; CTX-0294 the live present wiring; CTX-0311
//! replaced the minimal scanline ring with a rounded SDF primitive
//! (`RoundedFill`) plus inner-arc glyph clipping. These tests pin the live
//! present wiring:
//!
//! - `Runtime::present_frames` composes the decoration with the CTX-0177
//!   cell gaps at the live DPI scale and derives the per-View content grid
//!   (fractional-cell frames: the sub-cell remainder stays background);
//! - `tick` paints `gaps_out`/`gaps_in` bands as clear background, the
//!   `border` ring in `DECORATION_BORDER` as one rounded SDF primitive
//!   (`PresentStats::rounded_fills`), and the content inside the border;
//!   `radius` clips the frame corners and the inner arc clips glyphs;
//! - `bitty --safe` decoration (`0/0/1/0`) paints no gaps, a 1px square
//!   border, and square corners;
//! - decorated hit testing uses the frame (`cursor_to_present_cell`) and
//!   the global mapping subtracts the decoration outer inset.
//!
//! Every assertion is headless RGBA or pure frame math. Set
//! `CTX0294_EVIDENCE_DIR` to dump raw `.rgba`/`.dims` frames for PNG
//! conversion (live/readback evidence; never required for the tests).

use bitty_platform::CursorPosition;
use bitty_runtime::{Decoration, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};
use bitty_ui::CellPos;

fn runtime_with(decoration: Decoration) -> Runtime {
    Runtime::new(RuntimeConfig {
        decoration,
        ..RuntimeConfig::default()
    })
    .expect("decorated runtime must build")
}

fn single_leaf(rt: &mut Runtime) {
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn surface_width(rt: &Runtime) -> usize {
    let extent = rt.config().window_extent();
    usize::try_from(extent.width()).expect("width fits usize")
}

fn dump_evidence(name: &str, rgba: &[u8], width: u32, height: u32) {
    let Some(dir) = std::env::var_os("CTX0294_EVIDENCE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("{name}.rgba")), rgba);
    let _ = std::fs::write(
        dir.join(format!("{name}.dims")),
        format!("{width}x{height}\n"),
    );
}

#[test]
fn present_frames_apply_logical_decoration_at_live_scale() {
    // Unified 6/6/2/6/6 at 9x19 cells, 80x24 container: frame inset 6px,
    // content inset by border 2 + content inset 6 = 8px more; content grid
    // floors the remainder.
    let mut rt = runtime_with(Decoration::default());
    single_leaf(&mut rt);
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 1);
    let frame = frames[0];
    assert_eq!(frame.view, ViewId::new(1));
    assert_eq!(frame.frame.x, 6);
    assert_eq!(frame.frame.y, 6);
    assert_eq!(frame.frame.width, 708);
    assert_eq!(frame.frame.height, 444);
    assert_eq!(frame.content.x, 14);
    assert_eq!(frame.content.y, 14);
    assert_eq!(frame.content.width, 692);
    assert_eq!(frame.content.height, 428);
    // 692 = 76 * 9 + 8 remainder; 428 = 22 * 19 + 10.
    assert_eq!(frame.cols, 76);
    assert_eq!(frame.rows, 22);
    assert_eq!(frame.border, 2);
    assert_eq!(frame.radius, 6);
}

#[test]
fn live_present_paints_gap_bands_border_ring_and_fractional_remainder() {
    let mut rt = runtime_with(Decoration::default());
    single_leaf(&mut rt);
    let stats = rt.tick().expect("first tick presents");
    assert!(stats.headless);
    // CTX-0311: one rounded SDF ring primitive replaces the CTX-0294 scanline
    // fills; gaps and content stay plain fills.
    assert_eq!(stats.rounded_fills, 1, "one decoration ring primitive");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = surface_width(&rt);
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    assert_eq!(pad, 8);
    let bg = bitty_render::grid::DEFAULT_BG;
    let border = bitty_render::grid::DECORATION_BORDER;
    let y = pad + 200;
    // gaps_out band: 6px of clear background inside the Window.
    for x in [pad + 1, pad + 3, pad + 5] {
        assert_eq!(
            probe(&rgba, width, x, y),
            bg,
            "gaps_out must be bg at x={x}"
        );
    }
    // Border ring: the 2px band after the outer gap.
    assert_eq!(probe(&rgba, width, pad + 6, y), border, "border ring left");
    assert_eq!(
        probe(&rgba, width, pad + 7, y),
        border,
        "border ring left 2"
    );
    assert_eq!(
        probe(&rgba, width, pad + 6 + 708 - 1, y),
        border,
        "border ring right"
    );
    // The border+inset band is background: content starts at pad + 14
    // (gaps_out 6 + border 2 + content_inset 6).
    assert_eq!(probe(&rgba, width, pad + 14, y), bg, "content start");
    // Sub-cell remainder: content width 692 = 76 * 9 + 8; the trailing 8px
    // band stays background.
    assert_eq!(
        probe(&rgba, width, pad + 14 + 684 + 1, y),
        bg,
        "fractional-cell remainder stays bg"
    );
    let extent = rt.config().window_extent();
    dump_evidence(
        "01-default-decoration",
        &rgba,
        extent.width(),
        extent.height(),
    );
}

#[test]
fn radius_cuts_frame_corners_and_zero_radius_keeps_them_square() {
    let pad = 8usize;
    let y_corner = pad + 6;
    let x_corner = pad + 6;
    // radius = 6: the outer frame corner is outside the quarter circle and
    // stays clear background.
    let mut rounded = runtime_with(Decoration::new(0, 6, 2, 6, 0));
    single_leaf(&mut rounded);
    let stats = rounded.tick().expect("tick presents");
    assert_eq!(stats.rounded_fills, 1, "rounded frame uses the SDF ring");
    let rgba = rounded.headless_rgba().expect("rgba");
    let width = surface_width(&rounded);
    let bg = bitty_render::grid::DEFAULT_BG;
    assert_eq!(
        probe(&rgba, width, x_corner, y_corner),
        bg,
        "radius must cut the frame corner"
    );
    assert_eq!(
        probe(&rgba, width, x_corner + 1, y_corner),
        bg,
        "radius cut is at least one pixel wide"
    );
    let extent = rounded.config().window_extent();
    dump_evidence("02-radius-6-corner", &rgba, extent.width(), extent.height());

    // radius = 0: the same corner carries the border color (the SDF ring
    // degenerates to a square ring, still one rounded primitive).
    let mut square = runtime_with(Decoration::new(0, 6, 2, 0, 0));
    single_leaf(&mut square);
    let stats = square.tick().expect("tick presents");
    assert_eq!(
        stats.rounded_fills, 1,
        "square ring still uses the primitive"
    );
    let rgba = square.headless_rgba().expect("rgba");
    assert_eq!(
        probe(&rgba, width, x_corner, y_corner),
        bitty_render::grid::DECORATION_BORDER,
        "square corner must be border"
    );
    let extent = square.config().window_extent();
    dump_evidence("03-radius-0-corner", &rgba, extent.width(), extent.height());
}

#[test]
fn safe_mode_decoration_paints_zero_gaps_one_pixel_border() {
    // Accepted spec rule 5: --safe starts with 0/0/1/0.
    let mut rt = runtime_with(Decoration::SAFE);
    single_leaf(&mut rt);
    let frames = rt.present_frames();
    assert_eq!(frames[0].frame.x, 0);
    assert_eq!(frames[0].frame.y, 0);
    assert_eq!(frames[0].frame.width, 720);
    assert_eq!(frames[0].content.x, 1);
    assert_eq!(frames[0].border, 1);
    assert_eq!(frames[0].radius, 0);
    rt.tick().expect("tick presents");
    let rgba = rt.headless_rgba().expect("rgba");
    let width = surface_width(&rt);
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let bg = bitty_render::grid::DEFAULT_BG;
    // No outer gap: padding then the 1px border immediately.
    assert_eq!(
        probe(&rgba, width, pad, pad + 100),
        bitty_render::grid::DECORATION_BORDER
    );
    assert_eq!(probe(&rgba, width, pad + 1, pad + 100), bg);
    let extent = rt.config().window_extent();
    dump_evidence("04-safe-mode", &rgba, extent.width(), extent.height());
}

#[test]
fn cell_gaps_compose_with_px_decoration() {
    // CTX-0177 cell gaps stay supported: outer inset and sibling band gain
    // the decoration px on top of the cell geometry.
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 1,
        decoration: Decoration::default(),
        ..RuntimeConfig::default()
    })
    .expect("composed runtime must build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 2);
    // Outer inset x = 1 cell (9px) + 6px decoration.
    assert_eq!(frames[0].frame.x, 15);
    // Outer inset y = 1 cell (19px) + 6px decoration.
    assert_eq!(frames[0].frame.y, 25);
    // Sibling band = 2 cells (18px) + 6px decoration (unified gaps_in).
    assert_eq!(
        (i64::from(frames[1].frame.x) - frames[0].frame.right_exclusive()) as u32,
        24,
        "cell gaps and px decoration compose additively"
    );
    rt.tick().expect("tick presents");
    let rgba = rt.headless_rgba().expect("rgba");
    let width = surface_width(&rt);
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let bg = bitty_render::grid::DEFAULT_BG;
    // Band pixel between the sibling frames is background.
    let band_x = pad + usize::try_from(frames[0].frame.x).expect("frame x fits") + 708 / 2 - 11 + 3;
    assert_eq!(
        probe(&rgba, width, band_x, pad + 100),
        bg,
        "composed sibling band must be bg"
    );
    let extent = rt.config().window_extent();
    dump_evidence(
        "05-cell-gaps-composed",
        &rgba,
        extent.width(),
        extent.height(),
    );
}

#[test]
fn decorated_hit_testing_uses_frame_for_hit_and_content_for_cells() {
    let mut rt = runtime_with(Decoration::default());
    single_leaf(&mut rt);
    let pad = rt.window_padding_physical() as f64;
    // Content starts at gaps_out 6 + border 2 + content_inset 6 = 14px.
    // A position inside the content maps to the content-local cell.
    let inside = CursorPosition {
        x: pad + 14.0 + 9.0 * 2.0 + 1.0,
        y: pad + 14.0 + 19.0 * 4.0 + 1.0,
    };
    assert_eq!(
        rt.cursor_to_present_cell(inside),
        Some((ViewId::new(1), CellPos::new(4, 2)))
    );
    // The global mapping subtracts the outer gap plus border plus inset.
    assert_eq!(rt.cursor_to_cell(inside), CellPos::new(4, 2));
    // A position over the border ring still hits the frame and clamps to
    // the first content cell (spec rule 4: radius never widens hit testing
    // beyond the frame).
    let on_border = CursorPosition {
        x: pad + 7.0,
        y: pad + 14.0 + 19.0 * 4.0 + 1.0,
    };
    assert_eq!(
        rt.cursor_to_present_cell(on_border),
        Some((ViewId::new(1), CellPos::new(4, 0)))
    );
    // A position in the outer gap band belongs to no pane.
    let in_gap = CursorPosition {
        x: pad + 3.0,
        y: pad + 14.0 + 19.0 * 4.0 + 1.0,
    };
    assert_eq!(rt.cursor_to_present_cell(in_gap), None);
    // Over the window padding band: no pane.
    assert_eq!(
        rt.cursor_to_present_cell(CursorPosition { x: 2.0, y: 2.0 }),
        None
    );
}

#[test]
fn zero_decoration_is_the_undecorated_fast_path() {
    let mut rt = runtime_with(Decoration::ZERO);
    single_leaf(&mut rt);
    let frames = rt.present_frames();
    assert_eq!(frames[0].frame.x, 0);
    assert_eq!(frames[0].frame.width, 720);
    assert_eq!(frames[0].content, frames[0].frame);
    assert_eq!(frames[0].cols, 80);
    assert_eq!(frames[0].rows, 24);
    rt.tick().expect("tick presents");
    let rgba = rt.headless_rgba().expect("rgba");
    let width = surface_width(&rt);
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    // No border band: content starts immediately at the padding edge.
    let bg = bitty_render::grid::DEFAULT_BG;
    assert_eq!(probe(&rgba, width, pad, pad + 100), bg);
    assert_ne!(
        probe(&rgba, width, pad, pad + 100),
        bitty_render::grid::DECORATION_BORDER
    );
}
