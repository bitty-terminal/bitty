//! CTX-0393: session save/restore — atomic persistence, bounded restore,
//! fail-closed fallback, safe-mode skip.
//!
//! TDD red slice: exercises the planned `bitty_runtime::runtime::session`
//! API (capture/encode/decode/apply, atomic file save, XDG state helpers).
//! These tests fail closed (clean start, warning, never panic) on corrupt or
//! oversize input and never log session contents.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use bitty_runtime::runtime::session::{
    MAX_SESSION_FILE_BYTES, MAX_SESSION_SCROLLBACK_LINES_PER_PANE, decode_session, encode_session,
    session_file_for, state_home_for,
};
use bitty_runtime::{Focus, LayoutNode, Runtime, SplitAxis, View, ViewId};

/// Unique scratch dir per test (parallel-safe: pid + tag).
fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bitty-session-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ))
}

/// Feeds printable lines so scrollback accumulates on the primary grid.
fn feed_lines(rt: &mut Runtime, lines: usize) {
    for i in 0..lines {
        rt.handle_pty_bytes(format!("line {i:03}\r\n").as_bytes());
    }
}

#[test]
fn snapshot_encode_decode_round_trip_preserves_layout_scrollback_cwd() {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    feed_lines(&mut rt, 40);
    assert!(rt.state().scrollback_len() > 0, "must have scrollback");
    rt.handle_pty_bytes(b"\x1b]7;file:///tmp\x1b\\");
    assert_eq!(rt.state().cwd_report(), Some("file:///tmp"));

    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(layout);
    rt.focus_mut().set(ViewId::new(2));

    let snap = rt.capture_session_snapshot();
    let bytes = encode_session(&snap).expect("encode bounded snapshot");
    assert!(bytes.len() <= MAX_SESSION_FILE_BYTES);
    let back = decode_session(&bytes).expect("decode own encoding");
    assert_eq!(snap, back, "round trip must preserve everything");
}

