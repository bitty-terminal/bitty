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
//!
//! Session versions (CTX-1082): the storage codec speaks v1/v2 workspace
//! blocks. Core moved to v3 (window-global pinned block after the
//! workspace blocks); this adapter frames v3 around the storage codec —
//! v2 snapshots still encode byte-identically (the parity path), v3
//! snapshots splice the pinned block onto a v2-shaped body, and every
//! version decodes migrating in memory to the Core current version with an
//! empty pinned store for v1/v2 input.

use std::path::{Path, PathBuf};

use bitty_runtime::{
    LayoutNode, MAX_SESSION_CWD_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_GRID_DIM,
    MAX_SESSION_LINE_BYTES, MAX_SESSION_LINE_TEXT_BYTES, MAX_SESSION_PANES_TOTAL,
    MAX_SESSION_SCROLLBACK_LINES_PER_PANE, MAX_SESSION_WORKSPACES, PaneAttachment, PaneRoute,
    PinnedSnapshot, PresentationMode, SessionError, SessionSnapshot, SplitAxis, View, ViewId,
    WorkspaceSnapshot,
};
use bitty_runtime::{PaneSnapshot, SESSION_FORMAT_VERSION};
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
        // The storage model is v2-shaped: the workspace blocks encode
        // identically for v2 snapshots and as the v2-shaped body of v3
        // snapshots (the adapter splices the pinned block around them).
        version: bitty_storage::ceiling::SESSION_FORMAT_VERSION,
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
        // Migration normalizes at decode (CW-16): downstream only ever
        // sees the Core current version; v1/v2 input yields an empty
        // pinned store (those versions carry no pinned block).
        version: SESSION_FORMAT_VERSION,
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
        pinned: Vec::new(),
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

// ---------------------------------------------------------------------------
// v3 pinned framing (CTX-1082)
//
// The storage codec speaks v1/v2 workspace blocks. v3 appends one
// window-global `pinned` block after the workspace blocks:
//
// ```text
// bitty-session v3
// workspaces <n> active <a> mru <m0,m1,...>
// <v2 workspace blocks, byte-identical to a v2 file body>
// pinned <p>
// pin <view-id> <cols> <rows> <k> <cwd:0|1> <attach> <anchor-id|none> <after:0|1>
// <escaped cwd, iff cwd == 1>
// <k escaped scrollback lines, oldest first>
// end-pin
// end-session
// ```
//
// Pinned leaves are floating terminal panels by construction, so the pin
// record carries no route/mode tokens (a future route or mode arrives with
// its own format version). Encode splices the pinned block onto a
// v2-shaped body from the storage codec; decode strips it back off and
// delegates the workspace blocks, so the workspace codec can never drift
// between versions. Every bound mirrors the Core gate fail-closed.
// ---------------------------------------------------------------------------

/// Magic line for `version`: `bitty-session v<version>`.
fn session_magic(version: u32) -> String {
    format!("bitty-session v{version}")
}

/// Magic version of file `bytes` from the first line, if it parses.
fn file_magic_version(bytes: &[u8]) -> Option<u32> {
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?;
    text.strip_prefix("bitty-session v")?.parse().ok()
}

/// Core attachment to file token (mirrors the pane record spelling).
fn core_attachment_to_file(attach: PaneAttachment) -> &'static str {
    match attach {
        PaneAttachment::Primary => "primary",
        PaneAttachment::Session => "session",
        PaneAttachment::Detached => "detached",
    }
}

/// File token to Core attachment; `None` fails closed (never defaulted).
fn file_attachment_to_core(token: &str) -> Option<PaneAttachment> {
    match token {
        "primary" => Some(PaneAttachment::Primary),
        "session" => Some(PaneAttachment::Session),
        "detached" => Some(PaneAttachment::Detached),
        _ => None,
    }
}

