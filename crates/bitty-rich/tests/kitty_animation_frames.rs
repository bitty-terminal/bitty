//! Kitty animation frame store suite (S1 of #1849, Task CTX-1111).
//!
//! Per-image frame bitmaps in [`bitty_rich::KittyImageLayer`]: every frame
//! reuses the shipped per-frame caps (8192 px/side, 4096x4096 area,
//! 64 MiB RGBA), the per-image total stays within 64 MiB, the global store
//! within 256 MiB, and the frame count within 64 (`IMAGE_MAX_FRAMES`
//! parity — reconciled against terminal truth's 256 gap entries in the
//! layer docs). `a=c` compose blends (`C=0`) or replaces (`C=1`) a bounded
//! rect between independent source (`X=`, `Y=`) and destination (`x=`,
//! `y=`) positions with checked arithmetic; out-of-bounds rects
//! refuse with nothing composed.

use bitty_rich::{
    KITTY_ANIM_MAX_FRAMES, KITTY_COMPOSE_BLEND, KITTY_COMPOSE_REPLACE, KittyImageLayer,
    KittyPlacementError,
};

/// 2x2 opaque red frame (16 bytes).
fn red_2x2() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
}

/// 2x2 opaque blue frame (16 bytes).
fn blue_2x2() -> Vec<u8> {
    [0x00, 0x00, 0xFF, 0xFF].repeat(4)
}

/// 2x2 striped source: row 0 red, row 1 blue (16 bytes).
fn striped_2x2() -> Vec<u8> {
    let mut bytes = [0xFF, 0x00, 0x00, 0xFF].repeat(2);
    bytes.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF].repeat(2));
    bytes
}

/// Stores a 2x2 root image tagged with wire id 7 on the primary origin.
fn stored_root(layer: &mut KittyImageLayer) -> bitty_rich::KittyImageId {
    layer
        .store_for_origin_with_wire(2, 2, red_2x2(), 16, None, 7)
        .expect("root must store")
}

#[test]
fn frame_count_cap_is_64_with_nothing_stored_on_refusal() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    assert_eq!(layer.frame_count(id), Some(1));
    for _ in 1..KITTY_ANIM_MAX_FRAMES {
        layer
            .append_frame(id, 2, 2, blue_2x2())
            .expect("frame within cap must store");
    }
    assert_eq!(layer.frame_count(id), Some(KITTY_ANIM_MAX_FRAMES as u32));
    let bytes_before = layer.total_bytes();
    let err = layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect_err("65th frame must fail closed");
    assert!(matches!(err, KittyPlacementError::TooManyFrames { .. }));
    assert_eq!(layer.frame_count(id), Some(KITTY_ANIM_MAX_FRAMES as u32));
    assert_eq!(layer.total_bytes(), bytes_before);
}

#[test]
fn per_frame_caps_apply_to_every_frame() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    // Side cap fires on a 4-byte lie, before any allocation.
    assert_eq!(
        layer.append_frame(id, 100_000, 100_000, vec![0; 4]),
        Err(KittyPlacementError::DimensionsTooLarge {
            width: 100_000,
            height: 100_000,
            cap: bitty_rich::KITTY_DECODE_MAX_DIMENSION,
        })
    );
    // Length must match exactly.
    assert_eq!(
        layer.append_frame(id, 2, 2, vec![0; 7]),
        Err(KittyPlacementError::LengthMismatch {
            expected: 16,
            actual: 7
        })
    );
    assert_eq!(layer.frame_count(id), Some(1));
}

#[test]
fn frame_dims_must_match_root() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    // Same byte length but different shape: refused.
    let err = layer
        .append_frame(id, 4, 1, red_2x2())
        .expect_err("reshaped frame must fail closed");
    assert!(matches!(
        err,
        KittyPlacementError::FrameDimensionsMismatch { .. }
    ));
    assert_eq!(layer.frame_count(id), Some(1));
}

