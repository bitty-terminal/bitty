#![forbid(unsafe_code)]
//! Headless sixel placement (CTX-1668, bitty#2216).
//!
//! Proves the decode half and the paint half meet at the seam kitty graphics
//! already uses: a sixel `DCS` body decodes through
//! [`bitty_vt::decode_sixel`] and the resulting RGBA8 raster is stored and
//! displayed by [`KittyImageLayer`], producing one placement record in paint
//! order. Nothing here paints pixels, opens a window, or touches a GPU.
//!
//! The aggregate per-screen budget is that layer's own byte quota
//! ([`KITTY_PLACE_MAX_BYTES`], 256 MiB): a decoded sixel raster is charged
//! against it exactly like a decoded kitty bitmap, so the parser-side caps
//! (4 MiB payload, 256 colour registers, 8192 px per side, 4096 x 4096 area,
//! 64 MiB RGBA) compose with one screen-level bound instead of adding a
//! second accounting system.

use bitty_rich::{
    CellMetrics, KITTY_DECODE_MAX_BYTES, KITTY_PLACE_MAX_BYTES, KittyImageLayer,
    KittyPlacementError,
};
use bitty_vt::{
    SIXEL_MAX_BYTES, SIXEL_MAX_COLORS, SIXEL_MAX_PIXELS, SixelParams, SixelReject, decode_sixel,
};

const METRICS: CellMetrics = CellMetrics {
    width: 8,
    height: 16,
};

/// Twelve columns of solid green across two bands: a 12 x 12 bitmap.
fn twelve_by_twelve() -> Vec<u8> {
    b"#0;2;0;100;0!12~-!12~".to_vec()
}

#[test]
fn decoded_sixel_produces_a_placement_and_a_paint_record() {
    let payload = twelve_by_twelve();
    let image = decode_sixel(&payload, SixelParams::default()).expect("bounded sixel decodes");
    assert_eq!((image.width, image.height), (12, 12));
    assert_eq!(image.rgba.len(), 12 * 12 * 4);
    // Opaque green, top-left pixel.
    assert_eq!(&image.rgba[0..4], &[0, 255, 0, 255]);

    let mut layer = KittyImageLayer::new();
    let stored = layer
        .store(image.width, image.height, image.rgba.clone(), payload.len())
        .expect("the decoded raster is admitted by the shared image layer");
    let placement = layer
        .display(stored, 3, 2, 0, 0, METRICS, 0, 0)
        .expect("the stored raster places at the cursor cell");

    let record = layer
        .get_placement(placement)
        .expect("placement record exists");
    assert_eq!(record.image, stored);
    assert_eq!((record.anchor_col, record.anchor_row), (3, 2));
    // 12 px across an 8 px cell is two columns; 12 px down a 16 px cell is
    // one row (the pixel-to-cell derivation kitty placements already use).
    assert_eq!((record.cols, record.rows), (2, 1));

    let paint = layer.placements_in_paint_order();
    assert_eq!(paint.len(), 1, "exactly one blit record");
    assert_eq!(paint[0].id, placement);
    assert_eq!(
        layer.total_bytes(),
        image.rgba.len(),
        "the raster is charged to the layer's aggregate byte quota"
    );
}

#[test]
fn refused_sixel_never_reaches_the_placement_layer() {
    // The T-02 expansion vector: eight wire bytes claiming 65k columns.
    let error = decode_sixel(b"!65535~", SixelParams::default()).expect_err("over-wide refused");
    assert!(matches!(error, SixelReject::DimensionsTooLarge { .. }));

    let mut layer = KittyImageLayer::new();
    assert!(layer.placement_is_empty());
    // Nothing to store: the only inputs the seam accepts are decoded rasters,
    // so a refusal leaves the layer exactly as it was.
    assert_eq!(layer.total_bytes(), 0);
    assert!(matches!(
        layer.store(8193, 1, vec![0; 4], 8),
        Err(KittyPlacementError::DimensionsTooLarge { .. })
    ));
}

#[test]
fn sixel_bounds_nest_inside_the_screen_budget() {
    // Parser caps: 4 MiB wire, 256 registers, 8192 px/side, 4096 x 4096
    // area, 64 MiB RGBA. Screen budget: the shared image layer's 256 MiB.
    assert_eq!(SIXEL_MAX_COLORS, 256);
    assert_eq!(SIXEL_MAX_PIXELS, 4096 * 4096);
    assert_eq!(SIXEL_MAX_BYTES, 64 * 1024 * 1024);
    assert_eq!(KITTY_DECODE_MAX_BYTES, SIXEL_MAX_BYTES);
    assert_eq!(KITTY_PLACE_MAX_BYTES, 256 * 1024 * 1024);
    let maximal_image_fits = SIXEL_MAX_BYTES < KITTY_PLACE_MAX_BYTES;
    assert!(
        maximal_image_fits,
        "one maximal sixel image fits inside the per-screen budget"
    );
}