/// Validates the pinned block for the file path (mirrors the Core gate so
/// no caller can bypass the ceilings by handing a snapshot straight to
/// encode; content-free errors throughout).
fn validate_pinned_for_file(
    pinned: &[PinnedSnapshot],
    layout_leaves: &std::collections::BTreeSet<ViewId>,
) -> Result<(), SessionError> {
    let mut seen = std::collections::BTreeSet::new();
    for pin in pinned {
        if !seen.insert(pin.view.id()) || layout_leaves.contains(&pin.view.id()) {
            return Err(SessionError::Corrupt("duplicate pane"));
        }
        if pin.view.presentation() != PresentationMode::Floating {
            return Err(SessionError::Corrupt("pinned mode"));
        }
        let (cols, rows) = (usize::from(pin.view.cols()), usize::from(pin.view.rows()));
        if !(1..=MAX_SESSION_GRID_DIM).contains(&cols)
            || !(1..=MAX_SESSION_GRID_DIM).contains(&rows)
        {
            return Err(SessionError::Corrupt("pinned dims"));
        }
        if let Some(cwd) = &pin.cwd {
            if cwd.len() > MAX_SESSION_CWD_BYTES {
                return Err(SessionError::Corrupt("cwd bound"));
            }
        }
        if pin.scrollback.len() > MAX_SESSION_SCROLLBACK_LINES_PER_PANE {
            return Err(SessionError::Corrupt("scrollback bound"));
        }
        for line in &pin.scrollback {
            if line.len() > MAX_SESSION_LINE_TEXT_BYTES {
                return Err(SessionError::Corrupt("scrollback line bound"));
            }
        }
        if pin.attach == PaneAttachment::Detached
            && (pin.cwd.is_some() || !pin.scrollback.is_empty())
        {
            return Err(SessionError::Corrupt("detached pane state"));
        }
    }
    Ok(())
}

/// Serializes the pinned block (header plus one `pin` record per entry).
fn encode_pinned_block(pinned: &[PinnedSnapshot]) -> Vec<String> {
    let mut out = vec![format!("pinned {}", pinned.len())];
    for pin in pinned {
        let cwd_flag = u8::from(pin.cwd.is_some());
        let anchor = pin
            .anchor
            .map_or_else(|| String::from("none"), |id| id.0.to_string());
        let after = u8::from(pin.after);
        out.push(format!(
            "pin {} {} {} {} {cwd_flag} {} {anchor} {after}",
            pin.view.id().0,
            usize::from(pin.view.cols()),
            usize::from(pin.view.rows()),
            pin.scrollback.len(),
            core_attachment_to_file(pin.attach),
        ));
        if let Some(cwd) = &pin.cwd {
            out.push(session_codec::escape_field(cwd));
        }
        for line in &pin.scrollback {
            out.push(session_codec::escape_field(line));
        }
        out.push(String::from("end-pin"));
    }
    out
}