#[test]
fn per_image_total_bounded_at_64mib() {
    let mut layer = KittyImageLayer::new();
    // 1024x1024 RGBA = 4 MiB per frame; root + 15 frames = 64 MiB exactly.
    let big = vec![0x7Fu8; 1024 * 1024 * 4];
    let id = layer
        .store_for_origin_with_wire(1024, 1024, big.clone(), big.len(), None, 9)
        .expect("root must store");
    for _ in 1..16 {
        layer
            .append_frame(id, 1024, 1024, big.clone())
            .expect("frame within per-image total must store");
    }
    assert_eq!(layer.frame_count(id), Some(16));
    let bytes_before = layer.total_bytes();
    let err = layer
        .append_frame(id, 1024, 1024, big.clone())
        .expect_err("frame past the 64 MiB per-image total must fail closed");
    assert!(matches!(err, KittyPlacementError::AnimationTooLarge { .. }));
    assert_eq!(layer.total_bytes(), bytes_before);
}

#[test]
fn global_pressure_refuses_frame_without_evicting() {
    let mut layer = KittyImageLayer::new();
    // 2048x2048 RGBA = 16 MiB per image: the primary root plus 15 more on
    // distinct origins fills the 256 MiB global store exactly (each origin
    // holds one image, inside every per-origin quota).
    let big = vec![0x7Fu8; 2048 * 2048 * 4];
    let id = layer
        .store_for_origin_with_wire(2048, 2048, big.clone(), big.len(), None, 7)
        .expect("root must store");
    for origin in 11u64..26 {
        layer
            .store_for_origin_with_wire(2048, 2048, big.clone(), big.len(), Some(origin), 1)
            .expect("filler image must store");
    }
    assert_eq!(layer.total_bytes(), 256 * 1024 * 1024);
    // The frame itself is inside every per-image bound (2 frames, 32 MiB):
    // only the full global store refuses it.
    let err = layer
        .append_frame(id, 2048, 2048, big.clone())
        .expect_err("frame against a full global store must fail closed");
    assert_eq!(err, KittyPlacementError::QuotaExceeded);
    // Nothing evicted, nothing stored: the full store is intact.
    assert_eq!(layer.total_bytes(), 256 * 1024 * 1024);
    assert_eq!(layer.frame_count(id), Some(1));
}

#[test]
fn per_origin_quota_refuses_frame_without_starving_others() {
    let mut layer = KittyImageLayer::new();
    // 1024x1024 RGBA = 4 MiB per frame. Two images on the primary origin,
    // each root + 7 frames = 32 MiB, fill the 64 MiB per-origin quota
    // exactly while staying inside every per-image bound (8 frames,
    // 32 MiB) and the 256 MiB global store.
    let big = vec![0x7Fu8; 1024 * 1024 * 4];
    let first = layer
        .store_for_origin_with_wire(1024, 1024, big.clone(), big.len(), None, 7)
        .expect("first root must store");
    let second = layer
        .store_for_origin_with_wire(1024, 1024, big.clone(), big.len(), None, 8)
        .expect("second root must store");
    for _ in 1..8 {
        layer
            .append_frame(first, 1024, 1024, big.clone())
            .expect("frame within per-origin quota must store");
        layer
            .append_frame(second, 1024, 1024, big.clone())
            .expect("frame within per-origin quota must store");
    }
    assert_eq!(layer.bytes_for_origin(None), 64 * 1024 * 1024);
    // One more frame fits every per-image bound (9 frames, 36 MiB) but
    // would push the origin past 64 MiB: refused, never evicting.
    let bytes_before = layer.total_bytes();
    let err = layer
        .append_frame(first, 1024, 1024, big.clone())
        .expect_err("frame past the per-origin quota must fail closed");
    assert_eq!(err, KittyPlacementError::QuotaExceeded);
    assert_eq!(layer.frame_count(first), Some(8));
    assert_eq!(layer.total_bytes(), bytes_before);
    // S5 isolation holds: a second pane still admits images (the global
    // store is nowhere near full).
    layer
        .store_for_origin_with_wire(2, 2, red_2x2(), 16, Some(5), 1)
        .expect("other-origin store must succeed under per-origin pressure");
}

