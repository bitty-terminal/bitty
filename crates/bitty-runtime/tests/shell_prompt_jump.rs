//! Shell prompt jump over anchored `OSC 133` marks (M1-18, CTX-0665).
//!
//! Headless, deterministic: prompts are injected as real `OSC 133;A`
//! escape sequences through the PTY path (the same bytes a shell
//! integration script emits), then `shell_goto_prev_prompt` /
//! `shell_goto_next_prompt` move the focused viewport between the
//! resolved buffer rows. No PTY input is produced by jumping.
//!
//! CTX-0952 (issue #1670) adds full-session replay: `command` feeds the
//! complete `A`/`B`/`C`/`D` traffic shape a real shell integration emits
//! (prompt, input, output, exit status), and the tests cover the
//! `jump_to_prompt` direction API plus `select_command_output` over it.

#![forbid(unsafe_code)]

use bitty_runtime::Runtime;
use bitty_runtime::runtime::shell_jump::PromptDirection;

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

/// One full shell-integration command block: prompt mark + prompt text,
/// input mark + command line, output mark + output lines, done mark with
/// exit status (CTX-0952). This is the byte shape bash/zsh/fish
/// integrations emit around every command.
fn command(rt: &mut Runtime, cmd: &str, output: &[&str], exit: u32) {
    feed(rt, "\u{1b}]133;A\u{7}");
    feed(rt, "prompt$ ");
    feed(rt, "\u{1b}]133;B\u{7}");
    feed(rt, cmd);
    feed(rt, "\r\n");
    feed(rt, "\u{1b}]133;C\u{7}");
    for line in output {
        feed(rt, line);
        feed(rt, "\r\n");
    }
    feed(rt, &format!("\u{1b}]133;D;{exit}\u{7}"));
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

#[test]
fn jump_direction_api_travels_full_session_traffic() {
    // CTX-0952 acceptance: jumping works over real shell traffic — every
    // command carries the full `A`/`B`/`C`/`D` mark set plus exit status.
    let mut rt = make_runtime();
    let h = rt.state().height();
    for i in 0..(h + 4) {
        command(
            &mut rt,
            &format!("cmd{i:02}"),
            &[&format!("out{i:02}-a")],
            0,
        );
    }
    feed(&mut rt, "\u{1b}]133;A\u{7}");
    feed(&mut rt, "prompt$ ");
    assert!(rt.state().scrollback_len() > 0);
    // Empty marks are a no-op for the direction API too: from live there
    // is nothing below.
    assert!(!rt.jump_to_prompt(PromptDirection::Next));
    assert_eq!(view_offset(&rt), 0);
    // Walk two prompts up through output-bearing blocks, then back down.
    assert!(rt.jump_to_prompt(PromptDirection::Prev));
    let first = view_start(&rt);
    assert!(rt.jump_to_prompt(PromptDirection::Prev));
    let second = view_start(&rt);
    assert!(second < first, "second jump moves further up");
    assert!(rt.jump_to_prompt(PromptDirection::Next));
    assert_eq!(view_start(&rt), first, "next returns to the newer prompt");
    rt.drain_pending_input();
    assert_eq!(rt.pending_input_len(), 0, "jumping emits no PTY input");
}

#[test]
fn select_command_output_selects_exactly_last_output() {
    // CTX-0952 acceptance: select covers exactly the last command's
    // output lines — no prompt text, no older output.
    let mut rt = make_runtime();
    command(&mut rt, "first", &["old-one", "old-two"], 0);
    command(&mut rt, "second", &["new-a", "new-b"], 1);
    feed(&mut rt, "\u{1b}]133;A\u{7}");
    feed(&mut rt, "prompt$ ");
    assert!(rt.select_command_output());
    assert_eq!(rt.selection_text().as_deref(), Some("new-a\nnew-b"));
    let owner = rt.focused_view().expect("focused view");
    assert_eq!(rt.selection_owner(), Some(owner));
    // A zero-byte command after it selects nothing and leaves the live
    // selection untouched (empty marks are a no-op).
    command(&mut rt, "true", &[], 0);
    feed(&mut rt, "\u{1b}]133;A\u{7}");
    feed(&mut rt, "prompt$ ");
    assert!(!rt.select_command_output());
    assert_eq!(rt.selection_text().as_deref(), Some("new-a\nnew-b"));
}

#[test]
fn select_command_output_absent_marks_noop() {
    let mut rt = make_runtime();
    feed(&mut rt, "plain output, no integration\r\nmore lines\r\n");
    assert!(!rt.select_command_output());
    assert!(!rt.has_selection());
    assert_eq!(rt.selection_text(), None);
    assert_eq!(
        view_offset(&rt),
        0,
        "failed select never moves the viewport"
    );
    // The direction API agrees: no marks, no jumps, no panic.
    assert!(!rt.jump_to_prompt(PromptDirection::Prev));
    assert!(!rt.jump_to_prompt(PromptDirection::Next));
    assert_eq!(view_offset(&rt), 0);
}

#[test]
fn select_command_output_scrolled_history_noop_but_jump_reaches() {
    // Selections address live cells only: output that scrolled into
    // history fails closed instead of clamping to the wrong lines, while
    // prompt jump still reaches the retained zone (retention proof).
    let mut rt = make_runtime();
    let h = rt.state().height();
    command(&mut rt, "first", &["scrolled-a", "scrolled-b"], 0);
    // Unmarked traffic scrolls the marked output into history without
    // adding newer marks, so the last range still resolves — as history.
    for i in 0..(h + 4) {
        feed(&mut rt, &format!("plain filler line {i:02}\r\n"));
    }
    let (start, end) = rt
        .state()
        .last_command_output_rows()
        .expect("marked range survives scrolling");
    assert!(end < rt.state().scrollback_len(), "range is now history");
    assert_eq!(start + 1, end);
    assert!(
        !rt.select_command_output(),
        "history output is not selectable"
    );
    assert!(!rt.has_selection());
    // Jumping still walks the retained prompt zones across scrollback.
    assert!(rt.jump_to_prompt(PromptDirection::Prev));
    assert!(view_offset(&rt) > 0);
}
