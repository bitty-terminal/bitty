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

/// Drains the CTX-1110 async local-medium worker (file-backed `t=f`
/// frames settle on later pumps, never inside `handle_pty_bytes`).
/// Unix-only: the sole caller is the `#[cfg(unix)]` file-backed test
/// (Windows denies before any worker exists), keeping `--tests` Windows
/// builds `dead_code`-clean under CI `-D warnings`.
#[cfg(unix)]
fn pump_settled(rt: &mut Runtime) {
    for _ in 0..200 {
        rt.pump_kitty_local_completions();
        if rt.kitty_local_pending_count() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    rt.pump_kitty_local_completions();
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

/// 2x2 opaque blue RGBA payload (`f=32`), base64 `AAD//wAA//8AAP//AAD//w==`.
fn blue_2x2_b64() -> &'static str {
    "AAD//wAA//8AAP//AAD//w=="
}

/// 2x2 striped RGBA payload (`f=32`): row 0 red, row 1 blue.
fn striped_2x2_b64() -> &'static str {
    "/wAA//8AAP8AAP//AAD//w=="
}

/// 1x1 opaque blue RGBA payload (`f=32`), base64 `AAD//w==`.
fn blue_1x1_b64() -> &'static str {
    "AAD//w=="
}

/// 1x1 opaque green RGBA payload (`f=32`), base64 `AP8A/w==`.
fn green_1x1_b64() -> &'static str {
    "AP8A/w=="
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
    // Frozen virtual clock: both presents see the same instant, so any
    // repaint below comes from the compose itself, never from a 40ms gap
    // elapsing on a slow shard.
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // Replace frame 1's top-left pixel with frame 2's bottom-right pixel
    // (blue): distinct source (`X=`, `Y=`) and destination (`x=`, `y=`)
    // offsets, `C=1` replace mode.
    rt.handle_pty_bytes(&apc("Ga=c,i=1,r=2,c=1,X=1,Y=1,x=0,y=0,w=1,h=1,C=1,m=0;"));
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(before, after, "composed frame must repaint");
}