#[test]
fn root_resolution_is_origin_scoped_newest_wins() {
    let mut layer = KittyImageLayer::new();
    let first = stored_root(&mut layer);
    // Same wire id on another origin resolves independently.
    let other = layer
        .store_for_origin_with_wire(2, 2, blue_2x2(), 16, Some(5), 7)
        .expect("other-origin root must store");
    assert_eq!(layer.find_root_for_origin(None, 7), Some(first));
    assert_eq!(layer.find_root_for_origin(Some(5), 7), Some(other));
    // Anonymous wire ids never resolve.
    assert_eq!(layer.find_root_for_origin(None, 0), None);
    // A re-transmit of the same wire id supersedes (newest wins).
    let second = layer
        .store_for_origin_with_wire(2, 2, blue_2x2(), 16, None, 7)
        .expect("retransmit must store");
    assert_eq!(layer.find_root_for_origin(None, 7), Some(second));
}

#[test]
fn frame_bytes_select_is_1_based_and_fail_closed() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    let frame_no = layer.append_frame(id, 2, 2, blue_2x2()).expect("append");
    assert_eq!(frame_no, 2);
    let (w, h, root) = layer.frame_bytes(id, 1).expect("frame 1 is root");
    assert_eq!((w, h), (2, 2));
    assert_eq!(root, red_2x2().as_slice());
    let (_, _, second) = layer.frame_bytes(id, 2).expect("frame 2 stored");
    assert_eq!(second, blue_2x2().as_slice());
    // Frame 0, past-the-end, and unknown images select nothing.
    assert!(layer.frame_bytes(id, 0).is_none());
    assert!(layer.frame_bytes(id, 3).is_none());
}

#[test]
fn compose_replace_copies_bytes_exactly() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    // Replace the top-left pixel of frame 1 with frame 2's.
    layer
        .compose_frame(id, 1, 2, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE)
        .expect("in-bounds replace must compose");
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    let mut expected = red_2x2();
    expected[0..4].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    assert_eq!(root, expected.as_slice());
    // Frame 2 (source) is unchanged.
    let (_, _, second) = layer.frame_bytes(id, 2).expect("frame 2");
    assert_eq!(second, blue_2x2().as_slice());
}

#[test]
fn compose_blend_is_src_over_with_rounded_integers() {
    let mut layer = KittyImageLayer::new();
    // Opaque blue root, half-transparent red source frame.
    let id = layer
        .store_for_origin_with_wire(1, 1, vec![0x00, 0x00, 0xFF, 0xFF], 4, None, 3)
        .expect("root must store");
    layer
        .append_frame(id, 1, 1, vec![0xFF, 0x00, 0x00, 0x80])
        .expect("frame 2 must store");
    layer
        .compose_frame(id, 1, 2, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_BLEND)
        .expect("blend must compose");
    let (_, _, out) = layer.frame_bytes(id, 1).expect("frame 1");
    // w1 = 128, w2 = (255*127+127)/255 = 127, oa = 255;
    // r = (255*128 + 0 + 127)/255 = 128, b = (0 + 255*127 + 127)/255 = 127.
    assert_eq!(out, &[128, 0, 127, 255]);
}

