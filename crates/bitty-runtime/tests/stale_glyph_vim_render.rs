//! End-to-end retained-frame coverage for Vim/Neovim redraws.
//!
//! The terminal-state erase tests prove that wide-cell truth and damage are
//! correct. These tests cover the separate presentation contract: after a
//! scroll-copy frame, a retained glyph must not survive an ANSI erase or a
//! wide-to-narrow/narrow-to-wide replacement. Every incremental frame is
//! compared with a forced-full headless frame so a transient split PTY frame
//! cannot be mistaken for persistent retained-frame ghosting.

#![forbid(unsafe_code)]

use bitty_platform::PhysicalSize;
use bitty_runtime::{AnimationKind, Runtime, RuntimeConfig};
use bitty_ui::{LayoutNode, View, ViewId};

fn runtime() -> Runtime {
    let mut config = RuntimeConfig::default();
    config.animations.duration_ms = [0; AnimationKind::COUNT];
    let mut runtime = Runtime::with_deterministic_rasterizer(config).expect("runtime builds");
    runtime.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    runtime
        .handle_resize(PhysicalSize::new(736, 472))
        .expect("resize applies");
    runtime.handle_pty_bytes(b"\x1b[?25l");
    runtime.tick().expect("initial frame presents");
    runtime
}

fn populate(runtime: &mut Runtime, wide: bool) {
    for row in 0..24u8 {
        let text = if wide {
            format!("row-{row:02} 中OLD-SUFFIX")
        } else {
            format!("row-{row:02} OLD-SUFFIX")
        };
        runtime.handle_pty_bytes(format!("\x1b[{};1H{text}\x1b[2K", row + 1).as_bytes());
    }
    runtime.tick().expect("populated frame presents");
}

fn drive_rewrite(runtime: &mut Runtime, wide: bool, narrow: bool) {
    runtime.handle_pty_bytes(b"\x1b[24;1H\n");
    runtime.handle_pty_bytes(b"\x1b[11;1H\x1b[2K");
    let replacement = match (wide, narrow) {
        (true, false) => "NARROW",
        (false, true) => "中WIDE",
        _ => "NEW",
    };
    runtime.handle_pty_bytes(replacement.as_bytes());
}

fn force_full(runtime: &mut Runtime) {
    runtime
        .handle_resize(PhysicalSize::new(736, 472))
        .expect("forced full redraw resize applies");
}

fn assert_incremental_matches_full(wide: bool, narrow: bool) {
    let mut incremental = runtime();
    populate(&mut incremental, wide);
    let mut full = runtime();
    populate(&mut full, wide);

    drive_rewrite(&mut incremental, wide, narrow);
    drive_rewrite(&mut full, wide, narrow);
    force_full(&mut full);

    let incremental_stats = incremental.tick().expect("incremental frame presents");
    let full_stats = full.tick().expect("forced full frame presents");
    assert_eq!(
        incremental.headless_rgba(),
        full.headless_rgba(),
        "incremental and full RGBA diverged for wide={wide}, narrow={narrow}"
    );
    assert_eq!(
        incremental_stats.cells_examined, full_stats.cells_examined,
        "a retained glyph intersecting an erase must fail closed"
    );
}

#[test]
fn wide_to_narrow_rewrite_has_no_retained_glyph() {
    assert_incremental_matches_full(true, false);
}

#[test]
fn narrow_to_wide_rewrite_has_no_retained_glyph() {
    assert_incremental_matches_full(false, true);
}

#[test]
fn erase_modes_and_cursor_motion_remain_full_frame_equivalent() {
    for sequence in [
        b"\x1b[24;1H\n\x1b[11;1H\x1b[KNEW".as_slice(),
        b"\x1b[24;1H\n\x1b[11;1H\x1b[2JNEW".as_slice(),
        b"\x1b[24;1H\n\x1b[11;1H\x1b[1PNEW".as_slice(),
        b"\x1b[24;1H\n\x1b[11;1H\x1b[1@NEW".as_slice(),
        b"\x1b[24;1H\n\x1b[11;1H\r\x1b[3C\x1b[2D\x1b[KNEW".as_slice(),
    ] {
        let mut incremental = runtime();
        populate(&mut incremental, true);
        let mut full = runtime();
        populate(&mut full, true);
        incremental.handle_pty_bytes(sequence);
        full.handle_pty_bytes(sequence);
        force_full(&mut full);
        incremental
            .tick()
            .expect("incremental erase frame presents");
        full.tick().expect("full erase frame presents");
        assert_eq!(incremental.headless_rgba(), full.headless_rgba());
    }
}

#[test]
fn split_pty_and_synchronized_update_do_not_change_final_frame() {
    let mut incremental = runtime();
    populate(&mut incremental, true);
    let mut full = runtime();
    populate(&mut full, true);

    let bytes = b"\x1b[?2026h\x1b[24;1H\n\x1b[11;1H\x1b[2KNEW\x1b[?2026l";
    for chunk in bytes.chunks(3) {
        incremental.handle_pty_bytes(chunk);
    }
    full.handle_pty_bytes(bytes);
    force_full(&mut full);
    incremental.tick().expect("synchronized frame presents");
    full.tick().expect("full synchronized frame presents");
    assert_eq!(incremental.headless_rgba(), full.headless_rgba());
}

#[test]
fn alternate_screen_erase_does_not_reuse_primary_glyphs() {
    let mut incremental = runtime();
    populate(&mut incremental, true);
    let mut full = runtime();
    populate(&mut full, true);

    let bytes = b"\x1b[?1049h\x1b[1;1H\x1b[2JALT\x1b[?1049l";
    incremental.handle_pty_bytes(bytes);
    full.handle_pty_bytes(bytes);
    force_full(&mut full);
    incremental.tick().expect("alternate-screen frame presents");
    full.tick().expect("full alternate-screen frame presents");
    assert_eq!(incremental.headless_rgba(), full.headless_rgba());
}
