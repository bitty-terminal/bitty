//! Kitty transmit/display wired suite (issue #1802).
//!
//! The Core-owned decoder ([`bitty_rich::kitty_decode`]) and raster step
//! ([`bitty_rich::rasterize_kitty_clipped`]) are wired: admissible
//! payloads decode, store, place, advance the cursor, and paint topmost
//! blits. Hostile declarations are still refused before any allocation
//! (P0-AC-003), unknown formats never guess, and rejected transmissions
//! store nothing.

use bitty_rich::KittyPrecheckError;
use bitty_runtime::{KittyDisplayOutcome, KittyImageError, Runtime};

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
    // The viewport-clamped placement still paints one blit.
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
