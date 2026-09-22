//! Shell prompt jump over anchored `OSC 133` marks (M1-18, CTX-0665).
//!
//! Headless, deterministic: prompts are injected as real `OSC 133;A`
//! escape sequences through the PTY path (the same bytes a shell
//! integration script emits), then `shell_goto_prev_prompt` /
//! `shell_goto_next_prompt` move the focused viewport between the
//! resolved buffer rows. No PTY input is produced by jumping.

#![forbid(unsafe_code)]

use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed(rt: &mut Runtime, text: &str) {
    rt.handle_pty_bytes(text.as_bytes());
}

/// One prompt block: the mark a shell emits at its prompt, then the
/// command line itself.
fn prompt(rt: &mut Runtime, cmd: &str) {
    feed(rt, "\u{1b}]133;A\u{7}");
    feed(rt, cmd);
    feed(rt, "\r\n");
}

fn view_offset(rt: &Runtime) -> usize {
    let vid = rt.focused_view().expect("focused view");
    rt.layout().find_leaf(vid).expect("leaf").scroll_offset()
}

fn view_start(rt: &Runtime) -> usize {
    let vid = rt.focused_view().expect("focused view");
    let view = rt.layout().find_leaf(vid).expect("leaf");
    let total = rt.state().scrollback_len() + rt.state().height();
    let rows = view.rows() as usize;
    total
        .saturating_sub(rows)
        .saturating_sub(view.scroll_offset())
}

#[test]
fn no_marks_jump_fails_closed() {
    let mut rt = make_runtime();
    feed(&mut rt, "plain output, no integration\n");
    assert!(!rt.shell_goto_prev_prompt());
    assert!(!rt.shell_goto_next_prompt());
    assert_eq!(view_offset(&rt), 0);
}

#[test]
fn prev_jump_from_live_lands_on_last_prompt() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    for i in 0..(h + 4) {
        prompt(&mut rt, &format!("cmd{i:02}"));
    }
    feed(&mut rt, "trailing output");
    assert!(rt.state().scrollback_len() > 0);
    // Reference is the live viewport top: prompts at or below it are
    // already on screen, so prev skips them.
    let live_start = view_start(&rt);
    let expect = rt
        .state()
        .prev_prompt_buffer_row(live_start)
        .expect("a prompt above live");
    assert!(rt.shell_goto_prev_prompt());
    assert_eq!(view_start(&rt), expect, "target lands at viewport top");
    assert!(view_offset(&rt) > 0);
}

#[test]
fn prev_then_next_hops_between_prompts() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    for i in 0..(h + 4) {
        prompt(&mut rt, &format!("cmd{i:02}"));
    }
    feed(&mut rt, "trailing output");
    // Walk two prompts up, then one back down.
    assert!(rt.shell_goto_prev_prompt());
    let first = view_start(&rt);
    assert!(rt.shell_goto_prev_prompt());
    let second = view_start(&rt);
    assert!(second < first, "second jump moves further up");
    assert!(rt.shell_goto_next_prompt());
    assert_eq!(view_start(&rt), first, "next returns to the newer prompt");
    // One more next reaches the newest prompt and returns toward live.
    let live_start = {
        let total = rt.state().scrollback_len() + rt.state().height();
        let vid = rt.focused_view().expect("focused view");
        let rows = rt.layout().find_leaf(vid).expect("leaf").rows() as usize;
        total.saturating_sub(rows)
    };
    assert!(rt.shell_goto_next_prompt());
    assert_eq!(view_start(&rt), live_start);
    // Nothing below the newest prompt: fail closed.
    assert!(!rt.shell_goto_next_prompt());
    assert_eq!(view_start(&rt), live_start);
}

#[test]
fn next_from_live_fails_closed_prev_reaches() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    for i in 0..(h + 2) {
        prompt(&mut rt, &format!("cmd{i:02}"));
    }
    // Live viewport: nothing below the top row, so next fails; prev
    // reaches the most recent prompt above.
    assert!(!rt.shell_goto_next_prompt());
    assert_eq!(view_offset(&rt), 0);
    assert!(rt.shell_goto_prev_prompt());
    assert!(view_offset(&rt) > 0);
}

#[test]
fn jump_produces_no_pty_input_and_is_deterministic() {
    let mut rt = make_runtime();
    let h = rt.state().height();
    for i in 0..(h + 4) {
        prompt(&mut rt, &format!("cmd{i:02}"));
    }
    rt.drain_pending_input();
    assert!(rt.shell_goto_prev_prompt());
    assert!(rt.shell_goto_prev_prompt());
    let offset = view_offset(&rt);
    assert_eq!(rt.pending_input_len(), 0);
    // Same jumps from the same state land identically twice.
    let mut rt2 = make_runtime();
    for i in 0..(h + 4) {
        prompt(&mut rt2, &format!("cmd{i:02}"));
    }
    assert!(rt2.shell_goto_prev_prompt());
    assert!(rt2.shell_goto_prev_prompt());
    assert_eq!(view_offset(&rt2), offset);
    assert_eq!(rt2.pending_input_len(), 0);
}
