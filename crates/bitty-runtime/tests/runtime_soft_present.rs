//! Headless end-to-end proof: PTY bytes -> parser -> state -> damage -> render DrawList -> software present.
//!
//! This test proves the Correct Terminal hot path without any display server
//! or GPU. It drives `Runtime` exactly as the production flow will:
//!   1. `handle_pty_bytes` feeds raw bytes into the VT parser.
//!   2. The parser emits `TerminalAction`s applied to `State`.
//!   3. `State` accumulates damage against a generation counter.
//!   4. `tick` plans a frame from `Snapshot + Damage`, produces a `DrawList`,
//!      and composites it onto a `Surface::headless` RGBA buffer.
//!   5. The buffer is inspected for non-trivial content and determinism.
//!
//! The test runs on every CI leg, including headless Linux and Windows's
//! `cargo check --target x86_64-pc-windows-gnu` completeness gate. No window
//! system, adapter, or font file is required. Real GPU present remains
//! env-gated (`BITTY_RENDER_GPU_TESTS=1` in `bitty-render`) and is explicitly
//! out of scope here; its absence is not described as a failure.

use bitty_platform::PhysicalSize;
use bitty_runtime::{Runtime, RuntimeConfig};

#[test]
fn soft_present_proves_bytes_to_pixels_without_gpu_or_display() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime must build");
    assert!(rt.is_headless(), "default must be headless");

    // First tick must present the full initial grid (pending full redraw).
    let first = rt.tick().expect("initial full redraw must present");
    assert!(first.headless);
    assert!(first.fills > 0, "full redraw must emit background fills");
    let first_rgba = rt.headless_rgba().expect("rgba after first present");
    assert_eq!(
        first_rgba.len(),
        rt.surface_extent().expect("extent after build").width() as usize
            * rt.surface_extent().expect("extent").height() as usize
            * 4,
        "rgba buffer must match surface extent"
    );
    assert!(
        first_rgba.iter().any(|&b| b != 0),
        "full redraw must produce non-zero pixels (background)"
    );

    // Feed a mix of printable text, VT controls, and OSC title — the same mix
    // a real shell would emit.
    let payload = b"hello \x1b[31mred\x1b[0m world\x1b]0;my-title\x07\r\n\x1b[2K";
    rt.handle_pty_bytes(payload);

    // Cold-path queue must have observed the title and damage.
    assert!(
        rt.cold_queue_len() > 0,
        "handler must enqueue cold events for title and damage"
    );

    // Second tick must present the damage driven by the payload and produce a
    // distinct RGBA buffer (glyphs + background changes).
    let second = rt.tick().expect("damage from payload must present");
    assert!(second.headless);
    assert!(second.generation > first.generation);
    assert!(
        second.glyphs > 0 || second.fills > 0,
        "payload must emit glyphs or fills"
    );
    let second_rgba = rt.headless_rgba().expect("rgba after payload present");
    assert_eq!(
        second_rgba.len(),
        first_rgba.len(),
        "extent must not have changed"
    );
    assert_ne!(
        first_rgba, second_rgba,
        "payload must change the pixel buffer deterministically"
    );

    // Determinism: replaying the same payload on a fresh runtime must land on
    // identical snapshot state and identical RGBA bytes.
    let mut rt2 = Runtime::new(RuntimeConfig::default()).expect("second runtime must build");
    rt2.tick().expect("initial must present");
    rt2.handle_pty_bytes(payload);
    let replay = rt2.tick().expect("replay payload must present");
    let replay_rgba = rt2.headless_rgba().expect("rgba after replay");
    assert_eq!(second.generation, replay.generation);
    assert_eq!(second.fills, replay.fills);
    assert_eq!(second.glyphs, replay.glyphs);
    assert_eq!(second_rgba, replay_rgba, "replay must be bit-identical");

    // Idle frame must burn no CPU beyond the damage check (frame-on-demand).
    assert_eq!(rt.tick(), None, "idle tick with no new bytes must be None");
    assert_eq!(rt2.tick(), None);

    // Resize reconfigures the headless surface and forces a full redraw — the
    // only honest non-GPU path. Zero-size resizes are skipped.
    let new_extent = PhysicalSize::new(640, 400);
    rt.handle_resize(new_extent)
        .expect("valid resize must succeed");
    assert_eq!(rt.surface_extent(), Some(new_extent));
    let after_resize = rt.tick().expect("resize must force full redraw");
    assert!(after_resize.headless);
    let resized_rgba = rt.headless_rgba().expect("rgba after resize");
    assert_eq!(
        resized_rgba.len(),
        new_extent.width() as usize * new_extent.height() as usize * 4
    );

    // Zero-size resize must not disturb the extent, matching the GPU contract
    // `map_resize_to_surface_extent`.
    rt.handle_resize(PhysicalSize::new(0, 0))
        .expect("zero resize must be a no-op");
    assert_eq!(rt.surface_extent(), Some(new_extent));

    // Wide-char and erase handling (RFC invariant 2: no orphan spacers) must
    // survive the headless path without panic.
    rt.handle_pty_bytes("中".as_bytes());
    let wide = rt.tick().expect("wide char must present");
    assert!(wide.glyphs > 0 || wide.fills > 0);
    rt.handle_pty_bytes(b"\x1b[2K");
    assert!(rt.tick().is_some(), "erase-line damage must present");
}
