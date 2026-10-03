//! W-146 storage adapters (CTX-0939): implement the Core-owned
//! durable-commit traits with the extracted `bitty-storage` mechanics.
//!
//! This module is the only place in the workspace that names
//! `bitty_storage`: Core library crates consume persistence exclusively
//! through their own traits (`SessionFileBackend`, `KvCommitBackend`), so
//! the dependency direction stays one-way. Validation-before-mutation,
//! permission gates, budgets, fencing, and counts-only logging all stay in
//! Core; this adapter converts Core snapshots to the storage model,
//! delegates the byte mechanics, and maps errors back 1:1 (kinds and counts
//! only, never contents).
//!
//! Byte parity with the pre-rewire Core mechanics is pinned by the golden
//! fixtures under `tests/fixtures/` (generated from the Core codec before
//! the move) plus the ceiling-equality assertions in the test module below.

use std::path::{Path, PathBuf};

use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PresentationMode, SessionError, SessionSnapshot,
    SplitAxis, View, ViewId, WorkspaceSnapshot,
};
use bitty_runtime::{MAX_SESSION_FILE_BYTES, PaneSnapshot};
use bitty_storage::session_codec;

// ---------------------------------------------------------------------------
// Session backend
// ---------------------------------------------------------------------------

/// Storage-backed [`bitty_runtime::SessionFileBackend`]: session codec,
/// atomic commit, capped load, and XDG path resolution via `bitty-storage`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StorageSessionBackend;

impl StorageSessionBackend {
    /// Builds the backend (stateless: every bound comes from Core constants
    /// and the storage mechanics).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn core_attachment_to_storage(
    attach: bitty_runtime::PaneAttachment,
) -> session_codec::PaneAttachment {
    match attach {
        PaneAttachment::Primary => session_codec::PaneAttachment::Primary,
        PaneAttachment::Session => session_codec::PaneAttachment::Session,
        PaneAttachment::Detached => session_codec::PaneAttachment::Detached,
    }
}

fn storage_attachment_to_core(
    attach: session_codec::PaneAttachment,
) -> bitty_runtime::PaneAttachment {
    match attach {
        session_codec::PaneAttachment::Primary => PaneAttachment::Primary,
        session_codec::PaneAttachment::Session => PaneAttachment::Session,
        session_codec::PaneAttachment::Detached => PaneAttachment::Detached,
    }
}

fn core_mode_to_storage(mode: PresentationMode) -> session_codec::PresentationMode {
    match mode {
        PresentationMode::Tiled => session_codec::PresentationMode::Tiled,
        PresentationMode::Floating => session_codec::PresentationMode::Floating,
        PresentationMode::Fullscreen => session_codec::PresentationMode::Fullscreen,
        PresentationMode::Scratchpad => session_codec::PresentationMode::Scratchpad,
    }
}

fn storage_mode_to_core(mode: session_codec::PresentationMode) -> PresentationMode {
    match mode {
        session_codec::PresentationMode::Tiled => PresentationMode::Tiled,
        session_codec::PresentationMode::Floating => PresentationMode::Floating,
        session_codec::PresentationMode::Fullscreen => PresentationMode::Fullscreen,
        session_codec::PresentationMode::Scratchpad => PresentationMode::Scratchpad,
    }
}

fn core_layout_to_storage(node: &LayoutNode) -> session_codec::LayoutNode {
    match node {
        LayoutNode::Leaf(view) => session_codec::LayoutNode::Leaf {
            id: view.id().0,
            cols: usize::from(view.cols()),
            rows: usize::from(view.rows()),
        },
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => session_codec::LayoutNode::Split {
            horizontal: matches!(axis, SplitAxis::Horizontal),
            ratio: *ratio,
            first: Box::new(core_layout_to_storage(first)),
            second: Box::new(core_layout_to_storage(second)),
        },
        LayoutNode::Stack(children) => {
            session_codec::LayoutNode::Stack(children.iter().map(core_layout_to_storage).collect())
        }
        LayoutNode::Overlay { base, .. } => core_layout_to_storage(base),
    }
}

