//! CTX-0393 / CTX-0939 (W-146): session save/restore parity through the
//! injected storage backend — atomic persistence, bounded restore,
//! fail-closed fallback, safe-mode skip.
//!
//! Moved from `bitty-runtime/tests/session_save_restore.rs` when the file
//! codec, atomic commit, and path mechanics moved to `bitty-storage`: every
//! test here installs the real storage-backed seam
//! ([`StorageSessionBackend`]) and proves the integrated path preserves the
//! accepted behavior byte-for-byte. These tests fail closed (clean start,
//! warning, never panic) on corrupt or oversize input and never log session
//! contents.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use bitty_runtime::runtime::session::{
    MAX_SESSION_CWD_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_SCROLLBACK_LINES_PER_PANE,
    PaneAttachment, PaneRoute, PaneSnapshot, PinnedSnapshot, SESSION_FORMAT_VERSION, SessionError,
    SessionSnapshot, WorkspaceSnapshot,
};
use bitty_runtime::{
    Focus, LayoutNode, PresentationMode, Runtime, SessionFileBackend, SplitAxis, UiRect, View,
    ViewId,
};

use crate::storage_backends::StorageSessionBackend;

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

/// Storage-backed seam for every test in this module (backend-present
/// configuration): the real `bitty-storage` mechanics behind the Core-owned
/// trait.
fn backend() -> StorageSessionBackend {
    StorageSessionBackend::new()
}

/// Runtime with the storage backend installed (backend-present).
fn present_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(Arc::new(backend())));
    rt
}

/// One-leaf workspace snapshot carrying a captured cwd (hermetic, no PTY).
/// v1-legacy shape (`attach: None`): apply resolves the owner through the
/// startup-owner derivation, exactly like a migrated v1 file.
fn single_leaf_snapshot(id: u64, cwd: Option<String>, history: &str) -> SessionSnapshot {
    SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: id,
            name: format!("ws{id}"),
            layout: LayoutNode::leaf(View::new(ViewId::new(id), 80, 24)),
            focus: Some(ViewId::new(id)),
            panes: vec![PaneSnapshot {
                view: ViewId::new(id),
                cwd,
                scrollback: vec![history.to_string()],
                attach: None,
                route: PaneRoute::Terminal,
                mode: PresentationMode::Tiled,
            }],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    }
}

#[test]
fn snapshot_encode_decode_round_trip_preserves_layout_scrollback_cwd() {
    let mut rt = present_runtime();
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
    let bytes = backend()
        .encode_snapshot(&snap)
        .expect("encode bounded snapshot");
    assert!(bytes.len() <= MAX_SESSION_FILE_BYTES);
    let back = backend()
        .decode_snapshot(&bytes)
        .expect("decode own encoding");
    assert_eq!(snap, back, "round trip must preserve everything");
}

