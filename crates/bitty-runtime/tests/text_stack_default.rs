//! CTX-1607: the shaped `harfrust`/`swash` stack is the production default.
//!
//! These are host-font tests: a container with no discoverable font cannot
//! shape and legitimately falls back, so each test skips in that case
//! instead of failing the default-selection contract.
//!
//! "The host has the face" is decided by the resolver, not by a successful
//! `load_font`: since issue #2931 an absent family loads a monospaced stand-in,
//! so a loaded face no longer means the configured family is installed.

use bitty_config::types::LigaturePolicy;
use bitty_render::{FontQuery, FontStyle, GlyphRasterizer, SwashSingle};
use bitty_runtime::config::DEFAULT_FONT_FAMILY;
use bitty_runtime::{Runtime, RuntimeConfig, TextStack, TextStackPreference};

/// True when the renderer will paint with a face the host actually has.
///
/// The platform default resolves through `DEFAULT_FONT_FAMILY_CANDIDATES`
/// (issue #2931), so this asks the resolver — the same database and candidate
/// list `SwashSingle::load_font` uses — whether any candidate exists.
/// `ResolvedPrimary::installed() == false` means `load_font` falls back to the
/// generic monospaced stand-in (`DejaVu Sans Mono` on the CI image): a usable
/// text face, but not the documented default and with no programming
/// ligatures, so these contracts have nothing to assert on such a host.
fn host_fonts_available(family: &str) -> bool {
    if !bitty_render::font_defaults::resolve_primary(family).installed() {
        return false;
    }
    SwashSingle::new()
        .and_then(|mut shaped| {
            shaped.load_font(&FontQuery {
                family: family.to_string(),
                style: FontStyle::Normal,
                point_size: 12.0,
            })
        })
        .is_ok()
}

#[test]
fn default_runtime_selects_the_shaped_stack() {
    if !host_fonts_available(DEFAULT_FONT_FAMILY) {
        eprintln!("skipped: no installed face for the default family (only a stand-in loads)");
        return;
    }
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime builds");
    assert_eq!(
        rt.text_stack(),
        TextStack::Shaped,
        "the production default must select the shaped stack (note: {:?})",
        rt.text_stack_note()
    );
    assert!(rt.is_shaped());
    assert!(
        rt.text_stack_note().is_none(),
        "the preferred stack must win without a fallback note"
    );
    // Headless render through the production tick: the shaped default paints
    // the prompt frame instead of idling.
    rt.handle_pty_bytes(b"hello");
    let stats = rt.tick().expect("first tick presents the full frame");
    assert!(stats.headless);
    assert!(stats.glyphs > 0, "shaped default must paint text");
}

#[test]
fn classic_preference_forces_the_crossfont_fallback() {
    let cfg = RuntimeConfig {
        text_stack: TextStackPreference::Classic,
        ..RuntimeConfig::default()
    };
    let rt = Runtime::new(cfg).expect("headless runtime builds");
    assert!(
        !rt.is_shaped(),
        "classic preference must never select the shaped stack"
    );
    assert!(
        matches!(rt.text_stack(), TextStack::Crossfont | TextStack::Headless),
        "classic preference resolves crossfont (or headless on a bare host): {:?}",
        rt.text_stack()
    );
    match rt.text_stack() {
        TextStack::Crossfont => assert!(
            rt.text_stack_note().is_none(),
            "an explicit classic preference is not a fallback"
        ),
        TextStack::Headless => assert!(
            rt.text_stack_note().is_some(),
            "a headless resolution must explain the missing crossfont font"
        ),
        TextStack::Shaped => panic!("classic preference selected the shaped stack"),
    }
}

#[test]
fn deterministic_rasterizer_reports_headless() {
    let rt = Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("deterministic runtime builds");
    assert_eq!(rt.text_stack(), TextStack::Headless);
    assert!(!rt.is_crossfont());
    assert!(!rt.is_shaped());
}

/// First two cells of the first text row, exclusive of the cursor cell.
fn first_two_cells_pixels(rt: &Runtime) -> Vec<u8> {
    let cfg = RuntimeConfig::default();
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    // Window padding + decoration outer gap/border/content inset (same
    // geometry the cursor-overlay test pins).
    const DECORATION: usize = 14;
    let x0 = pad + DECORATION;
    let y0 = pad + DECORATION;
    let w = 2 * cfg.cell_width as usize;
    let h = cfg.cell_height as usize;
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let mut out = Vec::with_capacity(w * h * 4);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let index = (y * width + x) * 4;
            out.extend_from_slice(&rgba[index..index + 4]);
        }
    }
    out
}

#[test]
fn shaped_default_disables_ligatures_when_configured() {
    // The OS-shipped platform defaults (Menlo on macOS, Consolas on Windows)
    // carry no `->` ligature, and the preference faces (SF Mono, Cascadia
    // Mono) are absent from the CI images, so the resolved primary has no
    // programming ligature to disable there.
    if cfg!(target_os = "macos") || cfg!(windows) {
        eprintln!("skipped: the host default face has no programming ligatures on this platform");
        return;
    }
    if !host_fonts_available(DEFAULT_FONT_FAMILY) {
        eprintln!(
            "skipped: no installed face for the default family (a stand-in has no ligatures)"
        );
        return;
    }
    let render = |policy: LigaturePolicy| {
        let mut rt = Runtime::new(RuntimeConfig {
            disable_ligatures: policy,
            ..RuntimeConfig::default()
        })
        .expect("headless runtime builds");
        assert!(rt.is_shaped(), "host fonts must select the shaped stack");
        rt.handle_pty_bytes(b"->");
        rt.tick().expect("frame presents");
        first_two_cells_pixels(&rt)
    };
    let ligatures_on = render(LigaturePolicy::Never);
    let ligatures_off = render(LigaturePolicy::Always);
    assert_ne!(
        ligatures_on, ligatures_off,
        "font.disable_ligatures=always must change the painted glyphs"
    );
}