#[test]
fn compose_src_y_selects_source_row_on_wire() {
    // Striped frame 2 (row 0 red, row 1 blue): the same 1x1 `C=1` compose
    // onto (0, 0) must paint different pixels for `Y=0` versus `Y=1`.
    // Uniform fixtures pass vacuously here, so this pin uses distinct
    // rows to lock the live `Y=` source offset.
    fn painted_after_src_y(src_y: u32) -> (Vec<u8>, Runtime) {
        let mut rt = make_runtime();
        display_root(&mut rt);
        let seq = apc(&format!("Gf=32,s=2,v=2,a=f,i=1,m=0;{}", striped_2x2_b64()));
        rt.handle_pty_bytes(&seq);
        assert_eq!(rt.kitty_anim_frame_count(1), 2);
        // Frozen virtual clock (see above): both presents share one
        // instant, so the repaint comes from the compose itself.
        let t0 = Instant::now();
        rt.tick_at(t0);
        rt.handle_pty_bytes(&apc(&format!(
            "Ga=c,i=1,r=2,c=1,X=0,Y={src_y},x=0,y=0,w=1,h=1,C=1,m=0;"
        )));
        rt.tick_at(t0);
        let rgba = rt.headless_rgba().expect("frame must exist");
        (rgba, rt)
    }

    /// First pixel of the composed dest cell (0,0) in the headless frame.
    /// The cursor-anchored 2x2 image sits at the content origin, so dest
    /// (0,0) lands on `pad + content` at the live surface stride — no
    /// hardcoded pixels, same frame math the present path uses.
    fn dest_first_pixel(rgba: &[u8], rt: &Runtime) -> [u8; 4] {
        let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
        let extent = rt.surface_extent().expect("surface extent exists");
        let stride = usize::try_from(extent.width()).expect("width fits");
        let content = rt.present_frames()[0].content;
        let x = pad + usize::try_from(content.x).expect("content x fits");
        let y = pad + usize::try_from(content.y).expect("content y fits");
        let i = (y * stride + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    let (with_y0, rt0) = painted_after_src_y(0);
    let (with_y1, rt1) = painted_after_src_y(1);
    assert_ne!(
        with_y0, with_y1,
        "wire Y offset must select distinct source rows"
    );
    assert_eq!(
        dest_first_pixel(&with_y0, &rt0),
        [0xFF, 0x00, 0x00, 0xFF],
        "wire Y=0 must select the red row"
    );
    assert_eq!(
        dest_first_pixel(&with_y1, &rt1),
        [0x00, 0x00, 0xFF, 0xFF],
        "wire Y=1 must select the blue row"
    );
}

#[test]
fn compose_out_of_bounds_refused_with_nothing_composed() {
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    // Frozen virtual clock (see above): a refused compose must paint
    // nothing new even when the shard is too slow for a 40ms gap to
    // elapse between the two presents — the old wall-clock `tick()` let
    // the running animation advance mid-test and flaked QG2 shard 2/2.
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // 2x2 frames: source (0,0) 1x1 fits, but the destination (2,0) starts
    // past the right edge.
    rt.handle_pty_bytes(&apc("Ga=c,i=1,r=2,c=1,X=0,Y=0,x=2,y=0,w=1,h=1,C=1,m=0;"));
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(before, after, "refused compose must paint nothing new");
}

#[test]
fn partial_frame_with_base_stores_instead_of_mismatching() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    // 1x1 opaque blue (`f=32`) at offset (1,0), composited onto the red
    // root as base (`c=1`, `X=1` replace): a valid partial frame the old
    // exact-extent ingest refused with `FrameDimensionsMismatch`.
    rt.handle_pty_bytes(&apc("Gf=32,s=1,v=1,a=f,i=1,x=1,y=0,c=1,X=1,m=0;AAD//w=="));
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
}

#[test]
fn pixel_edit_changes_displayed_frame_without_appending() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // `r=1` pixel edit: 1x1 opaque blue (`f=32`) replaced (`X=1`) at the
    // frame origin. The canvas is frame 1 itself (kitty protocol frame
    // edit), so the displayed root repaints with no new frame appended.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=1,x=0,y=0,X=1,m=0;{}",
        blue_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert_eq!(rt.kitty_image_count(), 1);
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(before, after, "edited frame must repaint");
    // Queries are unaffected by the edit.
    rt.take_replies();
    rt.handle_pty_bytes(&apc("Gf=32,a=q,i=1,m=0;"));
    let replies: Vec<Vec<u8>> = rt
        .take_replies()
        .iter()
        .map(|reply| reply.to_vec())
        .collect();
    assert_eq!(replies, vec![b"\x1b_Gi=1;OK\x1b\\".to_vec()]);
}

#[test]
fn pixel_edit_targets_r_not_c_base() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    append_blue_frame(&mut rt);
    // Frozen virtual clock (see above): every present below shares one
    // instant, so repaints come from the edit itself, never from gaps.
    let t0 = Instant::now();
    rt.tick_at(t0);
    let root_shown = rt.headless_rgba().expect("frame must exist");
    // Show frame 2 through client-driven selection (no run needed).
    rt.handle_pty_bytes(&apc("Ga=a,i=1,c=2,m=0;"));
    rt.tick_at(t0);
    let frame2_before = rt.headless_rgba().expect("frame must exist");
    assert_ne!(
        root_shown, frame2_before,
        "frame 2 (blue) must differ from the root (red)"
    );
    // Edit frame 2 with a `c=1` base alongside: the `c=` base governs
    // `r=0` creation only — the edit canvas is frame `r` itself, so
    // frame 2 gains the green pixel while frame 1 stays red.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=2,c=1,x=0,y=0,X=1,m=0;{}",
        green_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
    rt.tick_at(t0);
    let frame2_after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(frame2_before, frame2_after, "frame r=2 must carry the edit");
    rt.handle_pty_bytes(&apc("Ga=a,i=1,c=1,m=0;"));
    rt.tick_at(t0);
    let root_after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(
        root_shown, root_after,
        "the named c=1 base must be untouched by the edit"
    );
}

