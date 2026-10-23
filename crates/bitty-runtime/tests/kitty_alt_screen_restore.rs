//! bitty#2437 regression: a kitty image on the primary screen survives a
//! full-screen TUI round trip.
//!
//! Owner repro: `chafa -f kitty image` paints in a panel, a TUI (`btop`) takes
//! the alternate screen and exits, and the image is gone. The critical byte
//! in that real sequence is the TUI's exit clear (`ED 2`) emitted **before**
//! `?1049l`: it runs while the alternate screen is still active. If the alt
//! clear reaches the primary plane's live image layer, the leave has nothing
//! to repaint.
//!
//! The fixtures use real APC framing: a multi-read `a=T` transmission like
//! chafa's, and the exact `?1049h … ED 2 … ?1049l` shape measured from btop
//! 1.4.7. Assertions are pixel-level, not just placement metadata.

use std::time::Instant;

use bitty_runtime::{Runtime, RuntimeConfig};

const RED_PNG: &[u8] = include_bytes!("fixtures/kitty-red-8x8.png");

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// chafa's `-f kitty` shape: `a=T,f=100,s=8,v=8,c=10,r=5,C=1` split over two
/// APC chunks (`m=1` then `m=0`), exactly as it reaches the tty.
fn chafa_apc() -> Vec<u8> {
    let payload = base64(RED_PNG);
    let (first, second) = payload.split_at(payload.len() / 2);
    let mut out =
        format!("\x1b_Ga=T,f=100,s=8,v=8,c=10,r=5,C=1,q=2,m=1,i=77;{first}\x1b\\").into_bytes();
    out.extend_from_slice(format!("\x1b_Gm=0;{second}\x1b\\").as_bytes());
    out
}

/// Runtime with the cursor hidden (its block would overlap the image rect
/// and blur the pixel comparison).
fn runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_min_present_interval(std::time::Duration::ZERO);
    rt.handle_pty_bytes(b"\x1b[?25l");
    rt
}

/// Bounding box and count of strongly red pixels in the presented frame.
fn red_bounds(rt: &Runtime) -> Option<(usize, usize, usize, usize, usize)> {
    let extent = rt.surface_extent()?;
    let rgba = rt.headless_rgba()?;
    let width = extent.width() as usize;
    let height = extent.height() as usize;
    let (mut min_x, mut min_y, mut max_x, mut max_y, mut count) =
        (usize::MAX, usize::MAX, 0usize, 0usize, 0usize);
    for y in 0..height {
        for x in 0..width {
            let index = (y * width + x) * 4;
            let (r, g, b) = (rgba[index], rgba[index + 1], rgba[index + 2]);
            if r > 200 && g < 60 && b < 60 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                count += 1;
            }
        }
    }
    (count > 0).then_some((min_x, min_y, max_x, max_y, count))
}

fn assert_image_restored(rt: &Runtime, before: Option<(usize, usize, usize, usize, usize)>) {
    let before = before.expect("image painted before the TUI");
    assert_eq!(rt.kitty_placement_count(), 1, "main placement retained");
    assert_eq!(rt.kitty_image_count(), 1, "stored image retained");
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "the restore frame must blit the image"
    );
    let after = red_bounds(rt).expect("image repainted after the TUI");
    assert_eq!(
        after, before,
        "restored image must paint at its original primary-screen rect"
    );
}

/// btop 1.4.7 exit bytes around the screen switch (measured from a real PTY):
/// synchronized update off, mouse modes off, clear the *alternate* screen,
/// then leave it. The image must survive the alt-plane clear.
const BTOP_EXIT: &[u8] =
    b"\x1b[?2026l\x1b[?1002l\x1b[?1015l\x1b[?1006l\x1b[2J\x1b[0;0f\x1b[?1049l\x1b[?25h";

#[test]
fn primary_image_survives_a_tui_that_clears_the_alt_screen() {
    let mut rt = runtime();
    rt.handle_pty_bytes(&chafa_apc());
    assert_eq!(rt.kitty_placement_count(), 1, "chafa placement admitted");
    assert!(rt.tick().is_some(), "image frame presents");
    let before = red_bounds(&rt);

    // btop start: enter alt, paint its own screen (including a full clear,
    // which is why the engine must keep planes separate).
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[2Jbtop frame");
    assert!(rt.tick().is_some(), "alt frame presents");
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "the primary image must not paint over the TUI"
    );
    assert!(
        red_bounds(&rt).is_none(),
        "no primary image pixels while the TUI owns the screen"
    );

    // btop exit: ED 2 on the alternate screen, then ?1049l, then the shell
    // prompt. The alt clear must not touch the stashed primary plane.
    rt.handle_pty_bytes(BTOP_EXIT);
    // btop leaves the cursor visible; hide it again so the pixel oracle
    // measures the image alone (a live cursor legitimately paints on top).
    rt.handle_pty_bytes(b"\x1b[?25l");
    assert!(rt.tick().is_some(), "restore frame presents");
    rt.handle_pty_bytes(b"$ ");
    assert!(rt.tick().is_some(), "prompt frame presents");
    assert_image_restored(&rt, before);
}