/// Encodes a v3 snapshot: a v2-shaped workspace body from the storage
/// codec with the magic rewritten and the pinned block spliced before
/// `end-session`. Fails closed before any I/O when any ceiling trips,
/// including escaped pinned lines that would exceed the decode line cap.
fn encode_v3_snapshot(snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
    let layout_leaves: std::collections::BTreeSet<ViewId> = snap
        .workspaces
        .iter()
        .flat_map(|ws| ws.layout.leaf_ids())
        .collect();
    validate_pinned_for_file(&snap.pinned, &layout_leaves)?;
    let layout_panes: usize = snap.workspaces.iter().map(|ws| ws.panes.len()).sum();
    if layout_panes + snap.pinned.len() > MAX_SESSION_PANES_TOTAL {
        return Err(SessionError::Corrupt("too many panes"));
    }
    let body = session_codec::encode_session(&core_snapshot_to_storage(snap))
        .map_err(map_session_error)?;
    let text = String::from_utf8(body).map_err(|_| SessionError::Corrupt("encoding"))?;
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    // The codec always terminates with a trailing newline: pop it, assert
    // the exact framing shape, splice, then re-terminate.
    if lines.pop().as_deref() != Some("") {
        return Err(SessionError::Corrupt("encoding"));
    }
    if lines.first().map(String::as_str)
        != Some(session_magic(bitty_storage::ceiling::SESSION_FORMAT_VERSION).as_str())
        || lines.last().map(String::as_str) != Some("end-session")
    {
        return Err(SessionError::Corrupt("encoding"));
    }
    lines[0] = session_magic(SESSION_FORMAT_VERSION);
    lines.pop();
    lines.extend(encode_pinned_block(&snap.pinned));
    lines.push(String::from("end-session"));
    lines.push(String::new());
    let out = lines.join("\n");
    // Self-compatibility: escaped pinned lines can exceed the decode line
    // cap while the raw text stays within its own bound. Reject here,
    // fail-closed before any I/O, so accepted output always decodes.
    for line in out.split('\n') {
        if line.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionError::TooLarge {
                what: "session line",
                actual: line.len(),
                limit: MAX_SESSION_LINE_BYTES,
            });
        }
    }
    let bytes = out.into_bytes();
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge {
            what: "session file",
            actual: bytes.len(),
            limit: MAX_SESSION_FILE_BYTES,
        });
    }
    Ok(bytes)
}

/// Parses one `pin ... end-pin` record; `head` is the already-read header.
fn parse_pin_record(
    lines: &[&str],
    pos: &mut usize,
    head: &str,
) -> Result<PinnedSnapshot, SessionError> {
    let head = head
        .strip_prefix("pin ")
        .ok_or(SessionError::Corrupt("pin"))?;
    let parts: Vec<&str> = head.split(' ').collect();
    let [
        id_raw,
        cols_raw,
        rows_raw,
        count_raw,
        cwd_raw,
        attach_raw,
        anchor_raw,
        after_raw,
    ] = parts.as_slice()
    else {
        return Err(SessionError::Corrupt("pin"));
    };
    let id: u64 = id_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pin id"))?;
    let cols: usize = cols_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pin dims"))?;
    let rows: usize = rows_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pin dims"))?;
    if !(1..=MAX_SESSION_GRID_DIM).contains(&cols) || !(1..=MAX_SESSION_GRID_DIM).contains(&rows) {
        return Err(SessionError::Corrupt("pinned dims"));
    }
    let count: usize = count_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pane lines"))?;
    if count > MAX_SESSION_SCROLLBACK_LINES_PER_PANE {
        return Err(SessionError::Corrupt("scrollback bound"));
    }
    let attach = file_attachment_to_core(attach_raw).ok_or(SessionError::Corrupt("attach"))?;
    let anchor = match *anchor_raw {
        "none" => None,
        raw => Some(
            raw.parse()
                .map(ViewId::new)
                .map_err(|_| SessionError::Corrupt("pin anchor"))?,
        ),
    };
    let after = match *after_raw {
        "0" => false,
        "1" => true,
        _ => return Err(SessionError::Corrupt("pin")),
    };
    let cwd = match *cwd_raw {
        "0" => None,
        "1" => {
            let decoded =
                session_codec::unescape_field(next(lines, pos)?).map_err(map_session_error)?;
            if decoded.len() > MAX_SESSION_CWD_BYTES {
                return Err(SessionError::Corrupt("cwd bound"));
            }
            Some(decoded)
        }
        _ => return Err(SessionError::Corrupt("pin")),
    };
    let mut scrollback = Vec::with_capacity(count);
    for _ in 0..count {
        let decoded =
            session_codec::unescape_field(next(lines, pos)?).map_err(map_session_error)?;
        if decoded.len() > MAX_SESSION_LINE_TEXT_BYTES {
            return Err(SessionError::Corrupt("scrollback line bound"));
        }
        scrollback.push(decoded);
    }
    if next(lines, pos)? != "end-pin" {
        return Err(SessionError::Corrupt("marker"));
    }
    Ok(PinnedSnapshot {
        view: View::with_presentation(ViewId::new(id), cols, rows, PresentationMode::Floating),
        cwd,
        scrollback,
        attach,
        anchor,
        after,
    })
}