#[test]
fn edit_missing_frame_refused_with_nothing_recorded() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // Frame 99 is not held: the edit fails closed — no pixels change and
    // (store-first) no truth gap is recorded either.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=99,x=0,y=0,X=1,m=0;{}",
        blue_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(1)
            .map(|anim| anim.frame_count()),
        None,
        "refused edit must record no truth gap"
    );
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(before, after, "refused edit must paint nothing new");
}

#[test]
fn edit_out_of_bounds_refused_with_nothing_changed() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // 2x2 frame: the 1x1 edit at (2,0) starts past the right edge.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=1,x=2,y=0,X=1,m=0;{}",
        blue_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(1)
            .map(|anim| anim.frame_count()),
        None,
        "refused edit must record no truth gap"
    );
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(before, after, "refused edit must paint nothing new");
}

#[test]
fn edit_overcap_refused_with_nothing_stored() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    // 5000x5000 trips the area cap at the pre-check, before any
    // allocation — the per-frame caps bind edits exactly like appends.
    rt.handle_pty_bytes(&apc(
        "Gf=32,s=5000,v=5000,a=f,i=1,r=1,x=0,y=0,X=1,m=0;bm90LWEtcG5n;",
    ));
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(1)
            .map(|anim| anim.frame_count()),
        None,
        "refused edit must record no truth gap"
    );
    // The store stays usable: a valid edit still applies to frame 1.
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=1,x=0,y=0,X=1,m=0;{}",
        blue_1x1_b64()
    )));
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(before, after, "valid edit must apply after a refusal");
}

#[test]
fn transmit_only_edit_with_z_refused_with_nothing_changed() {
    // CTX-1123 preflight fix: transmit-only stores frame 1 with no
    // animation descriptor, so an `r>0` edit carrying nonzero `z` must
    // refuse before any pixel mutation (fail closed: refused truth
    // changes neither pixels nor truth). The displayed root shares the
    // same no-descriptor shape, which makes the stored pixels observable
    // through the headless present for a byte-identical check.
    let mut rt = make_runtime();
    // Displayed 2x2 red on wire 1: stores frame 1 with no descriptor,
    // placed so pixel bytes are observable through the headless present.
    display_root(&mut rt);
    // Transmit-only 2x2 red on wire 7: same no-descriptor shape (pixels
    // held, no descriptor, never placed).
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=2,v=2,a=t,i=7,m=0;{}",
        red_2x2_b64()
    )));
    assert_eq!(rt.kitty_image_count(), 2);
    assert_eq!(rt.kitty_anim_frame_count(7), 1);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(7)
            .map(|anim| anim.frame_count()),
        None
    );
    let t0 = Instant::now();
    rt.tick_at(t0);
    let before = rt.headless_rgba().expect("frame must exist");
    // Both edits carry `z=40`: no descriptor holds frame 1, so both must
    // refuse with nothing stored and nothing recorded.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=7,r=1,x=0,y=0,X=1,z=40,m=0;{}",
        blue_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(7), 1);
    assert_eq!(rt.kitty_image_count(), 2);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(7)
            .map(|anim| anim.frame_count()),
        None,
        "refused edit must record no truth gap"
    );
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=1,x=0,y=0,X=1,z=40,m=0;{}",
        blue_1x1_b64()
    )));
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert_eq!(
        rt.state()
            .kitty_placements()
            .animation(1)
            .map(|anim| anim.frame_count()),
        None,
        "refused edit must record no truth gap"
    );
    rt.tick_at(t0);
    let after = rt.headless_rgba().expect("frame must exist");
    assert_eq!(
        before, after,
        "refused edit must leave pixels byte-identical"
    );
}