#[test]
fn enter_clear_and_leave_in_one_pty_batch_restore_the_image() {
    // A fast TUI can emit the whole round trip inside one read. Plane swaps
    // are applied at the mode transition itself (#2442), so the alt clear
    // still cannot reach the primary layer; the next tick repaints.
    let mut rt = runtime();
    rt.handle_pty_bytes(&chafa_apc());
    assert!(rt.tick().is_some());
    let before = red_bounds(&rt);

    let mut batch = Vec::from(&b"\x1b[?1049h\x1b[H\x1b[2Jtui"[..]);
    batch.extend_from_slice(BTOP_EXIT);
    batch.extend_from_slice(b"\x1b[?25l");
    rt.handle_pty_bytes(&batch);
    assert!(rt.tick().is_some(), "single-batch round trip presents");
    assert_image_restored(&rt, before);
}

#[test]
fn repeated_tui_cycles_keep_restoring_the_image() {
    // The stash must be recreated on every entry; a second TUI run cannot
    // consume the first cycle's restore.
    let mut rt = runtime();
    rt.handle_pty_bytes(&chafa_apc());
    assert!(rt.tick().is_some());
    let before = red_bounds(&rt);

    for cycle in 0..3 {
        rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H\x1b[2Jtui");
        assert!(rt.tick().is_some(), "cycle {cycle}: alt presents");
        assert!(red_bounds(&rt).is_none(), "cycle {cycle}: image hidden");
        rt.handle_pty_bytes(BTOP_EXIT);
        rt.handle_pty_bytes(b"\x1b[?25l");
        assert!(rt.tick().is_some(), "cycle {cycle}: restore presents");
        assert_image_restored(&rt, before);
    }
}

#[test]
fn alt_screen_images_die_with_the_tui_plane() {
    // Plane isolation in the other direction: an image transmitted on the
    // alternate screen must not leak onto the primary grid after exit.
    let mut rt = runtime();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H");
    rt.handle_pty_bytes(&chafa_apc());
    assert_eq!(rt.kitty_placement_count(), 1, "alt placement admitted");
    assert!(rt.tick().is_some());
    assert!(
        red_bounds(&rt).is_some(),
        "the TUI's own image paints on the alt screen"
    );

    rt.handle_pty_bytes(BTOP_EXIT);
    rt.handle_pty_bytes(b"\x1b[?25l");
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_placement_count(),
        0,
        "alt placements die with the alt plane"
    );
    assert!(
        red_bounds(&rt).is_none(),
        "no alt image leaks onto the restored primary screen"
    );
}

#[test]
fn restore_waits_out_a_synchronized_update_window() {
    // Some TUIs leave the alternate screen before disabling synchronized
    // updates (`?2026`). Frames are deferred while the mode is active, so the
    // restore must survive the hold and commit once the mode exits; a restore
    // that only lives for one dirty tick would be lost here.
    let mut rt = runtime();
    rt.handle_pty_bytes(&chafa_apc());
    assert!(rt.tick().is_some());
    let before = red_bounds(&rt);

    // The primary screen owns an open synchronized update (a shell that
    // enabled the mode before the TUI started; the register survives entry).
    rt.handle_pty_bytes(b"\x1b[?2026h");
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H\x1b[2Jtui");
    assert!(
        rt.tick().is_none(),
        "the open update defers the alt frame too"
    );

    let t0 = Instant::now();
    assert!(
        rt.tick_at(t0).is_none(),
        "an active synchronized update defers the frame"
    );
    // Leave the alt screen and clear it while the update is still open.
    rt.handle_pty_bytes(b"\x1b[2J\x1b[?1049l");
    assert!(
        rt.tick_at(t0 + std::time::Duration::from_millis(1))
            .is_none(),
        "the deferred window keeps holding the restore frame"
    );
    rt.handle_pty_bytes(b"\x1b[?2026l\x1b[?25l");
    assert!(
        rt.tick_at(t0 + std::time::Duration::from_millis(2))
            .is_some(),
        "mode exit commits the restored primary frame"
    );
    assert_image_restored(&rt, before);
}