fn storage_layout_to_core(node: &session_codec::LayoutNode) -> LayoutNode {
    match node {
        session_codec::LayoutNode::Leaf { id, cols, rows } => {
            LayoutNode::leaf(View::new(ViewId::new(*id), *cols, *rows))
        }
        session_codec::LayoutNode::Split {
            horizontal,
            ratio,
            first,
            second,
        } => LayoutNode::split(
            if *horizontal {
                SplitAxis::Horizontal
            } else {
                SplitAxis::Vertical
            },
            *ratio,
            storage_layout_to_core(first),
            storage_layout_to_core(second),
        ),
        session_codec::LayoutNode::Stack(children) => {
            LayoutNode::stack(children.iter().map(storage_layout_to_core).collect())
        }
    }
}

fn core_snapshot_to_storage(snap: &SessionSnapshot) -> session_codec::SessionSnapshot {
    session_codec::SessionSnapshot {
        version: snap.version,
        workspaces: snap
            .workspaces
            .iter()
            .map(|ws| session_codec::WorkspaceSnapshot {
                seq: ws.seq,
                name: ws.name.clone(),
                layout: core_layout_to_storage(&ws.layout),
                focus: ws.focus.map(|focus| focus.0),
                panes: ws
                    .panes
                    .iter()
                    .map(|pane| session_codec::PaneSnapshot {
                        view: pane.view.0,
                        cwd: pane.cwd.clone(),
                        scrollback: pane.scrollback.clone(),
                        attach: pane.attach.map(core_attachment_to_storage),
                        route: match pane.route {
                            PaneRoute::Terminal => session_codec::PaneRoute::Terminal,
                        },
                        mode: core_mode_to_storage(pane.mode),
                    })
                    .collect(),
            })
            .collect(),
        active: snap.active,
        mru: snap.mru.clone(),
    }
}

fn storage_snapshot_to_core(snap: &session_codec::SessionSnapshot) -> SessionSnapshot {
    SessionSnapshot {
        version: snap.version,
        workspaces: snap
            .workspaces
            .iter()
            .map(|ws| {
                let mut layout = storage_layout_to_core(&ws.layout);
                let panes = ws
                    .panes
                    .iter()
                    .map(|pane| {
                        let view = ViewId::new(pane.view);
                        let mode = storage_mode_to_core(pane.mode);
                        // The storage layout carries identity plus geometry
                        // only; the v2 mode token stamps the restored leaf
                        // so the live tree keeps the requested display mode.
                        if let Some(leaf) = layout.find_leaf_mut(view) {
                            leaf.set_presentation(mode);
                        }
                        PaneSnapshot {
                            view,
                            cwd: pane.cwd.clone(),
                            scrollback: pane.scrollback.clone(),
                            attach: pane.attach.map(storage_attachment_to_core),
                            route: match pane.route {
                                session_codec::PaneRoute::Terminal => PaneRoute::Terminal,
                            },
                            mode,
                        }
                    })
                    .collect();
                WorkspaceSnapshot {
                    seq: ws.seq,
                    name: ws.name.clone(),
                    layout,
                    focus: ws.focus.map(ViewId::new),
                    panes,
                }
            })
            .collect(),
        active: snap.active,
        mru: snap.mru.clone(),
    }
}

fn map_session_error(err: session_codec::SessionError) -> SessionError {
    match err {
        session_codec::SessionError::NotFound => SessionError::NotFound,
        session_codec::SessionError::NoStateDir => SessionError::NoStateDir,
        session_codec::SessionError::TooLarge {
            what,
            actual,
            limit,
        } => SessionError::TooLarge {
            what,
            actual,
            limit,
        },
        session_codec::SessionError::Corrupt(why) => SessionError::Corrupt(why),
        session_codec::SessionError::UnsupportedVersion(version) => {
            SessionError::UnsupportedVersion(version)
        }
        session_codec::SessionError::Io(message) => SessionError::Io(message),
    }
}

