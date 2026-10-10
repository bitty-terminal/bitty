#![forbid(unsafe_code)]
//! S5 per-origin quotas (#1849 slice 1, CTX-1093): noisy-origin isolation.
//!
//! The decoded-image store used to evict globally (FIFO over every origin),
//! so a noisy pane could evict another pane's stored images (availability
//! loss; dangling placements fail closed at lookup). S5 partitions the
//! shipped global caps per origin: each origin gets its own image, byte,
//! and placement ceilings inside the global bounds, eviction runs FIFO
//! *within* the admitting origin only, and global pressure refuses the new
//! admission instead of evicting a victim (no cross-origin eviction).
//!
//! Headless, bounded, no window, no GPU, no network.

use bitty_rich::{
    CellMetrics, KITTY_PER_ORIGIN_MAX_BYTES, KITTY_PER_ORIGIN_MAX_IMAGES,
    KITTY_PER_ORIGIN_MAX_PLACEMENTS, KITTY_PLACE_MAX_BYTES, KITTY_PLACE_MAX_IMAGES,
    KITTY_PLACE_MAX_ITEMS, KittyImageId, KittyImageLayer, KittyPlacementError,
};

const METRICS: CellMetrics = CellMetrics {
    width: 8,
    height: 16,
};

const VICTIM: Option<u64> = None;
const NOISY: Option<u64> = Some(7);

/// 2x2 opaque red RGBA bitmap (16 bytes).
fn tiny_rgba() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
}

fn store_on(layer: &mut KittyImageLayer, origin: Option<u64>) -> KittyImageId {
    layer
        .store_for_origin(2, 2, tiny_rgba(), 1, origin)
        .expect("tiny store must be admitted")
}

fn place_on(
    layer: &mut KittyImageLayer,
    image: KittyImageId,
    origin: Option<u64>,
) -> bitty_rich::KittyPlacementId {
    layer
        .display_for_origin(image, 0, 0, 1, 1, METRICS, 0, 0, origin)
        .expect("placement of a live image must be admitted")
}

#[test]
fn per_origin_quotas_hold_shipped_values() {
    // Per-origin ceilings live inside the shipped global caps: the image
    // count keeps ledger parity (64) so single-origin count behavior is
    // unchanged, while bytes and placements quarter the global bounds for
    // up to four full origins.
    assert_eq!(KITTY_PER_ORIGIN_MAX_IMAGES, 64);
    assert_eq!(KITTY_PER_ORIGIN_MAX_BYTES, 64 * 1024 * 1024);
    assert_eq!(KITTY_PER_ORIGIN_MAX_PLACEMENTS, 32);
    assert_eq!(KITTY_PLACE_MAX_IMAGES, 64);
    assert_eq!(KITTY_PLACE_MAX_BYTES, 256 * 1024 * 1024);
    assert_eq!(KITTY_PLACE_MAX_ITEMS, 128);
    assert_eq!(KITTY_PER_ORIGIN_MAX_IMAGES, KITTY_PLACE_MAX_IMAGES);
    assert_eq!(KITTY_PER_ORIGIN_MAX_BYTES, KITTY_PLACE_MAX_BYTES / 4);
    assert_eq!(KITTY_PER_ORIGIN_MAX_PLACEMENTS, KITTY_PLACE_MAX_ITEMS / 4);
}

