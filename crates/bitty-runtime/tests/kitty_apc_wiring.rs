//! Kitty `APC G` live-path fail-closed suite (CTX-0256, W-141).
//!
//! The parser still pre-scans `APC G`, base64-unwraps, and reassembles `m=`
//! chunks under the ledger cap, emitting `KittyGraphics`; the runtime still
//! routes to `kitty_display_image_owned`, which logs the rejection
//! rate-limited. W-141 moved the decoder to the `bitty-graphics` extension
//! (wiring pending), so completed transmissions fail closed: nothing is
//! stored, nothing paints, and the PTY path never panics. End-to-end
//! pixel proof returns with the wiring task.

use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

#[test]
fn apc_g_single_shot_fails_closed_without_storing() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn apc_g_transmit_only_fails_closed_without_storing() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,a=t,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn apc_g_unknown_format_fails_closed_without_storing() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=7,s=2,v=2,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}
