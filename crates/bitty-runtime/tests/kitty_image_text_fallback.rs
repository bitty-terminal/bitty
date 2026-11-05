//! Image-cell text fallback for direct placement (`a=p`/`a=T`) — issue #2875
//! scope 6, the a11y/scene contract.
//!
//! A direct Kitty placement paints pixels over grid cells but owns no cell
//! of its own: the cells keep whatever text the application wrote (usually
//! blanks). A text projection — `bitty ctl terminal text`, an accessibility
//! reader, an AI snapshot — therefore used to report a blank hole where the
//! user sees an image.
//!
//! These tests pin the runtime side of the fix end to end through PTY bytes:
//! the painted footprint `Runtime::kitty_image_cell_spans_for_view` reports,
//! its projection through `bitty_runtime::inspect`, and the fail-closed cases
//! (deleted image, footprint clipped to the grid).

#![forbid(unsafe_code)]

use bitty_runtime::inspect::{ImageCellSpan, grid_text_from_snapshot_with_images};
use bitty_runtime::{AnimationPolicy, PresentFrame, Runtime, RuntimeConfig};

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
const RED_2X2_B64: &str = "/wAA//8AAP//AAD//wAA/w==";

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// Runtime with animation ticks disabled so `tick` always presents, used by
/// the coordinate-space guard that inspects the presented frame.
fn make_presenting_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build")
}

/// The focused leaf's present frame (its content rect is what the paint pass
/// fills; it can be smaller than the grid).
fn focused_frame(rt: &Runtime) -> PresentFrame {
    let focused = rt.focused_view().expect("focused leaf");
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == focused)
        .expect("focused leaf has a present frame")
}

/// Transmit-and-display a 2x2 RGBA image anchored at the cursor with a
/// 2x1 cell span, the shape a client uses for an inline image.
fn display_2x1(rt: &mut Runtime, image_id: u32) {
    let seq = format!("\x1b_Ga=T,f=32,s=2,v=2,c=2,r=1,i={image_id};{RED_2X2_B64}\x1b\\");
    rt.handle_pty_bytes(seq.as_bytes());
}

/// Primary view id in a fresh runtime.
fn primary_view(rt: &Runtime) -> bitty_runtime::ViewId {
    rt.focused_view()
        .expect("a fresh runtime focuses the primary view")
}

#[test]
fn placed_image_reports_a_cell_footprint_that_projects_a_stand_in() {
    let mut rt = make_runtime();
    let view = primary_view(&rt);
    assert!(
        rt.kitty_image_cell_spans_for_view(view).is_empty(),
        "no image, no footprint"
    );

    display_2x1(&mut rt, 41);
    assert_eq!(rt.kitty_image_count(), 1);

    let spans = rt.kitty_image_cell_spans_for_view(view);
    assert_eq!(
        spans,
        vec![ImageCellSpan {
            row: 0,
            col: 0,
            rows: 1,
            cols: 2
        }],
        "a 2x1 span anchored at the cursor"
    );
    // The primary-grid seam (the devtools grid-text projection) must report
    // the same footprint, never a second, divergent geometry.
    assert_eq!(rt.kitty_image_cell_spans_for_primary(), spans);

    // The grid holds blanks there, and the projection now reports the image
    // instead of an empty hole.
    let snapshot = rt.snapshot();
    let plain = grid_text_from_snapshot_with_images(&snapshot, 2, 16, &[]);
    assert_eq!(plain.lines[0], "", "the grid itself is blank");
    let projected = grid_text_from_snapshot_with_images(&snapshot, 2, 16, &spans);
    assert_eq!(projected.lines[0], "\u{FFFC}\u{FFFC}");
}

#[test]
fn deleting_the_image_drops_the_footprint() {
    // The stand-in must describe what is on screen: after `a=d,i=` paints
    // nothing, the projection goes back to plain grid text.
    let mut rt = make_runtime();
    let view = primary_view(&rt);
    display_2x1(&mut rt, 42);
    assert_eq!(rt.kitty_image_cell_spans_for_view(view).len(), 1);

    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=42\x1b\\");
    assert!(
        rt.kitty_image_cell_spans_for_view(view).is_empty(),
        "a deleted image covers nothing"
    );
}

#[test]
fn text_under_the_image_is_preserved_in_the_projection() {
    // A client that paints an image over text (a preview over the document)
    // must not lose that text to the accessibility projection: only blank
    // cells take the stand-in.
    let mut rt = make_runtime();
    let view = primary_view(&rt);
    rt.handle_pty_bytes(b"hello");
    // Rewind to the top-left and paint over the word with `C=1` (no cursor
    // advance) so the text stays in the cells.
    rt.handle_pty_bytes(b"\x1b[1;1H");
    rt.handle_pty_bytes(
        format!("\x1b_Ga=T,f=32,s=2,v=2,c=5,r=1,C=1,i=43;{RED_2X2_B64}\x1b\\").as_bytes(),
    );
    let spans = rt.kitty_image_cell_spans_for_view(view);
    assert_eq!(
        spans,
        vec![ImageCellSpan {
            row: 0,
            col: 0,
            rows: 1,
            cols: 5
        }]
    );
    let projected = grid_text_from_snapshot_with_images(&rt.snapshot(), 2, 16, &spans);
    assert_eq!(
        projected.lines[0], "hello",
        "hidden text is still content for a reader"
    );
}

