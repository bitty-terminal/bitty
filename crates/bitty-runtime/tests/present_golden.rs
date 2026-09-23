//! Golden present-frame digests for the `tick_at` phase split (CTX-0474).
//!
//! `tick_at` is a ~1245-line god function; splitting it into named phases is
//! a behavior-preserving refactor, so these tests pin the exact presented
//! output of a scenario matrix *before* the split and must keep passing
//! unchanged *after* it.
//!
//! Each digest folds the full [`PresentStats`] and the presented RGBA buffer
//! through a fixed FNV-1a hash (no `DefaultHasher`: its algorithm is not a
//! stable contract). The runtime comes from
//! `Runtime::with_deterministic_rasterizer`, which selects the headless glyph
//! rasterizer: bitmaps derive from the scalar and point size alone (no font
//! files, no font metrics), cell metrics/theme/layout come from
//! `RuntimeConfig`, and the software compositor uses integer arithmetic plus
//! IEEE-exact `f32` math. The folded RGBA therefore excludes every
//! host-dependent input (system fonts above all), so the same digests hold on
//! Linux, macOS, and Windows CI; the production crossfont path stays covered
//! by its own tests.
//!
//! The digests were re-recorded under CTX-0492 (issue #792): #785 (CTX-0471)
//! merged after these constants were first recorded and made the software
//! compositor merge adjacent same-color cell backgrounds into maximal
//! horizontal runs. The folded [`PresentStats`] counters therefore changed
//! (`fills` now counts merged rectangles; the empty first frame went
//! 1673 -> 23) while the composited RGBA stayed byte-identical: reverting
//! #785 restores every previous digest exactly and the FNV-1a hash of
//! `headless_rgba` is unchanged for the whole matrix. The phase extraction
//! itself remains behavior-preserving; a change here still means the frame
//! changed, not that the golden value should be refreshed. Refresh only with
//! an explicit behavior-change task.
//!
//! The digests were re-recorded under #1342 (thin default border `2` -> `1`):
//! rings paint 1px and content grids gain a column wherever the freed pixel
//! crosses a cell boundary.
#![forbid(unsafe_code)]

use bitty_runtime::{
    AnimationPolicy, LayoutNode, PASTE_BANNER_FULL_DURATION, PresentStats, Runtime, RuntimeConfig,
    SplitAxis, View, ViewId,
};
use std::time::{Duration, Instant};

/// Animations off: a layout change would otherwise arm a bounded transition
/// whose frames depend on wall-clock progress. These digests pin the
/// frame-on-demand contract, not the animation feature.
///
/// `with_deterministic_rasterizer` is the host-font-free seam: without it the
/// composited RGBA depends on the platform font stack and the digests below
/// would not be portable.
fn make_runtime() -> Runtime {
    let rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build");
    assert!(
        !rt.is_crossfont(),
        "golden matrix requires the deterministic headless rasterizer"
    );
    rt
}