/// Advances the line cursor, failing closed on truncation.
fn next<'a>(lines: &[&'a str], pos: &mut usize) -> Result<&'a str, SessionError> {
    let line = lines
        .get(*pos)
        .copied()
        .ok_or(SessionError::Corrupt("truncated"))?;
    *pos += 1;
    Ok(line)
}

/// Workspace count from a `workspaces <n> active <a> mru <...>` header,
/// bounded fail-closed (the delegated workspace decode re-validates the
/// header fully afterward; this only sizes the header walk).
fn parse_workspace_count(header: &str) -> Result<usize, SessionError> {
    let rest = header
        .strip_prefix("workspaces ")
        .ok_or(SessionError::Corrupt("header"))?;
    let (count_raw, _) = rest
        .split_once(' ')
        .ok_or(SessionError::Corrupt("header"))?;
    let count: usize = count_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("header"))?;
    if count == 0 || count > MAX_SESSION_WORKSPACES {
        return Err(SessionError::Corrupt("workspace count"));
    }
    Ok(count)
}

/// Index of the `pinned <n>` header line, found by walking the workspace
/// blocks count-driven.
///
/// Never by marker scan: scrollback content may contain marker-like lines
/// (`pinned 1`, `end-pin`), while the count-driven walk skips them exactly
/// like the storage parser does. Every shape violation here is
/// [`SessionError::Corrupt`]; the delegated workspace decode re-validates
/// the blocks fully afterward. v3 workspace blocks are always v2-shaped
/// (the encoder never writes v1 records into a v3 file).
fn find_pinned_header(lines: &[&str]) -> Result<usize, SessionError> {
    let count = parse_workspace_count(
        lines
            .get(1)
            .copied()
            .ok_or(SessionError::Corrupt("truncated"))?,
    )?;
    let mut pos = 2;
    for _ in 0..count {
        // `workspace <seq> <focus>`, `name <...>`, `layout <...>` in order.
        if lines
            .get(pos)
            .is_none_or(|line| !line.starts_with("workspace "))
        {
            return Err(SessionError::Corrupt("workspace"));
        }
        pos += 1;
        if lines.get(pos).is_none_or(|line| !line.starts_with("name ")) {
            return Err(SessionError::Corrupt("name"));
        }
        pos += 1;
        if lines
            .get(pos)
            .is_none_or(|line| !line.starts_with("layout "))
        {
            return Err(SessionError::Corrupt("layout"));
        }
        pos += 1;
        // Pane records until `end-workspace`, skipped count-driven.
        loop {
            let line = lines
                .get(pos)
                .copied()
                .ok_or(SessionError::Corrupt("truncated"))?;
            pos += 1;
            if line == "end-workspace" {
                break;
            }
            let rest = line
                .strip_prefix("pane ")
                .ok_or(SessionError::Corrupt("pane"))?;
            let parts: Vec<&str> = rest.split(' ').collect();
            if parts.len() != 8 {
                return Err(SessionError::Corrupt("pane"));
            }
            let scrollback: usize = parts[3]
                .parse()
                .map_err(|_| SessionError::Corrupt("pane lines"))?;
            match parts[4] {
                "0" => {}
                "1" => pos = pos.saturating_add(1),
                _ => return Err(SessionError::Corrupt("pane")),
            }
            pos = pos.saturating_add(scrollback);
            if lines.get(pos) != Some(&"end-pane") {
                return Err(SessionError::Corrupt("marker"));
            }
            pos += 1;
        }
    }
    match lines.get(pos) {
        Some(line) if line.starts_with("pinned ") => Ok(pos),
        _ => Err(SessionError::Corrupt("marker")),
    }
}

