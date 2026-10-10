//! Kitty animation playback suite (S1 of #1849, Task CTX-1111).
//!
//! `a=f` frame ingest, `a=a` tick-driven playback, and `a=c` compose,
//! end to end through `APC G` bytes plus virtual-clock ticks:
//!
//! - a running animation advances its current frame on the present tick
//!   (gap order, loop budgets, gapless skips);
//! - stopped animations freeze, loading ones park on the last frame;
//! - over-cap frames refuse with nothing stored;
//! - alt-screen and hidden origins pause, scrolled-off placements keep
//!   time;
//! - compose blends/replaces bounded rects and refuses out-of-bounds
//!   rects with nothing composed;
//! - only animating placements gain damage;
//! - the S6 raster cache stays frame-aware (no stale frame hits);
//! - virtual (`U=1`) and file-backed (`t=f`) frames reuse the same path;
//! - animated images still answer status queries.

use bitty_runtime::Runtime;
use std::time::{Duration, Instant};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

/// 2x2 opaque blue RGBA payload (`f=32`), base64 `AAD//wAA//8AAP//AAD//w==`.
fn blue_2x2_b64() -> &'static str {
    "AAD//wAA//8AAP//AAD//w=="
}

/// 2x2 opaque blue RGBA frame bytes (`f=32`).
fn blue_2x2() -> Vec<u8> {
    [0x00, 0x00, 0xFF, 0xFF].repeat(4)
}

fn apc(seq: &str) -> Vec<u8> {
    format!("\x1b_{seq}\x1b\\").into_bytes()
}

/// Displays a 2x2 red root image on wire id 1 (cursor-anchored, 2x2 span).
fn display_root(rt: &mut Runtime) {
    let seq = apc(&format!("Gf=32,s=2,v=2,i=1,c=2,r=2,m=0;{}", red_2x2_b64()));
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
}

/// Appends one 2x2 blue frame to wire id 1 through `APC G` bytes (so
/// terminal truth records the frame gap alongside the stored pixels).
fn append_blue_frame(rt: &mut Runtime) {
    let seq = apc(&format!("Gf=32,s=2,v=2,a=f,i=1,m=0;{}", blue_2x2_b64()));
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
}

/// Starts (or stops) the wire-1 animation: `Ga=a,i=1,s=<state>;`.
fn set_anim_state(rt: &mut Runtime, state: u8) {
    rt.handle_pty_bytes(&apc(&format!("Ga=a,i=1,s={state},m=0;")));
}

/// Gives the root frame a 40ms gap (`Ga=a,i=1,r=1,z=40;`) so both frames
/// pace one 40ms quantum each.
fn set_root_gap(rt: &mut Runtime) {
    rt.handle_pty_bytes(&apc("Ga=a,i=1,r=1,z=40,m=0;"));
}

/// Two-frame running animation on wire 1, both gaps 40ms.
fn running_two_frame(rt: &mut Runtime) {
    display_root(rt);
    append_blue_frame(rt);
    set_root_gap(rt);
    set_anim_state(rt, 3);
}

#[test]
fn tick_advances_running_animation_in_gap_order() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    assert_eq!(rt.kitty_anim_current_frame(1), 1);
    let t0 = Instant::now();
    rt.tick_at(t0);
    // One 40ms quantum consumes the root gap: frame 2 shows.
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
    // Another 40ms consumes frame 2 and wraps (infinite loops by default).
    rt.tick_at(t0 + Duration::from_millis(80));
    assert_eq!(rt.kitty_anim_current_frame(1), 1);
}

#[test]
fn loop_budget_v2_stops_on_root() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    // `v=2` plays exactly one loop, then stops on the root frame.
    rt.handle_pty_bytes(&apc("Ga=a,i=1,v=2,m=0;"));
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(80));
    assert_eq!(rt.kitty_anim_current_frame(1), 1);
    // Stopped: further ticks never advance again.
    rt.tick_at(t0 + Duration::from_millis(8000));
    assert_eq!(rt.kitty_anim_current_frame(1), 1);
}

#[test]
fn stopped_animation_never_advances() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    set_anim_state(&mut rt, 1);
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(10_000));
    assert_eq!(rt.kitty_anim_current_frame(1), 1);
}

#[test]
fn loading_parks_on_last_frame() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    // Loading runs toward the last frame but never wraps.
    set_anim_state(&mut rt, 2);
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
    rt.tick_at(t0 + Duration::from_millis(10_000));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
}