#[test]
fn noisy_origin_is_refused_at_global_bound_victim_survives() {
    // The victim holds 5 images on the primary origin. The noisy origin
    // stores until the global image bound is reached; the next store is
    // refused (no cross-origin eviction) and every victim image survives.
    let mut layer = KittyImageLayer::new();
    let victim_ids: Vec<KittyImageId> = (0..5).map(|_| store_on(&mut layer, VICTIM)).collect();
    let mut noisy_ids = Vec::new();
    loop {
        match layer.store_for_origin(2, 2, tiny_rgba(), 1, NOISY) {
            Ok(id) => noisy_ids.push(id),
            Err(KittyPlacementError::QuotaExceeded) => break,
            Err(other) => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }
    assert_eq!(
        layer.len(),
        KITTY_PLACE_MAX_IMAGES,
        "global image bound must hold"
    );
    assert_eq!(noisy_ids.len(), KITTY_PLACE_MAX_IMAGES - 5);
    for id in &victim_ids {
        assert!(
            layer.get(*id).is_some(),
            "victim image {id:?} must survive noisy pressure"
        );
    }
    // Deterministic refusal: retrying stores nothing and evicts nothing.
    assert_eq!(
        layer.store_for_origin(2, 2, tiny_rgba(), 1, NOISY),
        Err(KittyPlacementError::QuotaExceeded)
    );
    assert_eq!(layer.len(), KITTY_PLACE_MAX_IMAGES);
    for id in &victim_ids {
        assert!(layer.get(*id).is_some());
    }
}

#[test]
fn byte_quota_evicts_fifo_within_noisy_origin_only() {
    // 2048x2048 RGBA is exactly 16 MiB: four fill the 64 MiB per-origin
    // byte quota, so the fifth evicts the noisy origin's own oldest image
    // while the victim's tiny image survives.
    let mut layer = KittyImageLayer::new();
    let victim = store_on(&mut layer, VICTIM);
    let big = vec![0x7F; 2048_usize * 2048 * 4];
    assert_eq!(big.len(), 16 * 1024 * 1024);
    let mut noisy_ids = Vec::new();
    for _ in 0..5 {
        noisy_ids.push(
            layer
                .store_for_origin(2048, 2048, big.clone(), 1, NOISY)
                .expect("within-quota big store must be admitted"),
        );
    }
    assert!(
        layer.get(noisy_ids[0]).is_none(),
        "noisy oldest must be evicted first (FIFO within origin)"
    );
    for id in &noisy_ids[1..] {
        assert!(layer.get(*id).is_some());
    }
    assert!(
        layer.get(victim).is_some(),
        "victim image must survive noisy byte pressure"
    );
    assert_eq!(layer.bytes_for_origin(NOISY), KITTY_PER_ORIGIN_MAX_BYTES);
    assert_eq!(layer.image_count_for_origin(NOISY), 4);
}

#[test]
fn single_origin_image_fifo_is_unchanged() {
    // One origin storing past the count cap keeps FIFO behavior: the
    // oldest go first, 64 stay.
    let mut layer = KittyImageLayer::new();
    let mut ids = Vec::new();
    for _ in 0..KITTY_PLACE_MAX_IMAGES + 3 {
        ids.push(store_on(&mut layer, VICTIM));
    }
    assert_eq!(layer.len(), KITTY_PLACE_MAX_IMAGES);
    for id in ids.iter().take(3) {
        assert!(layer.get(*id).is_none(), "oldest must be evicted first");
    }
    for id in ids.iter().skip(3) {
        assert!(layer.get(*id).is_some());
    }
}

#[test]
fn placement_quota_is_per_origin_fifo() {
    // Each origin gets 32 placements: the noisy origin's 33rd placement
    // evicts its own oldest while the victim's 32 placements survive.
    let mut layer = KittyImageLayer::new();
    let image = store_on(&mut layer, VICTIM);
    let mut victim_pids = Vec::new();
    for _ in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS {
        victim_pids.push(place_on(&mut layer, image, VICTIM));
    }
    let mut noisy_pids = Vec::new();
    for _ in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS + 1 {
        noisy_pids.push(place_on(&mut layer, image, NOISY));
    }
    assert_eq!(
        layer.placement_count_for_origin(VICTIM),
        KITTY_PER_ORIGIN_MAX_PLACEMENTS
    );
    assert_eq!(
        layer.placement_count_for_origin(NOISY),
        KITTY_PER_ORIGIN_MAX_PLACEMENTS
    );
    assert!(
        layer.get_placement(noisy_pids[0]).is_none(),
        "noisy oldest placement must be evicted first"
    );
    for pid in victim_pids {
        assert!(
            layer.get_placement(pid).is_some(),
            "victim placements must survive noisy pressure"
        );
    }
}

#[test]
fn global_placement_bound_refuses_without_evicting() {
    // Four origins fill the 128 global placement bound (4 x 32). A fifth
    // origin is refused instead of evicting any victim.
    let mut layer = KittyImageLayer::new();
    let image = store_on(&mut layer, VICTIM);
    let origins = [None, Some(7), Some(8), Some(9)];
    for origin in origins {
        for _ in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS {
            place_on(&mut layer, image, origin);
        }
    }
    assert_eq!(layer.placement_len(), KITTY_PLACE_MAX_ITEMS);
    assert_eq!(
        layer.display_for_origin(image, 0, 0, 1, 1, METRICS, 0, 0, Some(99)),
        Err(KittyPlacementError::QuotaExceeded)
    );
    assert_eq!(layer.placement_len(), KITTY_PLACE_MAX_ITEMS);
    for origin in origins {
        assert_eq!(
            layer.placement_count_for_origin(origin),
            KITTY_PER_ORIGIN_MAX_PLACEMENTS,
            "origin {origin:?} must keep every placement"
        );
    }
}

#[test]
fn origin_clear_keeps_other_origins_and_images() {
    // ED2 / alternate-screen clearing stays origin-scoped: clearing the
    // noisy origin drops only its placements while stored images stay
    // inert under the store quotas.
    let mut layer = KittyImageLayer::new();
    let victim_image = store_on(&mut layer, VICTIM);
    let noisy_image = store_on(&mut layer, NOISY);
    place_on(&mut layer, victim_image, VICTIM);
    place_on(&mut layer, noisy_image, NOISY);
    assert_eq!(layer.placement_len(), 2);
    layer.clear_origin(NOISY);
    assert_eq!(layer.placement_len(), 1);
    assert!(layer.placement_for_origin_is_empty(NOISY));
    assert!(!layer.placement_for_origin_is_empty(VICTIM));
    assert_eq!(layer.len(), 2, "images survive origin clears, inert");
}

#[test]
fn fail_closed_posture_is_preserved() {
    // Unsupported wire actions still store without painting, unknown
    // images still refuse placement, and the quota refusal stores nothing.
    for action in ['p', 'q', 'f', 'a', 'c', 'd'] {
        assert!(
            !bitty_rich::KittyAction::from_a(Some(action)).displays(),
            "a={action} must stay stored-never-painted"
        );
    }
    let mut layer = KittyImageLayer::new();
    assert_eq!(
        layer.display_for_origin(KittyImageId(999), 0, 0, 1, 1, METRICS, 0, 0, VICTIM),
        Err(KittyPlacementError::ImageNotFound(KittyImageId(999)))
    );
    assert!(layer.placement_is_empty());
    assert_eq!(
        KittyPlacementError::QuotaExceeded.to_string(),
        "kitty quota exceeded: global bound held by other origins, refused without evicting"
    );
}