#[test]
fn compose_blend_endpoints_match_replace_and_identity() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    // Opaque source blends exactly like replace.
    layer
        .compose_frame(id, 1, 2, 0, 0, 0, 0, 2, 2, KITTY_COMPOSE_BLEND)
        .expect("opaque blend must compose");
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, blue_2x2().as_slice());
    // Fully transparent source leaves the destination untouched.
    let id2 = layer
        .store_for_origin_with_wire(1, 1, vec![9, 8, 7, 255], 4, None, 4)
        .expect("root must store");
    layer
        .append_frame(id2, 1, 1, vec![0, 0, 0, 0])
        .expect("frame 2 must store");
    layer
        .compose_frame(id2, 1, 2, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_BLEND)
        .expect("transparent blend must compose");
    let (_, _, kept) = layer.frame_bytes(id2, 1).expect("frame 1");
    assert_eq!(kept, &[9, 8, 7, 255]);
}

#[test]
fn compose_refusals_compose_nothing() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    let snapshot = layer.frame_bytes(id, 1).expect("frame 1").2.to_vec();
    // Unknown frames.
    assert!(matches!(
        layer.compose_frame(id, 9, 1, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    assert!(matches!(
        layer.compose_frame(id, 1, 9, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    assert!(matches!(
        layer.compose_frame(id, 0, 1, 0, 0, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    // Unknown mode (only C=0 blend / C=1 replace exist).
    assert_eq!(
        layer.compose_frame(id, 1, 2, 0, 0, 0, 0, 1, 1, 4),
        Err(KittyPlacementError::UnknownComposeMode(4))
    );
    // Out-of-bounds rects: source overflow, destination overflow, and a
    // source offset at the edge (each side is checked independently).
    assert!(matches!(
        layer.compose_frame(id, 1, 2, 1, 1, 0, 0, 2, 2, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    assert!(matches!(
        layer.compose_frame(id, 1, 2, 0, 0, 1, 1, 2, 2, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    assert!(matches!(
        layer.compose_frame(id, 1, 2, 2, 0, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    // Arithmetic overflow in the rect end cannot wrap into bounds.
    assert!(matches!(
        layer.compose_frame(id, 1, 2, u32::MAX, 0, 0, 0, 2, 2, KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    // Every refusal left the destination byte-identical.
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, snapshot.as_slice());
}

#[test]
fn compose_copies_between_independent_source_and_dest_offsets() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    // Copy frame 2's bottom-right pixel (blue) onto frame 1's top-left.
    layer
        .compose_frame(id, 1, 2, 1, 1, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE)
        .expect("offset compose must compose");
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    let mut expected = red_2x2();
    expected[0..4].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    assert_eq!(root, expected.as_slice());
    // The untouched corner keeps the root color: source and destination
    // rects really are independent.
    assert_eq!(&root[12..16], &[0xFF, 0x00, 0x00, 0xFF]);
    // Frame 2 (source) is unchanged.
    let (_, _, second) = layer.frame_bytes(id, 2).expect("frame 2");
    assert_eq!(second, blue_2x2().as_slice());
}

#[test]
fn compose_src_y_selects_source_row() {
    // Striped source (row 0 red, row 1 blue): a 1x1 REPLACE from
    // (0, src_y) onto (0, 0) must pick the addressed source row, so the
    // two results differ. Uniform fixtures pass vacuously here, which is
    // why this pin uses distinct rows.
    fn composed_with_src_y(src_y: u32) -> Vec<u8> {
        let mut layer = KittyImageLayer::new();
        let id = stored_root(&mut layer);
        layer
            .append_frame(id, 2, 2, striped_2x2())
            .expect("striped frame 2 must store");
        layer
            .compose_frame(id, 1, 2, 0, src_y, 0, 0, 1, 1, KITTY_COMPOSE_REPLACE)
            .expect("in-bounds replace must compose");
        layer.frame_bytes(id, 1).expect("frame 1").2.to_vec()
    }

    let with_y0 = composed_with_src_y(0);
    let with_y1 = composed_with_src_y(1);
    assert_eq!(
        &with_y0[0..4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "src_y=0 must select the red row"
    );
    assert_eq!(
        &with_y1[0..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "src_y=1 must select the blue row"
    );
    assert_ne!(
        with_y0, with_y1,
        "Y source offset must change the composed bytes"
    );
}

#[test]
fn compose_zero_size_defaults_to_full_extent() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    // Wire `w=`/`h=` absent (0) means the full frame, per the protocol.
    layer
        .compose_frame(id, 1, 2, 0, 0, 0, 0, 0, 0, KITTY_COMPOSE_REPLACE)
        .expect("default-size compose must compose");
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, blue_2x2().as_slice());
}

#[test]
fn partial_frame_composites_onto_base_before_store() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    // 1x1 opaque blue at (1, 0), replaced onto the red root as base.
    let frame_no = layer
        .append_frame_composited(
            id,
            1,
            0,
            1,
            1,
            vec![0x00, 0x00, 0xFF, 0xFF],
            1,
            0,
            KITTY_COMPOSE_REPLACE,
        )
        .expect("partial onto base must store");
    assert_eq!(frame_no, 2);
    let (_, _, frame) = layer.frame_bytes(id, 2).expect("frame 2");
    let mut expected = red_2x2();
    expected[4..8].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    assert_eq!(frame, expected.as_slice());
    // The root (base) is unchanged.
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, red_2x2().as_slice());
}

#[test]
fn partial_frame_without_base_fills_background_color() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    // No base frame: the canvas is the `Y=` color (opaque green), with one
    // blue pixel replaced at (0, 0).
    let frame_no = layer
        .append_frame_composited(
            id,
            0,
            0,
            1,
            1,
            vec![0x00, 0x00, 0xFF, 0xFF],
            0,
            0x00FF_00FF,
            KITTY_COMPOSE_REPLACE,
        )
        .expect("partial onto color must store");
    assert_eq!(frame_no, 2);
    let (_, _, frame) = layer.frame_bytes(id, 2).expect("frame 2");
    let mut expected = [0x00, 0xFF, 0x00, 0xFF].repeat(4);
    expected[0..4].copy_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    assert_eq!(frame, expected.as_slice());
}

#[test]
fn partial_frame_refusals_store_nothing() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    // Update rect past the root edge.
    assert!(matches!(
        layer.append_frame_composited(
            id,
            2,
            0,
            1,
            1,
            vec![0x00, 0x00, 0xFF, 0xFF],
            1,
            0,
            KITTY_COMPOSE_REPLACE,
        ),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    // Missing base frame.
    assert!(matches!(
        layer.append_frame_composited(
            id,
            0,
            0,
            1,
            1,
            vec![0x00, 0x00, 0xFF, 0xFF],
            9,
            0,
            KITTY_COMPOSE_REPLACE,
        ),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    // Unknown mode.
    assert_eq!(
        layer.append_frame_composited(id, 0, 0, 1, 1, vec![0x00, 0x00, 0xFF, 0xFF], 1, 0, 4,),
        Err(KittyPlacementError::UnknownComposeMode(4))
    );
    assert_eq!(layer.frame_count(id), Some(1));
}

#[test]
fn frame_eviction_cascades_with_image_accounting() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    assert_eq!(layer.total_bytes(), 32);
    assert!(layer.remove(id));
    assert_eq!(layer.total_bytes(), 0);
    assert_eq!(layer.frame_count(id), None);
}

#[test]
fn edit_replace_changes_frame_pixels_in_place() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    layer
        .append_frame(id, 2, 2, blue_2x2())
        .expect("frame 2 must store");
    let bytes_before = layer.total_bytes();
    // Replace frame 2's top-left pixel with opaque green: the edited frame
    // changes, the root is untouched, and no new frame appears.
    layer
        .edit_frame(
            id,
            2,
            0,
            0,
            1,
            1,
            &[0x00, 0xFF, 0x00, 0xFF],
            KITTY_COMPOSE_REPLACE,
        )
        .expect("in-bounds replace must edit");
    let (_, _, second) = layer.frame_bytes(id, 2).expect("frame 2");
    let mut expected = blue_2x2();
    expected[0..4].copy_from_slice(&[0x00, 0xFF, 0x00, 0xFF]);
    assert_eq!(second, expected.as_slice());
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, red_2x2().as_slice());
    assert_eq!(layer.frame_count(id), Some(2));
    assert_eq!(layer.total_bytes(), bytes_before);
}

#[test]
fn edit_blend_is_src_over_on_the_frame_itself() {
    let mut layer = KittyImageLayer::new();
    // Opaque blue root, half-transparent red edit at (0, 0).
    let id = layer
        .store_for_origin_with_wire(1, 1, vec![0x00, 0x00, 0xFF, 0xFF], 4, None, 3)
        .expect("root must store");
    layer
        .edit_frame(
            id,
            1,
            0,
            0,
            1,
            1,
            &[0xFF, 0x00, 0x00, 0x80],
            KITTY_COMPOSE_BLEND,
        )
        .expect("blend must edit");
    let (_, _, out) = layer.frame_bytes(id, 1).expect("frame 1");
    // Same rounded src-over math as `a=c` blend (see
    // `compose_blend_is_src_over_with_rounded_integers`).
    assert_eq!(out, &[128, 0, 127, 255]);
    assert_eq!(layer.frame_count(id), Some(1));
}

#[test]
fn edit_refusals_edit_nothing() {
    let mut layer = KittyImageLayer::new();
    let id = stored_root(&mut layer);
    let snapshot = layer.frame_bytes(id, 1).expect("frame 1").2.to_vec();
    let bytes_before = layer.total_bytes();
    // Missing frames (past the end and zero).
    assert!(matches!(
        layer.edit_frame(
            id,
            9,
            0,
            0,
            1,
            1,
            &[0x00, 0x00, 0xFF, 0xFF],
            KITTY_COMPOSE_REPLACE
        ),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    assert!(matches!(
        layer.edit_frame(
            id,
            0,
            0,
            0,
            1,
            1,
            &[0x00, 0x00, 0xFF, 0xFF],
            KITTY_COMPOSE_REPLACE
        ),
        Err(KittyPlacementError::NoSuchFrame { .. })
    ));
    // Unknown mode (only X=0 blend / X=1 replace exist).
    assert_eq!(
        layer.edit_frame(id, 1, 0, 0, 1, 1, &[0x00, 0x00, 0xFF, 0xFF], 4),
        Err(KittyPlacementError::UnknownComposeMode(4))
    );
    // Out-of-bounds rects: offset past the edge and end overflow past
    // `u32::MAX` (checked `u64` arithmetic cannot wrap into bounds).
    assert!(matches!(
        layer.edit_frame(
            id,
            1,
            2,
            0,
            1,
            1,
            &[0x00, 0x00, 0xFF, 0xFF],
            KITTY_COMPOSE_REPLACE
        ),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    assert!(matches!(
        layer.edit_frame(id, 1, u32::MAX, 0, 2, 2, &[0xFF; 16], KITTY_COMPOSE_REPLACE),
        Err(KittyPlacementError::ComposeOutOfBounds { .. })
    ));
    // Reshaped payload (length does not match the claimed extent).
    assert!(matches!(
        layer.edit_frame(
            id,
            1,
            0,
            0,
            1,
            1,
            &[0x00, 0x00, 0xFF],
            KITTY_COMPOSE_REPLACE
        ),
        Err(KittyPlacementError::LengthMismatch { .. })
    ));
    // Every refusal left the frame byte-identical with stable accounting.
    let (_, _, root) = layer.frame_bytes(id, 1).expect("frame 1");
    assert_eq!(root, snapshot.as_slice());
    assert_eq!(layer.frame_count(id), Some(1));
    assert_eq!(layer.total_bytes(), bytes_before);
}
