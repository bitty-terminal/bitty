//! Kitty preview pipeline regressions (#2442, #2453).
//!
//! Real-world image viewers exercise two different KGP shapes:
//!
//! - **chafa / `icat`:** a direct `a=T` placement, anchored at the cursor.
//! - **yazi 26.x:** `a=T,C=1,U=1` **without `c=`/`r=`**, then one
//!   `U+10EEEE` cell per cell of `ceil(pixels / cell)`, with the image id
//!   in a truecolor foreground and row/column diacritics. The virtual
//!   placement is only a prototype; the visible image is the text run.
//!
//! The tests pin the Core behavior the owner repro needs: the placeholder
//! run paints a real image, direct placements letterbox an aspect
//! mismatch inside the requested `c`/`r` rect (centered, not stretched),
//! XTWINOPS answers the draining pane (not the focused one), a failed
//! display drops the previous rendered image, and a layout resize repaints
//! the image exactly once at its new centered position.
//!
//! Fixtures are tiny and deterministic; no external viewer is required.

#![cfg(unix)]

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

/// Kitty row/column diacritics 0..=9 (`rowcolumn-diacritics.txt`), the
/// prefix yazi 26.x embeds in its KGP driver.
const DIACRITICS: [char; 10] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}',
];

/// 8x8 opaque red PNG (`crates/bitty-runtime/tests/fixtures`).
const RED_PNG: &[u8] = include_bytes!("fixtures/kitty-red-8x8.png");

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Split 50/50 and give the right pane a live shell so it owns a grid.
fn split_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    // Deterministic single-tick presents: no pacing coalescing between the
    // alternate-screen transitions in these tests.
    rt.set_min_present_interval(std::time::Duration::ZERO);
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    let frame = pane_frame(&rt);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "sleep 60"], frame.0, frame.1)
        .expect("spawn pane shell");
    rt
}

fn pane_frame(rt: &Runtime) -> (u16, u16, i32, i32) {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == PANE)
        .expect("pane presented");
    (frame.cols, frame.rows, frame.content.x, frame.content.y)
}

/// Bounds + count of opaque red pixels in the presented frame.
fn red_bounds(rt: &Runtime) -> Option<(usize, usize, usize, usize, usize)> {
    let extent = rt.config().window_extent();
    let width = usize::try_from(extent.width()).ok()?;
    let rgba = rt.headless_rgba()?;
    let mut min_x = usize::MAX;
    let mut min_y = usize::MAX;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut count = 0usize;
    for y in 0..usize::try_from(extent.height()).ok()? {
        for x in 0..width {
            let i = (y * width + x) * 4;
            if rgba[i] > 200 && rgba[i + 1] < 40 && rgba[i + 2] < 40 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                count += 1;
            }
        }
    }
    (count > 0).then_some((min_x, min_y, max_x, max_y, count))
}

/// yazi 26.x KGP driver shape: `a=T,C=1,U=1`, no `c=`/`r=`, then
/// placeholder rows at absolute positions with truecolor image id and
/// row/column diacritics.
fn yazi_shape(id: u32, width: u32, height: u32, at_row: u16, at_col: u16) -> Vec<u8> {
    let rgba = [255u8, 0, 0, 255].repeat((width * height) as usize);
    let mut out = format!(
        "\x1b_Gq=2,a=T,C=1,U=1,f=32,s={width},v={height},i={id},m=0;{}\x1b\\",
        base64(&rgba)
    )
    .into_bytes();
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
    // One row per `ceil(height / cell_height)`; the test uses the 9x19
    // default metrics so the span is exact for these fixture sizes.
    let cols = width.div_ceil(9) as usize;
    let rows = height.div_ceil(19) as usize;
    assert!(
        cols <= DIACRITICS.len() && rows <= DIACRITICS.len(),
        "fixture exceeds the embedded diacritic table"
    );
    for (y, row_mark) in DIACRITICS.iter().take(rows).enumerate() {
        out.extend_from_slice(format!("\x1b[{};{}H", at_row + y as u16 + 1, at_col + 1).as_bytes());
        for col_mark in DIACRITICS.iter().take(cols) {
            let mut buf = [0u8; 4];
            out.extend_from_slice('\u{10EEEE}'.encode_utf8(&mut buf).as_bytes());
            let mut mark = [0u8; 4];
            out.extend_from_slice(row_mark.encode_utf8(&mut mark).as_bytes());
            let mut mark = [0u8; 4];
            out.extend_from_slice(col_mark.encode_utf8(&mut mark).as_bytes());
        }
    }
    out
}

