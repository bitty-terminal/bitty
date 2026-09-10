//! Kitty `APC G` end-to-end wiring (CTX-0256): PTY bytes reach the display seam.
//!
//! The parser pre-scans `APC G`, base64-unwraps, reassembles `m=` chunks
//! under the ledger cap, and emits `KittyGraphics`; the runtime routes to
//! `kitty_display_image`, preserving transmit-only and unknown-action
//! semantics from CTX-0248. These tests prove bytes-on-the-wire reach stored
//! images and paint pixels headlessly, reusing the `kitty_images_present`
//! pattern (9x19 cells, 8px padding, 80x24 grid).

use bitty_runtime::{Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn window_width() -> usize {
    let cfg = RuntimeConfig::default();
    usize::try_from(cfg.window_extent().width()).expect("width fits usize")
}

#[test]
fn apc_g_single_shot_routes_to_display_and_paints() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.tick().expect("display forces a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = window_width();
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    // CTX-0294: the default decoration outer gap + border (8px) shifts the
    // content origin before cell (0,0).
    let pad = pad + 8;
    assert_eq!(
        probe(&rgba, width, pad + 1, pad + 1),
        [0xFF, 0, 0, 0xFF],
        "APC G image must paint topmost"
    );
}

#[test]
fn apc_g_chunked_reassembly_routes_to_display() {
    let mut rt = make_runtime();
    // Split the 24-char base64 across `m=1` + `m=0`.
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=2,v=2,m=1;/wAA//8A\x1b\\");
    assert_eq!(rt.kitty_image_count(), 0, "first chunk stores nothing yet");
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Gm=1;AP//AAD/\x1b\\");
    assert_eq!(rt.kitty_image_count(), 0, "middle chunk stores nothing yet");
    rt.handle_pty_bytes(b"\x1b_Gm=0;/wAA/w==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.tick().expect("display forces a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = window_width();
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    let pad = pad + 8;
    assert_eq!(probe(&rgba, width, pad + 1, pad + 1), [0xFF, 0, 0, 0xFF]);
}

#[test]
fn apc_g_transmit_only_stores_without_painting() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,a=t,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.tick().expect("first tick still presents the grid");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = window_width();
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    let pad = pad + 8;
    assert_ne!(
        probe(&rgba, width, pad + 1, pad + 1),
        [0xFF, 0, 0, 0xFF],
        "transmit-only must paint no image pixels"
    );
}

#[test]
fn apc_g_unknown_action_stores_without_painting() {
    let mut rt = make_runtime();
    // `a=p` (put) is unsupported: stored, not painted (0248 preserved).
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=1,v=1,a=p,m=0;/wAA/w==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn apc_g_bad_base64_and_oversize_claim_store_nothing() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=1,v=1,m=0;!!!!\x1b\\");
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=9000,v=1,m=0;AA==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Gf=7,s=2,v=2,m=0;/wAA//8AAP//AAD//wAA/w==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 0, "unknown f stores nothing");
}

#[test]
fn apc_g_interleaved_text_keeps_order_and_grid() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"A");
    let seq = format!("\x1b_Gf=32,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    rt.handle_pty_bytes(b"B");
    assert_eq!(rt.kitty_image_count(), 1);
    // Grid text on both sides survives image routing (images never mutate grid).
    rt.tick().expect("display forces a present");
    assert!(rt.headless_rgba().is_some());
}
