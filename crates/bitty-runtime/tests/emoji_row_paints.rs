//! Runtime acceptance for bitty#2994: a line with an emoji paints like any
//! other line, in the focused pane with a visible cursor and in a paged pane.
//!
//! These drive the real present path (the same `tick` the window uses, through
//! the headless software seam) with the production text stack. They are
//! font-independent on purpose: whether the emoji resolves to a color face, a
//! monochrome outline, or the tofu fallback, the line must still emit a glyph
//! per printable cell and the cursor must still paint. The deterministic
//! color-glyph pixel assertions live in `bitty-render`'s
//! `emoji_line_paint.rs`.
//!
//! **Scope:** symptom canaries, not regression tests. The present path these
//! drive is `gpu.rs`, which never refused a whole line for one color glyph, so
//! every case here also passes on the parent commit; `stats.glyphs` is
//! `draw_list.glyphs.len()` and includes glyphs the compositor drops, and
//! `painted_pixels > 0` is satisfied by pane chrome alone. The regression
//! proof is `emoji_fail_closed.rs` (software backend, `sw-fallback`) and the
//! `pipeline::tests` pair for the atlas-overrun refusal.

use bitty_runtime::Runtime;

fn painted_pixels(rt: &Runtime) -> usize {
    let Some(extent) = rt.surface_extent() else {
        return 0;
    };
    let Some(rgba) = rt.headless_rgba() else {
        return 0;
    };
    let bg = bitty_render::grid::ThemePalette::bitty_dark().background;
    rgba.chunks_exact(4)
        .filter(|px| [px[0], px[1], px[2], px[3]] != bg)
        .count()
        .min(extent.width() as usize * extent.height() as usize)
}

fn runtime_with(bytes: &[u8]) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("runtime");
    rt.handle_pty_bytes(bytes);
    rt
}

/// "ab😀cd" is five printable cells: a, b, the wide emoji, c, d. A line that
/// blanked on the emoji would emit fewer glyph instances (or none).
#[test]
fn focused_emoji_line_emits_a_glyph_for_every_cell() {
    let mut rt = runtime_with("ab\u{1F600}cd".as_bytes());
    let stats = rt.tick().expect("first frame presents");
    // The four ASCII cells always resolve. The emoji's own glyph is
    // font-dependent: Apple Color Emoji is an `sbix` strike, so a rasterizer
    // without that strike legitimately paints a fallback or nothing for it.
    // The regression this guards is the whole line going blank after the
    // emoji, which shows up as fewer than four glyphs and no pixels at all.
    assert!(
        stats.glyphs >= 4,
        "every text cell of the emoji line emits a glyph, got {}",
        stats.glyphs
    );
    assert!(painted_pixels(&rt) > 0, "the emoji line paints pixels");
}

/// The block cursor is an overlay fill pushed after the glyph layer is built.
/// Hiding it must remove painted pixels, so the differential proves the cursor
/// still reaches the screen on an emoji line.
#[test]
fn visible_cursor_paints_on_an_emoji_line() {
    let mut with_cursor = runtime_with("ab\u{1F600}cd".as_bytes());
    with_cursor.tick().expect("frame with cursor");

    let mut hidden = runtime_with("ab\u{1F600}cd\x1b[?25l".as_bytes());
    hidden.tick().expect("frame without cursor");

    let visible = painted_pixels(&with_cursor);
    let without = painted_pixels(&hidden);
    assert!(
        visible > without,
        "the visible cursor must add painted pixels on an emoji line \
         (with cursor {visible}, hidden {without})"
    );
}

/// A paged pane composites history rows into the viewport. Emoji rows from
/// scrollback must paint after a page-up, not just on the live bottom row.
#[test]
fn paged_emoji_pane_still_paints_its_rows() {
    let mut rt = Runtime::with_defaults().expect("runtime");
    for index in 0..60 {
        rt.handle_pty_bytes(format!("line {index} \u{1F600} tail\r\n").as_bytes());
    }
    let live = rt.tick().expect("live frame presents");
    assert!(live.glyphs > 0, "the live emoji rows emit glyphs");

    assert!(rt.scroll_focused_page(true), "the pane pages up");
    let paged = rt.tick().expect("paged frame presents");
    assert!(
        paged.glyphs >= 24,
        "a paged emoji pane emits glyphs for its visible rows, got {}",
        paged.glyphs
    );
    assert!(
        painted_pixels(&rt) > 0,
        "the paged emoji pane paints pixels"
    );
}