#[test]
fn apply_restores_layout_focus_and_primary_scrollback() {
    let mut rt = present_runtime();
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
    let bytes = backend().encode_snapshot(&snap).expect("encode");
    let back = backend().decode_snapshot(&bytes).expect("decode");

    let mut fresh = present_runtime();
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

    let mut rt = present_runtime();
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
fn partial_temp_write_is_ignored_and_save_sweeps_only_stale_temp() {
    let dir = scratch_dir("atomic");
    let path = dir.join("session");
    let mut rt = present_runtime();
    feed_lines(&mut rt, 10);
    rt.save_session_to_path(&path).expect("save works");

    // Simulate a crashed save: temp file present, final untouched.
    let litter = dir.join("session.tmp.12345");
    std::fs::write(&litter, b"partial garbage").expect("temp write");
    let mut loaded = present_runtime();
    loaded
        .load_session_from_path(&path)
        .expect("temp files must be ignored");

    // W-146 sweep rule (CTX-0003): the pre-write sweep is age-gated, so a
    // fresh temp — possibly a concurrent saver's live file — is never
    // deleted. Only crashed-save litter older than the stale age is swept.
    rt.save_session_to_path(&path).expect("re-save works");
    assert!(litter.exists(), "fresh temp must survive the sweep");

    // Age the litter past the stale horizon: the next save sweeps it.
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    std::fs::File::options()
        .write(true)
        .open(&litter)
        .expect("open litter")
        .set_modified(aged)
        .expect("age litter");
    rt.save_session_to_path(&path).expect("re-save works");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp.") || n.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "save must sweep stale temp files");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safe_mode_never_reads_session_state() {
    let dir = scratch_dir("safe");
    let dir_str = dir.to_string_lossy().into_owned();
    let path = backend()
        .session_file_for(Some(dir_str.as_str()), None)
        .expect("path resolves");
    let mut rt = present_runtime();
    feed_lines(&mut rt, 10);
    rt.save_session_to_path(&path).expect("save works");

    // Even an unreadable file must not matter: safe mode touches nothing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    }
    // Point the default-path lookup at the scratch dir via XDG env injection.
    let probed = backend()
        .session_file_for(Some(dir_str.as_str()), None)
        .expect("path resolves");
    assert_eq!(probed, path);

    let mut fresh = present_runtime();
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

    let mut rt = present_runtime();
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
    let err = backend()
        .decode_snapshot(raw.as_bytes())
        .expect_err("over-cap lines must fail");
    assert!(
        !format!("{err}").contains("overflow"),
        "errors must never echo session contents"
    );
}

#[test]
fn xdg_state_helpers_derive_session_paths_without_hardcoded_hosts() {
    // Explicit XDG root wins.
    assert_eq!(
        backend().session_file_for(Some("/example/xdg-state"), Some("/example/place/home-dir")),
        Some(PathBuf::from("/example/xdg-state/bitty/sessions/session"))
    );
    // Empty XDG falls back to HOME (XDG base-dir spec).
    assert_eq!(
        backend().session_file_for(Some("  "), Some("/example/place/home-dir")),
        Some(PathBuf::from(
            "/example/place/home-dir/.local/state/bitty/sessions/session"
        ))
    );
    // Nothing usable yields None (fail-closed, no panic).
    assert_eq!(backend().session_file_for(None, None), None);
    assert_eq!(backend().session_file_for(Some(""), Some(" ")), None);

    let file = backend()
        .session_file_for(Some("/example/xdg-state"), None)
        .expect("file resolves");
    assert_eq!(
        file,
        PathBuf::from("/example/xdg-state/bitty/sessions/session")
    );
}

/// P2-1: the secret-capable session file must be owner-only from the first
/// byte — no crash window at `0644`.
#[cfg(unix)]
#[test]
fn saved_file_is_created_mode_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch_dir("mode");
    let path = dir.join("session");
    let rt = present_runtime();
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
    let err = backend()
        .decode_snapshot(raw.as_bytes())
        .expect_err("duplicate pane must fail");
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
    let err = backend()
        .decode_snapshot(raw.as_bytes())
        .expect_err("empty workspace must fail");
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
                attach: None,
                route: PaneRoute::Terminal,
                mode: PresentationMode::Tiled,
            }],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    };
    let err = backend()
        .encode_snapshot(&snap)
        .expect_err("hostile cwd must fail closed");
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
    let bytes = backend()
        .encode_snapshot(&ok_snap)
        .expect("max raw cwd still encodes");
    backend()
        .decode_snapshot(&bytes)
        .expect("encode output must always decode");
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
    let mut rt = present_runtime();
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
    let mut rt = present_runtime();
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
            attach: None,
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
        }],
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![leaf(100, "ws0-history"), leaf(200, "ws1-history")],
        active: 0,
        mru: vec![0, 1],
        pinned: Vec::new(),
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
    let mut rt = present_runtime();
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
            attach: None,
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
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
        pinned: Vec::new(),
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

/// CTX-0501 (CTX-0461 residual): a close that destroys the leaf owning the
/// runtime-global primary re-homes the grid onto the loaded slot's focused
/// leaf. When that leaf still carries a pending restore, the close respawn
/// hook skips it (it is the live primary owner, not a pending pane), so the
/// captured history must drain into the primary grid — parity with
/// `rehydrate_pane`'s owner branch — instead of lingering in the pending map
/// with no path that could ever spawn it.
#[test]
#[cfg(unix)]
fn workspace_close_rehomes_primary_onto_pending_leaf_and_drains_restore() {
    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
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
            attach: None,
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
        }],
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![leaf(100, "ws0-history"), leaf(200, "ws1-history")],
        active: 0,
        mru: vec![0, 1],
        pinned: Vec::new(),
    };
    rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(100)),
        "the active slot's focused leaf owns the primary after restore"
    );
    assert_eq!(
        rt.session_pending_len(),
        1,
        "ws1's history waits pending for its first switch"
    );
    assert!(!rt.has_pane_session(&ViewId::new(200)));

    // Closing the primary-owner workspace destroys leaf 100 and loads ws1;
    // the grid re-homes onto ws1's focused leaf, which is still pending.
    assert_eq!(rt.workspace_close_at(0).expect("close primary owner"), 0);
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(200)),
        "the grid re-homes onto the loaded focused leaf"
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "the re-homed owner's restore must drain, not linger unclaimed"
    );
    assert!(
        !rt.has_pane_session(&ViewId::new(200)),
        "primary ownership is not a pane spawn"
    );
    let history: Vec<String> = rt
        .state()
        .scrollback()
        .map(|line| {
            line.cells
                .iter()
                .filter(|c| !c.spacer)
                .map(|c| c.glyph)
                .collect::<String>()
        })
        .collect();
    assert!(
        history.iter().any(|line| line.contains("ws1-history")),
        "captured history must hydrate into the primary grid: {history:?}"
    );
}

