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
//! **Scope:** the first three cases are symptom canaries, not regression
//! tests. The present path they drive is `gpu.rs`, which never refused a whole
//! line for one color glyph, so those cases also pass on their parent commit;
//! `stats.glyphs` is `draw_list.glyphs.len()` and includes glyphs the
//! compositor drops, and `painted_pixels > 0` is satisfied by pane chrome
//! alone. Their regression proof is `emoji_fail_closed.rs` (software backend,
//! `sw-fallback`) and the `pipeline::tests` pair for the atlas-overrun
//! refusal.
//!
//! `han_prompt_line_keeps_its_glyphs_when_an_emoji_joins_the_run` (bitty#3021)
//! is a real regression test: on a host whose shaped stack resolves Han and
//! emoji through different fallback faces it fails on the parent commit
//! (13 glyphs without the emoji, 1 with it).

use bitty_runtime::{Runtime, RuntimeConfig};

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

/// bitty#3021: the live 0.0.36 report — the prompt line
/// `> 请问饿七五二二头痛医头用` with an emoji appended rendered blank apart
/// from the `>` prefix, because the shaped fallback left every cluster of the
/// run uncovered.
///
/// The reproduction needs a run whose *every* cluster is uncovered on the
/// primary face and whose fallback needs two different faces (Han + emoji).
/// `ab😀cd` above cannot show it: the ASCII cells keep the primary face, so
/// the fallback gap is the emoji alone. The self-calibrating guard below
/// renders the same line without the emoji first: a host that cannot shape or
/// cover the Han text has nothing to compare and skips, while a shaped host
/// must not lose those glyphs when the emoji joins the run.
#[test]
fn han_prompt_line_keeps_its_glyphs_when_an_emoji_joins_the_run() {
    const PROMPT: &str = "> 请问饿七五二二头痛医头用";
    let mut plain = runtime_with(PROMPT.as_bytes());
    let plain_stats = plain.tick().expect("plain frame presents");
    // 12 Han clusters plus the `>`; the space is blank.
    if plain_stats.glyphs < 13 {
        eprintln!(
            "skipped: host cannot shape the Han prompt line ({} glyphs)",
            plain_stats.glyphs
        );
        return;
    }
    let plain_rgba = plain.headless_rgba().expect("plain pixels");
    let width = usize::try_from(plain.surface_extent().expect("extent").width()).expect("usize");
    let cell_width = usize::try_from(RuntimeConfig::default().cell_width).expect("cell width");
    let cell_height = usize::try_from(RuntimeConfig::default().cell_height).expect("cell height");
    // 用 is a width-2 lead at cell column 24; the plain cursor sits on cell 26.
    const USE_COL: usize = 24;
    const END_COL: usize = 26;
    // Self-calibrate the content origin from the cursor overlay: hiding it
    // changes exactly the block's cell, so its left edge names the origin.
    plain.handle_pty_bytes(b"\x1b[?25l");
    plain.tick().expect("hidden-cursor frame");
    let plain_hidden = plain.headless_rgba().expect("hidden pixels");
    let (cursor_min, _) = changed_columns(&plain_rgba, &plain_hidden, width);
    let origin = cursor_min - END_COL * cell_width;
    let painted = |frame: &[u8], col: usize| {
        let bg = bitty_render::grid::ThemePalette::bitty_dark().background;
        (0..cell_height).any(|row| {
            (0..cell_width).any(|x| {
                let index = ((origin + row) * width + origin + col * cell_width + x) * 4;
                frame[index..index + 4] != bg
            })
        })
    };
    assert!(painted(&plain_rgba, USE_COL), "plain line paints 用");

    let mut with_emoji = runtime_with(format!("{PROMPT}\u{1F600}").as_bytes());
    let emoji_stats = with_emoji.tick().expect("emoji frame presents");
    assert!(
        emoji_stats.glyphs >= plain_stats.glyphs,
        "the emoji must not cost the run its glyphs (plain {}, emoji {})",
        plain_stats.glyphs,
        emoji_stats.glyphs
    );
    let emoji_rgba = with_emoji.headless_rgba().expect("emoji pixels");
    assert!(
        painted(&emoji_rgba, USE_COL),
        "the emoji must not blank the Han text before it"
    );
}

/// Inclusive `(min, max)` columns whose pixels differ between two frames.
fn changed_columns(before: &[u8], after: &[u8], width: usize) -> (usize, usize) {
    let mut min = usize::MAX;
    let mut max = 0usize;
    for (index, (a, b)) in before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .enumerate()
    {
        if a != b {
            let x = index % width;
            min = min.min(x);
            max = max.max(x);
        }
    }
    (min, max)
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
