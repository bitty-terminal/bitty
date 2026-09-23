//! Selection invalidation on erase (issue #1337).
//!
//! The selection highlight overlay paints from live-grid coordinates on
//! every present. When `clear` erases the grid, a kept selection keeps
//! painting its rects over the erased cells: a persistent block where the
//! old styled output was, even though grid truth is clean. Fail closed:
//! every ED mode that erases live-grid cells (Below/Above/All) drops the
//! selection, exactly like FullReset already did. ED 3 (scrollback-only)
//! leaves the live grid intact, so live-grid selections survive it.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime must build")
}

fn pristine_frame(rt: &mut Runtime) -> Vec<u8> {
    rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None, "idle runtime must not present");
    rt.headless_rgba().expect("rgba after first present")
}

/// Bottom-left styled block plus a selection covering it, both presented.
fn select_styled_block(rt: &mut Runtime) {
    let (w, h) = {
        let snap = rt.state().snapshot();
        (snap.width, snap.height)
    };
    assert!(w >= 8 && h >= 2);
    rt.handle_pty_bytes(format!("\x1b[{h};1H").as_bytes());
    rt.handle_pty_bytes(b"\x1b[38;5;2;48;5;13mSELBLOCK\x1b[0m");
    rt.tick().expect("styled output must present");
    rt.start_selection(CellPos::new((h - 1) as u16, 0));
    rt.end_selection(CellPos::new((h - 1) as u16, 7));
    assert!(rt.has_selection(), "selection must exist");
    rt.tick().expect("highlight must present");
}

#[test]
fn ed_all_clears_selection_and_leaves_pristine_pixels() {
    let mut rt = make_runtime();
    let pristine = pristine_frame(&mut rt);
    select_styled_block(&mut rt);
    assert_ne!(
        rt.headless_rgba().expect("rgba"),
        pristine,
        "highlight must change pixels"
    );

    // The exact bytes `clear` emits on xterm-256color.
    rt.handle_pty_bytes(b"\x1b[H\x1b[2J\x1b[3J");
    rt.tick().expect("clear must present");

    assert!(
        !rt.has_selection(),
        "ED All must drop the selection (fail-closed)"
    );
    assert_eq!(
        rt.headless_rgba().expect("rgba"),
        pristine,
        "post-clear frame must equal the pristine frame: no stale highlight"
    );
}

#[test]
fn ed_below_and_above_clear_selection() {
    for seq in [b"\x1b[J".as_slice(), b"\x1b[1J".as_slice()] {
        let mut rt = make_runtime();
        select_styled_block(&mut rt);
        rt.handle_pty_bytes(seq);
        rt.tick();
        assert!(
            !rt.has_selection(),
            "partial grid erase must drop the selection: {seq:?}"
        );
    }
}

#[test]
fn ed_scrollback_keeps_live_selection() {
    // ED 3 clears history only; the live grid (and its selection) survives.
    let mut rt = make_runtime();
    select_styled_block(&mut rt);
    rt.handle_pty_bytes(b"\x1b[3J");
    rt.tick();
    assert!(
        rt.has_selection(),
        "scrollback-only clear must keep the live selection (CTX-0060)"
    );
}