/// Decodes v3 file bytes: strips the pinned block, delegates the workspace
/// blocks to the storage codec (which validates them fully), then merges.
/// Any violation rejects the whole file; the returned snapshot always
/// carries the Core current version.
fn decode_v3_snapshot(bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge {
            what: "session file",
            actual: bytes.len(),
            limit: MAX_SESSION_FILE_BYTES,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| SessionError::Corrupt("utf-8"))?;
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in &lines {
        if line.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionError::TooLarge {
                what: "session line",
                actual: line.len(),
                limit: MAX_SESSION_LINE_BYTES,
            });
        }
    }
    if lines.first() != Some(&session_magic(SESSION_FORMAT_VERSION).as_str()) {
        return Err(SessionError::Corrupt("magic"));
    }
    // Walk the workspace blocks count-driven (never a marker scan:
    // scrollback content may contain marker-like lines).
    let pinned_at = find_pinned_header(&lines)?;
    let pinned_count: usize = lines[pinned_at]
        .strip_prefix("pinned ")
        .ok_or(SessionError::Corrupt("marker"))?
        .parse()
        .map_err(|_| SessionError::Corrupt("marker"))?;
    // Delegate the workspace blocks as a v2-shaped body: the v3 magic
    // rewrites to v2 and the pinned block swaps for the session trailer.
    let mut body = vec![session_magic(
        bitty_storage::ceiling::SESSION_FORMAT_VERSION,
    )];
    body.extend(lines[1..pinned_at].iter().map(|line| (*line).to_owned()));
    body.push(String::from("end-session"));
    body.push(String::new());
    let storage_snap =
        session_codec::decode_session(body.join("\n").as_bytes()).map_err(map_session_error)?;
    let mut snap = storage_snapshot_to_core(&storage_snap);
    // Parse the pinned records between the `pinned <n>` header and the
    // `end-session` trailer.
    let mut pos = pinned_at + 1;
    let mut pinned = Vec::with_capacity(pinned_count);
    for _ in 0..pinned_count {
        let head = lines
            .get(pos)
            .copied()
            .ok_or(SessionError::Corrupt("truncated"))?;
        pos += 1;
        pinned.push(parse_pin_record(&lines, &mut pos, head)?);
    }
    if lines.get(pos) != Some(&"end-session") || pos + 1 != lines.len() {
        return Err(SessionError::Corrupt("marker"));
    }
    // The workspace half is storage-validated; the pinned half enforces
    // the same gate here so no caller bypasses the ceilings (apply
    // re-validates the merged snapshot before any mutation).
    let layout_leaves: std::collections::BTreeSet<ViewId> = snap
        .workspaces
        .iter()
        .flat_map(|ws| ws.layout.leaf_ids())
        .collect();
    validate_pinned_for_file(&pinned, &layout_leaves)?;
    let layout_panes: usize = snap.workspaces.iter().map(|ws| ws.panes.len()).sum();
    if layout_panes + pinned.len() > MAX_SESSION_PANES_TOTAL {
        return Err(SessionError::Corrupt("too many panes"));
    }
    snap.pinned = pinned;
    Ok(snap)
}

