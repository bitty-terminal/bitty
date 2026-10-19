//! CTX-1368 / issue #2261: multi-line paste confirmation frame regression.
//!
//! Shells repaint a bracketed paste in two passes: the inserted text first,
//! then a full command-line redraw that returns to the start of the first
//! line with `CUF`/`CUB` and overwrites the same cells. Bitty's cursor move
//! used to count a wide pair's trailing spacer as an extra column, so the
//! redraw started four columns to the right and left the first token on
//! screen (`abc abc`). Delivery was always exactly once; the defect was the
//! frame's grid/cursor arithmetic.
//!
//! These tests replay that shape deterministically: the paste is fed in
//! chunks through the real runtime PTY path and every frame is presented.
//! They fail before the cursor fix (the first row contains the token twice)
//! and pass after it.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

fn runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("headless runtime")
}

fn row_text(rt: &Runtime, row: usize) -> String {
    let snap = rt.snapshot();
    let width = snap.width;
    if row >= snap.height {
        return String::new();
    }
    let mut out = String::new();
    for cell in &snap.cells[row * width..(row + 1) * width] {
        if cell.spacer {
            continue;
        }
        out.push(cell.glyph);
        for mark in cell.zerowidth.iter() {
            out.push(*mark);
        }
    }
    out.trim_end().to_string()
}

/// The shell's two-pass bracketed-paste paint, recorded from a live fish
/// session: a 35-column prompt, the first insertion pass, then the command
/// line redraw after `CUP`/`CUB` back to column 35.
fn paste_redraw_chunks() -> Vec<Vec<u8>> {
    let mut prompt = format!("{}> ", " ".repeat(33)).into_bytes();
    prompt.extend_from_slice(
        b"\x1b[?2004h\x1b[?2031h\x1b[>4;1m\x1b=\x1b]133;A\x07\x1b]133;B\x07\x1b[K\r\x1b[35C",
    );

    let mut first_paint = Vec::new();
    first_paint.extend_from_slice(b"\x1b[?2004l\x1b[?2031l\x1b[>4;0m\x1b>");
    first_paint.extend_from_slice(b"\x1b[?2004h\x1b[?2031h\x1b[>4;1m\x1b=");
    first_paint.extend_from_slice("abc\r\n".as_bytes());
    first_paint.extend_from_slice(b"\x1b[K\x1b[35C\xe4\xbd\xa0\xe5\xa5\xbd\r\n");
    first_paint.extend_from_slice(b"\x1b[K\x1b[35C\xc3\xa9\r\n");
    first_paint.extend_from_slice(
        b"\x1b[K\x1b[35C\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7\xe2\x80\x8d\xf0\x9f\x91\xa6\r\x1b[43C",
    );

    let mut redraw = Vec::new();
    redraw.extend_from_slice(b"\x1b[A\x1b[A\x1b[A\x1b[8D");
    redraw.extend_from_slice("abc\r\n".as_bytes());
    redraw.extend_from_slice(b"\x1b[35C\xe4\xbd\xa0\xe5\xa5\xbd\r\n");
    redraw.extend_from_slice(b"\x1b[35C\xc3\xa9\r\n");
    redraw.extend_from_slice(
        b"\x1b[35C\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7\xe2\x80\x8d\xf0\x9f\x91\xa6\r\x1b[43C",
    );

    vec![prompt, first_paint, redraw]
}

#[test]
fn multi_line_paste_redraw_paints_first_token_once() {
    let mut rt = runtime();
    let chunks = paste_redraw_chunks();
    for chunk in &chunks {
        rt.handle_pty_bytes(chunk);
        let _ = rt.tick();
    }

    assert_eq!(
        row_text(&rt, 0),
        "                                 > abc",
        "the redraw must overwrite the first paint, not append a duplicate"
    );

    // Damage oracle: replaying the exact same bytes as one full frame must
    // produce byte-identical pixels. A duplicated or stale token would make
    // the incremental surface diverge here.
    let mut full = runtime();
    full.handle_pty_bytes(&chunks.concat());
    let _ = full.tick();
    assert_eq!(
        rt.headless_rgba(),
        full.headless_rgba(),
        "incremental frames must equal one full repaint"
    );
}

#[test]
fn paste_banner_dismissal_repaints_the_underlying_row() {
    // Put the input line on the last row, where the confirmation pill is
    // painted. Arming and dismissing must leave no pixels behind.
    let mut armed = runtime();
    armed.handle_pty_bytes(b"\x1b[24;1H> ready");
    let _ = armed.tick();

    assert!(armed.request_paste("one\ntwo".to_string()));
    let _ = armed.tick().expect("banner frame presents");
    assert!(armed.has_pending_paste());
    assert!(armed.confirm_pending_paste(true));
    let _ = armed.tick().expect("dismissal frame presents");

    // Reference: identical PTY state, banner never armed.
    let mut plain = runtime();
    plain.handle_pty_bytes(b"\x1b[24;1H> ready");
    let _ = plain.tick();

    assert_eq!(
        armed.headless_rgba(),
        plain.headless_rgba(),
        "dismissing the banner must repaint every cell it covered"
    );
}