#[test]
fn edit_invalidates_raster_cache() {
    let mut rt = make_runtime();
    display_root(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    assert!(
        rt.kitty_raster_misses() >= 1,
        "first present must rasterize frame 1"
    );
    // Printing text forces a present on the same frame 1: a cache hit.
    rt.handle_pty_bytes(b"x");
    rt.tick_at(t0 + Duration::from_millis(1));
    assert!(
        rt.kitty_raster_hits() >= 1,
        "re-presenting frame 1 must hit"
    );
    let misses_warm = rt.kitty_raster_misses();
    let before = rt.headless_rgba().expect("frame must exist");
    // Pixel-edit the displayed root: the next present must re-raster the
    // edited bytes, never serve the stale cached blit.
    rt.handle_pty_bytes(&apc(&format!(
        "Gf=32,s=1,v=1,a=f,i=1,r=1,x=0,y=0,X=1,m=0;{}",
        blue_1x1_b64()
    )));
    rt.tick_at(t0 + Duration::from_millis(2));
    let after = rt.headless_rgba().expect("frame must exist");
    assert_ne!(before, after, "edited frame must repaint, not stale");
    assert!(
        rt.kitty_raster_misses() > misses_warm,
        "edited frame must re-raster its own key"
    );
}

#[test]
fn tick_damages_single_animating_image_exactly_once() {
    // CTX-1122 (S1 follow-up): one animating image must damage once per
    // tick with exactly one generation bump. The pre-fix path damaged
    // truth spans twice (State::advance plus damage_kitty_wire) with
    // duplicate rects and a double bump.
    let mut rt = make_runtime();
    running_two_frame(&mut rt);
    let t0 = Instant::now();
    rt.tick_at(t0);
    let generation = rt.state().generation();
    rt.tick_at(t0 + Duration::from_millis(40));
    assert_eq!(rt.kitty_anim_current_frame(1), 2);
    let bumped = rt.state().generation().saturating_sub(generation);
    assert_eq!(
        bumped, 1,
        "one advancing image must bump generation exactly once per tick"
    );
    let regions = rt.state().damage_since(generation);
    assert!(
        !regions.is_empty(),
        "advancing image must leave damage for the frame"
    );
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
#[cfg(unix)]
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
    let path_bytes = {
        use std::os::unix::ffi::OsStrExt;
        file.as_os_str().as_bytes().to_vec()
    };
    let mut seq = "\x1b_Gf=32,s=2,v=2,a=f,i=1,t=f,m=0;"
        .to_string()
        .into_bytes();
    seq.extend_from_slice(base64_encode(&path_bytes).as_bytes());
    seq.extend_from_slice(b"\x1b\\");
    rt.handle_pty_bytes(&seq);
    pump_settled(&mut rt);
    assert_eq!(rt.kitty_anim_frame_count(1), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg(windows)]
fn file_backed_frame_denied_on_windows_keeps_file() {
    // Windows has no `O_NOFOLLOW`/`/dev/shm` equivalent in this slice:
    // the `t=f` frame read fails closed with `UnsupportedPlatform`
    // before any I/O — nothing stored, file kept (mirrors the S3
    // `file_valid_denied_on_windows_keeps_file` verdict).
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bitty-ctx1111-anim-w-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir must build");
    let file = dir.join("frame.bin");
    std::fs::write(&file, blue_2x2()).expect("frame fixture must write");
    let mut rt = make_runtime();
    display_root(&mut rt);
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
    assert_eq!(rt.kitty_anim_frame_count(1), 1);
    assert!(file.is_file(), "denied t=f frame must not delete");
    let _ = std::fs::remove_dir_all(&dir);
}
