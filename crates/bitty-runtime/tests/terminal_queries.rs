//! Terminal query answers for CTX-0146 (Issue #238).
//!
//! Headless `Runtime::handle_pty_bytes -> take_replies` tests proving every
//! standard query fish/tide sends at startup gets a well-formed bounded
//! reply with bitty's true capabilities: input escape sequence goes in,
//! exact reply bytes come out. No filesystem, no network, no PTY needed.
//!
//! Query/answer shapes follow xterm `ctlseqs.txt` and the alacritty
//! secondary-DA shape (read-only references per DEC-0004); values are
//! bitty's own (`TERM=xterm-256color`, 256 colors, direct color, live mode
//! register).

use bitty_runtime::Runtime;

fn replies_text(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

#[test]
fn primary_da_answers_vt102_baseline() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[c");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?6c".to_vec()]);
    // Explicit Ps 0 is the same request.
    rt.handle_pty_bytes(b"\x1b[0c");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?6c".to_vec()]);
}

#[test]
fn legacy_decid_answers_primary_da() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1bZ");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?6c".to_vec()]);
}

#[test]
fn secondary_da_reports_versioned_identity() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[>c");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[>0;1;1c".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[>0c");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[>0;1;1c".to_vec()]);
}

#[test]
fn secondary_da_echo_does_not_retrigger() {
    // An echoing PTY (`cat`) reflects our own reply; its extra params must
    // not be mistaken for a fresh request (reply-echo loop guard).
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[>0;1;1c");
    assert!(replies_text(&mut rt).is_empty(), "echo must stay silent");
}

#[test]
fn xterm_version_names_bitty() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[>q");
    let replies = replies_text(&mut rt);
    assert_eq!(replies.len(), 1);
    let expected = format!("\x1bP>|Bitty {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes();
    assert_eq!(replies[0], expected);
    rt.handle_pty_bytes(b"\x1b[>0q");
    assert_eq!(replies_text(&mut rt), vec![expected]);
}

#[test]
fn decrqm_private_reports_live_modes() {
    let mut rt = Runtime::with_defaults().expect("build");
    // Bracketed paste off by default -> reset.
    rt.handle_pty_bytes(b"\x1b[?2004$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?2004;2$y".to_vec()]);
    // Enable, then it reports set.
    rt.handle_pty_bytes(b"\x1b[?2004h");
    assert!(replies_text(&mut rt).is_empty());
    rt.handle_pty_bytes(b"\x1b[?2004$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?2004;1$y".to_vec()]);
    // Power-on defaults: autowrap + cursor visible set, mouse off.
    rt.handle_pty_bytes(b"\x1b[?7$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?7;1$y".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[?25$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?25;1$y".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[?1000$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?1000;2$y".to_vec()]);
    // Unknown private mode -> well-formed negative, never silence.
    rt.handle_pty_bytes(b"\x1b[?9999$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?9999;0$y".to_vec()]);
    // Action-only pseudo-mode has no persistent state -> not recognized.
    rt.handle_pty_bytes(b"\x1b[?1048$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?1048;0$y".to_vec()]);
}

#[test]
fn decrqm_ansi_reports_insert_and_lnm() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[4$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[4;2$y".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[20$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[20;2$y".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[12$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[12;0$y".to_vec()]);
}

#[test]
fn decrqm_kitty_flags_follow_negotiation() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[?7727$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?7727;2$y".to_vec()]);
    rt.handle_pty_bytes(b"\x1b[?7727:1:2:5h");
    assert!(replies_text(&mut rt).is_empty());
    rt.handle_pty_bytes(b"\x1b[?7727$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?7727;1$y".to_vec()]);
}

#[test]
fn decrqm_split_across_reads_answers_once() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[?200");
    assert!(
        replies_text(&mut rt).is_empty(),
        "partial query stays silent"
    );
    rt.handle_pty_bytes(b"4$p");
    assert_eq!(replies_text(&mut rt), vec![b"\x1b[?2004;2$y".to_vec()]);
    assert!(replies_text(&mut rt).is_empty(), "no duplicate answer");
}

#[test]
fn xtgettcap_answers_terminal_name_and_colors() {
    let mut rt = Runtime::with_defaults().expect("build");
    // TN (544E) -> xterm-256color hex.
    rt.handle_pty_bytes(b"\x1bP+q544E\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1bP1+r544E=787465726D2D323536636F6C6F72\x1b\\".to_vec()]
    );
    // Co (436F) -> "256" hex.
    rt.handle_pty_bytes(b"\x1bP+q436F\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1bP1+r436F=323536\x1b\\".to_vec()]
    );
    // RGB -> uniform 8-bit direct color.
    rt.handle_pty_bytes(b"\x1bP+q524742\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1bP1+r524742=38\x1b\\".to_vec()]
    );
    // Unknown capability -> well-formed negative, never silence.
    rt.handle_pty_bytes(b"\x1bP+q5A5A\x1b\\");
    assert_eq!(replies_text(&mut rt), vec![b"\x1bP0+r\x1b\\".to_vec()]);
    // Malformed hex -> negative.
    rt.handle_pty_bytes(b"\x1bP+q544\x1b\\");
    assert_eq!(replies_text(&mut rt), vec![b"\x1bP0+r\x1b\\".to_vec()]);
    // Lowercase hex decodes too (TN as 544e).
    rt.handle_pty_bytes(b"\x1bP+q544e\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1bP1+r544e=787465726D2D323536636F6C6F72\x1b\\".to_vec()]
    );
}

#[test]
fn xtgettcap_multi_name_query_replies_in_order() {
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1bP+q544E;436F\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1bP1+r544E=787465726D2D323536636F6C6F72;436F=323536\x1b\\".to_vec()]
    );
}

#[test]
fn vttest_style_mixed_stream_keeps_grid_and_reply_order() {
    // vttest-shaped stream: cursor addressing, DSR, printable text, mode
    // set, then a private query. Grid effects and replies stay ordered and
    // queries never disturb cell content.
    let mut rt = Runtime::with_defaults().expect("build");
    rt.handle_pty_bytes(b"\x1b[H\x1b[5nABC\x1b[?25l\x1b[?25$p");
    let replies = replies_text(&mut rt);
    assert_eq!(
        replies,
        vec![b"\x1b[0n".to_vec(), b"\x1b[?25;2$y".to_vec()],
        "DSR reply first, DECRPM second"
    );
    let snap = rt.snapshot();
    let row: String = snap.cells.iter().take(3).map(|c| c.glyph).collect();
    assert_eq!(row, "ABC");
    assert!(!snap.cursor.visible, "cursor hidden by ?25l");
}

#[test]
fn query_flood_stays_within_reply_cap() {
    let mut rt = Runtime::with_defaults().expect("build");
    for _ in 0..1500 {
        rt.handle_pty_bytes(b"\x1b[?1$p");
    }
    assert!(rt.replies_overflowed(), "flood must trip the 4 KiB cap");
    let total: usize = rt.take_replies().iter().map(|b| b.len()).sum();
    assert!(total <= 4096, "bounded replies, got {total}");
    assert!(!rt.replies_overflowed(), "flag resets after drain");
}