impl bitty_runtime::SessionFileBackend for StorageSessionBackend {
    fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
        // Encode preserves the snapshot version: v2 snapshots still encode
        // byte-identically (the parity path the golden fixtures pin), while
        // v3 snapshots frame the pinned block around a v2-shaped body.
        // Production capture always emits v3; decode migrates everything to
        // v3 in memory, so the one-way drift matches the CW-16 precedent.
        match snap.version {
            2 if snap.pinned.is_empty() => {
                session_codec::encode_session(&core_snapshot_to_storage(snap))
                    .map_err(map_session_error)
            }
            2 => Err(SessionError::Corrupt("pinned version")),
            SESSION_FORMAT_VERSION => encode_v3_snapshot(snap),
            other => Err(SessionError::UnsupportedVersion(other)),
        }
    }

    fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
        // v3 frames around the storage codec; every older version delegates
        // and migrates in memory to the Core current version.
        if file_magic_version(bytes) == Some(SESSION_FORMAT_VERSION) {
            decode_v3_snapshot(bytes)
        } else {
            session_codec::decode_session(bytes)
                .map(|snap| storage_snapshot_to_core(&snap))
                .map_err(map_session_error)
        }
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
    /// outputs for exactly these inputs. Pinned to version 2 (CTX-1082):
    /// Core moved to v3, but the parity fixtures stay v2 so the adapter's
    /// v2 encode path keeps proving byte-identical outputs.
    fn golden_snapshot(name: &str) -> SessionSnapshot {
        match name {
            "basic" => SessionSnapshot {
                version: 2,
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
                pinned: Vec::new(),
            },
            "escapes" => SessionSnapshot {
                version: 2,
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
                pinned: Vec::new(),
            },
            "detached" => SessionSnapshot {
                version: 2,
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
                pinned: Vec::new(),
            },
            "multi" => SessionSnapshot {
                version: 2,
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
                pinned: Vec::new(),
            },
            "empty" => SessionSnapshot {
                version: 2,
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
                pinned: Vec::new(),
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
        // CTX-1082: intentional version divergence. Core moved to v3
        // (window-global pinned block); the storage codec stays v2-shaped
        // and this adapter frames v3 around it, so v2 snapshots still
        // encode byte-identically through the parity path below.
        assert_eq!(SESSION_FORMAT_VERSION, 3);
        assert_eq!(ceiling::SESSION_FORMAT_VERSION, 2);
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
    fn golden_decode_migrates_v2_to_v3_with_empty_pinned() {
        // CTX-1082: decode migrates v2 fixtures in memory to v3 with an
        // empty pinned store (CW-16 one-way drift); the re-encode is a v3
        // fixed point, not the v2 input bytes.
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
            assert!(
                snap.pinned.is_empty(),
                "v2 fixtures carry no pinned block for {name}"
            );
            let again = backend()
                .encode_snapshot(&snap)
                .expect("migrated snapshot re-encodes");
            assert!(
                again.starts_with(b"bitty-session v3\n"),
                "re-encoding writes v3 for {name}"
            );
            let back = backend()
                .decode_snapshot(&again)
                .expect("v3 re-decode works");
            assert_eq!(snap, back, "v3 decode/encode is a fixed point for {name}");
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
        assert!(snap.pinned.is_empty(), "v1 carries no pinned block");
        let pane = &snap.workspaces[0].panes[0];
        assert_eq!(pane.attach, None, "v1 carries no attachment record");
        assert_eq!(pane.route, PaneRoute::Terminal);
        assert_eq!(pane.mode, PresentationMode::Tiled);
        assert_eq!(pane.scrollback, vec!["migrated line".to_string()]);
        let bytes = backend()
            .encode_snapshot(&snap)
            .expect("migrated snapshot encodes");
        assert!(bytes.starts_with(b"bitty-session v3\n"));
        let back = backend()
            .decode_snapshot(&bytes)
            .expect("v3 re-decode works");
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
        // CTX-1082: v3 is live now, so the unsupported probe moves to v4.
        let raw = "bitty-session v4\nworkspaces 1 active 0 mru 0\n";
        let err = backend()
            .decode_snapshot(raw.as_bytes())
            .expect_err("v4 must be rejected");
        assert!(matches!(
            err,
            bitty_runtime::SessionError::UnsupportedVersion(4)
        ));
        // A v3 magic with a truncated body still fails closed as corrupt,
        // never as a version error.
        let truncated = "bitty-session v3\nworkspaces 1 active 0 mru 0\n";
        assert!(matches!(
            backend().decode_snapshot(truncated.as_bytes()),
            Err(bitty_runtime::SessionError::Corrupt(_))
        ));
    }

    /// One pinned entry with history, cwd, and a recorded anchor (v3).
    fn pinned_entry(id: u64, attach: PaneAttachment, anchor: Option<u64>) -> PinnedSnapshot {
        PinnedSnapshot {
            view: View::with_presentation(ViewId::new(id), 80, 24, PresentationMode::Floating),
            cwd: Some(String::from("file:///tmp/pinned")),
            scrollback: vec![
                String::from("back\\slash"),
                String::from("line\nbreak"),
                String::from("pinned history"),
            ],
            attach,
            anchor: anchor.map(ViewId::new),
            after: false,
        }
    }

    /// v3 workspace half shared by the pinned file tests: two tiled leaves
    /// with the primary on the focus owner.
    fn pinned_workspace() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            seq: 1,
            name: String::from("ws1"),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                leaf(1, PresentationMode::Tiled),
                leaf(2, PresentationMode::Tiled),
            ),
            focus: Some(ViewId::new(1)),
            panes: vec![
                pane(
                    1,
                    None,
                    &[],
                    Some(PaneAttachment::Primary),
                    PresentationMode::Tiled,
                ),
                pane(
                    2,
                    None,
                    &["live"],
                    Some(PaneAttachment::Session),
                    PresentationMode::Tiled,
                ),
            ],
        }
    }

    fn v3_snapshot_with_pinned() -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces: vec![pinned_workspace()],
            active: 0,
            mru: vec![0],
            pinned: vec![pinned_entry(9, PaneAttachment::Session, Some(2))],
        }
    }

    #[test]
    fn v3_pinned_block_round_trips_through_file() {
        let snap = v3_snapshot_with_pinned();
        let bytes = backend().encode_snapshot(&snap).expect("v3 encodes");
        let text = std::str::from_utf8(&bytes).expect("v3 is utf-8");
        assert!(
            text.starts_with("bitty-session v3\n"),
            "v3 snapshots write the v3 magic"
        );
        assert!(
            text.contains("\npinned 1\n"),
            "the pinned block follows the workspace blocks"
        );
        assert!(
            text.contains("pin 9 80 24 3 1 session 2 0\n"),
            "the pin record carries geometry, history count, cwd flag, attach, and anchor: {text:?}"
        );
        assert!(text.ends_with("end-pin\nend-session\n"));
        let back = backend().decode_snapshot(&bytes).expect("v3 decodes");
        assert_eq!(snap, back, "v3 file round-trips byte-identically");
        // Re-encoding the decoded snapshot is a fixed point.
        let again = backend().encode_snapshot(&back).expect("v3 re-encodes");
        assert_eq!(bytes, again, "v3 decode/encode is a fixed point");
    }

    #[test]
    fn v3_empty_pinned_block_round_trips() {
        let mut snap = v3_snapshot_with_pinned();
        snap.pinned.clear();
        let bytes = backend().encode_snapshot(&snap).expect("v3 encodes");
        let text = std::str::from_utf8(&bytes).expect("v3 is utf-8");
        assert!(text.contains("\npinned 0\nend-session\n"));
        let back = backend().decode_snapshot(&bytes).expect("v3 decodes");
        assert_eq!(snap, back);
    }

    #[test]
    fn v3_marker_like_scrollback_never_confuses_the_decoder() {
        // Scrollback is untrusted terminal output: lines mimicking frame
        // markers must ride as content, never reframe the parse. The
        // pinned header is found by a count-driven walk, not a scan.
        let mut snap = v3_snapshot_with_pinned();
        snap.workspaces[0].panes[1].scrollback = vec![
            String::from("pinned 1"),
            String::from("pin 9 80 24 0 0 session none 1"),
            String::from("end-pin"),
            String::from("end-session"),
            String::from("end-workspace"),
        ];
        snap.pinned[0].scrollback = vec![
            String::from("pinned 0"),
            String::from("end-pin"),
            String::from("end-session"),
        ];
        let bytes = backend().encode_snapshot(&snap).expect("v3 encodes");
        let back = backend().decode_snapshot(&bytes).expect("v3 decodes");
        assert_eq!(snap, back, "marker-like content round-trips as content");
    }

    #[test]
    fn v3_rejects_bad_pin_records_without_echo() {
        let v3_with_pin = |pin_lines: &str| {
            format!(
                "bitty-session v3\nworkspaces 1 active 0 mru 0\nworkspace 1 1\nname ws1\nlayout (leaf 1 80 24)\npane 1 80 24 0 0 session terminal tiled\nend-pane\nend-workspace\npinned 1\n{pin_lines}end-session\n"
            )
        };
        // Well-formed control record (anchor none, after set).
        let good = "pin 9 80 24 0 0 session none 1\nend-pin\n";
        backend()
            .decode_snapshot(v3_with_pin(good).as_bytes())
            .expect("control pin decodes");
        for (label, pin_lines) in [
            ("attach", "pin 9 80 24 0 0 floating none 1\nend-pin\n"),
            ("short", "pin 9 80 24 0 0 session\nend-pin\n"),
            ("anchor", "pin 9 80 24 0 0 session nope 1\nend-pin\n"),
            ("after", "pin 9 80 24 0 0 session none 2\nend-pin\n"),
            ("trailer", "pin 9 80 24 0 0 session none 1\n"),
            ("count", "pin 9 80 24 1 0 session none 1\nend-pin\n"),
            ("dims", "pin 9 0 24 0 0 session none 1\nend-pin\n"),
        ] {
            let err = backend()
                .decode_snapshot(v3_with_pin(pin_lines).as_bytes())
                .expect_err(&format!("bad pin {label} must fail"));
            assert!(
                !format!("{err}").contains("ws1"),
                "errors must never echo session contents"
            );
        }
        // A pinned id aliasing a layout leaf is corrupt.
        let alias = "pin 1 80 24 0 0 session none 1\nend-pin\n";
        let err = backend()
            .decode_snapshot(v3_with_pin(alias).as_bytes())
            .expect_err("pinned/layout alias must fail");
        assert_eq!(format!("{err}"), "session file corrupt (duplicate pane)");
        // A session-less pinned leaf carrying state is corrupt.
        let smuggled = "pin 9 80 24 1 0 detached none 1\nstowaway\nend-pin\n";
        let err = backend()
            .decode_snapshot(v3_with_pin(smuggled).as_bytes())
            .expect_err("stateful detached pin must fail");
        assert_eq!(
            format!("{err}"),
            "session file corrupt (detached pane state)"
        );
        // A v3 file without the pinned block is corrupt.
        let no_block = "bitty-session v3\nworkspaces 1 active 0 mru 0\nworkspace 1 1\nname ws1\nlayout (leaf 1 80 24)\npane 1 80 24 0 0 session terminal tiled\nend-pane\nend-workspace\nend-session\n";
        assert!(backend().decode_snapshot(no_block.as_bytes()).is_err());
    }

    #[test]
    fn v2_snapshot_claiming_pinned_fails_encode() {
        let mut snap = v3_snapshot_with_pinned();
        snap.version = 2;
        let err = backend()
            .encode_snapshot(&snap)
            .expect_err("v2 with pinned must fail");
        assert_eq!(format!("{err}"), "session file corrupt (pinned version)");
        // Without the pinned claim the same workspace half still encodes
        // through the v2 parity path.
        snap.pinned.clear();
        let bytes = backend()
            .encode_snapshot(&snap)
            .expect("v2 without pinned encodes");
        assert!(bytes.starts_with(b"bitty-session v2\n"));
    }

    #[test]
    fn future_version_snapshot_fails_encode() {
        let mut snap = v3_snapshot_with_pinned();
        snap.version = SESSION_FORMAT_VERSION + 1;
        let err = backend()
            .encode_snapshot(&snap)
            .expect_err("future version must fail");
        assert!(matches!(
            err,
            bitty_runtime::SessionError::UnsupportedVersion(_)
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
