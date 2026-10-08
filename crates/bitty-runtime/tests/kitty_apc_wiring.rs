//! Kitty `APC G` live-path suite (issue #1802).
//!
//! The parser pre-scans `APC G`, base64-unwraps, and reassembles `m=`
//! chunks under the ledger cap, emitting `KittyGraphics`; the runtime
//! routes to `kitty_display_image_owned`, which decodes, stores, places,
//! and paints. End-to-end pixel proof lives here: single-shot,
//! transmit-only, chunked transfer, PNG, and unknown-format rejection.

use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

/// 1x1 opaque red RGBA PNG (`f=100`), base64 of the `bitty-rich` fixture.
fn red_1x1_png_b64() -> &'static str {
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg=="
}

#[test]
fn apc_g_single_shot_stores_places_and_paints() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn apc_g_transmit_only_stores_without_placing() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,a=t,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn apc_g_chunked_transfer_reassembles_and_places() {
    let mut rt = make_runtime();
    let encoded = red_2x2_b64();
    let mid = encoded.len() / 2;
    let (first, rest) = encoded.split_at(mid);
    // First `m=1` chunk carries the `G` params; the `m=0` tail completes.
    let open = format!("\x1b_Gf=32,s=2,v=2,m=1;{first}\x1b\\");
    rt.handle_pty_bytes(open.as_bytes());
    assert_eq!(rt.kitty_image_count(), 0, "open chunk stores nothing yet");
    let tail = format!("\x1b_Gm=0;{rest}\x1b\\");
    rt.handle_pty_bytes(tail.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn apc_g_png_stores_places_and_paints() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn apc_g_unknown_format_fails_closed_without_storing() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=7,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}