/// M1-25 (#1151): restore must seed the primary shell's spawn cwd from the
/// captured `OSC 7` report — the real app restart path calls
/// `Runtime::spawn_shell_with_args` for the primary pane, not
/// `spawn_shell_for_view`, so the pending cwd must apply on that path too.
/// Split panes already consult the pending cwd; the primary attach must not
/// silently drop it.
#[test]
#[cfg(unix)]
fn primary_restore_spawns_with_captured_cwd() {
    bitty_test_support::require_pty!();
    let dir = scratch_dir("primary-cwd");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let report = format!("file://{}", dir.display());
    let snap = single_leaf_snapshot(100, Some(report), "primary-history");

    let mut rt = present_runtime();
    rt.apply_session_snapshot(&snap).expect("apply valid");
    // The lone leaf becomes the primary owner: its history hydrates straight
    // into the grid, so it has no pending-map entry — but the captured cwd
    // must still be staged for the primary attach.
    assert_eq!(
        rt.session_pending_len(),
        0,
        "primary history hydrates immediately, not via the pending map"
    );
    assert_eq!(
        rt.session_pending_cwd(&ViewId::new(100)),
        Some(dir.clone()),
        "captured primary cwd must be staged for the attach"
    );

    rt.spawn_shell_with_args("/bin/sh", &["-c", "pwd -P; exec sleep 30"])
        .expect("primary shell spawn");
    assert!(
        wait_for_primary_text(&mut rt, "bitty-session-primary-cwd"),
        "primary shell must start in the captured cwd; grid={:?}",
        primary_text(&rt)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1-25 (#1151): a captured cwd whose directory no longer exists must
/// fail open — the primary still spawns in the default cwd and the captured
/// history is still hydrated. A stale report must never error or blank the
/// restore.
#[test]
#[cfg(unix)]
fn primary_restore_stale_cwd_falls_back_and_still_hydrates() {
    bitty_test_support::require_pty!();
    let dir = scratch_dir("primary-stale");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let report = format!("file://{}", dir.display());
    std::fs::remove_dir_all(&dir).expect("delete scratch dir");
    let snap = single_leaf_snapshot(100, Some(report), "stale-history");

    let mut rt = present_runtime();
    rt.apply_session_snapshot(&snap).expect("apply valid");
    rt.spawn_shell_with_args("/bin/sh", &["-c", "pwd -P; exec sleep 30"])
        .expect("primary shell spawn");
    let cwd = default_cwd();
    assert!(
        wait_for_primary_text(&mut rt, &cwd),
        "stale capture must fall back to the default cwd without error; grid={:?}",
        primary_text(&rt)
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "primary attach must drain the pending restore even on cwd fallback"
    );
}

/// CW-16 (#994): versioning fails closed — a structurally valid file
/// claiming a future version is rejected whole, never applied partially,
/// and the runtime is left untouched. v3 is the current version (CTX-1082;
/// the old v3-rejection pin now lives on v4); v1 still migrates (see the
/// `v1_file_migrates_with_legacy_defaults` unit test).
#[test]
fn future_version_is_rejected_before_any_mutation() {
    let raw = concat!(
        "bitty-session v4\n",
        "workspaces 1 active 0 mru 0\n",
        "workspace 1 7\n",
        "name ws1\n",
        "layout (leaf 7 80 24)\n",
        "pane 7 80 24 0 0 primary terminal tiled\n",
        "end-pane\n",
        "end-workspace\n",
        "end-session\n",
    );
    let err = backend()
        .decode_snapshot(raw.as_bytes())
        .expect_err("v4 must be rejected");
    assert_eq!(err, SessionError::UnsupportedVersion(4));

    let dir = scratch_dir("version");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("session");
    std::fs::write(&path, raw).expect("write v4 file");
    let mut rt = present_runtime();
    let before = rt.layout().clone();
    let err = rt
        .load_session_from_path(&path)
        .expect_err("v4 load must fail closed");
    assert_eq!(err, SessionError::UnsupportedVersion(4));
    assert_eq!(
        rt.layout(),
        &before,
        "rejected file must not mutate runtime"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1-25 (#1151): a partially written temp sibling from a crashed saver must
/// never be read, and a *failed* save must not clobber the last-good file.
/// Drives the real `save_session_to_path` against a file path that cannot be
/// renamed onto (a directory), proving the previous complete session stays
/// authoritative through the failure.
#[test]
fn failed_save_never_clobbers_last_good_session() {
    let dir = scratch_dir("last-good");
    let good = dir.join("good-session");
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let mut rt = present_runtime();
    feed_lines(&mut rt, 12);
    rt.save_session_to_path(&good).expect("first save works");
    let first = std::fs::read(&good).expect("read good session");
    let snap_first = backend()
        .decode_snapshot(&first)
        .expect("good session decodes");

    // A path whose rename target is an existing directory fails the atomic
    // rename after the temp write; the save must report the error and leave
    // no partial file claiming to be a session.
    let blocked = dir.join("blocked");
    std::fs::create_dir_all(&blocked).expect("blocking dir");
    let err = rt
        .save_session_to_path(&blocked)
        .expect_err("rename onto a directory must fail");
    assert!(
        !format!("{err}").contains("line"),
        "errors must never echo session contents"
    );

    // Last-good is untouched and still decodes to the same snapshot.
    let after = std::fs::read(&good).expect("good session still present");
    assert_eq!(
        first, after,
        "failed save must not touch the last-good file"
    );
    assert_eq!(
        backend().decode_snapshot(&after).expect("still decodes"),
        snap_first,
        "last-good snapshot must survive a failed save"
    );

    // No temp litter survives the failed save for the blocked path.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp.") || n.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "failed save must clean its temp sibling: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1-25 (#1151): a corrupt file present at startup must yield a
/// `FreshWithWarning` outcome (counts/kinds only, no contents) and leave the
/// runtime on a clean start — the startup-level counterpart to the
/// path-level `corrupt_file_falls_back_to_clean_start` test.
#[test]
fn corrupt_store_at_startup_warns_and_starts_clean() {
    let dir = scratch_dir("startup-corrupt");
    let dir_str = dir.to_string_lossy().into_owned();
    let path = backend()
        .session_file_for(Some(dir_str.as_str()), None)
        .expect("path resolves");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("session dir");
    }
    std::fs::write(&path, b"\x00\x01not a session").expect("write garbage");

    let mut rt = present_runtime();
    let before = rt.layout().clone();
    let outcome = rt.restore_session_on_startup_with_env(false, Some(dir_str.as_str()), None);
    match &outcome {
        bitty_runtime::runtime::session::SessionStartupOutcome::FreshWithWarning(err) => {
            assert!(
                !format!("{err}").contains("not a session"),
                "warning must never echo file contents"
            );
        }
        other => panic!("corrupt store must warn, got {other:?}"),
    }
    assert_eq!(
        rt.layout(),
        &before,
        "corrupt store must leave a clean start"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1-25 (#1151): safe mode must not read the session store, so a *valid*
/// store present at startup is left completely unapplied even though it
/// would otherwise restore. Complements `safe_mode_never_reads_session_state`
/// (which uses an unreadable file) with a readable, would-restore file.
#[test]
fn safe_mode_does_not_apply_a_valid_store() {
    let dir = scratch_dir("safe-valid");
    let dir_str = dir.to_string_lossy().into_owned();
    let path = backend()
        .session_file_for(Some(dir_str.as_str()), None)
        .expect("path resolves");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("session dir");
    }
    let snap = single_leaf_snapshot(4242, None, "must-not-restore");
    std::fs::write(&path, backend().encode_snapshot(&snap).expect("encode"))
        .expect("write valid store");

    let mut rt = present_runtime();
    let before_layout = rt.layout().leaf_ids();
    let outcome = rt.restore_session_on_startup_with_env(true, Some(dir_str.as_str()), None);
    assert!(
        matches!(
            outcome,
            bitty_runtime::runtime::session::SessionStartupOutcome::SkippedSafeMode
        ),
        "safe mode must short-circuit, got {outcome:?}"
    );
    assert_eq!(
        rt.layout().leaf_ids(),
        before_layout,
        "safe mode must not apply a valid store"
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "no pending restore in safe mode"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Physical default cwd a child gets when nothing is inherited: the PTY
/// layer falls back to `$HOME`, else the process cwd (mirrors
/// `cwd_inherit.rs::default_cwd`).
// Callers are all `#[cfg(unix)]`; gate the helpers so the `-D warnings`
// gate stays green on non-unix targets.
#[cfg(unix)]
fn default_cwd() -> String {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    let cwd = home
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .expect("home or process cwd");
    std::fs::canonicalize(&cwd)
        .unwrap_or(cwd)
        .display()
        .to_string()
}

#[cfg(unix)]
fn primary_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|c| c.glyph).collect()
}

/// Polls and ticks until the primary grid shows `needle` or times out.
#[cfg(unix)]
fn wait_for_primary_text(rt: &mut Runtime, needle: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if primary_text(rt).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// CW-15 (#993): capture records the live attachment map — the primary
/// owner, a split pane session, and the restored flag stays clear on a
/// fresh runtime. Drives the real spawn paths (primary + split).
#[test]
#[cfg(unix)]
fn capture_records_live_attachment_map() {
    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    rt.focus_mut().set(ViewId::new(1));
    rt.spawn_shell("/bin/sh").expect("primary shell");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 40, 24)
        .expect("split shell");
    assert!(rt.has_pane_session(&ViewId::new(2)));
    assert!(!rt.session_restored(), "fresh runtime is not restored");

    let snap = rt.capture_session_snapshot();
    assert_eq!(snap.version, SESSION_FORMAT_VERSION);
    assert_eq!(snap.workspaces.len(), 1);
    let mut attaches: Vec<(ViewId, Option<PaneAttachment>)> = snap.workspaces[0]
        .panes
        .iter()
        .map(|pane| (pane.view, pane.attach))
        .collect();
    attaches.sort_by_key(|(view, _)| view.0);
    assert_eq!(
        attaches,
        vec![
            (ViewId::new(1), Some(PaneAttachment::Primary)),
            (ViewId::new(2), Some(PaneAttachment::Session)),
        ],
        "primary owner vs split session must be recorded distinctly"
    );
    for pane in &snap.workspaces[0].panes {
        assert_eq!(pane.route, PaneRoute::Terminal);
        assert_eq!(pane.mode, PresentationMode::Tiled);
    }

    // The captured map round-trips through the file byte-identically.
    let bytes = backend().encode_snapshot(&snap).expect("capture encodes");
    let back = backend()
        .decode_snapshot(&bytes)
        .expect("decode own encoding");
    assert_eq!(snap, back);
}

/// CW-15 (#993, WS-INV-26): a snapshot view colliding with a live pane
/// session fails closed before any mutation — rehydration must never merge
/// snapshot history into a live PTY grid. The live session survives.
#[test]
#[cfg(unix)]
fn apply_rejects_snapshot_claiming_live_session() {
    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    ));
    rt.focus_mut().set(ViewId::new(1));
    rt.spawn_shell("/bin/sh").expect("primary shell");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 40, 24)
        .expect("split shell");
    assert!(rt.has_pane_session(&ViewId::new(2)));

    // Snapshot claims the live split leaf (id 2) with hostile history.
    let snap = single_leaf_snapshot(2, None, "hostile-history");
    let before_layout = rt.layout().clone();
    let before_focus = rt.focus().focused();
    let err = rt
        .apply_session_snapshot(&snap)
        .expect_err("live attachment collision must fail");
    assert_eq!(format!("{err}"), "session file corrupt (attachment in use)");
    assert_eq!(rt.layout(), &before_layout, "layout untouched");
    assert_eq!(rt.focus().focused(), before_focus, "focus untouched");
    assert!(
        rt.has_pane_session(&ViewId::new(2)),
        "live session must survive the rejected restore"
    );
    assert!(
        !rt.session_restored(),
        "rejected apply sets no restored flag"
    );
    assert_eq!(rt.session_pending_len(), 0, "no pending residue");
}

/// CW-15/16 (#993/#994): a snapshot routes each pane by its attachment
/// — the startup owner hydrates the primary grid, `session` panes wait
/// pending, `detached` leaves restore empty with no entry and no respawn
/// claim. Hermetic (no PTY): asserts the routing state apply installs.
#[test]
fn apply_routes_panes_by_recorded_attachment() {
    let mk_pane = |id: u64, attach: Option<PaneAttachment>, history: &[&str]| -> PaneSnapshot {
        PaneSnapshot {
            view: ViewId::new(id),
            cwd: None,
            scrollback: history.iter().map(|line| line.to_string()).collect(),
            attach,
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
        }
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![
            WorkspaceSnapshot {
                seq: 1,
                name: "ws1".to_string(),
                layout: LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.5,
                    LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                    LayoutNode::leaf(View::new(ViewId::new(101), 80, 24)),
                ),
                focus: Some(ViewId::new(100)),
                panes: vec![
                    mk_pane(100, Some(PaneAttachment::Primary), &["owner-history"]),
                    mk_pane(101, Some(PaneAttachment::Detached), &[]),
                ],
            },
            WorkspaceSnapshot {
                seq: 2,
                name: "ws2".to_string(),
                layout: LayoutNode::leaf(View::new(ViewId::new(200), 80, 24)),
                focus: Some(ViewId::new(200)),
                panes: vec![mk_pane(
                    200,
                    Some(PaneAttachment::Session),
                    &["pending-history"],
                )],
            },
        ],
        active: 0,
        mru: vec![0, 1],
        pinned: Vec::new(),
    };
    // Through the file: pins the pane record end to end, not just structs.
    let bytes = backend().encode_snapshot(&snap).expect("snapshot encodes");
    let back = backend().decode_snapshot(&bytes).expect("file decodes");
    assert_eq!(snap, back);

    let mut rt = present_runtime();
    assert!(!rt.session_restored());
    let summary = rt.apply_session_snapshot(&back).expect("apply valid");
    assert_eq!(summary.workspaces, 2);
    assert_eq!(summary.panes, 3);
    assert_eq!(summary.scrollback_lines, 1, "only the owner hydrates now");
    assert_eq!(summary.pending, 1, "only the session pane waits pending");
    assert!(rt.session_restored());
    assert_eq!(rt.primary_view(), Some(ViewId::new(100)));
    assert!(rt.session_pending_contains(&ViewId::new(200)));
    assert!(
        !rt.session_pending_contains(&ViewId::new(101)),
        "detached leaf earns no pending entry"
    );
    assert!(
        rt.layout().leaf_ids().contains(&ViewId::new(101)),
        "detached leaf still restores its tile (empty, no respawn claim)"
    );
    let grid: String = rt
        .state()
        .scrollback()
        .map(|line| {
            line.cells
                .iter()
                .filter(|c| !c.spacer)
                .map(|c| c.glyph)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        grid.contains("owner-history"),
        "owner history hydrates the grid: {grid:?}"
    );
}

/// CW-16 (#994): a recorded `primary` away from the derived startup owner
/// downgrades to a pending respawn — the primary shell re-attaches at the
/// focused leaf on every launch, so honoring a stale owner would mix two
/// histories into the one global grid. Contents preserved, bindings fresh.
#[test]
fn recorded_primary_elsewhere_downgrades_to_pending() {
    let mk_pane = |id: u64, attach: PaneAttachment, history: &str| PaneSnapshot {
        view: ViewId::new(id),
        cwd: None,
        scrollback: vec![history.to_string()],
        attach: Some(attach),
        route: PaneRoute::Terminal,
        mode: PresentationMode::Tiled,
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(101), 80, 24)),
            ),
            focus: Some(ViewId::new(100)),
            panes: vec![
                mk_pane(100, PaneAttachment::Session, "focus-history"),
                mk_pane(101, PaneAttachment::Primary, "stale-owner-history"),
            ],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    };
    let bytes = backend().encode_snapshot(&snap).expect("snapshot encodes");
    let back = backend().decode_snapshot(&bytes).expect("file decodes");

    let mut rt = present_runtime();
    let summary = rt.apply_session_snapshot(&back).expect("apply valid");
    assert_eq!(summary.scrollback_lines, 0, "no grid write on downgrade");
    assert_eq!(summary.pending, 2, "both panes wait for fresh shells");
    assert_eq!(
        rt.primary_view(),
        Some(ViewId::new(100)),
        "owner follows the startup recipe (focus), not the stale record"
    );
    assert!(rt.session_pending_contains(&ViewId::new(100)));
    assert!(rt.session_pending_contains(&ViewId::new(101)));
}

/// CTX-0972 F4: ambiguous ownership (one explicit primary off the derived
/// owner plus a legacy `None` on the owner) is rejected at load. Before the
/// fix the file validated, then encoded to two primaries that the next load
/// rejected — validate accepted files that encode into files validate
/// rejects.
#[test]
fn ambiguous_resolved_ownership_rejected_at_load_not_round_trip() {
    let pane = |id: u64, attach: Option<PaneAttachment>| PaneSnapshot {
        view: ViewId::new(id),
        cwd: None,
        scrollback: Vec::new(),
        attach,
        route: PaneRoute::Terminal,
        mode: PresentationMode::Tiled,
    };
    // Focus pins the derived owner at leaf 100; the explicit primary sits
    // off-owner on leaf 101 while leaf 100 carries legacy `None`.
    let ambiguous = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(101), 80, 24)),
            ),
            focus: Some(ViewId::new(100)),
            panes: vec![pane(100, None), pane(101, Some(PaneAttachment::Primary))],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    };
    let mut rt = present_runtime();
    let err = rt
        .apply_session_snapshot(&ambiguous)
        .expect_err("ambiguous ownership must fail at load");
    assert_eq!(format!("{err}"), "session file corrupt (duplicate primary)");

    // Mechanism pin: the rejected file would have encoded to two primaries.
    let bytes = backend()
        .encode_snapshot(&ambiguous)
        .expect("storage-side explicit check still encodes the ambiguous file");
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        text.match_indices(" primary ").count(),
        2,
        "ambiguous file must encode to two primaries: {text:?}"
    );

    // 0-explicit still loads and encodes to exactly one primary.
    let mut legacy = ambiguous.clone();
    legacy.workspaces[0].panes[1].attach = Some(PaneAttachment::Session);
    let mut rt = present_runtime();
    rt.apply_session_snapshot(&legacy)
        .expect("legacy Nones must load");
    let bytes = backend()
        .encode_snapshot(&legacy)
        .expect("legacy must encode");
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        text.match_indices(" primary ").count(),
        1,
        "legacy Nones must encode to exactly one primary: {text:?}"
    );

    // 1-explicit-on-owner plus a None elsewhere loads.
    let mut on_owner = ambiguous.clone();
    on_owner.workspaces[0].panes[0].attach = Some(PaneAttachment::Primary);
    on_owner.workspaces[0].panes[1].attach = None;
    let mut rt = present_runtime();
    rt.apply_session_snapshot(&on_owner)
        .expect("primary on the owner must load");
}

