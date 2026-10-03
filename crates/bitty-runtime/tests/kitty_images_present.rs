//! Kitty transmit seam fail-closed suite (CTX-0248, W-141).
//!
//! W-141 extraction moved the bounded decoder to the `bitty-graphics`
//! extension crate and the Core-to-extension call shape is not wired yet,
//! so the runtime transmit entry points run only the Core-retained
//! declared-size pre-check and then fail closed. These tests pin that seam:
//! hostile declarations are refused before any allocation (P0-AC-003),
//! unknown formats never guess, admissible payloads fail closed with
//! `DecoderUnavailable` (storing and placing nothing), and rejected
//! transmissions leave the grid idle. Pixel-painting parity moved with the
//! decoder and returns with the wiring task.

use bitty_rich::KittyPrecheckError;
use bitty_runtime::{KittyImageError, Runtime};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`): admissible, but Core holds no
/// codec, so it must fail closed until wiring.
fn red_2x2() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
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
fn admissible_payload_fails_closed_until_wiring() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0)
        .expect_err("admissible payload has no Core codec yet");
    assert_eq!(
        err,
        KittyImageError::Decode(KittyPrecheckError::DecoderUnavailable)
    );
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(rt.kitty_last_frame_images(), 0);
}

#[test]
fn transmit_only_admissible_also_fails_closed() {
    let mut rt = make_runtime();
    let err = rt
        .kitty_display_image(32, Some(2), Some(2), Some('t'), 2, 2, 0, &red_2x2(), 0)
        .expect_err("transmit-only has no Core codec yet");
    assert_eq!(
        err,
        KittyImageError::Decode(KittyPrecheckError::DecoderUnavailable)
    );
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn rejected_transmission_leaves_grid_idle() {
    let mut rt = make_runtime();
    let _ = rt.kitty_display_image(32, Some(2), Some(2), None, 2, 2, 0, &red_2x2(), 0);
    // Nothing stored, no redraw forced: after the initial grid present the
    // runtime idles exactly as if no transmission had arrived.
    assert!(rt.tick().is_some(), "first tick still presents the grid");
    assert_eq!(rt.tick(), None, "rejected image must not force a present");
}
