//! ASCII fast-path acceptance (CTX-1230, issue #2092).
//!
//! Printable ASCII runs with an unshaped effective feature set bypass
//! harfbuzz shaping and emit straight through the char-keyed cached-glyph
//! path. These tests prove, against live host fonts:
//!
//! - fast-path output is identical to per-cell rendering (parity),
//! - no harfbuzz call happens for all-ASCII frames under `Always`
//!   (`shape_misses` stays flat while `ascii_fast_path_runs` counts),
//! - ligature-formable runs stay shaped under `Never` (ligature merges,
//!   harfbuzz runs),
//! - mixed ASCII+CJK rows split correctly (ASCII fast-paths, CJK shapes).
//!
//! Live-font gate: `BITTY_RENDER_FONT_TESTS=1` like the other shaping
//! tests. Default CI stays deterministic (all tests skip without the gate
//! or without the host face).
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test ascii_fast_path
//! ```

use bitty_config::types::LigaturePolicy;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontQuery, FontStyle, GlyphRasterizer, GridRenderer,
    SwashSingle,
};

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const POINT_SIZE: f32 = 12.0;

fn live_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

fn query_for(family: &str) -> FontQuery {
    FontQuery {
        family: family.to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

fn live_renderer_for(family: &str) -> Option<GridRenderer<FallbackRasterizer<SwashSingle>>> {
    if !live_tests_enabled() {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run live font tests");
        return None;
    }
    let inner = SwashSingle::new().ok()?;
    let mut raster = FallbackRasterizer::with_default_chain(inner);
    raster.load_font(&query_for(family)).ok()?;
    let query = query_for(family);
    GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap()).ok()
}

fn snapshot_for(text: &str) -> bitty_term_state::Snapshot {
    let mut state = bitty_term_state::State::new();
    for ch in text.chars() {
        state.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    let mut snap = state.snapshot();
    snap.cursor.visible = false;
    snap
}

fn full_damage(snap: &bitty_term_state::Snapshot) -> bitty_term_state::Damage {
    bitty_term_state::Damage {
        generation: snap.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(snap.height as u16, snap.width as u16),
        )]
        .into_boxed_slice(),
    }
}

#[test]
fn ascii_fast_path_matches_unshaped_and_skips_shaping() {
    let Some(mut renderer) = live_renderer_for("JetBrainsMono Nerd Font") else {
        eprintln!("skipped: no host font stack or JetBrainsMono Nerd Font");
        return;
    };
    let snap = snapshot_for("hello 123 ABC ~!?");
    let damage = full_damage(&snap);
    // `Always` forces programming ligatures off: every run is ASCII-eligible.
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("shaped renders");
    let counters = renderer.counters();
    assert!(
        counters.ascii_fast_path_runs > 0,
        "ASCII runs must take the fast path: {counters:?}"
    );
    let (hits, misses) = renderer.shape_stats();
    assert_eq!(
        (hits, misses),
        (0, 0),
        "no harfbuzz call may happen for all-ASCII Always frames"
    );
    // Parity against the unshaped per-cell path on a fresh renderer (same
    // font, same insertion order): glyphs and fills byte-identical.
    let Some(mut plain_renderer) = live_renderer_for("JetBrainsMono Nerd Font") else {
        return;
    };
    let plain = plain_renderer
        .render(&snap, &damage)
        .expect("plain renders");
    assert_eq!(
        shaped.glyphs, plain.glyphs,
        "fast-path glyphs must equal per-cell glyphs"
    );
    assert_eq!(
        shaped.fills, plain.fills,
        "fast-path backgrounds must equal per-cell backgrounds"
    );
}

#[test]
fn programming_ligature_stays_shaped_under_never() {
    let Some(mut renderer) = live_renderer_for("JetBrainsMono Nerd Font") else {
        eprintln!("skipped: no host font stack or JetBrainsMono Nerd Font");
        return;
    };
    // `->` is a JetBrains Mono programming ligature: under `Never` the run
    // must reach harfbuzz (a shape miss) and must not fast-path, while
    // `Always` keeps two per-cell glyphs. Whether the merge materializes is
    // host-face-dependent (on this seat the resolved Nerd face shapes `->`
    // per scalar, so no count assertion on the merge itself — the `fi`/Noto
    // test below pins an actual merge); the routing gate is what this test
    // owns: ligature-live feature sets never bypass shaping.
    let snap = snapshot_for("->");
    let damage = full_damage(&snap);
    let (_, misses_before) = renderer.shape_stats();
    let never = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("never renders");
    let (_, misses_after) = renderer.shape_stats();
    assert!(
        misses_after > misses_before,
        "ligature-live runs must reach harfbuzz"
    );
    assert_eq!(
        renderer.counters().ascii_fast_path_runs,
        0,
        "ligature-live runs must not fast-path"
    );
    let always = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("always renders");
    assert_eq!(always.glyphs.len(), 2, "Always keeps per-cell glyphs");
    assert_eq!(
        never.glyphs.len(),
        2,
        "both policies emit per-cell glyphs on this face: {}",
        never.glyphs.len()
    );
}