/// CW-16 (#994): per-leaf presentation modes survive the file round trip
/// into the live tree — the mode token stamps the restored leaf at decode
/// and apply installs that tree live.
#[test]
fn modes_survive_file_round_trip_into_live_layout() {
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::leaf(View::new(ViewId::new(7), 80, 24)),
            focus: Some(ViewId::new(7)),
            panes: vec![PaneSnapshot {
                view: ViewId::new(7),
                cwd: None,
                scrollback: vec!["mode-history".to_string()],
                attach: Some(PaneAttachment::Primary),
                route: PaneRoute::Terminal,
                mode: PresentationMode::Floating,
            }],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    };
    let bytes = backend().encode_snapshot(&snap).expect("snapshot encodes");
    assert!(
        String::from_utf8_lossy(&bytes).contains("pane 7 80 24 1 0 primary terminal floating"),
        "pane header carries attach, route, and mode tokens"
    );
    let back = backend().decode_snapshot(&bytes).expect("file decodes");
    assert_eq!(back.workspaces[0].panes[0].mode, PresentationMode::Floating);

    let mut rt = present_runtime();
    rt.apply_session_snapshot(&back).expect("apply valid");
    let mode = rt
        .layout()
        .find_leaf(ViewId::new(7))
        .expect("leaf restored")
        .presentation();
    assert_eq!(mode, PresentationMode::Floating);
}

