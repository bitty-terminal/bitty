//! Kitty per-pane origin binding (CTX-0254, headless pixel proof).
//!
//! CTX-0256 wired `APC G` into the live PTY path, and pane sessions drain
//! through that same pipeline (`handle_pane_bytes` swaps the pane's grid
//! into the primary slots). Without origin binding, a background pane's
//! program could paint image pixels over the focused pane — a cross-pane
//! spoof. These tests prove the binding end to end on real PTY bytes:
//! pane-tagged placements never paint on another leaf, own-pane images
//! still paint when their leaf is focused, primary images stay on the
//! primary leaf, alternate-screen clears are per-origin, and closing a
//! session drops its placements.
//!
//! Unix-only: pane sessions need a POSIX shell plus PTY master semantics
//! (mirrors `pane_sessions.rs`).

#![cfg(unix)]

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

fn two_pane_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    rt.set_layout(layout);
    rt
}

fn spawn_quiet_pane(rt: &mut Runtime, view: ViewId) {
    rt.spawn_shell_for_view(view, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell must spawn");
}

/// 2x2 opaque red RGBA display (`f=32,s=2,v=2`) as raw PTY bytes.
fn red_apc() -> Vec<u8> {
    format!(
        "\x1b_Gf=32,s=2,v=2,m=0;{}\x1b\\",
        "/wAA//8AAP//AAD//wAA/w=="
    )
    .into_bytes()
}

fn has_opaque_red(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).any(|px| px == [0xFF, 0, 0, 0xFF])
}

#[test]
fn background_pane_image_never_paints_on_focused_pane() {
    let mut rt = two_pane_runtime();
    assert!(rt.set_focus(ViewId::new(1)));
    spawn_quiet_pane(&mut rt, ViewId::new(2));
    // Pane 2's program emits a display image; routing tags it with pane 2's
    // origin. The focused leaf (1, primary grid) must stay clean.
    rt.handle_pane_bytes(ViewId::new(2), &red_apc());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.tick().expect("display forces a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    assert!(
        !has_opaque_red(&rgba),
        "background pane must not paint over the focused pane"
    );
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "placement is retained for its own leaf"
    );
    assert_eq!(rt.kitty_last_frame_images(), 0);
}

#[test]
fn pane_image_paints_on_its_own_focused_leaf() {
    let mut rt = two_pane_runtime();
    assert!(rt.set_focus(ViewId::new(1)));
    spawn_quiet_pane(&mut rt, ViewId::new(2));
    rt.handle_pane_bytes(ViewId::new(2), &red_apc());
    // Focusing the emitting pane paints its image there — binding confines,
    // it does not suppress.
    assert!(rt.set_focus(ViewId::new(2)));
    rt.tick().expect("focus + display force a present");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    assert!(
        has_opaque_red(&rgba),
        "own-pane image must paint on its focused leaf"
    );
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn primary_image_paints_only_on_primary_leaf() {
    let mut rt = two_pane_runtime();
    spawn_quiet_pane(&mut rt, ViewId::new(2));
    // Focus the session leaf: the primary stream's image must not leak onto
    // the pane's grid (symmetric half of the spoof fix).
    assert!(rt.set_focus(ViewId::new(2)));
    rt.handle_pty_bytes(&red_apc());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.tick().expect("display forces a present");
    assert!(
        !has_opaque_red(&rt.headless_rgba().expect("rgba")),
        "primary image must not paint over the session leaf"
    );
    // The session-less leaf shows the primary grid: the image paints there.
    assert!(rt.set_focus(ViewId::new(1)));
    rt.tick().expect("focus change forces a present");
    assert!(
        has_opaque_red(&rt.headless_rgba().expect("rgba")),
        "primary image paints where the primary grid is shown"
    );
}

#[test]
fn pane_alt_screen_clears_only_its_origin() {
    let mut rt = two_pane_runtime();
    assert!(rt.set_focus(ViewId::new(1)));
    spawn_quiet_pane(&mut rt, ViewId::new(2));
    rt.handle_pty_bytes(&red_apc());
    rt.handle_pane_bytes(ViewId::new(2), &red_apc());
    assert_eq!(rt.kitty_placement_count(), 2);
    // Pane 2 enters the alternate screen: only its own placement drops.
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b[?1049h");
    assert!(rt.set_focus(ViewId::new(2)));
    rt.tick().expect("alt transition forces a present");
    assert_eq!(
        rt.kitty_placement_count(),
        1,
        "pane alt entry must not wipe the primary placement"
    );
    assert!(
        !has_opaque_red(&rt.headless_rgba().expect("rgba")),
        "alt leaf carries no image pixels"
    );
    // The surviving primary placement still paints on the primary leaf.
    assert!(rt.set_focus(ViewId::new(1)));
    rt.tick().expect("focus change forces a present");
    assert!(
        has_opaque_red(&rt.headless_rgba().expect("rgba")),
        "primary placement survives the pane's alt entry"
    );
}

#[test]
fn close_pane_session_drops_its_placements() {
    let mut rt = two_pane_runtime();
    assert!(rt.set_focus(ViewId::new(1)));
    spawn_quiet_pane(&mut rt, ViewId::new(2));
    rt.handle_pane_bytes(ViewId::new(2), &red_apc());
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.close_pane_session(&ViewId::new(2)));
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "closed pane must not leave paintable placements behind"
    );
}