#[test]
fn typographic_ligature_stays_shaped_under_never() {
    let Some(mut renderer) = live_renderer_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    // `fi` ligates in Noto Sans (see shaped_phase_b): same gate as above
    // for the typographic case.
    let snap = snapshot_for("fi");
    let damage = full_damage(&snap);
    let never = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("never renders");
    let always = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("always renders");
    assert_eq!(always.glyphs.len(), 2, "Always keeps per-cell glyphs");
    assert!(
        never.glyphs.len() < always.glyphs.len(),
        "Never must merge the `fi` ligature: never={} always={}",
        never.glyphs.len(),
        always.glyphs.len()
    );
}

#[test]
fn mixed_ascii_cjk_row_splits_fast_and_shaped() {
    let Some(mut renderer) = live_renderer_for("JetBrainsMono Nerd Font") else {
        eprintln!("skipped: no host font stack or JetBrainsMono Nerd Font");
        return;
    };
    // `A` fast-paths; `漢B`... precisely: the wide leading cell fuses with
    // the same-style `A` into one mixed run (`A漢`, spacer breaks after),
    // which stays shaped, while the trailing `B` run fast-paths. The CJK
    // scalar shapes through dynamic fallback (double-width single glyph,
    // spacer paints background only).
    let snap = snapshot_for("A漢B");
    assert_eq!(snap.cells[0].glyph, 'A');
    assert_eq!(snap.cells[1].glyph, '漢');
    assert_eq!(snap.cells[1].width, 2);
    assert!(snap.cells[2].spacer);
    assert_eq!(snap.cells[3].glyph, 'B');
    let damage = full_damage(&snap);
    let (_, misses_before) = renderer.shape_stats();
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("mixed renders");
    let (_, misses_after) = renderer.shape_stats();
    assert!(
        misses_after > misses_before,
        "the mixed run must reach harfbuzz"
    );
    assert_eq!(
        renderer.counters().ascii_fast_path_runs,
        1,
        "the trailing ASCII run must fast-path: {:?}",
        renderer.counters()
    );
    assert_eq!(
        shaped.glyphs.len(),
        3,
        "A + wide CJK + B emit three glyphs: {}",
        shaped.glyphs.len()
    );
    assert_eq!(snap.cells[1].glyph, '漢', "grid truth untouched");
}

#[test]
fn box_drawing_runs_never_take_ascii_fast_path() {
    // CTX-1234 (issue #2096): the verify2 scare suspected #2095 of
    // misrouting box runs past harfbuzz shaping. Box scalars are non-ASCII
    // (multi-byte UTF-8), so neither eligibility half can hold: pure-box
    // and mixed box/ASCII rows must shape under both ligature policies —
    // never fast-path — while still emitting one glyph per cell.
    for policy in [LigaturePolicy::Always, LigaturePolicy::Never] {
        for text in [
            "\u{250C}\u{2500}\u{2510}\u{2502}\u{2514}\u{2518}\u{251C}\u{253C}\u{2524}\u{2554}\u{2550}\u{2557}\u{2503}",
            "\u{2502}abc\u{2502}",
        ] {
            let Some(mut renderer) = live_renderer_for("JetBrainsMono Nerd Font") else {
                eprintln!("skipped: no host font stack or JetBrainsMono Nerd Font");
                return;
            };
            let snap = snapshot_for(text);
            let damage = full_damage(&snap);
            let (_, misses_before) = renderer.shape_stats();
            let list = renderer
                .render_shaped(&snap, &damage, policy, &[])
                .expect("box run renders");
            let (_, misses_after) = renderer.shape_stats();
            assert_eq!(
                renderer.counters().ascii_fast_path_runs,
                0,
                "box runs must never fast-path ({policy:?}, {text:?})"
            );
            assert!(
                misses_after > misses_before,
                "box runs must reach harfbuzz ({policy:?}, {text:?})"
            );
            assert_eq!(
                list.glyphs.len(),
                text.chars().count(),
                "every box cell must emit a glyph ({policy:?}, {text:?}): {}",
                list.glyphs.len()
            );
            assert_eq!(
                renderer.counters().missing_glyphs,
                0,
                "no box cell may go tofu on a covering host ({policy:?}, {text:?})"
            );
        }
    }
}