/// CW-15/16 (#993/#994): a `detached` leaf in a restored inactive
/// workspace never respawns — the first switch gives a shell only to the
/// attached (`session`) leaf. Drives the real switch-spawn path.
#[test]
#[cfg(unix)]
fn detached_leaf_never_respawns_on_first_switch() {
    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
    rt.spawn_shell("/bin/sh")
        .expect("primary shell records recipe");

    let mk_pane = |id: u64, attach: PaneAttachment, history: &[&str]| PaneSnapshot {
        view: ViewId::new(id),
        cwd: None,
        scrollback: history.iter().map(|line| line.to_string()).collect(),
        attach: Some(attach),
        route: PaneRoute::Terminal,
        mode: PresentationMode::Tiled,
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![
            WorkspaceSnapshot {
                seq: 1,
                name: "ws1".to_string(),
                layout: LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                focus: Some(ViewId::new(100)),
                panes: vec![mk_pane(100, PaneAttachment::Primary, &["ws0-history"])],
            },
            WorkspaceSnapshot {
                seq: 2,
                name: "ws2".to_string(),
                layout: LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.5,
                    LayoutNode::leaf(View::new(ViewId::new(200), 40, 24)),
                    LayoutNode::leaf(View::new(ViewId::new(201), 40, 24)),
                ),
                focus: Some(ViewId::new(200)),
                panes: vec![
                    mk_pane(200, PaneAttachment::Session, &["ws1-history"]),
                    mk_pane(201, PaneAttachment::Detached, &[]),
                ],
            },
        ],
        active: 0,
        mru: vec![0, 1],
        pinned: Vec::new(),
    };
    rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(rt.session_pending_len(), 1, "detached leaf claims nothing");

    assert!(rt.workspace_switch(1), "switch to ws1");
    assert!(
        rt.has_pane_session(&ViewId::new(200)),
        "attached leaf respawns on first switch"
    );
    assert!(
        !rt.has_pane_session(&ViewId::new(201)),
        "detached leaf stays empty: no shell it never had"
    );
    assert_eq!(
        rt.session_pending_len(),
        0,
        "pending drains into fresh shells"
    );
}