#[test]
fn apply_restores_layout_focus_and_primary_scrollback() {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    feed_lines(&mut rt, 30);
    let before: Vec<String> = rt
        .state()
        .scrollback()
        .map(|line| {
            line.cells
                .iter()
                .filter(|c| !c.spacer)
                .map(|c| c.glyph)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    assert!(!before.is_empty());

    let snap = rt.capture_session_snapshot();
    let bytes = encode_session(&snap).expect("encode");
    let back = decode_session(&bytes).expect("decode");

    let mut fresh = Runtime::with_defaults().expect("defaults build");
    let summary = fresh.apply_session_snapshot(&back).expect("apply valid");
    assert_eq!(summary.workspaces, 1);
    assert_eq!(fresh.layout().leaf_ids(), vec![ViewId::new(1)]);
    assert_eq!(fresh.focus().focused(), Some(ViewId::new(1)));

    let after: Vec<String> = fresh
        .state()
        .scrollback()
        .map(|line| {
            line.cells
                .iter()
                .filter(|c| !c.spacer)
                .map(|c| c.glyph)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    assert_eq!(before, after, "primary scrollback must rehydrate");
    // Focus type is exercised so the import stays live if layout asserts move.
    let _ = Focus::with_focus(ViewId::new(1));
}

#[test]
fn corrupt_file_falls_back_to_clean_start() {
    let dir = scratch_dir("corrupt");
    let path = dir.join("session");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(&path, b"definitely not a session file\xff\xfe\n").expect("write garbage");

    let mut rt = Runtime::with_defaults().expect("defaults build");
    let before = rt.layout().clone();
    let err = rt
        .load_session_from_path(&path)
        .expect_err("corrupt must fail");
    assert!(
        !format!("{err}").contains("definitely"),
        "errors must never echo file contents"
    );
    assert_eq!(
        rt.layout(),
        &before,
        "failed restore must leave a clean start untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_temp_write_is_ignored_and_save_cleans_temp() {
    let dir = scratch_dir("atomic");
    let path = dir.join("session");
    let mut rt = Runtime::with_defaults().expect("defaults build");
    feed_lines(&mut rt, 10);
    rt.save_session_to_path(&path).expect("save works");

    // Simulate a crashed save: temp file present, final untouched.
    std::fs::write(dir.join("session.tmp.12345"), b"partial garbage").expect("temp write");
    let mut loaded = Runtime::with_defaults().expect("defaults build");
    loaded
        .load_session_from_path(&path)
        .expect("temp files must be ignored");

    // No temp files may survive a save.
    rt.save_session_to_path(&path).expect("re-save works");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "save must clean temp files");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safe_mode_never_reads_session_state() {
    let dir = scratch_dir("safe");
    let dir_str = dir.to_string_lossy().into_owned();
    let path = session_file_for(Some(dir_str.as_str()), None).expect("path resolves");
    let mut rt = Runtime::with_defaults().expect("defaults build");
    feed_lines(&mut rt, 10);
    rt.save_session_to_path(&path).expect("save works");

    // Even an unreadable file must not matter: safe mode touches nothing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    }
    // Point the default-path lookup at the scratch dir via XDG env injection.
    let probed = session_file_for(Some(dir_str.as_str()), None).expect("path resolves");
    assert_eq!(probed, path);

    let mut fresh = Runtime::with_defaults().expect("defaults build");
    let outcome = fresh.restore_session_on_startup_with_env(true, Some(dir_str.as_str()), None);
    assert!(
        matches!(
            outcome,
            bitty_runtime::runtime::session::SessionStartupOutcome::SkippedSafeMode
        ),
        "safe mode must skip restore, got {outcome:?}"
    );
    assert_eq!(fresh.session_pending_len(), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oversize_file_is_rejected_fail_closed() {
    let dir = scratch_dir("oversize");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("session");
    let big = vec![b'x'; MAX_SESSION_FILE_BYTES + 1];
    std::fs::write(&path, &big).expect("write big");

    let mut rt = Runtime::with_defaults().expect("defaults build");
    let before = rt.layout().clone();
    let err = rt
        .load_session_from_path(&path)
        .expect_err("oversize must fail");
    assert!(matches!(
        err,
        bitty_runtime::runtime::session::SessionError::TooLarge { .. }
    ));
    assert_eq!(rt.layout(), &before, "oversize must leave clean start");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pane_scrollback_beyond_cap_is_rejected_on_decode() {
    // Hand-crafted file: the pane header claims more lines than the
    // per-pane restore cap. Encode-side validation would reject this
    // before writing; decode must reject it fail-closed too.
    let mut raw = String::from(
        "bitty-session v1\nworkspaces 1 active 0 mru 0\nworkspace 1 none\nname ws1\nlayout (leaf 1 80 24)\n",
    );
    raw.push_str(&format!(
        "pane 1 80 24 {} 0\n",
        MAX_SESSION_SCROLLBACK_LINES_PER_PANE + 1
    ));
    for i in 0..MAX_SESSION_SCROLLBACK_LINES_PER_PANE + 1 {
        raw.push_str(&format!("overflow line {i}\n"));
    }
    raw.push_str("end-pane\nend-workspace\nend-session\n");
    let err = decode_session(raw.as_bytes()).expect_err("over-cap lines must fail");
    assert!(
        !format!("{err}").contains("overflow"),
        "errors must never echo session contents"
    );
}

#[test]
fn xdg_state_helpers_derive_session_paths_without_hardcoded_hosts() {
    // Explicit XDG root wins.
    assert_eq!(
        state_home_for(Some("/tmp/xdg-state"), Some("/home/user")),
        Some(PathBuf::from("/tmp/xdg-state"))
    );
    // Empty XDG falls back to HOME (XDG base-dir spec).
    assert_eq!(
        state_home_for(Some("  "), Some("/home/user")),
        Some(PathBuf::from("/home/user/.local/state"))
    );
    // Nothing usable yields None (fail-closed, no panic).
    assert_eq!(state_home_for(None, None), None);
    assert_eq!(state_home_for(Some(""), Some(" ")), None);

    let file = session_file_for(Some("/tmp/xdg-state"), None).expect("file resolves");
    assert_eq!(file, PathBuf::from("/tmp/xdg-state/bitty/sessions/session"));
}