impl bitty_runtime::SessionFileBackend for StorageSessionBackend {
    fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
        session_codec::encode_session(&core_snapshot_to_storage(snap)).map_err(map_session_error)
    }

    fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
        session_codec::decode_session(bytes)
            .map(|snap| storage_snapshot_to_core(&snap))
            .map_err(map_session_error)
    }

    fn commit_session_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
        // Core-owned ceiling, enforced before any filesystem touch (mirrors
        // the pre-rewire fail-closed pre-check; the backend re-enforces it
        // as a backstop).
        if bytes.len() > MAX_SESSION_FILE_BYTES {
            return Err(SessionError::TooLarge {
                what: "session file",
                actual: bytes.len(),
                limit: MAX_SESSION_FILE_BYTES,
            });
        }
        bitty_storage::atomic_io::save_bytes_atomic(path, bytes, MAX_SESSION_FILE_BYTES)
            .map_err(|err| SessionError::Io(err.to_string()))
    }

    fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError> {
        match bitty_storage::atomic_io::load_bytes_capped(path, MAX_SESSION_FILE_BYTES) {
            Ok(bytes) => Ok(bytes),
            Err(bitty_storage::atomic_io::LoadError::NotFound) => Err(SessionError::NotFound),
            Err(bitty_storage::atomic_io::LoadError::TooLarge { actual, limit }) => {
                Err(SessionError::TooLarge {
                    what: "session file",
                    actual,
                    limit,
                })
            }
            Err(bitty_storage::atomic_io::LoadError::Io(message)) => Err(SessionError::Io(message)),
        }
    }

    fn session_file_for(
        &self,
        xdg_state_home: Option<&str>,
        home: Option<&str>,
    ) -> Option<PathBuf> {
        session_codec::session_file_for(xdg_state_home, home)
    }

    fn session_file(&self) -> Option<PathBuf> {
        session_codec::session_file()
    }
}

// ---------------------------------------------------------------------------
// Plugin KV backend
// ---------------------------------------------------------------------------

/// Storage-backed [`bitty_runtime::plugin_runtime::KvCommitBackend`]: atomic
/// temp-plus-rename commits (user-only permissions) and capped loads via
/// `bitty-storage`. Quota and key/value validation stay in Core and run
/// before every commit; only the bytes move through here.
#[derive(Debug, Default, Clone, Copy)]
pub struct StorageKvBackend;

impl StorageKvBackend {
    /// Builds the backend (stateless: the bound comes from the Core ceiling).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl bitty_runtime::plugin_runtime::KvCommitBackend for StorageKvBackend {
    fn commit_store_bytes(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), bitty_runtime::plugin_runtime::KvCommitError> {
        use bitty_runtime::plugin_runtime::{KvCommitError, STORE_FILE_MAX_BYTES};
        bitty_storage::atomic_io::save_bytes_atomic(path, bytes, STORE_FILE_MAX_BYTES)
            .map_err(|_| KvCommitError::new("could not commit the plugin state file"))
    }

