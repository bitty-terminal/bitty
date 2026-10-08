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
fn rejected_transmission_leaves_grid_idle() {
    let mut rt = make_runtime();
    let _ = rt.kitty_display_image(7, Some(2), Some(2), None, 1, 1, 0, &red_2x2(), 0);
    // Nothing stored, no redraw forced: after the initial grid present the
    // runtime idles exactly as if no transmission had arrived.
    assert!(rt.tick().is_some(), "first tick still presents the grid");
    assert_eq!(rt.tick(), None, "rejected image must not force a present");
}
