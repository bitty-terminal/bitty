//! Kitty transmit/display wired suite (issue #1802).
//!
//! The Core-owned decoder ([`bitty_rich::kitty_decode`]) and raster step
//! ([`bitty_rich::rasterize_kitty_clipped`]) are wired: admissible
//! payloads decode, store, place, advance the cursor, and paint topmost
//! blits. Hostile declarations are still refused before any allocation
//! (P0-AC-003), unknown formats never guess, and rejected transmissions
//! store nothing.

use bitty_rich::KittyPrecheckError;
use bitty_runtime::{KittyDisplayOutcome, KittyImageError, Runtime, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`).
fn red_2x2() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
}

/// 1x1 opaque red RGBA PNG (`f=100`). Same bytes as the
/// `bitty-rich::kitty_decode` unit fixture.
fn red_1x1_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

#[test]
fn unknown_format_rejected_without_storing() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_display_image(7, Some(2), Some(2), None, 1, 1, 0, &red_2x2(), 0)
        .expect_err("f=7 must fail closed");
    assert_eq!(err, KittyImageError::UnknownFormat(7));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn oversize_declaration_rejected_before_alloc() {
    let mut rt = make_runtime();
    // 9000-wide declaration on a 4-byte payload: the side cap fires before
    // any pixel buffer exists (P0-AC-003).
    let err = rt
        .kitty_display_image(32, Some(9000), Some(1), None, 1, 1, 0, &[0; 4], 0)
        .expect_err("oversize declaration must fail closed");
    assert!(matches!(err, KittyImageError::Decode(_)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn empty_payload_rejected_first() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_transmit_image(32, Some(2), Some(2), &[], 0)
        .expect_err("empty payload must fail closed");
    assert_eq!(
        err,
        KittyImageError::Decode(KittyPrecheckError::EmptyPayload)
    );
    assert_eq!(rt.kitty_image_count(), 0);
}

#[test]
fn length_mismatch_rejected_before_alloc() {
    let mut rt = make_runtime();
    // 2x2 RGBA needs exactly 16 bytes.
    let err = rt
        .kitty_transmit_image(32, Some(2), Some(2), &[0; 5], 5)
        .expect_err("short payload must fail closed");
    assert_eq!(
        err,
        KittyImageError::Decode(KittyPrecheckError::LengthMismatch {
            expected: 16,
            actual: 5
        })
    );
    assert_eq!(rt.kitty_image_count(), 0);
}

#[test]
fn malformed_png_rejected_without_storing() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_display_image(100, None, None, None, 1, 1, 0, b"not a png", 0)
        .expect_err("garbage PNG must fail closed");
    assert!(matches!(
        err,
        KittyImageError::Decode(KittyPrecheckError::MalformedPng(_))
    ));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn admissible_rgba_stores_places_and_advances_cursor() {
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("admissible payload must store and place");
    let (image, _placement) = match outcome {
        KittyDisplayOutcome::Displayed { image, placement } => (image, placement),
        other => panic!("expected Displayed, got {other:?}"),
    };
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    // Cell reservation: the cursor advances past the 2x2 placement span so
    // subsequently printed text starts after the image, not over it.
    let cursor = rt.state().cursor().position;
    assert_eq!((cursor.col, cursor.row), (2, 2));
    let _ = image;
}

#[test]
fn transmit_only_stores_without_placing_or_moving_cursor() {
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), Some('t'), 2, 2, 0, &red_2x2(), 0)
        .expect("transmit-only must store");
    assert!(matches!(outcome, KittyDisplayOutcome::Stored { .. }));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    let cursor = rt.state().cursor().position;
    assert_eq!((cursor.col, cursor.row), (0, 0));
}

#[test]
fn unsupported_action_stores_without_painting() {
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), Some('q'), 2, 2, 0, &red_2x2(), 0)
        .expect("unsupported action must store");
    assert!(matches!(
        outcome,
        KittyDisplayOutcome::StoredNotDisplayed { .. }
    ));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn png_payload_stores_and_places() {
    let mut rt = make_runtime();
    let outcome = rt
        .kitty_display_image(100, None, None, None, 0, 0, 0, &red_1x1_png(), 0)
        .expect("valid PNG must store and place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
}

#[test]
fn displayed_image_paints_topmost_blit() {
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(rt.tick().is_some(), "display forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "placed image must composite one blit"
    );
}

#[test]
fn text_after_image_keeps_placement_and_blit() {
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    // Printing text after the image must not erase the placement (fastfetch
    // logo shape: image left, text right): the cursor already advanced past
    // the span, and the placement survives grid writes.
    rt.handle_pty_bytes(b"hello");
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn display_erase_clears_placement_but_keeps_store_shape() {
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    assert_eq!(rt.kitty_placement_count(), 1);
    // Whole-screen erase (ED 2) clears placements (kitty/ghostty parity).
    rt.handle_pty_bytes(b"\x1b[2J");
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn place_action_for_stored_image_paints_blit() {
    // Issue #2875: `a=p` names a previously transmitted image and places it
    // without pixels. Terminal truth records the anchor; the pixel layer now
    // mirrors it, so the `a=t` then `a=p` client shape (image.nvim style)
    // paints instead of leaving a blank rectangle.
    let mut rt = make_runtime();
    let encoded = "/wAA//8AAP//AAD//wAA/w==";
    let transmit = format!("\x1b_Gf=32,s=2,v=2,a=t,i=7,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(transmit.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=7,c=2,r=2\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1, "place action stores nothing new");
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "place action places the stored image on the pixel layer"
    );
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "the placed image composites one blit"
    );
    // The identity delete clears both layers: no orphan blit survives.
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=7\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 0);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 0);
}

#[test]
fn ed_scroll_and_clear_clears_placements_but_keeps_store_shape() {
    // Issue #1802 acceptance pin: ED 22 (ScrollAndClear) clears
    // placements like ED 2, while stored images stay inert.
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.handle_pty_bytes(b"\x1b[22J");
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(
        rt.kitty_image_count(),
        1,
        "ED 22 clears placements only; stored images stay inert"
    );
}

#[test]
fn ed_below_and_above_keep_placements() {
    // Issue #1802 acceptance pin: partial erases (ED 0 below, ED 1
    // above) never clear placements; only whole-screen erases do.
    for seq in [
        b"\x1b[0J".as_slice(),
        b"\x1b[J".as_slice(),
        b"\x1b[1J".as_slice(),
    ] {
        let mut rt = make_runtime();
        let _ = rt
            .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
            .expect("display must succeed");
        rt.handle_pty_bytes(seq);
        assert_eq!(
            rt.kitty_placement_count(),
            1,
            "partial erase must keep the placement"
        );
        assert_eq!(rt.kitty_image_count(), 1);
    }
}

#[test]
fn rejected_transmission_leaves_grid_idle() {
    let mut rt = make_runtime();
    let _ = rt.kitty_display_image(7, Some(2), Some(2), None, 1, 1, 0, &red_2x2(), 0);
    // Nothing stored, no redraw forced: after the initial grid present the
    // runtime idles exactly as if no transmission had arrived.
    assert!(rt.tick().is_some(), "first tick still presents the grid");
    assert_eq!(rt.tick(), None, "rejected image must not force a present");
}

#[test]
fn maximal_row_placement_bounds_scroll_work() {
    // CTX-1072 (#1850): a maximal-row (r=65535) placement of a 1x1 image
    // must not drive about 65k linefeeds per placement. Scroll is capped
    // to the trusted viewport height and the absolute ceiling, so work
    // stays bounded while the cursor still lands at the bottom.
    let mut rt = make_runtime();
    let viewport_rows = rt.state().height();
    let scrollback_before = rt.state().scrollback_len();
    let pixel = vec![0xFF, 0x00, 0x00, 0xFF];
    let start = std::time::Instant::now();
    let outcome = rt
        .kitty_display_image(32, Some(1), Some(1), None, 1, u16::MAX, 0, &pixel, 0)
        .expect("maximal-row placement must still place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(rt.kitty_placement_count(), 1);
    let elapsed = start.elapsed();
    // Bounded work: completes quickly (far below the unbounded 65k-apply
    // cost) and pushes at most one screen into scrollback.
    assert!(
        elapsed.as_secs() < 5,
        "maximal-row placement took {elapsed:?}, expected bounded work"
    );
    let scrolled = rt
        .state()
        .scrollback_len()
        .saturating_sub(scrollback_before);
    assert!(
        scrolled <= viewport_rows.max(1),
        "scrollback grew by {scrolled} lines, viewport is {viewport_rows}"
    );
    assert!(
        scrolled <= usize::from(bitty_rich::KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT),
        "scrollback grew by {scrolled} lines, exceeding the absolute ceiling"
    );
    // Cursor still lands at the bottom row, past the span.
    let cursor = rt.state().cursor().position;
    assert_eq!(
        usize::from(cursor.row),
        viewport_rows.saturating_sub(1),
        "cursor must clamp to the bottom row"
    );
    // The fitted 1x1 source is centered inside the maximal 1 x u16::MAX
    // cell rect and lands far outside the viewport, so zero blits is the
    // spec-correct fail-closed result; the bounded-scroll assertions above
    // are the point of this pin.
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 0);
}

#[test]
fn oversize_pixel_backed_placement_scrolls_full_span_like_kitty() {
    // Issue #1954: a placement taller than the viewport (chafa emits a
    // 150-row placement in a 40-row viewport) must scroll the full span
    // like kitty, so the image bottom lands at the cursor row and later
    // text rows print below the blit instead of underneath it. Capping
    // the scroll at one viewport leaves the bottom edge below the
    // screen: every later row prints under the image (S7 paints blits
    // above text) and the input line stays invisible.
    let mut rt = make_runtime();
    let viewport_rows = rt.state().height();
    let scrollback_before = rt.state().scrollback_len();
    // 60 cell rows of real pixels at the default 9x19 cell: explicit
    // spans (the chafa --size path), taller than the 24-row viewport.
    let tall_rows: u16 = 60;
    let pixels = vec![0x88u8; 9 * 19 * usize::from(tall_rows) * 4];
    let outcome = rt
        .kitty_display_image(
            32,
            Some(9),
            Some(19 * u32::from(tall_rows)),
            None,
            1,
            tall_rows,
            0,
            &pixels,
            0,
        )
        .expect("oversize pixel-backed placement must still place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(rt.kitty_placement_count(), 1);
    // Full kitty overflow from the origin anchor, not the viewport cap:
    // kitty moves the cursor down by rows - 1 and the screen scrolls to
    // fit, so rows - 1 linefeeds from row 0 scroll rows - viewport.
    let want_scroll = usize::from(tall_rows - 1).saturating_sub(viewport_rows.saturating_sub(1));
    assert!(
        want_scroll > viewport_rows,
        "fixture must overflow past the old viewport cap"
    );
    let scrolled = rt
        .state()
        .scrollback_len()
        .saturating_sub(scrollback_before);
    assert_eq!(
        scrolled, want_scroll,
        "oversize pixel-backed span must scroll its full overflow like kitty"
    );
    assert!(
        scrolled <= usize::from(bitty_rich::KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT),
        "scroll stays under the absolute ceiling"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!(
        usize::from(cursor.row),
        viewport_rows.saturating_sub(1),
        "cursor must land at the bottom row"
    );
    // The viewport-clipped placement still paints one blit.
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // Post-placement rows stay addressable: typed text lands on the
    // grid past the span instead of being swallowed by the image.
    rt.handle_pty_bytes(b"AB");
    let snap = rt.snapshot();
    let row_start = viewport_rows.saturating_sub(1) * snap.width;
    assert_eq!(snap.cells[row_start + 1].glyph, 'A');
    assert_eq!(snap.cells[row_start + 2].glyph, 'B');
    let cursor = rt.state().cursor().position;
    assert_eq!(usize::from(cursor.row), viewport_rows.saturating_sub(1));
    assert_eq!(usize::from(cursor.col), 3);
}

#[test]
fn oversize_derived_span_scrolls_full_span() {
    // Issue #1954, no---size path: spans derived from pixels (no
    // explicit c=/r=) taller than the viewport scroll the full span
    // exactly like the explicit-span path above.
    let mut rt = make_runtime();
    let viewport_rows = rt.state().height();
    let scrollback_before = rt.state().scrollback_len();
    let tall_rows: u16 = 60;
    let pixels = vec![0x88u8; 9 * 19 * usize::from(tall_rows) * 4];
    let outcome = rt
        .kitty_display_image(
            32,
            Some(9),
            Some(19 * u32::from(tall_rows)),
            None,
            0,
            0,
            0,
            &pixels,
            0,
        )
        .expect("derived oversize placement must still place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    let want_scroll = usize::from(tall_rows - 1).saturating_sub(viewport_rows.saturating_sub(1));
    let scrolled = rt
        .state()
        .scrollback_len()
        .saturating_sub(scrollback_before);
    assert_eq!(
        scrolled, want_scroll,
        "derived oversize span must scroll its full overflow like kitty"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!(
        usize::from(cursor.row),
        viewport_rows.saturating_sub(1),
        "cursor must land at the bottom row"
    );
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn oversize_placement_near_bottom_scrolls_to_fit() {
    // Issue #1954 headed shape: the image is placed near the bottom of
    // the viewport (right after a shell prompt), so almost every
    // linefeed scrolls. The image bottom still lands at the cursor row.
    let mut rt = make_runtime();
    let viewport_rows = rt.state().height();
    rt.handle_pty_bytes(b"\x1b[21;1H");
    assert_eq!(usize::from(rt.state().cursor().position.row), 20);
    let scrollback_before = rt.state().scrollback_len();
    let tall_rows: u16 = 60;
    let pixels = vec![0x88u8; 9 * 19 * usize::from(tall_rows) * 4];
    let outcome = rt
        .kitty_display_image(
            32,
            Some(9),
            Some(19 * u32::from(tall_rows)),
            None,
            1,
            tall_rows,
            0,
            &pixels,
            0,
        )
        .expect("oversize placement near the bottom must still place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    // 59 linefeeds from row 20: 3 walk to the bottom, 56 scroll.
    let want_scroll = usize::from(tall_rows - 1)
        .saturating_sub(viewport_rows.saturating_sub(1).saturating_sub(20));
    let scrolled = rt
        .state()
        .scrollback_len()
        .saturating_sub(scrollback_before);
    assert_eq!(
        scrolled, want_scroll,
        "near-bottom oversize span must scroll to fit like kitty"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!(
        usize::from(cursor.row),
        viewport_rows.saturating_sub(1),
        "cursor must land at the bottom row"
    );
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn exact_and_smaller_spans_keep_prior_scroll_behavior() {
    // Issue #1954 non-goals: placements that fit (or exactly fill) the
    // viewport keep the pre-existing scroll accounting bit-for-bit.
    let mut rt = make_runtime();
    let viewport_rows = rt.state().height();
    // Smaller span: no overflow, no scroll, cursor past the span.
    let scrollback_before = rt.state().scrollback_len();
    let outcome = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("small placement must place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(
        rt.state().scrollback_len() - scrollback_before,
        0,
        "fitting span must not scroll"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!((cursor.col, cursor.row), (2, 2));
    // Exact-viewport span from the origin: the cursor walks to the last
    // row with plain linefeeds, so no scroll is driven at all —
    // unchanged by the oversize fix.
    let mut rt = make_runtime();
    let exact_rows = u16::try_from(viewport_rows).unwrap_or(u16::MAX);
    let exact_pixels = vec![0x88u8; 9 * 19 * viewport_rows * 4];
    let scrollback_before = rt.state().scrollback_len();
    let outcome = rt
        .kitty_display_image(
            32,
            Some(9),
            Some(19 * u32::try_from(viewport_rows).unwrap_or(u32::MAX)),
            None,
            1,
            exact_rows,
            0,
            &exact_pixels,
            0,
        )
        .expect("exact-viewport placement must place");
    assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    assert_eq!(
        rt.state().scrollback_len() - scrollback_before,
        0,
        "exact-viewport span from the origin must not scroll"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!(
        usize::from(cursor.row),
        viewport_rows.saturating_sub(1),
        "cursor must land at the bottom row"
    );
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn placement_delete_then_render_leaves_no_orphan_blit() {
    // CTX-1072 (#1850): protocol-level deletion clears the rendered
    // placement through the origin-scoped wire identity mapping, so the
    // next present paints no orphan blit.
    let mut rt = make_runtime();
    let encoded = "/wAA//8AAP//AAD//wAA/w==";
    let display = format!("\x1b_Gf=32,s=2,v=2,i=7,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(display.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=7\x1b\\");
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "protocol delete must clear the rendered placement"
    );
    assert!(rt.tick().is_some(), "delete forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "no orphan blit may survive deletion"
    );
}

#[test]
fn delete_image_id_zero_deletes_nothing_anonymous_survives() {
    // CodeRabbit Minor (pty.rs:942): `a=d,d=i` with no `i=` key yields
    // `image_id` 0, and `delete_by_wire(origin, 0, None)` wipes every
    // anonymous placement on the origin. Kitty treats `i=0` as no image
    // named, so the delete must leave anonymous placements alone.
    let mut rt = make_runtime();
    let encoded = "/wAA//8AAP//AAD//wAA/w==";
    // No `i=` key: anonymous placement (`wire_image == 0`).
    let display = format!("\x1b_Gf=32,s=2,v=2,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(display.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // Missing `i=` defaults to 0: must delete nothing.
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i\x1b\\");
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "d=i with no i= must delete nothing"
    );
    // Explicit `i=0` also names no image.
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=0\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 1, "d=i,i=0 must delete nothing");
    let _ = rt.tick();
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "anonymous blit must survive a zero-id delete"
    );
}

#[test]
fn primary_origin_placement_quota_is_32_fifo() {
    // S5 per-origin quotas (#1849, CTX-1093): the primary origin keeps 32
    // placements; the 33rd evicts the oldest of its own origin (FIFO),
    // deterministically. `C=1` keeps the cursor still so grid scroll plays
    // no role in the count.
    let mut rt = make_runtime();
    let pixel = vec![0xFF, 0x00, 0x00, 0xFF];
    for _ in 0..bitty_rich::KITTY_PER_ORIGIN_MAX_PLACEMENTS + 1 {
        let outcome = rt
            .kitty_display_image(32, Some(1), Some(1), None, 1, 1, 1, &pixel, 0)
            .expect("within-quota display must succeed");
        assert!(matches!(outcome, KittyDisplayOutcome::Displayed { .. }));
    }
    assert_eq!(
        rt.kitty_placement_count(),
        bitty_rich::KITTY_PER_ORIGIN_MAX_PLACEMENTS,
        "primary origin keeps exactly its placement quota"
    );
    assert_eq!(
        rt.kitty_image_count(),
        bitty_rich::KITTY_PER_ORIGIN_MAX_PLACEMENTS + 1,
        "stores are unaffected by placement eviction"
    );
}

#[test]
fn primary_origin_image_fifo_stays_64() {
    // S5 keeps single-origin count behavior: transmit-only stores past 64
    // evict oldest-first, 64 stay.
    let mut rt = make_runtime();
    let pixel = vec![0xFF, 0x00, 0x00, 0xFF];
    for _ in 0..bitty_rich::KITTY_PLACE_MAX_IMAGES + 3 {
        rt.kitty_transmit_image(32, Some(1), Some(1), &pixel, 4)
            .expect("within-quota transmit must succeed");
    }
    assert_eq!(rt.kitty_image_count(), bitty_rich::KITTY_PLACE_MAX_IMAGES);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn ed2_clear_keeps_store_shape_under_quotas() {
    // ED2 clearing is unchanged by S5: placements drop, stored images stay
    // inert under the store quotas.
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(rt.kitty_image_count(), 1);
    rt.handle_pty_bytes(b"\x1b[2J");
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(
        rt.kitty_image_count(),
        1,
        "ED2 clears placements only; stored images stay inert"
    );
}

#[test]
fn focused_cursor_wins_over_covering_image() {
    // #1849 S7 cursor-on-top: an image rect covering the cursor cell must
    // not hide the focused cursor. The cursor fill stays in the overlay
    // after the combined images, so the headless pixel at the cursor cell
    // carries the theme cursor hue while a sibling covered cell stays
    // image red. The cursor overlay is not counted in the 32-blit budget.
    use bitty_runtime::RuntimeConfig;

    let mut rt = make_runtime();
    // 2x2-cell opaque red image placed at the cursor origin (0,0); the
    // cursor advances past it to (2,2).
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(rt.tick().is_some(), "display forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // Move the cursor back onto the image-covered cell (0,0).
    rt.handle_pty_bytes(b"\x1b[1;1H");
    let stats = rt.tick().expect("cursor move must present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "covering image must still composite its blit"
    );
    assert_eq!(
        stats.images, 1,
        "cursor overlay must not consume the blit budget"
    );
    // Same geometry pins as `tick_cursor_overlay_uses_theme_cursor_hue`
    // in `runtime_present.rs`: default 9x19 cells, 8px window padding,
    // 14px decoration inset, 736px window width.
    let cfg = RuntimeConfig::default();
    assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    assert_eq!(pad, 8, "default padding inset is 8px at scale 1.0");
    const DECORATION: usize = 14;
    let width = usize::try_from(cfg.window_extent().width()).expect("width fits usize");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    // Cursor cell (0,0): theme cursor #f5e0dc at 0xA0 alpha, premultiplied
    // overwrite by the overlay fill — not image red.
    let cx = pad + DECORATION + 4;
    let cy = pad + DECORATION + 9;
    let idx = (cy * width + cx) * 4;
    assert_eq!(
        &rgba[idx..idx + 4],
        &[153, 140, 138, 160],
        "cursor cell covered by the image must still carry the cursor hue"
    );
    // Sibling covered cell (1,0): opaque image red, proving the punch is
    // confined to the cursor rect.
    let rx = pad + DECORATION + cfg.cell_width as usize + 4;
    let ridx = (cy * width + rx) * 4;
    assert_eq!(
        &rgba[ridx..ridx + 4],
        &[255, 0, 0, 255],
        "non-cursor covered cell must stay image red"
    );
}

#[test]
fn raster_cache_reuses_across_ticks_byte_identical() {
    // S6 (#1849): the second present with unchanged placement geometry
    // must serve the blit from the cache (hit) with the same composited
    // blit count as the uncached first present.
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 1, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(rt.tick().is_some(), "display forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 1);
    assert_eq!(rt.kitty_raster_len(), 1, "first present warms one entry");
    assert!(
        rt.kitty_raster_misses() >= 1,
        "first present must miss into the uncached path"
    );
    assert!(
        rt.kitty_raster_bytes() <= 64 * 1024 * 1024,
        "resident bytes stay under the 64 MiB ceiling"
    );
    // Grid damage without moving the placement: the image rect is
    // unchanged, so the next present must hit.
    rt.handle_pty_bytes(b"x");
    assert!(rt.tick().is_some(), "grid damage must present again");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "cached present must composite the same blit"
    );
    assert!(
        rt.kitty_raster_hits() >= 1,
        "second present with identical geometry must hit"
    );
    assert!(
        rt.kitty_raster_len() <= 32,
        "entries stay under the 32-entry ceiling"
    );
}

#[test]
fn raster_cache_disabled_still_paints_uncached() {
    // S6 disable-path: a disabled cache still paints via the uncached
    // fallback, so correctness never depends on it.
    let mut rt = make_runtime();
    rt.set_kitty_raster_enabled(false);
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 1, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(rt.tick().is_some(), "display forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "disabled cache must still paint the blit uncached"
    );
    assert_eq!(rt.kitty_raster_hits(), 0, "disabled cache never hits");
    assert_eq!(rt.kitty_raster_len(), 0, "disabled cache admits nothing");
    // Re-enabling re-warms on the next present with identical bytes.
    rt.set_kitty_raster_enabled(true);
    rt.handle_pty_bytes(b"y");
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    assert_eq!(
        rt.kitty_raster_len(),
        1,
        "re-enabled cache re-warms from the uncached path"
    );
}

#[test]
fn raster_cache_invalidate_on_store_drops_stale() {
    // S6 invalidation: a store (which may evict same-origin images) drops
    // cached blits wholesale; the next present re-warms and still paints
    // the live placement, never stale bytes.
    let mut rt = make_runtime();
    let _ = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 1, &red_2x2(), 0)
        .expect("display must succeed");
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_raster_len(), 1, "first present warms the cache");
    // Transmit-only store of a blue image: clears the cache, places nothing.
    let blue = [0x00, 0x00, 0xFF, 0xFF].repeat(4);
    rt.kitty_transmit_image(32, Some(2), Some(2), &blue, 16)
        .expect("transmit must succeed");
    assert_eq!(
        rt.kitty_raster_len(),
        0,
        "store must invalidate cached blits"
    );
    assert_eq!(rt.kitty_placement_count(), 1, "red placement survives");
    rt.handle_pty_bytes(b"z");
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "live placement re-warms and still paints after invalidation"
    );
}

#[test]
fn idle_runtime_holds_no_bytes_in_bounded_stores() {
    // CTX-1144 (#1968): settled idle single-pane meets RSS <= 150MB and
    // PSS <= 120MB (release, WS5, 10s settle: HEAD ~130MB RSS / ~103MB PSS,
    // flat vs the Oct 09 baseline; the ~184MB snapshot was early + driven,
    // not settled). Every bounded image, raster, background, fx, pin, and
    // local-medium store constructs empty and admits only on use, so a
    // future eager allocation fails here instead of regressing silently.
    let rt = make_runtime();
    assert_eq!(rt.kitty_image_count(), 0, "no image stored at idle");
    assert_eq!(rt.kitty_placement_count(), 0, "no placement at idle");
    assert_eq!(rt.kitty_virtual_count(), 0, "no virtual prototype at idle");
    assert_eq!(rt.kitty_last_frame_images(), 0, "no blit painted at idle");
    assert_eq!(rt.kitty_raster_len(), 0, "raster cache empty at idle");
    assert_eq!(rt.kitty_raster_bytes(), 0, "raster cache holds no bytes");
    assert_eq!(
        rt.kitty_raster_hits(),
        0,
        "no raster hit before first paint"
    );
    assert_eq!(
        rt.kitty_raster_misses(),
        0,
        "no raster miss before first paint"
    );
    assert_eq!(rt.background_image_count(), 0, "no background at idle");
    assert_eq!(rt.background_bytes(), 0, "background holds no bytes");
    assert_eq!(rt.background_loads(), 0, "no background decoded at idle");
    assert_eq!(
        rt.background_raster_stats(),
        (0, 0, 0, 0),
        "background raster cache empty at idle"
    );
    assert_eq!(rt.pinned_count(), 0, "no pinned panel at idle");
    assert!(rt.pinned_views().is_empty(), "no pinned views at idle");
    assert_eq!(
        rt.kitty_local_pending_count(),
        0,
        "no local-medium work at idle"
    );
    assert_eq!(
        rt.kitty_local_deadline(),
        None,
        "no worker deadline at idle"
    );
    assert!(!rt.animations_active(), "no animation active at idle");
    assert_eq!(rt.animation_deadline(), None, "no wakeup armed at idle");
    assert_eq!(
        rt.fx_progress(ViewId::new(1), std::time::Instant::now()),
        None,
        "no fx pulse armed at idle"
    );
}

// ---------------------------------------------------------------------------
// Relative placements (issue #2875): `a=p` with `P=`/`Q=`/`H=`/`V=`
// ---------------------------------------------------------------------------
//
// A relative placement must paint at the absolute rect derived from its
// parent's *painted* origin plus the wire cell offsets, follow the parent
// through scrolls, die with it, and fail closed on a missing/virtual/cyclic
// parent. These pins assert through the same headless surface the shipped
// kitty paint tests use: placement counts, composite blit counts, and the
// pixels of the presented frame (`headless_rgba`).

/// Default headless cell metrics (9x19, pinned by
/// `focused_cursor_wins_over_covering_image`). A `2 x 2`-cell source of
/// exactly `18 x 38` pixels therefore fills its cell rect with no
/// letterboxing, so a colour bounding box in the frame *is* the placement
/// geometry.
const CELL_W: usize = 9;
const CELL_H: usize = 19;
const PARENT_W: u32 = (CELL_W as u32) * 2;
const PARENT_H: u32 = (CELL_H as u32) * 2;
const RED: [u8; 4] = [0xFF, 0x00, 0x00, 0xFF];
const BLUE: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

/// Opaque `w x h` RGBA8 fixture in one colour.
fn solid_rgba(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    rgba.repeat((w * h) as usize)
}

/// Stores an image under wire id `wire_image` with `a=t` (no placement).
fn transmit_solid(rt: &mut Runtime, wire_image: u32, rgba: [u8; 4]) {
    let pixels = solid_rgba(PARENT_W, PARENT_H, rgba);
    rt.kitty_display_image_with_wire(
        32,
        Some(PARENT_W),
        Some(PARENT_H),
        Some('t'),
        0,
        0,
        0,
        &pixels,
        0,
        wire_image,
        0,
    )
    .expect("transmit must store");
}

/// Absolute `a=p` at cell `(col, row)` with `C=1` (no cursor motion), the
/// canonical parent shape.
fn place_absolute(rt: &mut Runtime, image: u32, pin: u32, col: u16, row: u16) {
    rt.handle_pty_bytes(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    rt.handle_pty_bytes(format!("\x1b_Ga=p,i={image},p={pin},c=2,r=2,C=1\x1b\\").as_bytes());
}

/// Relative `a=p` (`P=`/`Q=` plus `H=`/`V=`) for the same 2x2-cell span.
fn place_relative(
    rt: &mut Runtime,
    image: u32,
    pin: u32,
    parent: (u32, u32),
    offset: (i32, i32),
    z: i32,
) {
    let (dx, dy) = offset;
    let (parent_image, parent_pin) = parent;
    rt.handle_pty_bytes(
        format!(
            "\x1b_Ga=p,i={image},p={pin},c=2,r=2,C=1,P={parent_image},Q={parent_pin},H={dx},V={dy},z={z}\x1b\\"
        )
        .as_bytes(),
    );
}

/// Red parent `i=7,p=1` at `origin` plus the blue child `i=8,p=2` placed
/// with `P=7,Q=1,H=2,V=1` (the relative wire form) and `z=child_z`.
fn relative_pair(origin: (u16, u16), child_z: i32) -> Runtime {
    let mut rt = make_runtime();
    transmit_solid(&mut rt, 7, RED);
    transmit_solid(&mut rt, 8, BLUE);
    place_absolute(&mut rt, 7, 1, origin.0, origin.1);
    place_relative(&mut rt, 8, 2, (7, 1), (2, 1), child_z);
    rt
}

/// Headless frame width in physical pixels.
fn frame_width() -> usize {
    usize::try_from(
        bitty_runtime::RuntimeConfig::default()
            .window_extent()
            .width(),
    )
    .expect("window width fits usize")
}

/// Bounding box `(x, y, w, h)` of the pixels exactly equal to `rgba`.
fn solid_bbox(frame: &[u8], width: usize, rgba: [u8; 4]) -> Option<(usize, usize, usize, usize)> {
    let rows = frame.len() / (width * 4);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (usize::MAX, usize::MAX, 0, 0);
    let mut found = false;
    for row in 0..rows {
        for col in 0..width {
            let index = (row * width + col) * 4;
            if frame[index..index + 4] == rgba {
                found = true;
                min_x = min_x.min(col);
                max_x = max_x.max(col);
                min_y = min_y.min(row);
                max_y = max_y.max(row);
            }
        }
    }
    found.then(|| (min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}

/// Count of pixels exactly equal to `rgba` inside `rect`.
fn count_pixels(
    frame: &[u8],
    width: usize,
    rect: (usize, usize, usize, usize),
    rgba: [u8; 4],
) -> usize {
    let (x, y, w, h) = rect;
    let mut count = 0;
    for row in y..y + h {
        for col in x..x + w {
            let index = (row * width + col) * 4;
            if frame[index..index + 4] == rgba {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn relative_child_paints_at_parent_origin_plus_offsets() {
    // Issue #2875: `a=p,...,P=7,Q=1,H=2,V=1` paints at the parent's painted
    // origin shifted by two cells right and one cell down — the wire `H=`/`V=`
    // offsets — instead of at the cursor or not at all.
    let mut rt = relative_pair((0, 0), 0);
    assert_eq!(rt.kitty_placement_count(), 2, "parent and child both place");
    assert!(rt.tick().is_some(), "placement forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 2, "both images composite");
    let width = frame_width();
    let frame = rt.headless_rgba().expect("rgba after tick");
    let parent = solid_bbox(&frame, width, RED).expect("parent paints");
    let child = solid_bbox(&frame, width, BLUE).expect("relative child paints");
    assert_eq!(
        (parent.2, parent.3),
        (2 * CELL_W, 2 * CELL_H),
        "fixture parent fills its 2x2 cell rect"
    );
    assert_eq!(
        (child.2, child.3),
        (2 * CELL_W, 2 * CELL_H),
        "the child keeps its own span, not the parent's"
    );
    assert_eq!(
        (child.0, child.1),
        (parent.0 + 2 * CELL_W, parent.1 + CELL_H),
        "child origin must be parent origin + H=2 / V=1 cells"
    );
}

#[test]
fn relative_child_follows_parent_through_a_scroll() {
    // Issue #2875: the child anchor is derived from the parent at paint
    // time, so a grid scroll moves both by the same pixel delta with no
    // per-child scroll bookkeeping.
    //
    // Two scroll phases: the first also moves the painted window onto the
    // cursor row (`cursor_follow_window_start`, a pane property unrelated to
    // kitty), so only the second phase has an exact, window-sized delta.
    // Both phases pin the invariant that matters: the child keeps its `V=1`
    // offset below wherever the parent actually paints.
    let mut rt = relative_pair((0, 12), 0);
    assert!(rt.tick().is_some());
    let width = frame_width();
    let before = rt.headless_rgba().expect("rgba after first tick");
    let parent_before = solid_bbox(&before, width, RED).expect("parent paints");
    let child_before = solid_bbox(&before, width, BLUE).expect("child paints");
    assert_eq!(child_before.1, parent_before.1 + CELL_H);

    let scrollback_before = rt.state().scrollback_len();
    // Linefeeds at the bottom row scroll the grid by three rows.
    rt.handle_pty_bytes(b"\x1b[24;1H\n\n\n");
    assert_eq!(
        rt.state().scrollback_len() - scrollback_before,
        3,
        "fixture must scroll exactly three rows"
    );
    assert!(rt.tick().is_some(), "scroll forces a present");
    let first = rt.headless_rgba().expect("rgba after first scroll");
    let parent_first = solid_bbox(&first, width, RED).expect("parent still paints");
    let child_first = solid_bbox(&first, width, BLUE).expect("child still paints");
    assert!(
        parent_first.1 < parent_before.1,
        "the scroll must move the parent up"
    );
    assert_eq!(
        child_first.1,
        parent_first.1 + CELL_H,
        "the child keeps its V=1 offset below the scrolled parent"
    );

    // Second phase: the cursor already sits on the bottom row, so the whole
    // window shift is the scroll delta and the geometry must be exact.
    let scrollback_mid = rt.state().scrollback_len();
    rt.handle_pty_bytes(b"\n\n\n");
    assert_eq!(rt.state().scrollback_len() - scrollback_mid, 3);
    assert!(rt.tick().is_some(), "scroll forces a present");
    let second = rt.headless_rgba().expect("rgba after second scroll");
    let parent_second = solid_bbox(&second, width, RED).expect("parent still paints");
    let child_second = solid_bbox(&second, width, BLUE).expect("child still paints");
    assert_eq!(
        parent_second.1,
        parent_first.1 - 3 * CELL_H,
        "the parent moved up one cell per scrolled row"
    );
    assert_eq!(
        child_second.1,
        parent_second.1 + CELL_H,
        "the child keeps its V=1 offset below the scrolled parent"
    );
    assert_eq!(
        (child_second.0, child_second.2, child_second.3),
        (child_before.0, child_before.2, child_before.3),
        "a vertical scroll must not change the child's column or span"
    );
}

#[test]
fn relative_child_disappears_when_its_parent_is_deleted() {
    // Issue #2875: deleting the parent cascades in terminal truth *and* in
    // the pixel layer, so no orphan blit survives (and a later re-place of
    // the same wire identity cannot resurrect the dropped child).
    let mut rt = relative_pair((0, 0), 0);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 2);

    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=7,p=1\x1b\\");
    assert_eq!(
        rt.state().kitty_placements().len(),
        0,
        "terminal truth cascades to the relative child"
    );
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "the pixel layer cascades with truth"
    );
    assert!(rt.tick().is_some(), "delete forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 0, "no orphan child blit");
    let width = frame_width();
    let frame = rt.headless_rgba().expect("rgba after delete");
    assert!(
        solid_bbox(&frame, width, BLUE).is_none(),
        "the child must leave no pixels on screen"
    );
}

#[test]
fn relative_child_fully_off_grid_paints_nothing() {
    // Issue #2875 fail-closed: a resolved rect entirely outside the grid is
    // clipped away by the viewport intersection, while the parent keeps
    // painting. `V=30` lands 30 rows below a 24-row viewport.
    let mut rt = make_runtime();
    transmit_solid(&mut rt, 7, RED);
    transmit_solid(&mut rt, 8, BLUE);
    place_absolute(&mut rt, 7, 1, 0, 0);
    place_relative(&mut rt, 8, 2, (7, 1), (0, 30), 0);
    assert_eq!(
        rt.kitty_placement_count(),
        2,
        "both placements are admitted; only painting is clipped"
    );
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "only the on-grid parent composites a blit"
    );
    let width = frame_width();
    let frame = rt.headless_rgba().expect("rgba after tick");
    assert!(
        solid_bbox(&frame, width, RED).is_some(),
        "parent still paints"
    );
    assert!(
        solid_bbox(&frame, width, BLUE).is_none(),
        "an off-grid child paints no pixels"
    );
}

#[test]
fn relative_below_text_child_paints_under_glyphs() {
    // Issue #2875 #2: z-order is the child's own `z=`. A `z=-1` child
    // composites below text, so glyph ink punches through its rect; the same
    // child at `z=0` sits above the text and covers it completely. The
    // control run is what makes this non-tautological.
    //
    // The parent sits at row 2 and the cursor parks inside the pane window
    // but outside the child rect, so the painted window does not follow the
    // cursor away from the image (`cursor_follow_window_start`).
    let text_over_child = b"\x1b[4;1HMMMM";
    let cursor_away = b"\x1b[4;20H";
    let mut below = relative_pair((0, 2), -1);
    below.handle_pty_bytes(text_over_child);
    below.handle_pty_bytes(cursor_away);
    assert!(below.tick().is_some());
    let width = frame_width();
    let frame = below.headless_rgba().expect("rgba after tick");
    let rect = solid_bbox(&frame, width, BLUE).expect("below-text child paints");
    let blue = count_pixels(&frame, width, rect, BLUE);
    let area = rect.2 * rect.3;
    assert!(blue > 0, "the child must paint its own pixels");
    assert!(
        blue < area,
        "text must paint over a z=-1 child ({blue} of {area} pixels stay image blue)"
    );

    let mut above = relative_pair((0, 2), 0);
    above.handle_pty_bytes(text_over_child);
    above.handle_pty_bytes(cursor_away);
    assert!(above.tick().is_some());
    let frame = above.headless_rgba().expect("rgba after tick");
    let rect = solid_bbox(&frame, width, BLUE).expect("above-text child paints");
    let blue = count_pixels(&frame, width, rect, BLUE);
    let area = rect.2 * rect.3;
    assert_eq!(
        blue, area,
        "a z=0 child covers the text: the control proves the layering assertion above"
    );
}

#[test]
fn relative_child_without_a_parent_paints_nothing() {
    // Issues #2875 fail-closed: terminal truth refuses a missing parent
    // (`ENOPARENT`) and a virtual prototype as a parent (`EINVAL`), so
    // neither reaches the pixel layer. The absolute control run proves the
    // same command without `P=` does paint here, so the assertions below
    // test the parent link, not a dead paint path.
    let mut rt = make_runtime();
    transmit_solid(&mut rt, 8, BLUE);
    // Control: the identical `a=p` without `P=` places and paints.
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=8,p=2,c=2,r=2,C=1\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 1, "absolute control places");
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1, "absolute control paints");
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=8,p=2\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 0);

    // Missing parent: `P=99` names no stored placement.
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=8,p=2,c=2,r=2,C=1,P=99,Q=5,H=1,V=1\x1b\\");
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "a missing parent places nothing on the pixel layer"
    );
    // Virtual prototype parent: `a=p,U=1` registers image 9 as a prototype,
    // which is never a paintable parent.
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=9,p=3,c=2,r=2\x1b\\");
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=8,p=2,c=2,r=2,C=1,P=9,Q=0,H=1,V=1\x1b\\");
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "a virtual prototype is never a paintable parent"
    );
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "no blit for a refused child"
    );
    let width = frame_width();
    let frame = rt.headless_rgba().expect("rgba after tick");
    assert!(solid_bbox(&frame, width, BLUE).is_none());
}

#[test]
fn relative_parent_cycle_fails_closed_without_hanging() {
    // Issue #2875 fail-closed (depth bound): re-putting the parent with the
    // child as *its* parent is wire-reachable, because every re-put only
    // requires the named parent to exist. The paint walk must terminate and
    // paint nothing instead of looping.
    let mut rt = make_runtime();
    transmit_solid(&mut rt, 7, RED);
    transmit_solid(&mut rt, 8, BLUE);
    place_absolute(&mut rt, 7, 1, 0, 0);
    place_relative(&mut rt, 8, 2, (7, 1), (2, 1), 0);
    // Re-put p1 as a child of p2: p1 -> p2 -> p1.
    place_relative(&mut rt, 7, 1, (8, 2), (0, 0), 0);
    assert!(rt.tick().is_some(), "cyclic placements must still present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "a cyclic parent chain paints nothing"
    );
}

/// #2131: yazi's KgpOld driver flow must display, paint, and erase through
/// raw PTY bytes with no manual flags.
///
/// With an unknown brand yazi picks KgpOld and emits `a=T` transmit+display
/// (`C=1` keeps the cursor, `q=2` stays quiet, `z=-1` sits behind text)
/// followed by `a=d,d=A` erases. This pins the wire half of kitty-by-default:
/// the probe battery (see `terminal_queries::yazi_probe_battery`) selects
/// the driver, this test proves the driver paints.
#[test]
fn yazi_kgpold_display_paints_and_erases() {
    let mut rt = make_runtime();
    // yazi move_lock equivalent: cursor home before the transmit.
    rt.handle_pty_bytes(b"\x1b[H");
    // 2x2 opaque red RGBA (`f=32`) in two chunks, yazi's exact control keys.
    rt.handle_pty_bytes(b"\x1b_Gq=2,a=T,z=-1,C=1,f=32,s=2,v=2,m=1;/wAA//8AAP//\x1b\\");
    rt.handle_pty_bytes(b"\x1b_Gm=0;AAD//wAA/w==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1, "a=T must store the image");
    assert_eq!(rt.kitty_placement_count(), 1, "a=T must place it");
    assert!(
        rt.take_replies().is_empty(),
        "q=2 must suppress transmit replies"
    );
    let cursor = rt.state().cursor().position;
    assert_eq!(
        (cursor.col, cursor.row),
        (0, 0),
        "C=1 must keep the cursor home"
    );
    let stats = rt.tick().expect("display must force a present");
    assert!(stats.images >= 1, "tick must paint the image");
    assert_eq!(rt.kitty_last_frame_images(), 1);
    let rgba = rt.headless_rgba().expect("frame must render");
    assert!(
        rgba.iter().any(|b| *b != 0),
        "painted frame must contain image pixels"
    );
    // yazi image_erase for KgpOld: delete everything visible.
    rt.handle_pty_bytes(b"\x1b_Gq=2,a=d,d=A\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 0, "erase-all must clear");
    assert_eq!(
        rt.kitty_image_count(),
        1,
        "erase removes placement, keeps image"
    );
}

/// Minimal standard-alphabet base64 encoder (the file has no encoder: the
/// existing fixtures ship pre-encoded).
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

/// Bounding box and count of opaque red pixels in the presented frame.
fn red_bounds(rt: &Runtime) -> Option<(usize, usize, usize, usize, usize)> {
    let extent = rt
        .surface_extent()
        .unwrap_or_else(|| rt.config().window_extent());
    let width = usize::try_from(extent.width()).ok()?;
    let rgba = rt.headless_rgba()?;
    let (mut min_x, mut min_y, mut max_x, mut max_y, mut count) =
        (usize::MAX, usize::MAX, 0usize, 0usize, 0usize);
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

#[test]
fn spanless_bodiless_place_paints_at_the_images_natural_cell_size() {
    // Issue #2875 field evidence: an inline placement in the *main grid*
    // never painted. image.nvim's "normal" kitty backend transmits with
    // `a=t`, then displays with `a=p` and **no `c=`/`r=`** (the span is the
    // image's natural cell size, `ceil(pixels / cell)`) plus `z=-1` and
    // `C=1`. Terminal truth cannot derive that span (it holds no pixels)
    // and refused the command, so nothing was painted and the concealed
    // source text stayed visible.
    let mut rt = make_runtime();
    // Deterministic single-tick presents: no pacing coalescing between the
    // cursor move and the image frame (the pixel probe needs the exact
    // frame the blit was built for).
    rt.set_min_present_interval(std::time::Duration::ZERO);
    // Park the cursor inside the visible cursor-follow window: the headless
    // default shows a 76x22 frame of an 80x24 grid, so grid row 0 is above
    // the painted area and a placement there would clip.
    rt.handle_pty_bytes(b"\x1b[5;3H");
    // Hide the cursor: the focused cursor composites above images by
    // design (#1849 S7), so it would mask one cell of the probe.
    rt.handle_pty_bytes(b"\x1b[?25l");
    // 18x38 px at the headless 9x19 cell => 2 columns x 2 rows.
    let rgba = [0xFFu8, 0x00, 0x00, 0xFF].repeat(18 * 38);
    let encoded = base64(&rgba);
    rt.handle_pty_bytes(
        format!("\x1b_Ga=t,f=32,s=18,v=38,C=1,q=2,i=5,m=0;{encoded}\x1b\\").as_bytes(),
    );
    assert_eq!(rt.kitty_image_count(), 1, "transmit stores the pixels");
    assert_eq!(rt.kitty_placement_count(), 0, "transmit places nothing");
    // The placement names no span: the runtime must derive it from the
    // pixels the transmit already stored.
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=5,p=5,z=-1,C=1,q=2\x1b\\");
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "a spanless a=p of a stored image must place, not be dropped"
    );
    assert!(rt.tick().is_some(), "the placement forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "the inline placement must composite a blit"
    );
    let (x0, y0, x1, y1, count) = red_bounds(&rt).expect("the inline image must paint");
    // The image is painted over the cells it names: `ceil(18 / 9)` = 2
    // columns and `ceil(38 / 19)` = 2 rows, and an 18x38 source fills that
    // 18x38 box exactly (no letterbox, aspect already matches).
    assert_eq!(x1 - x0 + 1, 18, "two derived columns at 9px per cell");
    assert_eq!(y1 - y0 + 1, 38, "two derived rows at 19px per cell");
    assert!(
        count >= 18 * 38,
        "the whole fitted image is opaque: {count}"
    );
    // `C=1` keeps the cursor on the anchor cell (1-based row 5, col 3).
    let cursor = rt.state().cursor().position;
    assert_eq!((cursor.col, cursor.row), (2, 4), "C=1 keeps the cursor");
}

#[test]
fn spanless_bodiless_place_advances_the_cursor_past_the_derived_span() {
    // The same shape without `C=1`: kitty moves the cursor past the image,
    // so the derived span must reach terminal truth, not only the blit.
    let mut rt = make_runtime();
    let rgba = [0xFFu8, 0x00, 0x00, 0xFF].repeat(18 * 38);
    let encoded = base64(&rgba);
    rt.handle_pty_bytes(format!("\x1b_Ga=t,f=32,s=18,v=38,q=2,i=6,m=0;{encoded}\x1b\\").as_bytes());
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=6,p=6,q=2\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 1);
    let cursor = rt.state().cursor().position;
    assert_eq!(
        (cursor.col, cursor.row),
        (2, 2),
        "the cursor advances past the derived 2x2 span"
    );
    // Terminal truth carries the same span, so the a11y footprint agrees.
    let spans = rt.kitty_image_cell_spans_for_primary();
    assert_eq!(spans.len(), 1, "one painted footprint");
    assert_eq!((spans[0].cols, spans[0].rows), (2, 2), "footprint span");
}

#[cfg(unix)]
#[test]
fn spanless_place_waits_for_an_async_local_transmit_and_then_paints() {
    // image.nvim's default transmit medium is a *file* (`t=f`): the read
    // runs on the async worker, so a spanless `a=p` in the same write
    // arrives before the pixels exist. The request must be retained and
    // replayed when the store lands, not dropped forever.
    //
    // Unix-only: the sandboxed local-medium open (`t=f`/`t=t`/`t=s`) is a
    // Unix feature; other platforms refuse the medium outright.
    let dir = std::env::temp_dir().join(format!("bitty-2875-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("inline.png");
    // 8x8 opaque red PNG fixture, 1 cell at the default metrics.
    std::fs::write(&path, red_1x1_png()).expect("write fixture");

    let mut rt = make_runtime();
    // Deterministic single-tick presents (see the sibling spanless test).
    rt.set_min_present_interval(std::time::Duration::ZERO);
    // First present sizes the surface; later ticks then commit one frame
    // per call, which the red-pixel probe needs.
    rt.tick();
    // Park the cursor inside the visible cursor-follow window (see the
    // sibling test): a placement on grid row 0 clips away in the default
    // 80x24 grid / 76x22 frame shape.
    rt.handle_pty_bytes(b"\x1b[4;2H");
    // The cursor composites above images; hide it so the probe sees the
    // image itself.
    rt.handle_pty_bytes(b"\x1b[?25l");
    let name = base64(path.to_string_lossy().as_bytes());
    // Both commands in one write, exactly like the plugin's tty write.
    rt.handle_pty_bytes(
        format!("\x1b_Ga=t,f=100,t=f,C=1,q=2,i=9,m=0;{name}\x1b\\\x1b_Ga=p,i=9,p=9,C=1,q=2\x1b\\")
            .as_bytes(),
    );
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "the placement cannot be sized before the pixels land"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while rt.kitty_placement_count() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = rt.poll_pty();
    }
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "the retained placement replays once the store completes"
    );
    assert!(rt.tick().is_some(), "the replay forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 1, "the replayed blit paints");
    assert!(
        red_bounds(&rt).is_some(),
        "the inline image covers its cells after the async store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spanless_place_uses_the_wire_display_region_as_the_span() {
    // image.nvim's crop path sends `w=`/`h=` (the visible pixel region) and
    // no `c=`/`r=`. Kitty sizes the placement from that region, not from the
    // full stored image, so the cells follow the crop.
    let mut rt = make_runtime();
    // 18x76 px: the full image would span 2x4 cells at the 9x19 metrics.
    let rgba = [0xFFu8, 0x00, 0x00, 0xFF].repeat(18 * 76);
    let encoded = base64(&rgba);
    rt.handle_pty_bytes(format!("\x1b_Ga=t,f=32,s=18,v=76,q=2,i=8,m=0;{encoded}\x1b\\").as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    // The client asks for a single 19px row of it.
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=8,p=8,w=18,h=19,q=2\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 1, "the size derives from w=/h=");
    let cursor = rt.state().cursor().position;
    assert_eq!(
        (cursor.col, cursor.row),
        (2, 1),
        "2 columns x 1 row, from the display region"
    );
    let spans = rt.kitty_image_cell_spans_for_primary();
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].cols, spans[0].rows), (2, 1), "crop-sized span");
}

#[test]
fn spanless_place_without_pixels_paints_nothing_and_stays_bounded() {
    // A stream that places without ever transmitting must not paint, must
    // not panic, and must not grow the retained queue without bound.
    let mut rt = make_runtime();
    for id in 1..=40u32 {
        rt.handle_pty_bytes(format!("\x1b_Ga=p,i={id},p={id},C=1,q=2\x1b\\").as_bytes());
    }
    assert_eq!(rt.kitty_placement_count(), 0, "nothing to paint");
    assert_eq!(rt.kitty_image_count(), 0, "nothing stored");
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 0);
}

/// One full tile row of `U+10EEEE` placeholder cells naming image 42 at
/// `tile_row`, columns 0 and 1.
///
/// `SGR 38;5;42` names the image; the frozen kitty `rowcolumn-diacritics`
/// table maps `U+0305` to 0 and `U+030D` to 1 on both axes, so the first
/// diacritic is the tile row and the second is the tile column.
fn print_placeholder_tile_row(rt: &mut Runtime, tile_row: &str) {
    let sequence =
        format!("\x1b[38;5;42m\u{10EEEE}{tile_row}\u{0305}\u{10EEEE}{tile_row}\u{030D}\x1b[39m");
    rt.handle_pty_bytes(sequence.as_bytes());
}

#[test]
fn unicode_virtual_zero_dimension_span_keeps_the_stored_aspect() {
    // CTX-1788 (PR #3109 round-2): the `t=f` unicode-virtual shape. Pixels
    // arrive through a separate `a=t` transmit and the placement that turns
    // the image into a `U+10EEEE` run is bodiless (`a=p,U=1` with no
    // payload), so the prototype registers with zero pixel dimensions
    // (`register_virtual_for_origin(.., 0, 0, base)`). `present` must then
    // take the pixel size from the stored image; without it `fit_aspect_rect`
    // gets `0x0` and fails open to the whole cell span, so the image is
    // stretched to the span instead of letterboxed inside it.
    //
    // The span has to be fully explicit: terminal truth refuses a place with
    // an empty axis (`cols == 0 || rows == 0`), and the runtime refuses a
    // fully spanless prototype, so `c=2,r=2` is the only admitted shape —
    // and with `c=`/`r=` present the *only* thing the pixel size can still
    // decide is the aspect fit.
    //
    // The nearest existing coverage uses the synchronous `a=T` form with
    // explicit `c=`/`r=`, which always carries decoded dimensions, so the
    // fallback is dead there.
    let mut rt = make_runtime();
    // Deterministic single-tick presents (see the sibling spanless tests).
    rt.set_min_present_interval(std::time::Duration::ZERO);
    // First present sizes the surface.
    rt.tick();
    // 18x19 px at the headless 9x19 cell metrics: two columns wide, one row
    // tall. Placed in a 2x2-cell span it must letterbox, not stretch.
    let rgba = [0xFFu8, 0x00, 0x00, 0xFF].repeat(18 * 19);
    let encoded = base64(&rgba);
    rt.handle_pty_bytes(
        format!("\x1b_Ga=t,f=32,s=18,v=19,q=2,i=42,m=0;{encoded}\x1b\\").as_bytes(),
    );
    assert_eq!(rt.kitty_image_count(), 1, "transmit stores the pixels");
    // Bodiless virtual placement: no payload, so no pixel dimensions.
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=2,r=2,q=2\x1b\\");
    assert_eq!(
        rt.kitty_virtual_count(),
        1,
        "prototype registered with zero pixel dimensions"
    );
    // Print both tile rows inside the visible cursor-follow window, and hide
    // the cursor so it cannot mask a cell of the probe.
    rt.handle_pty_bytes(b"\x1b[?25l");
    rt.handle_pty_bytes(b"\x1b[5;3H");
    print_placeholder_tile_row(&mut rt, "\u{0305}");
    rt.handle_pty_bytes(b"\x1b[6;3H");
    print_placeholder_tile_row(&mut rt, "\u{030D}");
    assert_eq!(
        rt.state().kitty_unicode_runs_on_row(4).len(),
        1,
        "tile row 0 decoded on grid row 4"
    );
    assert_eq!(
        rt.state().kitty_unicode_runs_on_row(5).len(),
        1,
        "tile row 1 decoded on grid row 5"
    );

    assert!(rt.tick().is_some(), "the placeholder run forces a present");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "both tile rows share one span, so they composite as one blit"
    );
    let (x0, y0, x1, y1, count) =
        red_bounds(&rt).expect("the placeholder run must paint the stored image");
    assert_eq!(x1 - x0 + 1, 18, "the image fills the span's width");
    assert_eq!(
        y1 - y0 + 1,
        19,
        "the 18x19 store letterboxes into the 2-row span; 38 means the pixel \
         size was dropped and the span stretched it"
    );
    assert!(
        count >= 18 * 19,
        "the whole fitted image is opaque: {count}"
    );
}

#[test]
fn long_running_virtual_and_multi_pane_chafa_never_lock_out_3159() {
    // Issue #3159:
    // 1. Repeated `a=T,U=1,i=42` frames (e.g. hermes inline-image or animated
    //    mascot updates) must replace the prior `(origin, 42)` image and
    //    prototype in place instead of accumulating 32 prototypes and 64
    //    stored images.
    // 2. When an origin holds 32 distinct `U=1` virtual prototypes, a direct
    //    `chafa -f kitty` (`a=T`) blit placement on that origin must reclaim
    //    one slot and paint instead of failing with `QuotaExceeded`.
    // 3. When multiple panes collectively exceed the 64-image global cap
    //    without any single pane reaching 64 images, `a=q` support probes
    //    must still reply `OK` and subsequent `chafa -f kitty` transmissions
    //    must reclaim and display.
    let mut rt = make_runtime();
    rt.set_min_present_interval(std::time::Duration::ZERO);
    rt.tick();

    let encoded = base64(&red_2x2());
    for _ in 0..80 {
        let seq = format!("\x1b_Ga=T,U=1,f=32,s=2,v=2,i=42,c=2,r=2,q=2,m=0;{encoded}\x1b\\");
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert_eq!(
        rt.kitty_image_count(),
        1,
        "repeated a=T,U=1 with the same wire_image must replace the previous image"
    );
    assert_eq!(
        rt.kitty_virtual_count(),
        1,
        "repeated a=T,U=1 with the same wire_image must replace the previous prototype"
    );

    // Fill all 32 virtual prototype slots on the primary origin with distinct
    // image ids, then emit a direct `chafa -f kitty` (`a=T`) blit placement.
    for id in 100..132u32 {
        let seq = format!("\x1b_Ga=T,U=1,f=32,s=2,v=2,i={id},c=2,r=2,q=2,m=0;{encoded}\x1b\\");
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert_eq!(rt.kitty_virtual_count(), 32);
    let chafa_blit = format!("\x1b_Ga=T,f=32,s=2,v=2,c=2,r=2,q=2,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(chafa_blit.as_bytes());
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "direct chafa -f kitty blit must reclaim one cross-kind slot and display"
    );

    // Multi-pane global store saturation: split a second pane so neither
    // origin individually reaches 64 images when the global store hits 64.
    #[cfg(unix)]
    {
        use bitty_runtime::{LayoutNode, SplitAxis, View};
        let primary = ViewId::new(1);
        let pane = ViewId::new(2);
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(primary, 80, 24)),
            LayoutNode::leaf(View::new(pane, 80, 24)),
        ));
        rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 60"], 40, 24)
            .expect("spawn pane shell");
        for _ in 0..40 {
            rt.handle_pane_bytes(pane, chafa_blit.as_bytes());
        }
        assert_eq!(
            rt.kitty_image_count(),
            bitty_rich::KITTY_PLACE_MAX_IMAGES,
            "global store stays bounded at 64 images across multiple panes"
        );
    }

    // Support probe (`a=q`) followed by `a=T` display on the primary pane must
    // reply `OK` and render even when the global 64-image store is full across
    // multiple panes.
    let _ = rt.take_replies();
    let probe = format!("\x1b_Gi=31,f=32,s=2,v=2,a=q,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(probe.as_bytes());
    let replies: Vec<Vec<u8>> = rt.take_replies().into_iter().map(|b| b.to_vec()).collect();
    assert_eq!(
        replies,
        vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()],
        "support probe must reply OK in an active multi-pane display session"
    );
    rt.handle_pty_bytes(chafa_blit.as_bytes());
    assert!(
        rt.tick().is_some(),
        "subsequent chafa -f kitty display must present after multi-pane reclamation"
    );
    assert!(
        rt.kitty_last_frame_images() >= 1,
        "subsequent chafa -f kitty display must composite into the frame"
    );

    // Main -> alt retransmit -> main: stashed main-screen images must survive
    // same-wire-id retransmits on the alternate screen (both combined virtual
    // `a=T,U=1,i=77` and direct `a=T,i=78`).
    let mut rt_alt = make_runtime();
    rt_alt.set_min_present_interval(std::time::Duration::ZERO);
    rt_alt.tick();
    let main_virt = format!("\x1b_Ga=T,U=1,f=32,s=2,v=2,i=77,c=2,r=2,q=2,m=0;{encoded}\x1b\\");
    let main_blit = format!("\x1b_Ga=T,f=32,s=2,v=2,i=78,c=2,r=2,q=2,m=0;{encoded}\x1b\\");
    rt_alt.handle_pty_bytes(main_virt.as_bytes());
    rt_alt.handle_pty_bytes(main_blit.as_bytes());
    assert_eq!(rt_alt.kitty_image_count(), 2);
    assert_eq!(rt_alt.kitty_virtual_count(), 1);
    assert_eq!(rt_alt.kitty_placement_count(), 1);

    // Enter alternate screen and retransmit both wire ids.
    rt_alt.handle_pty_bytes(b"\x1b[?1049h");
    rt_alt.handle_pty_bytes(main_virt.as_bytes());
    rt_alt.handle_pty_bytes(main_blit.as_bytes());
    assert!(
        rt_alt.kitty_image_count() >= 4,
        "stashed main-screen images must be preserved alongside alt-screen images"
    );

    // Leave alternate screen: restored main-screen placement and prototype
    // must still have their stored images intact and paint.
    rt_alt.handle_pty_bytes(b"\x1b[?1049l");
    assert_eq!(rt_alt.kitty_virtual_count(), 1);
    assert_eq!(rt_alt.kitty_placement_count(), 1);
    assert!(rt_alt.tick().is_some());
    assert_eq!(
        rt_alt.kitty_last_frame_images(),
        1,
        "restored main-screen placement must still resolve its preserved image"
    );
}