/// CTX-1082: the window-global pinned store round-trips through the v3
/// file — the parked leaf (identity, geometry, floating mode), its unpin
/// anchor, and its history come back; the attached entry waits pending and
/// unpin-after-restore returns the floating panel. Hermetic (no PTY): the
/// history rides the snapshot, not a live shell.
#[test]
fn pinned_store_round_trips_through_v3_file() {
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            ),
            focus: Some(ViewId::new(1)),
            panes: vec![
                PaneSnapshot {
                    view: ViewId::new(1),
                    cwd: None,
                    scrollback: vec!["owner-history".to_string()],
                    attach: Some(PaneAttachment::Primary),
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
                },
                PaneSnapshot {
                    view: ViewId::new(2),
                    cwd: None,
                    scrollback: Vec::new(),
                    attach: Some(PaneAttachment::Detached),
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
                },
            ],
        }],
        active: 0,
        mru: vec![0],
        pinned: vec![PinnedSnapshot {
            view: View::with_presentation(ViewId::new(9), 80, 24, PresentationMode::Floating),
            cwd: Some(String::from("file:///tmp/pinned")),
            scrollback: vec!["pinned-history".to_string()],
            attach: PaneAttachment::Session,
            anchor: Some(ViewId::new(2)),
            after: false,
        }],
    };
    // Through the file: the v3 record carries the pinned block end to end.
    let bytes = backend().encode_snapshot(&snap).expect("v3 encodes");
    assert!(bytes.starts_with(b"bitty-session v3\n"));
    let back = backend().decode_snapshot(&bytes).expect("v3 decodes");
    assert_eq!(snap, back, "pinned snapshot round-trips byte-identically");

    let mut rt = present_runtime();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let summary = rt.apply_session_snapshot(&back).expect("apply valid");
    assert_eq!(summary.panes, 3, "layout leaves plus the pinned entry");
    assert_eq!(summary.pending, 1, "only the pinned session waits pending");
    assert_eq!(rt.pinned_views(), vec![ViewId::new(9)]);
    assert!(rt.pinned_occupied());
    let parked = rt
        .pinned_views()
        .into_iter()
        .next()
        .expect("pinned leaf restored");
    assert!(rt.session_pending_contains(&parked));
    assert!(
        rt.present_frames()
            .iter()
            .any(|frame| frame.view == parked
                && frame.tier == Some(bitty_runtime::OverlayTier::Float)),
        "restored pin composites over the scene"
    );

    // Unpin-after-restore returns the floating panel beside its anchor.
    let restored = rt.unpin_floating(parked).expect("unpin must apply");
    assert_eq!(restored, parked);
    assert!(!rt.pinned_occupied());
    assert_eq!(rt.focused_view(), Some(parked));
    assert_eq!(
        rt.layout()
            .find_leaf(parked)
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating
    );
}

