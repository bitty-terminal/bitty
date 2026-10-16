//! #2133: TUI exit must leave the main scrollback untouched.
//!
//! A btop-shaped alternate-screen cycle (enter, full-screen scrolling
//! draws, exit) must not capture alt-screen rows into primary history.
//! The stray blank lines in the issue were alt UI padding rows leaking
//! through the scroll-up capture path; the viewport looked right while
//! scrollback filled with stale TUI junk.

use bitty_runtime::Runtime;

fn scrollback_len(rt: &Runtime) -> usize {
    rt.state().scrollback_len()
}

#[test]
fn tui_cycle_preserves_scrollback_and_grid() {
    let mut rt = Runtime::with_defaults().expect("defaults must build");
    // Pre-TUI shell history with scrollback (30 lines on a 24-row grid).
    let mut pre = String::new();
    for i in 1..=30 {
        pre.push_str(&format!("content-line-{i:02}\r\n"));
    }
    rt.handle_pty_bytes(pre.as_bytes());
    assert!(rt.tick().is_some(), "pre-TUI frame must present");
    let sb_before = scrollback_len(&rt);
    assert!(sb_before > 0, "fixture needs scrollback to guard");
    let cells_before: Vec<char> = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
    let pixels_before = rt.headless_rgba().expect("pre-TUI pixels must render");

    // btop-shaped cycle: 1049 enter, full-screen CUP draws that scroll the
    // alt grid past its bottom, 1049 leave.
    let mut tui = Vec::new();
    tui.extend_from_slice(b"\x1b[?1049h\x1b[2J");
    for row in 1..=50u16 {
        tui.extend_from_slice(format!("\x1b[{row};1Halt-ui-row-{row:02}\r\n").as_bytes());
    }
    tui.extend_from_slice(b"\x1b[?1049l");
    rt.handle_pty_bytes(&tui);
    for _ in 0..4 {
        if rt.tick().is_none() {
            break;
        }
    }

    assert_eq!(
        scrollback_len(&rt),
        sb_before,
        "alt-screen rows must be discarded, never captured into scrollback"
    );
    let cells_after: Vec<char> = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
    assert_eq!(
        cells_after, cells_before,
        "main grid must be byte-identical across the TUI cycle"
    );
    let pixels_after = rt.headless_rgba().expect("post-TUI pixels must render");
    assert_eq!(
        pixels_after, pixels_before,
        "presented frame must be pixel-identical: no stray blank lines"
    );
    assert!(
        rt.state().check_invariants().is_ok(),
        "state invariants must hold after the TUI cycle"
    );
}
