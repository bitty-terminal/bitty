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
    MAX_SESSION_CWD_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_SCROLLBACK_LINES_PER_PANE,
    PaneSnapshot, SESSION_FORMAT_VERSION, SessionError, SessionSnapshot, WorkspaceSnapshot,
    decode_session, encode_session, session_file_for, state_home_for,
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

/// P2-1: the secret-capable session file must be owner-only from the first
/// byte — no crash window at `0644`.
#[cfg(unix)]
#[test]
fn saved_file_is_created_mode_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch_dir("mode");
    let path = dir.join("session");
    let rt = Runtime::with_defaults().expect("defaults build");
    rt.save_session_to_path(&path).expect("save works");
    let mode = std::fs::metadata(&path)
        .expect("stat saved file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "session file must be 0600, got {mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P3-5: `ViewId`s are runtime-global — one leaf id backing a pane in two
/// workspaces must reject the whole file.
#[test]
fn duplicate_view_id_across_workspaces_is_rejected() {
    let raw = concat!(
        "bitty-session v1\n",
        "workspaces 2 active 0 mru 0,1\n",
        "workspace 1 7\n",
        "name ws1\n",
        "layout (leaf 7 80 24)\n",
        "pane 7 80 24 0 0\n",
        "end-pane\n",
        "end-workspace\n",
        "workspace 2 7\n",
        "name ws2\n",
        "layout (leaf 7 80 24)\n",
        "pane 7 80 24 0 0\n",
        "end-pane\n",
        "end-workspace\n",
        "end-session\n",
    );
    let err = decode_session(raw.as_bytes()).expect_err("duplicate pane must fail");
    assert!(
        format!("{err}").contains("duplicate pane"),
        "unexpected error: {err}"
    );
}

/// P3-6: a workspace with no leaves would apply as a dead workspace.
#[test]
fn empty_stack_workspace_is_rejected() {
    let raw = concat!(
        "bitty-session v1\n",
        "workspaces 1 active 0 mru 0\n",
        "workspace 1 none\n",
        "name ws1\n",
        "layout (stack)\n",
        "end-workspace\n",
        "end-session\n",
    );
    let err = decode_session(raw.as_bytes()).expect_err("empty workspace must fail");
    assert!(
        format!("{err}").contains("empty workspace"),
        "unexpected error: {err}"
    );
}

/// P3-9 self-compatibility: 3000 backslashes fit the raw cwd bound but
/// escape to 6000 bytes — past the decode line cap. Encode must reject
/// before any I/O so its own output always decodes.
#[test]
fn hostile_cwd_that_escapes_past_line_cap_fails_closed_pre_io() {
    let hostile = "\\".repeat(3000);
    assert!(hostile.len() <= MAX_SESSION_CWD_BYTES);
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::leaf(View::new(ViewId::new(7), 80, 24)),
            focus: Some(ViewId::new(7)),
            panes: vec![PaneSnapshot {
                view: ViewId::new(7),
                cwd: Some(hostile),
                scrollback: Vec::new(),
            }],
        }],
        active: 0,
        mru: vec![0],
    };
    let err = encode_session(&snap).expect_err("hostile cwd must fail closed");
    assert!(
        matches!(
            err,
            SessionError::TooLarge {
                what: "session line",
                ..
            }
        ),
        "unexpected error: {err}"
    );
    // No over-blocking: a max-size cwd without escapes still encodes, and
    // accepted output always decodes.
    let mut ok_snap = snap.clone();
    ok_snap.workspaces[0].panes[0].cwd = Some("a".repeat(MAX_SESSION_CWD_BYTES));
    let bytes = encode_session(&ok_snap).expect("max raw cwd still encodes");
    decode_session(&bytes).expect("encode output must always decode");
}

