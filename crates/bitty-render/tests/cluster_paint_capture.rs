//! Rasterized before/after capture for multi-glyph grapheme clusters
//! (CTX-1609, issue #1767).
//!
//! Prints one row per cluster — committed grid cells, shaped glyphs, painted
//! glyphs, and a pixel digest of the composited frame — and asserts the two
//! flags composite to different pixels.
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --features sw-fallback \
//!     --test cluster_paint_capture -- --nocapture
//! ```

#![cfg(feature = "sw-fallback")]

use bitty_config::types::LigaturePolicy;
use bitty_render::software::SurfaceRgba;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphRasterizer, GridRenderer,
    RasterKey, RunAttrs, ShapedCluster, SwashSingle,
};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, State, TerminalAction};
use bitty_vt::GraphemeCell;

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const POINT_SIZE: f32 = 12.0;

fn query() -> FontQuery {
    FontQuery {
        family: "Noto Sans".to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

fn live_chain() -> Option<FallbackRasterizer<SwashSingle>> {
    if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run the live-font capture");
        return None;
    }
    let inner = SwashSingle::new().ok()?;
    let mut raster = FallbackRasterizer::with_default_chain(inner);
    raster.load_font(&query()).ok()?;
    Some(raster)
}

fn warm(raster: &mut FallbackRasterizer<SwashSingle>, scalar: char) {
    let primary = raster.fonts()[0];
    let key = RasterKey::new(scalar, primary, POINT_SIZE).expect("valid raster key");
    let _ = raster.rasterize(key);
}

fn state_with_text(text: &str) -> State {
    let mut state = State::new();
    for ch in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
    }
    state
}

fn full_damage(state: &State) -> Damage {
    let snapshot = state.snapshot();
    Damage {
        generation: snapshot.generation,
        regions: vec![DamagedRegion::Grid(DamageRect::full(
            snapshot.height as u16,
            snapshot.width as u16,
        ))]
        .into_boxed_slice(),
    }
}

fn shape(raster: &mut FallbackRasterizer<SwashSingle>, text: &str) -> Vec<ShapedCluster> {
    let chain: Vec<FontId> = raster.fonts().to_vec();
    raster
        .inner_mut()
        .shape_run(
            text,
            &chain,
            &RunAttrs {
                features: Vec::new(),
                point_size: POINT_SIZE,
            },
        )
        .expect("cluster shapes")
}

/// One cluster through the real shaped pipeline: committed cells, shaped and
/// painted glyph counts, and the composited pixels.
fn capture(
    mut raster: FallbackRasterizer<SwashSingle>,
    scalar: char,
    text: &str,
) -> (usize, usize, usize, usize, Vec<u8>) {
    warm(&mut raster, scalar);
    let shaped = shape(&mut raster, text).len();
    let state = state_with_text(text);
    let snapshot = state.snapshot();
    let committed = usize::from(snapshot.cells[0].width).max(1);
    let damage = full_damage(&state);
    let mut renderer = GridRenderer::new(
        raster,
        &query(),
        CellMetrics::new(10, 22).expect("cell metrics"),
    )
    .expect("renderer builds");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let unshaped = _warm.glyphs.len();
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("shaped render succeeds");
    let painted = list.glyphs.len();
    let extent = renderer
        .cell_metrics()
        .extent_for(snapshot.width, snapshot.height.min(1));
    let mut surface = SurfaceRgba::try_new(extent.width, extent.height).expect("surface");
    surface.clear(bitty_render::grid::DEFAULT_BG);
    bitty_render::grid::composite_frame(&renderer, &list, &mut surface).expect("frame composites");
    (
        committed,
        shaped,
        unshaped,
        painted,
        surface.as_bytes().to_vec(),
    )
}

fn digest(bytes: &[u8]) -> String {
    // FNV-1a over the RGBA bytes, hex-truncated: enough to compare frames.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[test]
fn cluster_paint_capture() {
    for (text, label, base) in [
        ("\u{0915}\u{093F}", "कि", '\u{0915}'),
        ("\u{0915}\u{094D}\u{0937}", "क्ष", '\u{0915}'),
        ("\u{0E01}\u{0E33}", "กำ", '\u{0E01}'),
        ("\u{0995}\u{09C7}", "কে", '\u{0995}'),
        ("\u{1100}\u{1161}", "가", '\u{1100}'),
        ("\u{1F1EB}\u{1F1F7}", "🇫🇷", '\u{1F1EB}'),
        ("\u{1F1EB}\u{1F1EE}", "🇫🇮", '\u{1F1EB}'),
    ] {
        let Some(raster) = live_chain() else {
            return;
        };
        let (committed, shaped, unshaped, painted, pixels) = capture(raster, base, text);
        println!(
            "{label}\tcommitted={committed}\tshaped_glyphs={shaped}\tunshaped_glyphs={unshaped}\tpainted_glyphs={painted}\tpixels={}",
            digest(&pixels)
        );
    }

    // The two flags must not composite to identical pixels.
    let Some(fr_raster) = live_chain() else {
        return;
    };
    let Some(fi_raster) = live_chain() else {
        return;
    };
    let (_, _, _, fr_glyphs, fr_pixels) = capture(fr_raster, '\u{1F1EB}', "\u{1F1EB}\u{1F1F7}");
    let (_, _, _, fi_glyphs, fi_pixels) = capture(fi_raster, '\u{1F1EB}', "\u{1F1EB}\u{1F1EE}");
    assert!(fr_glyphs >= 1 && fi_glyphs >= 1, "both flags paint");
    assert_ne!(
        digest(&fr_pixels),
        digest(&fi_pixels),
        "FR and FI must composite to different pixels"
    );
}