#[test]
fn yazi_placeholder_run_paints_a_real_image() {
    let mut rt = split_runtime();
    let (_, _, content_x, content_y) = pane_frame(&rt);
    let id = 0x010203u32;
    // 80x76 -> 9 columns x 4 rows at 9x19.
    rt.handle_pane_bytes(PANE, &yazi_shape(id, 80, 76, 5, 0));
    assert_eq!(rt.kitty_virtual_count(), 1, "spanless U=1 registers");
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "virtual prototypes are not blit placements"
    );
    assert!(rt.tick().is_some(), "placeholder run forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        4,
        "one real image blit per placeholder row"
    );
    let (x0, y0, x1, y1, count) = red_bounds(&rt).expect("placeholder image painted");
    // Pane content + window padding (8px) is the grid origin; the run
    // starts at grid row 5.
    assert_eq!(x0, usize::try_from(content_x).unwrap() + 8);
    assert_eq!(y0, usize::try_from(content_y).unwrap() + 8 + 5 * 19);
    // The 9-cell span is 81px; preserving the 80x76 aspect fits the
    // image to 80x76 (a 1px band on the span's right edge), exactly like
    // kitty's "fit to the rectangle, aspect preserved" rule.
    assert_eq!(x1 - x0 + 1, 80);
    assert_eq!(y1 - y0 + 1, 76);
    assert_eq!(count, 80 * 76);
}

#[test]
fn direct_png_placement_letterboxes_centered_in_requested_rect() {
    let mut rt = split_runtime();
    let (_, _, content_x, content_y) = pane_frame(&rt);
    // 8x8 square image into a 10x5 cell rect (90x95 px): kitty preserves
    // the aspect ratio and centers the 90x90 result, leaving a 2px top and
    // 3px bottom letterbox instead of stretching to 95px.
    let seq = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=10,r=5,C=1,q=2,m=0;{}\x1b\\",
        base64(RED_PNG)
    );
    rt.handle_pane_bytes(PANE, seq.as_bytes());
    assert!(rt.tick().is_some(), "placement presents");
    let (x0, y0, x1, y1, count) = red_bounds(&rt).expect("png painted");
    assert_eq!(x0, usize::try_from(content_x).unwrap() + 8);
    assert_eq!(y0, usize::try_from(content_y).unwrap() + 8 + 2);
    assert_eq!(x1 - x0 + 1, 90);
    assert_eq!(y1 - y0 + 1, 90);
    assert_eq!(count, 90 * 90);
}

#[test]
fn layout_resize_repaints_the_image_once_at_its_centered_position() {
    let mut rt = split_runtime();
    let seq = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=10,r=5,C=1,q=2,m=0;{}\x1b\\",
        base64(RED_PNG)
    );
    rt.handle_pane_bytes(PANE, seq.as_bytes());
    rt.tick();
    let (_, _, _, _, first_count) = red_bounds(&rt).expect("first paint");

    // Move the split edge: the pane content origin changes, and the blit
    // must be recomputed exactly once at the new origin (no stale copy).
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.62,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.tick();
    let (_, _, content_x, content_y) = pane_frame(&rt);
    let (x0, y0, _, _, after_count) = red_bounds(&rt).expect("repaint");
    assert_eq!(x0, usize::try_from(content_x).unwrap() + 8);
    assert_eq!(y0, usize::try_from(content_y).unwrap() + 8 + 2);
    assert_eq!(
        after_count, first_count,
        "exactly one image survives the resize"
    );
}

#[test]
fn xtwinops_answers_the_draining_pane_not_the_focused_one() {
    let mut rt = split_runtime();
    // Asymmetric split so the pane and the focused primary answer with
    // different cell counts.
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.35,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.tick();
    let (pane_cols, pane_rows, ..) = pane_frame(&rt);
    let (cell_w, cell_h) = rt.live_cell_size();
    rt.handle_pane_bytes(PANE, b"\x1b[14t\x1b[16t");
    let replies = rt.take_pane_replies(PANE);
    assert_eq!(replies.len(), 2, "14t and 16t answered: {replies:?}");
    assert_eq!(
        replies[0].as_ref(),
        format!(
            "\x1b[4;{};{}t",
            u32::from(pane_rows) * cell_h,
            u32::from(pane_cols) * cell_w
        )
        .as_bytes(),
        "14t reports the pane's pixel text area"
    );
    assert_eq!(
        replies[1].as_ref(),
        format!("\x1b[6;{cell_h};{cell_w}t").as_bytes(),
        "16t reports the live cell size"
    );
    // Focused primary pane asks the same question: its answer must differ
    // (the split gives the panes different widths), proving the reply is
    // routed to the draining grid rather than whichever leaf has focus.
    rt.handle_pty_bytes(b"\x1b[14t");
    let primary = rt.take_replies();
    assert_eq!(primary.len(), 1);
    assert_ne!(
        primary[0].as_ref(),
        replies[0].as_ref(),
        "pane and primary report their own geometry"
    );
}