/// CTX-1082 compat: a v2 file (no pinned block) still loads — decode
/// migrates it to an empty pinned store and the world restores without
/// pinned entries.
#[test]
fn old_v2_file_loads_with_empty_pinned_store() {
    let raw = concat!(
        "bitty-session v2\n",
        "workspaces 1 active 0 mru 0\n",
        "workspace 1 7\n",
        "name ws1\n",
        "layout (leaf 7 80 24)\n",
        "pane 7 80 24 1 0 primary terminal tiled\n",
        "compat line\n",
        "end-pane\n",
        "end-workspace\n",
        "end-session\n",
    );
    let snap = backend()
        .decode_snapshot(raw.as_bytes())
        .expect("v2 must still decode");
    assert_eq!(snap.version, SESSION_FORMAT_VERSION);
    assert!(snap.pinned.is_empty(), "v2 carries no pinned block");

    let mut rt = present_runtime();
    let summary = rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(summary.panes, 1);
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(7)]);
    assert!(!rt.pinned_occupied(), "no pinned entries after a v2 load");
}

/// Snapshot shared by the pinned-respawn tests: two layout leaves plus
/// one attached pinned entry carrying history.
fn respawn_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(101), 80, 24)),
            ),
            focus: Some(ViewId::new(100)),
            panes: vec![
                PaneSnapshot {
                    view: ViewId::new(100),
                    cwd: None,
                    scrollback: vec!["owner-history".to_string()],
                    attach: Some(PaneAttachment::Primary),
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
                },
                PaneSnapshot {
                    view: ViewId::new(101),
                    cwd: None,
                    scrollback: Vec::new(),
                    attach: Some(PaneAttachment::Detached),
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
                },
            ],
        }],
        active: 0,
        mru: vec![0],
        pinned: vec![PinnedSnapshot {
            view: View::with_presentation(ViewId::new(9), 80, 24, PresentationMode::Floating),
            cwd: None,
            scrollback: vec!["pinned-history".to_string()],
            attach: PaneAttachment::Session,
            anchor: Some(ViewId::new(101)),
            after: false,
        }],
    }
}