/// P3-7 companion: no file at all is a quiet clean start. The load-side
/// `NotFound` mapping (a delete raced between the exists-probe and the
/// read, which cannot be arranged deterministically here) is unit-tested
/// in `session.rs` against `startup_outcome_from_load`.
#[test]
fn missing_session_file_is_quiet_fresh_start() {
    let dir = scratch_dir("missing");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let dir_str = dir.to_string_lossy().into_owned();
    let mut rt = Runtime::with_defaults().expect("defaults build");
    let outcome = rt.restore_session_on_startup_with_env(false, Some(dir_str.as_str()), None);
    assert!(
        matches!(
            outcome,
            bitty_runtime::runtime::session::SessionStartupOutcome::Fresh
        ),
        "missing file must be quiet Fresh, got {outcome:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// P2-2: a restored inactive workspace arrives with layout plus pending
/// history but no live shells; the first switch to it must respawn the
/// pending leaves (fresh shells, hydrated history) instead of leaving
/// them empty forever.
#[test]
#[cfg(unix)]
fn inactive_workspace_respawns_pending_panes_on_first_switch() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.spawn_shell("/bin/sh")
        .expect("primary shell records recipe");

    let leaf = |id: u64, history: &str| WorkspaceSnapshot {
        seq: id,
        name: format!("ws{id}"),
        layout: LayoutNode::leaf(View::new(ViewId::new(id), 80, 24)),
        focus: Some(ViewId::new(id)),
        panes: vec![PaneSnapshot {
            view: ViewId::new(id),
            cwd: None,
            scrollback: vec![history.to_string()],
        }],
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![leaf(100, "ws0-history"), leaf(200, "ws1-history")],
        active: 0,
        mru: vec![0, 1],
    };
    rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(
        rt.session_pending_len(),
        1,
        "inactive leaf history must wait pending"
    );
    assert!(!rt.has_pane_session(&ViewId::new(200)));

    assert!(rt.workspace_switch(1), "switch to inactive workspace");
    assert!(
        rt.has_pane_session(&ViewId::new(200)),
        "first switch must respawn the pending leaf"
    );
    assert!(
        rt.pane_pid(&ViewId::new(200)).is_some(),
        "respawned leaf must own a live child"
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "pending history must drain into the fresh shell"
    );
}

/// CTX-0461 (CTX-0393 P3 follow-up): the close path installs a slot exactly
/// like a switch, so a close that lands on a restored workspace with pending
/// history must respawn that workspace's pending leaves. `workspace_switch`
/// early-returns on the already-active index, so without the close-path hook
/// those leaves would stay empty forever. Closing an inactive restored
/// workspace must also drop the pending restores of the leaves it destroys
/// (they can never respawn; the entries would linger and misreport).
#[test]
#[cfg(unix)]
fn workspace_close_respawns_loaded_pending_panes_and_purges_removed() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.spawn_shell("/bin/sh")
        .expect("primary shell records recipe");

    let leaf = |id: u64, history: &str| WorkspaceSnapshot {
        seq: id,
        name: format!("ws{id}"),
        layout: LayoutNode::leaf(View::new(ViewId::new(id), 80, 24)),
        focus: Some(ViewId::new(id)),
        panes: vec![PaneSnapshot {
            view: ViewId::new(id),
            cwd: None,
            scrollback: vec![history.to_string()],
        }],
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![
            leaf(100, "ws0-history"),
            leaf(200, "ws1-history"),
            leaf(300, "ws2-history"),
            leaf(400, "ws3-history"),
        ],
        active: 0,
        mru: vec![0, 1, 2, 3],
    };
    rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(
        rt.session_pending_len(),
        3,
        "inactive leaves arrive pending"
    );

    // ws1 gets a live shell on its first switch (CTX-0393 P2-2), then the
    // active workspace closes through the kill-confirm gate: the close loads
    // ws2, whose pending leaf must respawn on this path.
    assert!(rt.workspace_switch(1), "switch to ws1");
    assert!(rt.has_pane_session(&ViewId::new(200)));
    assert!(matches!(
        rt.workspace_close_request(),
        bitty_runtime::WsCloseRequest::Pending { .. }
    ));
    assert!(
        rt.confirm_pending_ws_close(),
        "confirm kills ws1 and closes it"
    );
    assert!(
        rt.has_pane_session(&ViewId::new(300)),
        "close must respawn the loaded workspace's pending leaf"
    );
    assert!(
        rt.pane_pid(&ViewId::new(300)).is_some(),
        "respawned leaf must own a live child"
    );
    assert_eq!(
        rt.session_pending_len(),
        1,
        "ws2's history drained; only ws3 (never activated) stays pending"
    );

    // Closing an inactive restored workspace (ws3, pending-only) takes its
    // pending restores with it: leaf 400 can never respawn, so its entry
    // must not linger in the map.
    assert_eq!(
        rt.workspace_close_at(2).expect("close inactive ws3"),
        0,
        "pending-only workspace tears down no sessions"
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "removed leaves must not linger pending"
    );
}