#[test]
fn gapless_root_skips_without_consuming_time() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    append_blue_frame(&mut rt);
    // No root gap: the root is gapless and never displays.
    set_anim_state(&mut rt, 3);
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(1));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
}

#[test]
fn overcap_frame_refused_with_nothing_stored() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let images_before = rt.kitty_image_count();
    // 5000x5000 trips the area cap at decode, before any allocation.
    let err = rt
        .kitty_append_frame_owned(32, Some(5000), Some(5000), vec![0; 8].into_boxed_slice(), 1)
        .expect_err("over-area frame must fail closed");
    assert!(matches!(err, bitty_runtime::KittyImageError::Decode(_)));
    assert_eq!(rt.kitty_image_count(), images_before);
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    // Same byte length but reshaped trips the ingest dimension match.
    let err = rt
        .kitty_append_frame_owned(32, Some(4), Some(1), blue_2x2().into_boxed_slice(), 1)
        .expect_err("reshaped frame must fail closed");
    assert!(matches!(err, bitty_runtime::KittyImageError::Placement(_)));
    assert_eq!(rt.kitty_image_count(), images_before);
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    // The store stays usable: a valid frame still appends as frame 2.
    let frame_no = rt
        .kitty_append_frame_owned(32, Some(2), Some(2), blue_2x2().into_boxed_slice(), 1)
        .expect("valid frame must append");
    assert_eq!(frame_no, 2);
    // Same refusal through `APC G` bytes records no truth gap either
    // (store-first): malformed PNG decodes to nothing, stores nothing,
    // and the descriptor still names only the root frame.
    let seq = apc("Gf=100,a=f,i=1,m=0;bm90LWEtcG5n;");
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(1)
            .map(|anim| anim.frame_count()),
        None
    );
}

#[test]
fn frame_for_unknown_wire_refused() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let err = rt
        .kitty_append_frame_owned(32, Some(2), Some(2), blue_2x2().into_boxed_slice(), 77)
        .expect_err("frame for unheld wire id must fail closed");
    assert_eq!(err, bitty_runtime::KittyImageError::NoAnimationTarget(77));
    assert_eq!(rt.kitty_image_count(), 1);
}

#[test]
fn alt_screen_suppresses_advance() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    rt.handle_pty_bytes(b"\x1b[?1049h");
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(10_000));
    assert_eq!(
        rt.kitty_anim_current_frame(1),
        1,
        "alt-screen animation must freeze"
    );
    rt.handle_pty_bytes(b"\x1b[?1049l");
    rt.tick_at(t0 + Duration::from_millis(10_040));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
}

#[test]
fn scrolled_off_placement_keeps_time() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    // Scroll the 2-row placement fully off the top: it paints nothing but
    // its animation keeps time with its siblings.
    rt.handle_pty_bytes(&[b'\n'; 30]);
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
}

#[test]
fn compose_replace_changes_painted_pixels() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    rt.tick_at(Instant::now());
    let before = rt.headless_rgba().expect("frame must exist");
    // Replace frame 1's top-left pixel with frame 2's (blue).
    rt.handle_pty_bytes(&apc("Ga=c,i=1,r=2,c=1,x=0,y=0,w=1,h=1,X=1,m=0;"));
    rt.tick();
    let after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(before, after, "composed frame must repaint");
}

#[test]
fn compose_out_of_bounds_refused_with_nothing_composed() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    rt.tick_at(Instant::now());
    let before = rt.headless_rgba().expect("frame must exist");
    // 2x2 frames: a 4-wide rect at (1,1) overflows.
    rt.handle_pty_bytes(&apc("Ga=c,i=1,r=2,c=1,x=1,y=1,w=4,h=4,X=1,m=0;"));
    rt.tick();
    let after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(before, after, "refused compose must paint nothing new");
}