/// CTX-1082 (CodeRabbit 1897): a restored attached pin must gain a live
/// shell at startup — staging it pending is not enough, since hydrate
/// only restores scrollback into an existing session. Drives the real
/// startup spawn path.
#[test]
#[cfg(unix)]
fn restored_attached_pin_respawns_at_startup_with_history() {
    use crate::spawn::{SpawnSpec, spawn_startup_pane_shells};

    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.apply_session_snapshot(&respawn_snapshot())
        .expect("apply valid");
    assert_eq!(rt.pinned_views(), vec![ViewId::new(9)]);
    assert!(
        !rt.has_pane_session(&ViewId::new(9)),
        "no shell before startup spawn"
    );

    let spec = SpawnSpec {
        program: Some(String::from("/bin/sh")),
        ..Default::default()
    };
    let failed = spawn_startup_pane_shells(&mut rt, &spec);
    assert_eq!(failed, 0, "pinned shell spawns");
    assert!(
        rt.has_pane_session(&ViewId::new(9)),
        "restored pin has a live session after startup spawn"
    );
    assert!(
        !rt.session_pending_contains(&ViewId::new(9)),
        "pending history hydrates into the fresh grid"
    );
    assert_eq!(rt.pinned_views(), vec![ViewId::new(9)]);
    assert!(
        rt.present_frames()
            .iter()
            .any(|frame| frame.view == ViewId::new(9)
                && frame.tier == Some(bitty_runtime::OverlayTier::Float)),
        "respawned pin still composites over the scene"
    );
}

/// CTX-1082 (CodeRabbit 1897): the deferred switch path respawns a pinned
/// pending entry too (covering a startup spawn that failed). Drives the
/// real workspace-switch respawn.
#[test]
#[cfg(unix)]
fn restored_attached_pin_respawns_on_workspace_switch() {
    bitty_test_support::require_pty!();
    let mut rt = present_runtime();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.spawn_shell("/bin/sh")
        .expect("primary shell records recipe");
    let mut snap = respawn_snapshot();
    snap.workspaces.push(WorkspaceSnapshot {
        seq: 2,
        name: "ws2".to_string(),
        layout: LayoutNode::leaf(View::new(ViewId::new(200), 80, 24)),
        focus: Some(ViewId::new(200)),
        panes: vec![PaneSnapshot {
            view: ViewId::new(200),
            cwd: None,
            scrollback: vec!["ws1-history".to_string()],
            attach: Some(PaneAttachment::Session),
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
        }],
    });
    snap.mru = vec![0, 1];
    rt.apply_session_snapshot(&snap).expect("apply valid");
    assert!(
        !rt.has_pane_session(&ViewId::new(9)),
        "no shell before the switch"
    );

    assert!(rt.workspace_switch(1), "switch to ws2");
    assert!(
        rt.has_pane_session(&ViewId::new(9)),
        "deferred path respawns the pinned pending entry"
    );
    assert!(
        !rt.session_pending_contains(&ViewId::new(9)),
        "pending history hydrates into the fresh grid"
    );
}
