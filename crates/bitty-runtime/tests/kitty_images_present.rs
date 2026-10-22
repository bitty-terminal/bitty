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
fn place_action_for_stored_image_paints_no_blit_today() {
    // Issue #1802 acceptance pin, documents the R2 shape: `a=p` is
    // state-owned Terminal Truth (grid records / virtual prototypes),
    // not yet wired to the pixel layer, so placing a stored image
    // paints no blit today.
    let mut rt = make_runtime();
    let encoded = "/wAA//8AAP//AAD//wAA/w==";
    let transmit = format!("\x1b_Gf=32,s=2,v=2,a=t,i=7,m=0;{encoded}\x1b\\");
    rt.handle_pty_bytes(transmit.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Ga=p,i=7\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1, "place action stores nothing new");
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "place action places nothing on the pixel layer today"
    );
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "no blit may paint for a state-only place"
    );
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
    assert!(rt.kitty_last_frame_images() <= 1);
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