#[test]
fn footprint_of_a_span_placed_at_the_grid_bottom_stays_inside_the_grid() {
    // Terminal truth keeps a placement inside the grid (a 4-row span aimed
    // at the last row is anchored so it still fits), and the projected
    // footprint must agree: no span may point past the snapshot, and the
    // last row must still carry the stand-in.
    let mut rt = make_runtime();
    let view = primary_view(&rt);
    let height = rt.snapshot().height;
    rt.handle_pty_bytes(format!("\x1b[{height};1H").as_bytes());
    rt.handle_pty_bytes(
        format!("\x1b_Ga=T,f=32,s=2,v=2,c=2,r=4,i=44;{RED_2X2_B64}\x1b\\").as_bytes(),
    );
    let spans = rt.kitty_image_cell_spans_for_view(view);
    assert_eq!(spans.len(), 1, "one placement, one footprint");
    let span = spans[0];
    assert_eq!(span.cols, 2);
    assert_eq!(
        usize::try_from(span.row).expect("row fits usize")
            + usize::try_from(span.rows).expect("rows fit usize"),
        height,
        "the footprint ends exactly at the grid bottom"
    );
    // Projection over the footprint stays inside the grid and the bottom
    // row carries the stand-in.
    let projected = grid_text_from_snapshot_with_images(&rt.snapshot(), height, 8, &spans);
    assert_eq!(projected.lines.len(), height);
    assert_eq!(projected.lines[height - 1], "\u{FFFC}\u{FFFC}");
}

#[test]
fn footprint_rows_stay_in_grid_space_under_a_cropped_and_scrolled_view() {
    // F4 (#2875 follow-up), coordinate-space guard. The present leaf pass
    // resolves placements against
    // `(scrollback_len - scroll_offset) + cursor_follow_window_start(..)`
    // because it draws a cursor-follow *window* of the grid (default
    // headless: an 80x24 grid in a 76x22 content frame) or a scrollback
    // composite. The text projection consumes the whole grid, so the
    // footprint must stay in grid rows.
    //
    // The image is placed BEFORE the scrollback pressure on purpose: that
    // leaves `placement.scrollback_base == 0` against a non-zero live
    // scrollback length, so the grid base (`anchor - len`) and a
    // paint-style base (`anchor - (len - scroll_offset)`) resolve to
    // different rows. Injecting either the `+ window_start` term or the
    // `- scroll_offset` term into the footprint seam turns this test red.
    let mut rt = make_presenting_runtime();
    let view = primary_view(&rt);
    let height = rt.snapshot().height;
    // `C=1`: keep the cursor where it is, so placing the image cannot scroll
    // the grid and move the anchor out from under the assertion.
    rt.handle_pty_bytes(format!("\x1b[{height};3H").as_bytes());
    rt.handle_pty_bytes(
        format!("\x1b_Ga=T,f=32,s=2,v=2,c=2,r=1,C=1,i=45;{RED_2X2_B64}\x1b\\").as_bytes(),
    );
    assert!(rt.tick().is_some(), "the display must present");

    let frame = focused_frame(&rt);
    assert!(
        usize::from(frame.rows) < height,
        "precondition: the content frame ({}) must crop the {height}-row grid, \
         so the paint window start is non-zero",
        frame.rows
    );
    assert_eq!(
        rt.state().scrollback_len(),
        0,
        "precondition: the placement's scrollback base must be zero"
    );

    // Push the grid up: each line at the bottom row adds one scrollback row
    // and the image moves up with the content, still on the live grid.
    let scroll_added = 4usize;
    for i in 0..scroll_added {
        rt.handle_pty_bytes(format!("S{i}\r\n").as_bytes());
    }
    assert_eq!(
        rt.state().scrollback_len(),
        scroll_added,
        "each bottom-row newline must add exactly one scrollback row"
    );

    // Clear the row the image has scrolled onto: the lines above wrote text
    // across it, and the projection must have a blank hole to fill before
    // the stand-in assertion below is meaningful.
    let image_row = height - 1 - scroll_added;
    rt.handle_pty_bytes(format!("\x1b[{};1H\x1b[2K", image_row + 1).as_bytes());

    let anchored = ImageCellSpan {
        row: u32::try_from(image_row).expect("grid rows fit u32"),
        col: 2,
        rows: 1,
        cols: 2,
    };
    assert_eq!(
        rt.kitty_image_cell_spans_for_view(view),
        vec![anchored],
        "the footprint marks the grid row the image is anchored on, not the \
         frame row the paint pass draws it at"
    );
    // The stand-in lands on that same row of the projected grid, so text
    // and footprint agree.
    let projected = grid_text_from_snapshot_with_images(&rt.snapshot(), height, 8, &[anchored]);
    assert_eq!(
        projected.lines[usize::try_from(anchored.row).expect("row fits usize")],
        "  \u{FFFC}\u{FFFC}"
    );

    // Scrolled into history: the projection still reads the live grid, so
    // the footprint keeps grid rows instead of following the composite.
    let scrollback = rt.state().scrollback_len();
    {
        // CTX-0603: bind the guard before borrowing a leaf through it.
        let mut layout = rt.layout_mut();
        let leaf = layout.find_leaf_mut(view).expect("view 1 is a leaf");
        leaf.set_scroll_offset(2, scrollback);
        assert_eq!(leaf.scroll_offset(), 2, "the scroll must take effect");
    }
    assert_eq!(
        rt.kitty_image_cell_spans_for_view(view),
        vec![anchored],
        "a scrolled view keeps the footprint on the grid row the placement is \
         anchored on, not the paint pass's composite row"
    );
}
