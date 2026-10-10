//! CTX-1087 (#1891) architecture parity pins.
//!
//! Each item closed with tests/docs; no new Core capabilities, no value
//! changes. This file pins the cross-crate equalities declared by the
//! normative notes:
//!
//! - F11 z-order: `bitty-render`'s `IMAGE_Z_BELOW_BACKGROUND` aliases the
//!   normative `bitty-term-state::KITTY_Z_BELOW_BACKGROUND`.
//! - F11 present quotas: `bitty-render`'s per-frame mirrors equal the
//!   normative `bitty-rich` present ceilings.
//! - F11 store quotas: the rich Kitty placement layer aliases the rich
//!   image-store ceilings.
//! - F5 storage: the plugin-runtime sweep horizon stays `3600` in parity
//!   with `bitty-storage`.

#[test]
fn z_threshold_single_sourced() {
    assert_eq!(
        bitty_render::atlas::IMAGE_Z_BELOW_BACKGROUND,
        bitty_term_state::KITTY_Z_BELOW_BACKGROUND,
        "render z alias must equal normative term-state threshold (CTX-1087 F11)"
    );
    assert_eq!(
        bitty_term_state::KITTY_Z_BELOW_BACKGROUND,
        -1_073_741_824,
        "kitty INT32_MIN/2 layering rule value (no change)"
    );
}

#[test]
fn present_quotas_mirror_normative_rich() {
    assert_eq!(
        bitty_render::batch::MAX_IMAGE_BLITS_PER_FRAME,
        bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME,
        "render blit mirror must equal normative rich ceiling (CTX-1087 F11)"
    );
    assert_eq!(
        bitty_render::batch::MAX_IMAGE_UPLOAD_BYTES_PER_FRAME,
        bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME,
        "render byte mirror must equal normative rich ceiling (CTX-1087 F11)"
    );
    assert_eq!(bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME, 32);
    assert_eq!(
        bitty_rich::KITTY_PRESENT_MAX_BYTES_PER_FRAME,
        64 * 1024 * 1024
    );
}

#[test]
fn store_quotas_alias_image_store() {
    assert_eq!(
        bitty_rich::KITTY_PLACE_MAX_BYTES,
        bitty_rich::image::IMAGE_STORE_MAX_BYTES,
        "kitty place byte cap aliases image-store ceiling (CTX-1087 F11)"
    );
    // 64-image Kitty ledger parity: aliases the kitty placeholder ceiling
    // (term-state `IMAGE_STORE_MAX_ENTRIES`), distinct from the generic
    // 256-image store cap — both normative in their own layer, values
    // unchanged.
    assert_eq!(
        bitty_rich::KITTY_PLACE_MAX_IMAGES,
        bitty_rich::kitty::KITTY_MAX_PLACEHOLDERS,
        "kitty place count cap aliases kitty ledger ceiling (CTX-1087 F11)"
    );
    assert_eq!(
        bitty_rich::kitty::KITTY_MAX_PLACEHOLDERS,
        bitty_term_state::IMAGE_STORE_MAX_ENTRIES,
        "kitty ledger ceiling matches term-state placeholder entries (CTX-1087 F11)"
    );
    assert_eq!(
        bitty_rich::KITTY_PLACE_MAX_ITEMS,
        bitty_rich::image::IMAGE_MAX_PLACEMENTS,
        "kitty place item cap aliases image placement ceiling (CTX-1087 F11)"
    );
}

#[test]
fn stale_sweep_horizon_parity() {
    assert_eq!(
        bitty_runtime::plugin_runtime::STALE_TEMP_AGE_SECS,
        3_600,
        "plugin-runtime sweep horizon must stay 3600 (CTX-1087 F5)"
    );
}