/// FNV-1a 64-bit over the bytes; fixed constants, stable across toolchains.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Digest of one presented frame: every stats field plus the RGBA surface.
fn digest(rt: &Runtime, stats: &PresentStats) -> u64 {
    let mut bytes = Vec::with_capacity(128);
    for value in [
        stats.frame,
        stats.fills as u64,
        stats.rounded_fills as u64,
        stats.glyphs as u64,
        stats.cells_examined,
        stats.glyphs_emitted,
        stats.generation,
        stats.images as u64,
        stats.backgrounds as u64,
        stats.images_skipped as u64,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.push(u8::from(stats.headless));
    match rt.headless_rgba() {
        Some(rgba) => {
            bytes.push(1);
            bytes.extend_from_slice(&rgba);
        }
        None => bytes.push(0),
    }
    fnv1a(&bytes)
}

fn assert_digest(name: &str, rt: &Runtime, stats: &PresentStats, golden: u64) {
    let actual = digest(rt, stats);
    assert_eq!(
        actual, golden,
        "{name}: presented-frame digest changed (actual 0x{actual:016x}, golden 0x{golden:016x})"
    );
}

#[test]
fn golden_first_frame_then_idle() {
    let mut rt = make_runtime();
    let first = rt.tick().expect("first tick presents");
    assert_digest("first_frame", &rt, &first, 0xf194_7117_d9e4_06d1);
    assert!(rt.tick().is_none(), "no damage -> idle");
}

#[test]
fn golden_pty_bytes_incremental() {
    let mut rt = make_runtime();
    let first = rt.tick().expect("first tick presents");
    assert_digest("pty_first", &rt, &first, 0xf194_7117_d9e4_06d1);
    rt.handle_pty_bytes(b"hello ");
    let a = rt.tick().expect("first bytes present");
    assert_digest("pty_a", &rt, &a, 0x0f2b_d55f_c555_fdef);
    rt.handle_pty_bytes(b"world");
    let b = rt.tick().expect("second bytes present");
    assert_digest("pty_b", &rt, &b, 0xdb0d_8943_cffc_5944);
    assert!(rt.tick().is_none(), "back to idle");
}

#[test]
fn golden_alt_screen_enter_and_leave() {
    let mut rt = make_runtime();
    let _ = rt.tick().expect("first tick presents");
    rt.handle_pty_bytes(b"\x1b[?1049h");
    let enter = rt.tick().expect("alt entry presents");
    assert_digest("alt_enter", &rt, &enter, 0x70df_d3be_01c5_b977);
    rt.handle_pty_bytes(b"vim");
    let paint = rt.tick().expect("alt content presents");
    assert_digest("alt_paint", &rt, &paint, 0x0703_34c4_bbd8_8438);
    rt.handle_pty_bytes(b"\x1b[?1049l");
    let leave = rt.tick().expect("alt exit presents");
    assert_digest("alt_leave", &rt, &leave, 0x1630_ba5b_a706_a42d);
    assert!(rt.tick().is_none(), "back to idle");
}

#[test]
fn golden_paste_banner_full_then_flash_then_idle() {
    let mut rt = make_runtime();
    let t0 = Instant::now();
    let _ = rt.tick().expect("first tick presents");
    assert!(
        rt.paste_text_via_gate("line1\nline2".to_string()),
        "multi-line paste must gate"
    );
    // `pending_paste_since` is set at (or after) `t0`, so a virtual time
    // `t0 + 100ms` is always inside the full-summary phase.
    let full = rt
        .tick_at(t0 + Duration::from_millis(100))
        .expect("pending paste forces a present");
    assert_eq!(
        rt.paste_banner_collapsed_at(t0 + Duration::from_millis(100)),
        Some(false),
        "banner must still be in the full phase"
    );
    assert_digest("paste_full", &rt, &full, 0xb5e8_4cca_54ec_82ad);
    let flash_at = t0 + PASTE_BANNER_FULL_DURATION + Duration::from_secs(30);
    let flash = rt.tick_at(flash_at).expect("collapse transition presents");
    assert_eq!(
        rt.paste_banner_collapsed_at(flash_at),
        Some(true),
        "banner must have collapsed to the flash"
    );
    assert_digest("paste_flash", &rt, &flash, 0x86c7_ec16_7061_ec42);
    assert!(
        rt.tick_at(flash_at + Duration::from_millis(200)).is_none(),
        "banner phase is steady -> idle"
    );
}

#[test]
fn golden_help_overlay_shown_then_hidden() {
    let mut rt = make_runtime();
    let _ = rt.tick().expect("first tick presents");
    rt.set_help_rows(vec![
        "alt+x  toggle_zoom".to_string(),
        "ctrl+shift+v  paste_from_clipboard".to_string(),
    ]);
    assert!(rt.toggle_help(), "help toggles on");
    let shown = rt.tick().expect("overlay present");
    assert_digest("help_shown", &rt, &shown, 0x1253_5c93_0b11_817d);
    assert!(!rt.toggle_help(), "help toggles off");
    let hidden = rt.tick().expect("dismissal present");
    assert_digest("help_hidden", &rt, &hidden, 0xa769_7a2c_a636_3963);
    assert!(rt.tick().is_none(), "back to idle");
}

#[test]
fn golden_selection_overlay() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"select me");
    let _ = rt.tick().expect("grid present");
    rt.select_all();
    let selected = rt.tick().expect("selection present");
    assert_digest("selection", &rt, &selected, 0x1acb_38a0_5afa_81e1);
}

#[test]
fn golden_ime_preedit_overlay() {
    let mut rt = make_runtime();
    let _ = rt.tick().expect("first tick presents");
    rt.handle_ime_preedit(Some("preedit".to_string()), Some(2));
    let shown = rt.tick().expect("preedit present");
    assert_digest("preedit_shown", &rt, &shown, 0xaa44_398b_363b_1700);
    rt.handle_ime_preedit(None, None);
    let cleared = rt.tick().expect("preedit clear present");
    assert_digest("preedit_cleared", &rt, &cleared, 0xa769_7a2c_a636_3963);
}

#[test]
fn golden_split_leaves() {
    let mut rt = make_runtime();
    let _ = rt.tick().expect("first tick presents");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    rt.handle_pty_bytes(b"split");
    let split = rt.tick().expect("split present");
    assert_digest("split", &rt, &split, 0x39c2_b771_e396_d134);
    assert!(rt.tick().is_none(), "back to idle");
}

#[test]
fn golden_scrollback_viewport() {
    let mut rt = make_runtime();
    for i in 0..200 {
        rt.handle_pty_bytes(format!("line{i}\r\n").as_bytes());
    }
    let _ = rt.tick().expect("grid present");
    assert!(rt.scroll_focused_page(true), "scroll up pages the view");
    let scrolled = rt.tick().expect("scrolled present");
    assert_digest("scrollback", &rt, &scrolled, 0xbb4b_d8ce_82cc_6723);
}

#[test]
fn golden_wide_and_combining_glyphs() {
    let mut rt = make_runtime();
    let _ = rt.tick().expect("first tick presents");
    rt.handle_pty_bytes("A\u{4e2d}\u{1f389}e\u{0301}".as_bytes());
    let mixed = rt.tick().expect("mixed-width present");
    assert_digest("wide_combining", &rt, &mixed, 0x93e5_d5e4_eeff_b41a);
}