    fn load_store_bytes(
        &self,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, bitty_runtime::plugin_runtime::KvCommitError> {
        use bitty_runtime::plugin_runtime::{KvCommitError, STORE_FILE_MAX_BYTES};
        match bitty_storage::atomic_io::load_bytes_capped(path, STORE_FILE_MAX_BYTES) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(bitty_storage::atomic_io::LoadError::NotFound) => Ok(None),
            Err(bitty_storage::atomic_io::LoadError::TooLarge { actual, limit }) => Err(
                KvCommitError::new(format!("store file too large ({actual} > {limit})")),
            ),
            Err(bitty_storage::atomic_io::LoadError::Io(message)) => {
                Err(KvCommitError::new(format!("store read: {message}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_runtime::plugin_runtime::store::{
        JSON_MAX_DEPTH, STORE_FILE_MAX_BYTES, STORE_MAX_ENTRIES, STORE_MAX_KEY_BYTES,
        STORE_MAX_TOTAL_BYTES, STORE_MAX_VALUE_BYTES,
    };
    use bitty_runtime::{
        MAX_SESSION_CWD_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_GRID_DIM,
        MAX_SESSION_LAYOUT_DEPTH, MAX_SESSION_LINE_BYTES, MAX_SESSION_LINE_TEXT_BYTES,
        MAX_SESSION_NAME_CHARS, MAX_SESSION_PANES_PER_WORKSPACE, MAX_SESSION_PANES_TOTAL,
        MAX_SESSION_SCROLLBACK_LINES_PER_PANE, MAX_SESSION_WORKSPACES, PaneSnapshot,
        SESSION_APP_DIR_NAME, SESSION_FILE_NAME, SESSION_FORMAT_VERSION,
        SESSION_MIN_DECODE_VERSION, SESSIONS_DIR_NAME, SessionFileBackend, WorkspaceSnapshot,
    };

    fn backend() -> StorageSessionBackend {
        StorageSessionBackend::new()
    }

    fn leaf(id: u64, mode: PresentationMode) -> LayoutNode {
        LayoutNode::leaf(View::with_presentation(ViewId::new(id), 80, 24, mode))
    }

    fn pane(
        id: u64,
        cwd: Option<&str>,
        scrollback: &[&str],
        attach: Option<PaneAttachment>,
        mode: PresentationMode,
    ) -> PaneSnapshot {
        PaneSnapshot {
            view: ViewId::new(id),
            cwd: cwd.map(str::to_owned),
            scrollback: scrollback.iter().map(|s| (*s).to_owned()).collect(),
            attach,
            route: PaneRoute::Terminal,
            mode,
        }
    }

    /// Snapshots mirrored 1:1 from the pre-rewire golden generator
    /// (`zz_golden_dump`, since removed): the committed fixtures under
    /// `tests/fixtures/session-parity/` are the pre-rewire Core codec
    /// outputs for exactly these inputs.
    fn golden_snapshot(name: &str) -> SessionSnapshot {
        match name {
            "basic" => SessionSnapshot {
                version: SESSION_FORMAT_VERSION,
                workspaces: vec![WorkspaceSnapshot {
                    seq: 7,
                    name: String::from("ws7"),
                    layout: leaf(7, PresentationMode::Tiled),
                    focus: Some(ViewId::new(7)),
                    panes: vec![pane(
                        7,
                        Some("file:///tmp"),
                        &["hello world"],
                        None,
                        PresentationMode::Tiled,
                    )],
                }],
                active: 0,
                mru: vec![0],
            },
            "escapes" => SessionSnapshot {
                version: SESSION_FORMAT_VERSION,
                workspaces: vec![WorkspaceSnapshot {
                    seq: 1,
                    name: String::from("work zone \u{00e9}\u{4e2d}"),
                    layout: LayoutNode::split(
                        SplitAxis::Horizontal,
                        0.5,
                        leaf(1, PresentationMode::Floating),
                        leaf(2, PresentationMode::Tiled),
                    ),
                    focus: Some(ViewId::new(2)),
                    panes: vec![
                        pane(
                            1,
                            Some("file:///tmp/dir with spaces"),
                            &["back\\slash", "line\nbreak", "cr\rhere", "  padded  ", ""],
                            Some(PaneAttachment::Primary),
                            PresentationMode::Floating,
                        ),
                        pane(
                            2,
                            None,
                            &["plain"],
                            Some(PaneAttachment::Session),
                            PresentationMode::Tiled,
                        ),
                    ],
                }],
                active: 0,
                mru: vec![0],
            },
            "detached" => SessionSnapshot {
                version: SESSION_FORMAT_VERSION,
                workspaces: vec![WorkspaceSnapshot {
                    seq: 3,
                    name: String::from("ws3"),
                    layout: LayoutNode::split(
                        SplitAxis::Vertical,
                        0.25,
                        leaf(11, PresentationMode::Tiled),
                        leaf(12, PresentationMode::Scratchpad),
                    ),
                    focus: Some(ViewId::new(11)),
                    panes: vec![
                        pane(
                            11,
                            Some("file:///tmp"),
                            &["live"],
                            Some(PaneAttachment::Session),
                            PresentationMode::Tiled,
                        ),
                        pane(
                            12,
                            None,
                            &[],
                            Some(PaneAttachment::Detached),
                            PresentationMode::Scratchpad,
                        ),
                    ],
                }],
                active: 0,
                mru: vec![0],
            },
            "multi" => SessionSnapshot {
                version: SESSION_FORMAT_VERSION,
                workspaces: vec![
                    WorkspaceSnapshot {
                        seq: 1,
                        name: String::from("first"),
                        layout: leaf(1, PresentationMode::Tiled),
                        focus: Some(ViewId::new(1)),
                        panes: vec![pane(
                            1,
                            None,
                            &[],
                            Some(PaneAttachment::Session),
                            PresentationMode::Tiled,
                        )],
                    },
                    WorkspaceSnapshot {
                        seq: 2,
                        name: String::from("second"),
                        layout: LayoutNode::stack(vec![
                            leaf(2, PresentationMode::Tiled),
                            leaf(3, PresentationMode::Fullscreen),
                        ]),
                        focus: Some(ViewId::new(3)),
                        panes: vec![
                            pane(
                                2,
                                Some("file:///var/tmp"),
                                &["a", "b"],
                                Some(PaneAttachment::Session),
                                PresentationMode::Tiled,
                            ),
                            pane(
                                3,
                                None,
                                &["c"],
                                Some(PaneAttachment::Primary),
                                PresentationMode::Fullscreen,
                            ),
                        ],
                    },
                ],
                active: 1,
                mru: vec![1, 0],
            },
            "empty" => SessionSnapshot {
                version: SESSION_FORMAT_VERSION,
                workspaces: vec![WorkspaceSnapshot {
                    seq: 5,
                    name: String::from("empty"),
                    layout: leaf(5, PresentationMode::Tiled),
                    focus: Some(ViewId::new(5)),
                    panes: vec![pane(
                        5,
                        None,
                        &[],
                        Some(PaneAttachment::Session),
                        PresentationMode::Tiled,
                    )],
                }],
                active: 0,
                mru: vec![0],
            },
            _ => panic!("unknown golden snapshot"),
        }
    }

    #[test]
    fn core_and_storage_ceilings_match_exactly() {
        use bitty_storage::ceiling;
        assert_eq!(MAX_SESSION_FILE_BYTES, ceiling::MAX_SESSION_FILE_BYTES);
        assert_eq!(MAX_SESSION_LINE_BYTES, ceiling::MAX_SESSION_LINE_BYTES);
        assert_eq!(MAX_SESSION_WORKSPACES, ceiling::MAX_SESSION_WORKSPACES);
        assert_eq!(
            MAX_SESSION_PANES_PER_WORKSPACE,
            ceiling::MAX_SESSION_PANES_PER_WORKSPACE
        );
        assert_eq!(MAX_SESSION_PANES_TOTAL, ceiling::MAX_SESSION_PANES_TOTAL);
        assert_eq!(
            MAX_SESSION_SCROLLBACK_LINES_PER_PANE,
            ceiling::MAX_SESSION_SCROLLBACK_LINES_PER_PANE
        );
        assert_eq!(
            MAX_SESSION_LINE_TEXT_BYTES,
            ceiling::MAX_SESSION_LINE_TEXT_BYTES
        );
        assert_eq!(MAX_SESSION_CWD_BYTES, ceiling::MAX_SESSION_CWD_BYTES);
        assert_eq!(MAX_SESSION_NAME_CHARS, ceiling::MAX_SESSION_NAME_CHARS);
        assert_eq!(MAX_SESSION_LAYOUT_DEPTH, ceiling::MAX_SESSION_LAYOUT_DEPTH);
        assert_eq!(MAX_SESSION_GRID_DIM, ceiling::MAX_SESSION_GRID_DIM);
        assert_eq!(SESSION_FORMAT_VERSION, ceiling::SESSION_FORMAT_VERSION);
        assert_eq!(
            SESSION_MIN_DECODE_VERSION,
            ceiling::SESSION_MIN_DECODE_VERSION
        );
        assert_eq!(SESSION_APP_DIR_NAME, ceiling::SESSION_APP_DIR_NAME);
        assert_eq!(SESSIONS_DIR_NAME, ceiling::SESSIONS_DIR_NAME);
        assert_eq!(SESSION_FILE_NAME, ceiling::SESSION_FILE_NAME);
        assert_eq!(STORE_MAX_VALUE_BYTES, ceiling::STORE_MAX_VALUE_BYTES);
        assert_eq!(STORE_MAX_ENTRIES, ceiling::STORE_MAX_ENTRIES);
        assert_eq!(STORE_MAX_TOTAL_BYTES, ceiling::STORE_MAX_TOTAL_BYTES);
        assert_eq!(STORE_MAX_KEY_BYTES, ceiling::STORE_MAX_KEY_BYTES);
        assert_eq!(STORE_FILE_MAX_BYTES, ceiling::STORE_FILE_MAX_BYTES);
        assert_eq!(JSON_MAX_DEPTH, ceiling::JSON_MAX_DEPTH);
    }

    #[test]
    fn golden_encode_matches_pre_rewire_bytes() {
        for name in ["basic", "escapes", "detached", "multi", "empty"] {
            let snap = golden_snapshot(name);
            let bytes = backend()
                .encode_snapshot(&snap)
                .expect("golden snapshot encodes");
            let expected: &[u8] = match name {
                "basic" => include_bytes!("../tests/fixtures/session-parity/basic.session"),
                "escapes" => include_bytes!("../tests/fixtures/session-parity/escapes.session"),
                "detached" => {
                    include_bytes!("../tests/fixtures/session-parity/detached.session")
                }
                "multi" => include_bytes!("../tests/fixtures/session-parity/multi.session"),
                _ => include_bytes!("../tests/fixtures/session-parity/empty.session"),
            };
            assert_eq!(
                bytes.as_slice(),
                expected,
                "adapter encode must match the pre-rewire Core bytes for {name}"
            );
        }
    }

    #[test]
    fn golden_decode_is_a_fixed_point() {
        for name in ["basic", "escapes", "detached", "multi", "fullpane", "empty"] {
            let bytes: &[u8] = match name {
                "basic" => include_bytes!("../tests/fixtures/session-parity/basic.session"),
                "escapes" => include_bytes!("../tests/fixtures/session-parity/escapes.session"),
                "detached" => {
                    include_bytes!("../tests/fixtures/session-parity/detached.session")
                }
                "multi" => include_bytes!("../tests/fixtures/session-parity/multi.session"),
                "fullpane" => {
                    include_bytes!("../tests/fixtures/session-parity/fullpane.session")
                }
                _ => include_bytes!("../tests/fixtures/session-parity/empty.session"),
            };
            let snap = backend()
                .decode_snapshot(bytes)
                .expect("golden fixture decodes");
            assert_eq!(snap.version, SESSION_FORMAT_VERSION);
            let again = backend()
                .encode_snapshot(&snap)
                .expect("decoded snapshot re-encodes");
            assert_eq!(
                again.as_slice(),
                bytes,
                "decode/encode must be a fixed point for {name}"
            );
        }
    }

    #[test]
    fn golden_basic_resolves_legacy_attachment_through_startup_owner() {
        let bytes = include_bytes!("../tests/fixtures/session-parity/basic.session");
        let snap = backend()
            .decode_snapshot(bytes)
            .expect("basic fixture decodes");
        // The v1-legacy `None` attachment resolves through the startup-owner
        // derivation on encode (PRESERVE decision: Core validation counting
        // and the storage port both stay as-is).
        assert_eq!(
            snap.workspaces[0].panes[0].attach,
            Some(PaneAttachment::Primary)
        );
        assert_eq!(
            snap.workspaces[0].panes[0].scrollback,
            vec![String::from("hello world")]
        );
    }

    #[test]
    fn golden_escapes_preserve_layout_modes_and_history() {
        let bytes = include_bytes!("../tests/fixtures/session-parity/escapes.session");
        let snap = backend()
            .decode_snapshot(bytes)
            .expect("escapes fixture decodes");
        let ws = &snap.workspaces[0];
        assert_eq!(ws.layout.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        let mode = ws
            .layout
            .find_leaf(ViewId::new(1))
            .expect("leaf present")
            .presentation();
        assert_eq!(mode, PresentationMode::Floating);
        assert_eq!(
            ws.panes[0].scrollback,
            vec![
                String::from("back\\slash"),
                String::from("line\nbreak"),
                String::from("cr\rhere"),
                String::from("  padded  "),
                String::new(),
            ]
        );
    }

    #[test]
    fn v1_file_migrates_with_legacy_defaults() {
        let raw = concat!(
            "bitty-session v1\n",
            "workspaces 1 active 0 mru 0\n",
            "workspace 1 7\n",
            "name ws1\n",
            "layout (leaf 7 80 24)\n",
            "pane 7 80 24 1 0\n",
            "migrated line\n",
            "end-pane\n",
            "end-workspace\n",
            "end-session\n",
        );
        let snap = backend()
            .decode_snapshot(raw.as_bytes())
            .expect("v1 must migrate");
        assert_eq!(snap.version, SESSION_FORMAT_VERSION);
        let pane = &snap.workspaces[0].panes[0];
        assert_eq!(pane.attach, None, "v1 carries no attachment record");
        assert_eq!(pane.route, PaneRoute::Terminal);
        assert_eq!(pane.mode, PresentationMode::Tiled);
        assert_eq!(pane.scrollback, vec!["migrated line".to_string()]);
        let bytes = backend()
            .encode_snapshot(&snap)
            .expect("migrated snapshot encodes");
        assert!(bytes.starts_with(b"bitty-session v2\n"));
        let back = backend()
            .decode_snapshot(&bytes)
            .expect("v2 re-decode works");
        assert_eq!(
            back.workspaces[0].panes[0].attach,
            Some(PaneAttachment::Primary),
            "unspecified attachment resolves through the startup-owner derivation"
        );
    }

    #[test]
    fn v1_record_cannot_hide_v2_tokens() {
        let raw = concat!(
            "bitty-session v1\n",
            "workspaces 1 active 0 mru 0\n",
            "workspace 1 7\n",
            "name ws1\n",
            "layout (leaf 7 80 24)\n",
            "pane 7 80 24 0 0 primary terminal tiled\n",
            "end-pane\n",
            "end-workspace\n",
            "end-session\n",
        );
        assert!(backend().decode_snapshot(raw.as_bytes()).is_err());
    }

    #[test]
    fn v2_rejects_unknown_attach_route_mode_without_echo() {
        let pane_with = |header: &str| {
            format!(
                "bitty-session v2\nworkspaces 1 active 0 mru 0\nworkspace 1 7\nname ws1\nlayout (leaf 7 80 24)\n{header}\nend-pane\nend-workspace\nend-session\n"
            )
        };
        for (label, header) in [
            ("attach", "pane 7 80 24 0 0 floating terminal tiled"),
            ("route", "pane 7 80 24 0 0 session panel tiled"),
            ("mode", "pane 7 80 24 0 0 session terminal zoomed"),
            ("short", "pane 7 80 24 0 0"),
        ] {
            let err = backend()
                .decode_snapshot(pane_with(header).as_bytes())
                .expect_err(&format!("bad {label} must fail"));
            assert!(
                !format!("{err}").contains("ws1"),
                "errors must never echo session contents"
            );
        }
    }

    #[test]
    fn v2_rejects_bad_version_before_parsing() {
        let raw = "bitty-session v3\nworkspaces 1 active 0 mru 0\n";
        let err = backend()
            .decode_snapshot(raw.as_bytes())
            .expect_err("v3 must be rejected");
        assert!(matches!(
            err,
            bitty_runtime::SessionError::UnsupportedVersion(3)
        ));
    }

    #[test]
    fn kv_commit_produces_golden_bytes() {
        use bitty_lua::LuaValue;
        use bitty_runtime::plugin_runtime::PluginStore;

        let dir = std::env::temp_dir().join(format!(
            "bitty-kv-golden-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        let path = dir.join("store.json");
        let backend = StorageKvBackend::new();
        let mut store = PluginStore::with_backend(
            Some(path.clone()),
            Some(std::sync::Arc::new(backend)
                as std::sync::Arc<
                    dyn bitty_runtime::plugin_runtime::KvCommitBackend,
                >),
        );
        store
            .set("app.theme", LuaValue::String(String::from("dark\\mode\n")))
            .expect("set string");
        store
            .set("app.retries", LuaValue::Integer(3))
            .expect("set int");
        store
            .set("app.ratio", LuaValue::Number(0.5))
            .expect("set float");
        store
            .set("app.big", LuaValue::Number(1e20))
            .expect("set big float");
        store
            .set("app.enabled", LuaValue::Bool(true))
            .expect("set bool");
        store
            .set(
                "app.nested",
                LuaValue::Table(vec![
                    (LuaValue::String(String::from("a")), LuaValue::Integer(1)),
                    (
                        LuaValue::String(String::from("b")),
                        LuaValue::Table(vec![(
                            LuaValue::String(String::from("c")),
                            LuaValue::String(String::from("deep \u{00e9}")),
                        )]),
                    ),
                ]),
            )
            .expect("set nested");
        store
            .set(
                "app.list",
                LuaValue::array(vec![LuaValue::Integer(1), LuaValue::Integer(2)]),
            )
            .expect("set array");

        let committed = std::fs::read(&path).expect("read committed store");
        let expected = include_bytes!("../tests/fixtures/store-parity/store.json");
        assert_eq!(
            committed.as_slice(),
            expected.as_slice(),
            "storage-backed KV commit must match the pre-rewire Core bytes"
        );

        // The committed image loads back through the seam.
        let reloaded = PluginStore::load_with_backend(
            path.clone(),
            Some(std::sync::Arc::new(backend)
                as std::sync::Arc<
                    dyn bitty_runtime::plugin_runtime::KvCommitBackend,
                >),
        )
        .expect("reload committed store");
        assert_eq!(
            reloaded.get("app.theme"),
            Some(LuaValue::String(String::from("dark\\mode\n")))
        );
        assert_eq!(reloaded.get("app.retries"), Some(LuaValue::Integer(3)));

        // No temp litter survives a commit.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp litter: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kv_missing_load_starts_clean_and_ceiling_holds() {
        use bitty_runtime::plugin_runtime::PluginStore;

        let dir = std::env::temp_dir().join(format!(
            "bitty-kv-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        let path = dir.join("store.json");
        let backend = StorageKvBackend::new();
        let handle: std::sync::Arc<dyn bitty_runtime::plugin_runtime::KvCommitBackend> =
            std::sync::Arc::new(backend);

        // Missing file is a quiet clean start.
        let empty = PluginStore::load_with_backend(path.clone(), Some(handle.clone()))
            .expect("missing store loads empty");
        assert!(empty.is_empty());

        // Over-ceiling images fail closed.
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(&path, vec![b'x'; STORE_FILE_MAX_BYTES + 1]).expect("seed oversize");
        assert!(
            PluginStore::load_with_backend(path.clone(), Some(handle.clone())).is_err(),
            "over-ceiling store must fail"
        );

        // Corrupt images fail closed with content-free errors.
        std::fs::write(&path, b"{not json").expect("seed corrupt");
        let err = PluginStore::load_with_backend(path, Some(handle))
            .expect_err("corrupt store must fail");
        assert!(err.contains("store parse"), "content-free denial: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn kv_committed_file_is_owner_only() {
        use bitty_lua::LuaValue;
        use bitty_runtime::plugin_runtime::PluginStore;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "bitty-kv-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        let path = dir.join("store.json");
        let backend = StorageKvBackend::new();
        let mut store = PluginStore::with_backend(
            Some(path.clone()),
            Some(std::sync::Arc::new(backend)
                as std::sync::Arc<
                    dyn bitty_runtime::plugin_runtime::KvCommitBackend,
                >),
        );
        store
            .set("k", LuaValue::String(String::from("v")))
            .expect("commit");
        let mode = std::fs::metadata(&path)
            .expect("stat store")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "committed KV files stay user-only");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