#[test]
fn pane_pty_reports_nonzero_pixel_winsize() {
    let rt = split_runtime();
    let (cols, rows, ..) = pane_frame(&rt);
    let (cell_w, cell_h) = rt.live_cell_size();
    let (got_cols, got_rows, pixel_w, pixel_h) =
        rt.pane_pty_pixel_size(&PANE).expect("pane pty size");
    assert_eq!((got_cols, got_rows), (cols, rows));
    assert_eq!(
        pixel_w,
        cols.saturating_mul(cell_w.min(u32::from(u16::MAX)) as u16)
    );
    assert_eq!(
        pixel_h,
        rows.saturating_mul(cell_h.min(u32::from(u16::MAX)) as u16)
    );
    assert!(
        pixel_w > 0 && pixel_h > 0,
        "icat treats zero as unsupported"
    );
}

#[test]
fn alt_screen_placeholder_preview_paints_and_main_plane_restores() {
    // yazi is a fullscreen TUI: it enters the alternate screen before
    // emitting its KGP preview. Kitty keeps separate image planes per
    // screen; the main image must hide while alt is active, the alt
    // placeholder image must paint, and the main image must return on
    // leave.
    let mut rt = split_runtime();
    let main = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=10,r=5,C=1,q=2,i=3,m=0;{}\x1b\\",
        base64(RED_PNG)
    );
    rt.handle_pane_bytes(PANE, main.as_bytes());
    rt.tick();
    let (_, _, _, _, main_pixels) = red_bounds(&rt).expect("main image paints");
    assert_eq!(rt.kitty_placement_count(), 1);

    rt.handle_pane_bytes(PANE, b"\x1b[?1049h");
    rt.tick();
    assert!(
        red_bounds(&rt).is_none(),
        "main image hides while the alt plane is active"
    );
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "main placement is stashed, not discarded"
    );

    // Alt-plane yazi preview at row 3.
    let alt_id = 0x0A0B0Cu32;
    rt.handle_pane_bytes(PANE, &yazi_shape(alt_id, 80, 76, 2, 0));
    rt.tick();
    assert_eq!(rt.kitty_virtual_count(), 1, "alt prototype is live");
    assert!(rt.kitty_last_frame_images() >= 4, "alt preview paints");
    assert!(red_bounds(&rt).is_some(), "alt placeholder image visible");

    rt.handle_pane_bytes(PANE, b"\x1b[?1049l");
    rt.tick();
    assert_eq!(
        rt.kitty_virtual_count(),
        0,
        "alt prototype dies with the alt plane"
    );
    let (_, _, _, _, restored) = red_bounds(&rt).expect("main image returns");
    assert_eq!(restored, main_pixels, "main plane restored bit-identically");
}

#[test]
fn failed_png_replacement_drops_the_previous_placement() {
    let mut rt = split_runtime();
    let id = 7u32;
    let ok = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=4,r=4,C=1,q=2,i={id},m=0;{}\x1b\\",
        base64(RED_PNG)
    );
    rt.handle_pane_bytes(PANE, ok.as_bytes());
    rt.tick();
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(red_bounds(&rt).is_some());

    // Same wire image id, undecodable PNG: the replacement fails and the
    // old rendered placement must not survive as a stale rectangle.
    let bad = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=4,r=4,C=1,q=2,i={id},m=0;{}\x1b\\",
        base64(b"not a png")
    );
    rt.handle_pane_bytes(PANE, bad.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 0, "stale placement cleared");
    assert!(rt.tick().is_some(), "clear forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 0);
    assert!(
        red_bounds(&rt).is_none(),
        "no stale pixels remain after the failed replacement"
    );
}

#[test]
fn missing_local_image_clears_a_previous_placement() {
    let mut rt = split_runtime();
    let id = 9u32;
    let ok = format!(
        "\x1b_Ga=T,f=100,s=8,v=8,c=4,r=4,C=1,q=2,i={id},m=0;{}\x1b\\",
        base64(RED_PNG)
    );
    rt.handle_pane_bytes(PANE, ok.as_bytes());
    rt.tick();
    assert_eq!(rt.kitty_placement_count(), 1);

    // `t=f` missing file: the async worker reports the read error and the
    // previous rendered placement for the same wire id is cleared.
    let missing_path = format!(
        "/tmp/bitty/kitty-preview-missing-{}-{}.png",
        std::process::id(),
        id
    );
    let missing = format!(
        "\x1b_Ga=T,f=100,c=4,r=4,C=1,q=2,i={id},t=f;{}\x1b\\",
        base64(missing_path.as_bytes())
    );
    rt.handle_pane_bytes(PANE, missing.as_bytes());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while rt.kitty_placement_count() != 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = rt.poll_pty();
    }
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "missing local image clears the stale placement"
    );
    assert!(rt.tick().is_some(), "clear forces a present");
    assert!(red_bounds(&rt).is_none(), "no stale pixels remain");
}