#[test]
fn damage_scopes_to_animating_placements_only() {
    let mut rt = make_runtime();
    // Wire 1 animates on rows 0..1; wire 2 sits static on rows 2..3.
    display_root(&mut rt);
    append_blue_frame(&mut rt);
    let seq = apc(&format!("Gf=32,s=2,v=2,i=2,c=2,r=2,m=0;{}", red_2x2_b64()));
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_placement_count(), 2);
    set_root_gap(&mut rt);
    set_anim_state(&mut rt, 3);
    let generation = rt.state().generation();
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
    let regions = rt.state().damage_since(generation);
    assert!(!regions.is_empty(), "advance must damage");
    for region in &regions {
        match region {
            bitty_term_state::DamagedRegion::Grid(rect) => assert!(
                rect.bottom < 2,
                "damage must stay inside the animating rows 0..1, got {rect:?}"
            ),
            bitty_term_state::DamagedRegion::Scrollback { .. } => {
                panic!("advance must not scroll, got {region:?}")
            }
        }
    }
}

#[test]
fn raster_cache_is_frame_aware_across_ticks() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    let misses_frame1 = rt.kitty_raster_misses();
    assert!(misses_frame1 >= 1, "first present must rasterize frame 1");
    // Advance to frame 2: a new key misses, then admits.
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
    assert!(
        rt.kitty_raster_misses() > misses_frame1,
        "frame 2 must miss its own key, never hit frame 1's bytes"
    );
    // Printing text forces a present on the same frame 2: a cache hit.
    let hits_before = rt.kitty_raster_hits();
    rt.handle_pty_bytes(b"x");
    rt.tick_at(t0 + Duration::from_millis(41));
    assert!(
        rt.kitty_raster_hits() > hits_before,
        "re-presenting frame 2 must hit"
    );
}

#[test]
fn virtual_animated_image_advances_without_blits() {
    let mut rt = make_runtime();
    // Combined `a=T,U=1` root on wire 8 (paints via grid runs, never blits).
    let outcome = rt
        .kitty_display_virtual_owned_with_wire(
            32,
            Some(2),
            Some(2),
            2,
            2,
            [0xFF, 0x00, 0x00, 0xFF].repeat(4).into_boxed_slice(),
            8,
            0,
            false,
        )
        .expect("virtual root must register");
    assert!(matches!(
        outcome,
        bitty_runtime::KittyDisplayOutcome::VirtualPrototype { .. }
    ));
    // The `a=f` path records the truth gap alongside the stored pixels.
    let seq = apc(&format!("Gf=32,s=2,v=2,a=f,i=8,m=0;{}", blue_2x2_b64()));
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_anim_frame_count(8), 2);
    rt.handle_pty_bytes(&apc("Ga=a,i=8,s=3,m=0;"));
    let t0 = Instant::now();
    rt.tick_at(t0);
    rt.tick_at(t0 + Duration::from_millis(100));
    assert_eq!(
        rt.kitty_anim_current_frame(8),
        2,
        "virtual animation advances through the same path"
    );
    assert_eq!(
        rt.kitty_last_frame_images(),
        0,
        "virtual prototypes never emit blits (S4 double-paint rule)"
    );
}

#[test]
fn animated_image_answers_status_query() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    rt.take_replies();
    // Payload-less status lookup (bitty extension in spec shape).
    let seq = apc("Gf=32,a=q,i=1,m=0;");
    rt.handle_pty_bytes(&seq);
    let replies: Vec<Vec<u8>> = rt
        .take_replies()
        .iter()
        .map(|reply| reply.to_vec())
        .collect();
    assert_eq!(replies, vec![b"\x1b_Gi=1;OK\x1b\\".to_vec()]);
}

/// Standard-base64 encoder (no new dependency; paths only, tiny inputs).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(first >> 2) as usize] as char);
        out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[test]
fn file_backed_frame_reuses_s3_read() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bitty-ctx1111-anim-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir must build");
    let file = dir.join("frame.bin");
    std::fs::write(&file, blue_2x2()).expect("frame fixture must write");
    let mut rt = make_runtime();
    display_root(&mut rt);
    // `a=f` with `t=f` names the frame object; the S3 sandbox reads it
    // (bounded, fail closed) and the bytes ingest as frame 2.
    #[cfg(unix)]
    let path_bytes = {
        use std::os::unix::ffi::OsStrExt;
        file.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let path_bytes = file
        .to_str()
        .expect("test paths are UTF-8")
        .as_bytes()
        .to_vec();
    let mut seq = "\x1b_Gf=32,s=2,v=2,a=f,i=1,t=f,m=0;"
        .to_string()
        .into_bytes();
    seq.extend_from_slice(base64_encode(&path_bytes).as_bytes());
    seq.extend_from_slice(b"\x1b\\");
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
