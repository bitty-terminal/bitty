//! Atomic session save/restore (CTX-0393, issue #649).
//!
//! Browser-like restore: reopening bitty recreates workspaces, the layout
//! tree, focus, scrollback history, and per-pane working directories.
//! Like a browser tab restore, layout and history come back but dead
//! processes do not: shells respawn fresh in the captured `OSC 7` cwd
//! (fail-open to the platform default when the report is missing, stale, or
//! no longer a directory) and scrollback rehydrates as immutable history.
//!
//! # Format (v1, hand-rolled, no new dependencies)
//!
//! A UTF-8 text file, `\n`-separated, magic first line `bitty-session v1`:
//!
//! ```text
//! bitty-session v1
//! workspaces <n> active <a> mru <m0,m1,...>
//! workspace <seq> <focus-id|none>
//! name <escaped workspace name>
//! layout <s-expr: (leaf id cols rows) | (split h|v ratio-bits first second) | (stack children...)>
//! pane <view-id> <cols> <rows> <k> <cwd:0|1>
//! <escaped cwd, iff cwd == 1>
//! <k escaped scrollback lines, oldest first>
//! end-pane
//! end-workspace
//! end-session
//! ```
//!
//! Field escaping is backslash-only (`\\` → `\`, `\n` → LF, `\r` → CR);
//! whole-line fields (name, cwd, scrollback) may contain spaces. Transient
//! overlays (help popup, palette) are never persisted: capture strips them
//! to the base tree. Split ratios persist as `f32` bits so the value
//! round-trips exactly and re-clamps through [`LayoutNode::split`].
//!
//! # Atomicity
//!
//! Saves write a temp sibling (`<file>.tmp.<pid>`), `write_all` +
//! [`File::sync_all`](std::fs::File::sync_all), then atomically rename onto
//! the final name and best-effort sync the parent dir. A crash can only
//! leave a temp file behind: temps are never read, so a partial write is
//! always ignored (the previous complete session stays authoritative).
//! Concurrent savers are last-writer-wins; each rename is still atomic, so
//! the file is never partial. Saves clean stale `<file>.tmp.*` siblings
//! best-effort. Oversize snapshots fail closed *before* touching the
//! filesystem, so a failed save never truncates a good session.
//!
//! # Bounds (fail-closed: any violation rejects the whole file)
//!
//! | Bound | Value | Source |
//! |---|---|---|
//! | File bytes | [`MAX_SESSION_FILE_BYTES`] (1 MiB) | issue: bounded size |
//! | Line bytes | [`MAX_SESSION_LINE_BYTES`] (4096) | parser bound parity |
//! | Workspaces | [`MAX_SESSION_WORKSPACES`] (16) | `MAX_WORKSPACES` |
//! | Panes / workspace | [`MAX_SESSION_PANES_PER_WORKSPACE`] (32) | registry bound |
//! | Panes total | [`MAX_SESSION_PANES_TOTAL`] (128) | decode-CPU guard |
//! | Scrollback lines / pane | [`MAX_SESSION_SCROLLBACK_LINES_PER_PANE`] (200) | issue: cap restored lines |
//! | Scrollback line bytes | [`MAX_SESSION_LINE_BYTES`] (4096) | parser bound parity |
//! | Cwd bytes | [`MAX_SESSION_CWD_BYTES`] (4096) | `SHELL_CWD_MAX_BYTES` |
//! | Workspace name chars | [`MAX_SESSION_NAME_CHARS`] (32) | workspaceline bound |
//! | Layout depth | [`MAX_SESSION_LAYOUT_DEPTH`] (64) | recursion guard |
//! | View dims | `1..=1000` per axis | `MAX_GRID_DIM` |
//!
//! A corrupt, oversize, or version-mismatched file always yields a
//! [`SessionError`] and the caller starts clean (see
//! [`Runtime::restore_session_on_startup`]); the runtime is never left
//! half-restored — validation runs fully before any mutation.
//!
//! # Paths (no hardcoded hosts)
//!
//! Sessions live under `$XDG_STATE_HOME/bitty/sessions/session` with the
//! XDG fallback `$HOME/.local/state/...`; every helper takes injected
//! environment values so tests stay hermetic. Resolution returns `None`
//! (fail-closed, never a panic) when neither root is usable.
//!
//! # Trust posture (read before changing)
//!
//! The session file contains working-directory URLs and scrollback text,
//! which routinely include sensitive material (typed secrets, tokens in
//! command output, private paths). It is plaintext with no encryption; on
//! Unix the file is created mode `0600` (a chmod failure aborts the save
//! rather than leaving a readable file). Session contents are never written
//! to logs: every error carries only kinds and counts, and callers must
//! keep it that way (no `{:?}` of snapshots, lines, or cwds in eprintln).
//! Do not attach session files to bug reports. `bitty --safe` never reads
//! the session file, and safe/headless exits never overwrite it, so
//! recovery startup cannot clobber the saved session.
//!
//! # Shutdown budget
//!
//! Saving is one bounded capture (≤128 panes × 200 lines), one ≤1 MiB
//! write + fsync + rename, no retries, no network: typically milliseconds,
//! never indefinite. Failures warn on stderr and never block exit. Raw
//! `SIGTERM`/`SIGHUP` that bypasses the platform event loop skips the save
//! (documented gap — the atomic format still guarantees no corruption);
//! every in-loop exit path (`CloseRequested`-confirmed, `Closed`,
//! `Exiting`) saves through [`Runtime::save_session_on_exit`].

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use bitty_ui::{Focus, LayoutNode, SplitAxis, View, ViewId};

use super::panes::osc7_cwd_path;
use super::workspaces::WorkspaceSlot;
use super::*;

/// Session file format version written by this slice.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Maximum session file bytes read or written (issue: bounded size).
pub const MAX_SESSION_FILE_BYTES: usize = 1_048_576;

/// Maximum bytes of any single session-file line.
pub const MAX_SESSION_LINE_BYTES: usize = 4096;

/// Maximum workspaces per session (mirrors `MAX_WORKSPACES`).
pub const MAX_SESSION_WORKSPACES: usize = 16;

/// Maximum panes per workspace (mirrors `MAX_VIEWS_PER_WORKSPACE`).
pub const MAX_SESSION_PANES_PER_WORKSPACE: usize = 32;

/// Maximum panes across all workspaces (bounds decode CPU).
pub const MAX_SESSION_PANES_TOTAL: usize = 128;

/// Maximum scrollback lines persisted and restored per pane (issue: cap the
/// restored lines; capture keeps the newest lines).
pub const MAX_SESSION_SCROLLBACK_LINES_PER_PANE: usize = 200;

/// Maximum bytes of one persisted scrollback line.
pub const MAX_SESSION_LINE_TEXT_BYTES: usize = 4096;

/// Maximum bytes of a persisted cwd report (mirrors `SHELL_CWD_MAX_BYTES`).
pub const MAX_SESSION_CWD_BYTES: usize = 4096;

/// Maximum workspace-name chars (mirrors `WORKSPACE_NAME_MAX_CHARS`).
pub const MAX_SESSION_NAME_CHARS: usize = 32;

/// Maximum layout S-expression nesting depth (recursion guard).
pub const MAX_SESSION_LAYOUT_DEPTH: usize = 64;

/// Maximum grid dimension accepted on restore (mirrors `MAX_GRID_DIM`).
pub const MAX_SESSION_GRID_DIM: usize = 1000;

/// Directory name under the XDG state root.
pub const SESSION_APP_DIR_NAME: &str = "bitty";
/// Sessions subdirectory name.
pub const SESSIONS_DIR_NAME: &str = "sessions";
/// Session file name (version lives inside the file).
pub const SESSION_FILE_NAME: &str = "session";

/// Session magic line prefix.
const SESSION_MAGIC: &str = "bitty-session v";

/// Everything that can go wrong across session save/restore.
///
/// All variants are content-free (kinds and counts only): `Display` output
/// is safe for stderr and must never gain a snapshot/line/cwd payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// No session file exists at the resolved path.
    NotFound,
    /// No usable state root (`$XDG_STATE_HOME` / `$HOME` both unusable).
    NoStateDir,
    /// File or snapshot exceeds a [`MAX_SESSION_*`](MAX_SESSION_FILE_BYTES) bound.
    TooLarge {
        /// Which bound tripped (static label, never content).
        what: &'static str,
        /// Observed size.
        actual: usize,
        /// Enforced limit.
        limit: usize,
    },
    /// File or snapshot is structurally invalid (whole file rejected).
    Corrupt(&'static str),
    /// Version mismatch (whole file rejected).
    UnsupportedVersion(u32),
    /// Filesystem failure (message only, never file contents).
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "session file not found"),
            Self::NoStateDir => write!(f, "no session state dir"),
            Self::TooLarge {
                what,
                actual,
                limit,
            } => {
                write!(f, "session {what} too large ({actual} > {limit})")
            }
            Self::Corrupt(why) => write!(f, "session file corrupt ({why})"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported session version ({v})"),
            Self::Io(msg) => write!(f, "session io error ({msg})"),
        }
    }
}

impl std::error::Error for SessionError {}

/// One pane's persisted state: cwd report plus scrollback history.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneSnapshot {
    /// Leaf the history belongs to.
    pub view: ViewId,
    /// Captured `OSC 7` report, if the pane had one.
    pub cwd: Option<String>,
    /// Scrollback lines oldest-first, trimmed, bounded per pane.
    pub scrollback: Vec<String>,
}

/// One workspace's persisted state: identity plus layout, focus, and panes.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSnapshot {
    /// Stable creation sequence (`ws{seq}` identity).
    pub seq: u64,
    /// Display name.
    pub name: String,
    /// Layout tree (overlays stripped at capture).
    pub layout: LayoutNode,
    /// Focused leaf, when the workspace has leaves.
    pub focus: Option<ViewId>,
    /// Per-pane history, exactly covering the layout leaves.
    pub panes: Vec<PaneSnapshot>,
}

/// A persistable session: all workspaces plus active index and MRU order.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSnapshot {
    /// Always [`SESSION_FORMAT_VERSION`] from capture.
    pub version: u32,
    /// Workspace slots in index order (`>= 1`).
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// Active workspace index into `workspaces`.
    pub active: usize,
    /// MRU workspace indices, active fronted, each live index exactly once.
    pub mru: Vec<usize>,
}

/// Counts from a successful restore (no contents, safe to log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRestoreSummary {
    /// Workspaces rebuilt.
    pub workspaces: usize,
    /// Panes rebuilt.
    pub panes: usize,
    /// Scrollback lines rehydrated into live grids.
    pub scrollback_lines: usize,
    /// Panes stashed for hydration when their shell spawns.
    pub pending: usize,
}

/// Counts from a successful save (no contents, safe to log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSaveSummary {
    /// Workspaces persisted.
    pub workspaces: usize,
    /// Panes persisted.
    pub panes: usize,
    /// Scrollback lines persisted.
    pub scrollback_lines: usize,
    /// Encoded file bytes.
    pub bytes: usize,
}

/// Outcome of [`Runtime::restore_session_on_startup`] (and its `_with_env`
/// hermetic twin): content-free and safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStartupOutcome {
    /// `--safe` (or headless): the session file was never touched.
    SkippedSafeMode,
    /// No session file: clean start, no warning.
    Fresh,
    /// Session restored.
    Restored(SessionRestoreSummary),
    /// File present but unusable: clean start with a stderr warning.
    FreshWithWarning(SessionError),
}

/// Outcome of [`Runtime::save_session_on_exit`]: content-free, safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionExitSaveOutcome {
    /// Session persisted (counts only).
    Saved(SessionSaveSummary),
    /// No usable state dir: nothing written, no warning.
    SkippedNoStateDir,
    /// Save failed: stderr warning, exit proceeds.
    Warned(SessionError),
}

/// Pending per-pane restore for a leaf with no live grid yet (CTX-0393).
///
/// Filled by [`Runtime::apply_session_snapshot`] for session-less leaves;
/// drained into the fresh grid by the next successful spawn of that leaf
/// (primary spawn included). Never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaneRestore {
    /// Captured `OSC 7` report used as the spawn cwd fallback.
    pub cwd: Option<String>,
    /// Scrollback lines oldest-first awaiting hydration.
    pub scrollback: Vec<String>,
}

// ---------------------------------------------------------------------------
// XDG state paths (pure, hermetic; no hardcoded hosts)
// ---------------------------------------------------------------------------

/// `$XDG_STATE_HOME`, else `$HOME/.local/state`, else `None` (fail-closed).
#[must_use]
pub fn state_home_for(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home.filter(|h| !h.trim().is_empty())
        .map(|h| PathBuf::from(h).join(".local").join("state"))
}

/// Live-environment state home.
#[must_use]
pub fn state_home() -> Option<PathBuf> {
    state_home_for(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Session directory (`<state>/bitty/sessions`) from injected env values.
#[must_use]
pub fn session_dir_for(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    state_home_for(xdg_state_home, home)
        .map(|base| base.join(SESSION_APP_DIR_NAME).join(SESSIONS_DIR_NAME))
}

/// Live-environment session directory.
#[must_use]
pub fn session_dir() -> Option<PathBuf> {
    session_dir_for(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Session file path from injected env values.
#[must_use]
pub fn session_file_for(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    session_dir_for(xdg_state_home, home).map(|dir| dir.join(SESSION_FILE_NAME))
}

/// Live-environment session file path.
#[must_use]
pub fn session_file() -> Option<PathBuf> {
    session_file_for(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Field escaping (backslash-only; whole-line fields may contain spaces)
// ---------------------------------------------------------------------------

/// Escapes one whole-line field (`\` → `\\`, LF → `\n`, CR → `\r`).
fn escape_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Unescapes one whole-line field; rejects dangling/unknown escapes.
fn unescape_field(raw: &str) -> Result<String, SessionError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            _ => return Err(SessionError::Corrupt("bad escape")),
        }
    }
    Ok(out)
}

/// Truncates to at most `limit` bytes at a char boundary.
fn truncate_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// Layout S-expression codec (single line per workspace)
// ---------------------------------------------------------------------------

/// Encodes a layout tree as one S-expression line; transient overlays are
/// stripped to their base (never session truth).
fn encode_layout(node: &LayoutNode) -> String {
    match node {
        LayoutNode::Leaf(view) => {
            format!("(leaf {} {} {})", view.id().0, view.cols(), view.rows())
        }
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let axis = match axis {
                SplitAxis::Horizontal => "h",
                SplitAxis::Vertical => "v",
            };
            format!(
                "(split {axis} {} {} {})",
                ratio.to_bits(),
                encode_layout(first),
                encode_layout(second)
            )
        }
        LayoutNode::Stack(children) => {
            let mut out = String::from("(stack");
            for child in children {
                out.push(' ');
                out.push_str(&encode_layout(child));
            }
            out.push(')');
            out
        }
        LayoutNode::Overlay { base, .. } => encode_layout(base),
    }
}

/// Tokenizes one layout line into atoms and parens.
fn tokenize_layout(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (i, b) in line.bytes().enumerate() {
        match b {
            b'(' | b')' => {
                if let Some(s) = start.take() {
                    tokens.push(&line[s..i]);
                }
                tokens.push(&line[i..i + 1]);
            }
            b' ' | b'\t' => {
                if let Some(s) = start.take() {
                    tokens.push(&line[s..i]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(s) = start.take() {
        tokens.push(&line[s..]);
    }
    tokens
}

/// Recursive-descent layout parser with depth and leaf guards.
struct LayoutParser<'a> {
    tokens: Vec<&'a str>,
    pos: usize,
    leaves: usize,
}

impl<'a> LayoutParser<'a> {
    fn parse_node(&mut self, depth: usize) -> Result<LayoutNode, SessionError> {
        if depth > MAX_SESSION_LAYOUT_DEPTH {
            return Err(SessionError::Corrupt("layout too deep"));
        }
        let open = self
            .next()
            .ok_or(SessionError::Corrupt("layout truncated"))?;
        if open != "(" {
            return Err(SessionError::Corrupt("layout shape"));
        }
        let kind = self
            .next()
            .ok_or(SessionError::Corrupt("layout truncated"))?;
        let node = match kind {
            "leaf" => {
                let id: u64 = self
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or(SessionError::Corrupt("leaf id"))?;
                let cols: usize = self
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or(SessionError::Corrupt("leaf dims"))?;
                let rows: usize = self
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or(SessionError::Corrupt("leaf dims"))?;
                if !(1..=MAX_SESSION_GRID_DIM).contains(&cols)
                    || !(1..=MAX_SESSION_GRID_DIM).contains(&rows)
                {
                    return Err(SessionError::Corrupt("leaf dims range"));
                }
                self.leaves += 1;
                if self.leaves > MAX_SESSION_PANES_PER_WORKSPACE {
                    return Err(SessionError::Corrupt("too many panes"));
                }
                self.expect_close()?;
                LayoutNode::leaf(View::new(ViewId::new(id), cols, rows))
            }
            "split" => {
                let axis = match self.next() {
                    Some("h") => SplitAxis::Horizontal,
                    Some("v") => SplitAxis::Vertical,
                    _ => return Err(SessionError::Corrupt("split axis")),
                };
                let bits: u32 = self
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or(SessionError::Corrupt("split ratio"))?;
                let ratio = f32::from_bits(bits);
                let ratio = if ratio.is_finite() { ratio } else { 0.5 };
                let first = self.parse_node(depth + 1)?;
                let second = self.parse_node(depth + 1)?;
                self.expect_close()?;
                LayoutNode::split(axis, ratio, first, second)
            }
            "stack" => {
                let mut children = Vec::new();
                loop {
                    match self.peek() {
                        None => return Err(SessionError::Corrupt("layout truncated")),
                        Some(")") => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            if children.len() >= MAX_SESSION_PANES_PER_WORKSPACE {
                                return Err(SessionError::Corrupt("too many panes"));
                            }
                            children.push(self.parse_node(depth + 1)?);
                        }
                    }
                }
                LayoutNode::stack(children)
            }
            _ => return Err(SessionError::Corrupt("layout node")),
        };
        Ok(node)
    }

    fn next(&mut self) -> Option<&'a str> {
        let token = self.tokens.get(self.pos).copied();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.pos).copied()
    }

    fn expect_close(&mut self) -> Result<(), SessionError> {
        match self.next() {
            Some(")") => Ok(()),
            _ => Err(SessionError::Corrupt("layout shape")),
        }
    }
}

/// Decodes one layout expression line into a tree.
fn decode_layout(line: &str) -> Result<LayoutNode, SessionError> {
    let mut parser = LayoutParser {
        tokens: tokenize_layout(line),
        pos: 0,
        leaves: 0,
    };
    if parser.tokens.is_empty() {
        return Err(SessionError::Corrupt("empty layout"));
    }
    let node = parser.parse_node(0)?;
    if parser.pos != parser.tokens.len() {
        return Err(SessionError::Corrupt("layout trailing"));
    }
    Ok(node)
}

// ---------------------------------------------------------------------------
// Snapshot validation (shared by encode and apply: fail-closed pre-mutation)
// ---------------------------------------------------------------------------

/// Validates every bound without touching runtime state; content-free errors.
fn validate_snapshot(snap: &SessionSnapshot) -> Result<(), SessionError> {
    if snap.version != SESSION_FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion(snap.version));
    }
    if snap.workspaces.is_empty() || snap.workspaces.len() > MAX_SESSION_WORKSPACES {
        return Err(SessionError::Corrupt("workspace count"));
    }
    if snap.active >= snap.workspaces.len() {
        return Err(SessionError::Corrupt("active workspace"));
    }
    if snap.mru.len() != snap.workspaces.len() {
        return Err(SessionError::Corrupt("mru length"));
    }
    {
        let mut seen = vec![false; snap.workspaces.len()];
        for &index in &snap.mru {
            if index >= snap.workspaces.len() || seen[index] {
                return Err(SessionError::Corrupt("mru order"));
            }
            seen[index] = true;
        }
    }
    if snap.mru.first() != Some(&snap.active) {
        return Err(SessionError::Corrupt("mru head"));
    }
    let mut total_panes = 0usize;
    let mut seqs = std::collections::BTreeSet::new();
    for ws in &snap.workspaces {
        if !seqs.insert(ws.seq) {
            return Err(SessionError::Corrupt("workspace seq"));
        }
        if ws.name.chars().count() > MAX_SESSION_NAME_CHARS {
            return Err(SessionError::Corrupt("workspace name"));
        }
        let leaves = ws.layout.leaf_ids();
        if leaves.len() > MAX_SESSION_PANES_PER_WORKSPACE {
            return Err(SessionError::Corrupt("too many panes"));
        }
        if let Some(focus) = ws.focus {
            if !leaves.contains(&focus) {
                return Err(SessionError::Corrupt("focus not a leaf"));
            }
        }
        if ws.panes.len() != leaves.len() {
            return Err(SessionError::Corrupt("pane coverage"));
        }
        {
            let mut seen = std::collections::BTreeSet::new();
            for pane in &ws.panes {
                if !leaves.contains(&pane.view) || !seen.insert(pane.view) {
                    return Err(SessionError::Corrupt("pane coverage"));
                }
                if let Some(cwd) = &pane.cwd {
                    if cwd.len() > MAX_SESSION_CWD_BYTES {
                        return Err(SessionError::Corrupt("cwd bound"));
                    }
                }
                if pane.scrollback.len() > MAX_SESSION_SCROLLBACK_LINES_PER_PANE {
                    return Err(SessionError::Corrupt("scrollback bound"));
                }
                for line in &pane.scrollback {
                    if line.len() > MAX_SESSION_LINE_TEXT_BYTES {
                        return Err(SessionError::Corrupt("scrollback line bound"));
                    }
                }
            }
        }
        total_panes += ws.panes.len();
        if total_panes > MAX_SESSION_PANES_TOTAL {
            return Err(SessionError::Corrupt("too many panes"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Text codec (pure, bounded, content-free errors)
// ---------------------------------------------------------------------------

/// Encodes a validated snapshot to file bytes (fails closed before I/O).
pub fn encode_session(snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
    validate_snapshot(snap)?;
    let mut out = String::new();
    out.push_str(SESSION_MAGIC);
    out.push_str(&SESSION_FORMAT_VERSION.to_string());
    out.push('\n');
    let mru = snap
        .mru
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    out.push_str(&format!(
        "workspaces {} active {} mru {mru}\n",
        snap.workspaces.len(),
        snap.active
    ));
    for ws in &snap.workspaces {
        let focus = ws
            .focus
            .map_or_else(|| "none".to_string(), |id| id.0.to_string());
        out.push_str(&format!("workspace {} {focus}\n", ws.seq));
        out.push_str(&format!("name {}\n", escape_field(&ws.name)));
        out.push_str(&format!("layout {}\n", encode_layout(&ws.layout)));
        for pane in &ws.panes {
            let leaf = ws
                .layout
                .find_leaf(pane.view)
                .ok_or(SessionError::Corrupt("pane coverage"))?;
            let cwd_flag = u8::from(pane.cwd.is_some());
            out.push_str(&format!(
                "pane {} {} {} {} {cwd_flag}\n",
                pane.view.0,
                leaf.cols(),
                leaf.rows(),
                pane.scrollback.len()
            ));
            if let Some(cwd) = &pane.cwd {
                out.push_str(&escape_field(cwd));
                out.push('\n');
            }
            for line in &pane.scrollback {
                out.push_str(&escape_field(line));
                out.push('\n');
            }
            out.push_str("end-pane\n");
        }
        out.push_str("end-workspace\n");
    }
    out.push_str("end-session\n");
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

/// Cursor parser over session-file lines (content-free errors throughout).
struct SessionParser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

impl<'a> SessionParser<'a> {
    fn next(&mut self) -> Result<&'a str, SessionError> {
        let line = self
            .lines
            .get(self.pos)
            .copied()
            .ok_or(SessionError::Corrupt("truncated"))?;
        self.pos += 1;
        Ok(line)
    }

    fn expect(&mut self, marker: &'static str) -> Result<(), SessionError> {
        match self.next()? {
            line if line == marker => Ok(()),
            _ => Err(SessionError::Corrupt("marker")),
        }
    }
}

/// Decodes file bytes to a snapshot; any violation rejects the whole file.
pub fn decode_session(bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
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
    let mut parser = SessionParser { lines, pos: 0 };
    let magic = parser.next()?;
    let version: u32 = magic
        .strip_prefix(SESSION_MAGIC)
        .and_then(|v| v.parse().ok())
        .ok_or(SessionError::Corrupt("magic"))?;
    if version != SESSION_FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion(version));
    }
    let header = parser.next()?;
    let (count, active, mru) = parse_workspaces_header(header)?;
    if count == 0 || count > MAX_SESSION_WORKSPACES {
        return Err(SessionError::Corrupt("workspace count"));
    }
    let mut workspaces = Vec::with_capacity(count);
    let mut total_panes = 0usize;
    for _ in 0..count {
        let ws = parse_workspace(&mut parser, &mut total_panes)?;
        workspaces.push(ws);
    }
    parser.expect("end-session")?;
    if parser.pos != parser.lines.len() {
        return Err(SessionError::Corrupt("trailing"));
    }
    let snap = SessionSnapshot {
        version,
        workspaces,
        active,
        mru,
    };
    validate_snapshot(&snap)?;
    Ok(snap)
}

/// Parses `workspaces <n> active <a> mru <m0,m1,...>`.
fn parse_workspaces_header(line: &str) -> Result<(usize, usize, Vec<usize>), SessionError> {
    let rest = line
        .strip_prefix("workspaces ")
        .ok_or(SessionError::Corrupt("header"))?;
    let (count_raw, rest) = rest
        .split_once(" active ")
        .ok_or(SessionError::Corrupt("header"))?;
    let (active_raw, mru_raw) = rest
        .split_once(" mru ")
        .ok_or(SessionError::Corrupt("header"))?;
    let count: usize = count_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("header"))?;
    let active: usize = active_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("header"))?;
    if mru_raw.is_empty() {
        return Err(SessionError::Corrupt("mru order"));
    }
    let mut mru = Vec::new();
    for part in mru_raw.split(',') {
        mru.push(
            part.parse()
                .map_err(|_| SessionError::Corrupt("mru order"))?,
        );
    }
    Ok((count, active, mru))
}

/// Parses one `workspace ... end-workspace` block.
fn parse_workspace(
    parser: &mut SessionParser<'_>,
    total_panes: &mut usize,
) -> Result<WorkspaceSnapshot, SessionError> {
    let head = parser.next()?;
    let head = head
        .strip_prefix("workspace ")
        .ok_or(SessionError::Corrupt("workspace"))?;
    let (seq_raw, focus_raw) = head
        .split_once(' ')
        .ok_or(SessionError::Corrupt("workspace"))?;
    let seq: u64 = seq_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("workspace seq"))?;
    let focus = match focus_raw {
        "none" => None,
        raw => Some(ViewId::new(
            raw.parse().map_err(|_| SessionError::Corrupt("focus"))?,
        )),
    };
    let name_line = parser.next()?;
    let name = unescape_field(
        name_line
            .strip_prefix("name ")
            .ok_or(SessionError::Corrupt("name"))?,
    )?;
    let layout_line = parser.next()?;
    let layout = decode_layout(
        layout_line
            .strip_prefix("layout ")
            .ok_or(SessionError::Corrupt("layout"))?,
    )?;
    let leaves = layout.leaf_ids();
    let mut panes = Vec::new();
    loop {
        let line = parser.next()?;
        if line == "end-workspace" {
            break;
        }
        let pane = parse_pane(parser, line, &layout)?;
        panes.push(pane);
        *total_panes += 1;
        if *total_panes > MAX_SESSION_PANES_TOTAL {
            return Err(SessionError::Corrupt("too many panes"));
        }
    }
    if panes.len() != leaves.len() {
        return Err(SessionError::Corrupt("pane coverage"));
    }
    Ok(WorkspaceSnapshot {
        seq,
        name,
        layout,
        focus,
        panes,
    })
}

/// Parses one `pane ... end-pane` block; `head` is the already-read header.
fn parse_pane(
    parser: &mut SessionParser<'_>,
    head: &str,
    layout: &LayoutNode,
) -> Result<PaneSnapshot, SessionError> {
    let head = head
        .strip_prefix("pane ")
        .ok_or(SessionError::Corrupt("pane"))?;
    let parts: Vec<&str> = head.split(' ').collect();
    let [id_raw, cols_raw, rows_raw, count_raw, cwd_raw] = parts.as_slice() else {
        return Err(SessionError::Corrupt("pane"));
    };
    let id: u64 = id_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pane id"))?;
    let cols: usize = cols_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pane dims"))?;
    let rows: usize = rows_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pane dims"))?;
    if !(1..=MAX_SESSION_GRID_DIM).contains(&cols) || !(1..=MAX_SESSION_GRID_DIM).contains(&rows) {
        return Err(SessionError::Corrupt("pane dims range"));
    }
    let view = ViewId::new(id);
    let leaf = layout
        .find_leaf(view)
        .ok_or(SessionError::Corrupt("pane coverage"))?;
    if usize::from(leaf.cols()) != cols || usize::from(leaf.rows()) != rows {
        return Err(SessionError::Corrupt("pane dims mismatch"));
    }
    let count: usize = count_raw
        .parse()
        .map_err(|_| SessionError::Corrupt("pane lines"))?;
    if count > MAX_SESSION_SCROLLBACK_LINES_PER_PANE {
        return Err(SessionError::Corrupt("scrollback bound"));
    }
    let cwd = match *cwd_raw {
        "0" => None,
        "1" => {
            let raw = parser.next()?;
            let decoded = unescape_field(raw)?;
            if decoded.len() > MAX_SESSION_CWD_BYTES {
                return Err(SessionError::Corrupt("cwd bound"));
            }
            Some(decoded)
        }
        _ => return Err(SessionError::Corrupt("pane")),
    };
    let mut scrollback = Vec::with_capacity(count);
    for _ in 0..count {
        let raw = parser.next()?;
        let decoded = unescape_field(raw)?;
        if decoded.len() > MAX_SESSION_LINE_TEXT_BYTES {
            return Err(SessionError::Corrupt("scrollback line bound"));
        }
        scrollback.push(decoded);
    }
    parser.expect("end-pane")?;
    Ok(PaneSnapshot {
        view,
        cwd,
        scrollback,
    })
}

// ---------------------------------------------------------------------------
// Scrollback text projection (capture side; restore via State API)
// ---------------------------------------------------------------------------

/// Projects the newest `max_lines` scrollback lines to trimmed text.
///
/// Wide spacers are skipped, combining marks ride along, trailing blanks
/// trim (viewport parity with `View::visible_text_rows`), and lines
/// truncate to [`MAX_SESSION_LINE_TEXT_BYTES`] at a char boundary.
fn scrollback_tail_text(state: &State, max_lines: usize) -> Vec<String> {
    let len = state.scrollback_len();
    let skip = len.saturating_sub(max_lines);
    state
        .scrollback()
        .skip(skip)
        .map(|line| {
            let mut text = String::new();
            for cell in line.cells.iter() {
                if cell.spacer {
                    continue;
                }
                text.push(cell.glyph);
                for mark in cell.zerowidth.iter() {
                    text.push(*mark);
                }
            }
            truncate_bytes(text.trim_end(), MAX_SESSION_LINE_TEXT_BYTES).to_string()
        })
        .collect()
}

/// Normalizes a layout tree for persistence: transient overlays strip to
/// their base (never session truth), and leaves rebuild to identity plus
/// geometry only. Origin, scroll offset, and presentation mode are
/// presentation-only solver output and must not affect round-trip equality.
fn strip_overlays(node: &LayoutNode) -> LayoutNode {
    match node {
        LayoutNode::Leaf(view) => LayoutNode::leaf(View::new(
            view.id(),
            usize::from(view.cols()),
            usize::from(view.rows()),
        )),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => LayoutNode::split(*axis, *ratio, strip_overlays(first), strip_overlays(second)),
        LayoutNode::Stack(children) => {
            LayoutNode::stack(children.iter().map(strip_overlays).collect())
        }
        LayoutNode::Overlay { base, .. } => strip_overlays(base),
    }
}

// ---------------------------------------------------------------------------
// Atomic file I/O (write-temp + fsync + rename; temps never read)
// ---------------------------------------------------------------------------

/// Temp sibling for an atomic save (`<file>.tmp.<pid>`).
fn temp_sibling_for(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || SESSION_FILE_NAME.into(),
        |n| n.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}.tmp.{}", std::process::id()))
}

/// Removes stale `<file>.tmp.*` siblings best-effort (crashed-save litter).
fn clean_temp_siblings(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let prefix = path.file_name().map_or_else(
        || SESSION_FILE_NAME.into(),
        |n| format!("{}.tmp.", n.to_string_lossy()),
    );
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn io_error(context: &'static str, err: std::io::Error) -> SessionError {
    SessionError::Io(format!("{context}: {err}"))
}

/// Writes `bytes` atomically to `path` (temp + fsync + rename + 0600).
fn save_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge {
            what: "session file",
            actual: bytes.len(),
            limit: MAX_SESSION_FILE_BYTES,
        });
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io_error("create session dir", e))?;
        }
    }
    let temp = temp_sibling_for(path);
    let _ = std::fs::remove_file(&temp);
    let write_result = (|| -> Result<(), std::io::Error> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        }
        drop(file);
        std::fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(io_error("write session file", err));
    }
    clean_temp_siblings(path);
    Ok(())
}

/// Reads a session file with a hard size cap (missing → [`SessionError::NotFound`]).
fn load_bytes_capped(path: &Path) -> Result<Vec<u8>, SessionError> {
    let bytes = std::fs::read(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            SessionError::NotFound
        } else {
            io_error("read session file", err)
        }
    })?;
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge {
            what: "session file",
            actual: bytes.len(),
            limit: MAX_SESSION_FILE_BYTES,
        });
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Runtime integration (capture / apply / startup / exit)
// ---------------------------------------------------------------------------

impl Runtime {
    /// Captures the current session: every workspace slot (the active slot
    /// from the live layout/focus pair, inactive slots from their stashed
    /// copies), focus, and per-pane cwd plus the newest scrollback lines.
    ///
    /// Total and bounded: panes without a live grid (session-less leaves
    /// that do not own the primary grid) persist empty history and regain
    /// it through the pending map on apply. Never logs contents.
    pub fn capture_session_snapshot(&self) -> SessionSnapshot {
        let mut workspaces = Vec::with_capacity(self.workspaces.len());
        for (index, slot) in self.workspaces.iter().enumerate() {
            let (layout, focus) = if index == self.active_workspace {
                (self.layout.clone(), self.focus.clone())
            } else {
                (slot.layout.clone(), slot.focus.clone())
            };
            let layout = strip_overlays(&layout);
            let leaves = layout.leaf_ids();
            let mut panes = Vec::with_capacity(leaves.len());
            for view in &leaves {
                let state = self.session_state_for(*view);
                let (cwd, scrollback) = match state {
                    Some(state) => (
                        state.cwd_report().map(str::to_owned),
                        scrollback_tail_text(state, MAX_SESSION_SCROLLBACK_LINES_PER_PANE),
                    ),
                    None => (None, Vec::new()),
                };
                panes.push(PaneSnapshot {
                    view: *view,
                    cwd,
                    scrollback,
                });
            }
            workspaces.push(WorkspaceSnapshot {
                seq: slot.seq,
                name: slot.name.clone(),
                layout,
                focus: focus.focused(),
                panes,
            });
        }
        SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces,
            active: self.active_workspace,
            mru: self.workspace_mru.iter().copied().collect(),
        }
    }

    /// Returns the live grid backing `view`: its pane session, else the
    /// primary grid when `view` owns it, else `None` (session-less leaf).
    fn session_state_for(&self, view: ViewId) -> Option<&State> {
        if let Some(session) = self.pane_sessions.get(&view) {
            return Some(&session.state);
        }
        if self.primary_view == Some(view) {
            return Some(&self.state);
        }
        None
    }

    /// Applies a validated snapshot: rebuilds workspaces, the live
    /// layout/focus pair, and the primary owner, rehydrates scrollback into
    /// live grids, and stashes the rest in the pending map for the next
    /// spawn of each leaf.
    ///
    /// Validation runs fully before any mutation: a rejected snapshot
    /// leaves the runtime untouched (fail-closed clean start). Counts only
    /// in the summary — never contents.
    pub fn apply_session_snapshot(
        &mut self,
        snap: &SessionSnapshot,
    ) -> Result<SessionRestoreSummary, SessionError> {
        validate_snapshot(snap)?;
        let mut workspaces = Vec::with_capacity(snap.workspaces.len());
        for ws in &snap.workspaces {
            let focus = ws.focus.map_or_else(Focus::new, Focus::with_focus);
            workspaces.push(WorkspaceSlot {
                seq: ws.seq,
                name: ws.name.clone(),
                layout: ws.layout.clone(),
                focus,
            });
        }
        self.workspaces = workspaces;
        self.active_workspace = snap.active;
        self.workspace_mru = snap.mru.iter().copied().collect::<VecDeque<_>>();
        self.layout = self.workspaces[snap.active].layout.clone();
        self.focus = self.workspaces[snap.active].focus.clone();
        let leaves = self.layout.leaf_ids();
        self.primary_view = self
            .focus
            .focused()
            .filter(|focus| leaves.contains(focus))
            .or_else(|| leaves.first().copied());
        self.pending_ws_close = None;
        self.session_pending.clear();

        let mut panes = 0usize;
        let mut scrollback_lines = 0usize;
        for ws in &snap.workspaces {
            for pane in &ws.panes {
                panes += 1;
                scrollback_lines += self.rehydrate_pane(pane);
            }
        }
        let pending = self.session_pending.len();
        self.pending_full_redraw = true;
        Ok(SessionRestoreSummary {
            workspaces: snap.workspaces.len(),
            panes,
            scrollback_lines,
            pending,
        })
    }

    /// Rehydrates one pane: into its live grid when one exists, else into
    /// the pending map. Returns the rehydrated line count.
    fn rehydrate_pane(&mut self, pane: &PaneSnapshot) -> usize {
        let lines: Vec<&str> = pane.scrollback.iter().map(String::as_str).collect();
        if let Some(session) = self.pane_sessions.get_mut(&pane.view) {
            return session.state.restore_scrollback_text(&lines);
        }
        if self.primary_view == Some(pane.view) {
            return self.state.restore_scrollback_text(&lines);
        }
        self.session_pending.insert(
            pane.view,
            PendingPaneRestore {
                cwd: pane.cwd.clone(),
                scrollback: pane.scrollback.clone(),
            },
        );
        0
    }

    /// Drains the pending restore for `view` into its fresh grid after a
    /// successful spawn. Returns the hydrated line count (`0` when there
    /// was nothing pending or no live grid yet — the entry is kept then).
    pub fn hydrate_session_pending_for(&mut self, view: ViewId) -> usize {
        let live = self.pane_sessions.contains_key(&view) || self.primary_view == Some(view);
        if !live {
            return 0;
        }
        let Some(pending) = self.session_pending.remove(&view) else {
            return 0;
        };
        let lines: Vec<&str> = pending.scrollback.iter().map(String::as_str).collect();
        if let Some(session) = self.pane_sessions.get_mut(&view) {
            session.state.restore_scrollback_text(&lines)
        } else {
            self.state.restore_scrollback_text(&lines)
        }
    }

    /// Pending restores awaiting a shell spawn (count only).
    #[must_use]
    pub fn session_pending_len(&self) -> usize {
        self.session_pending.len()
    }

    /// Validated spawn directory from a pending restore: the captured
    /// `OSC 7` URL decoded to a local path that must still exist.
    /// Fail-open (`None`) on any doubt — the caller keeps its default.
    #[must_use]
    pub fn session_pending_cwd(&self, view: &ViewId) -> Option<PathBuf> {
        let report = self.session_pending.get(view)?.cwd.as_deref()?;
        let path = osc7_cwd_path(report)?;
        path.is_dir().then_some(path)
    }

    /// Captures, encodes, and atomically persists the session to `path`.
    pub fn save_session_to_path(&self, path: &Path) -> Result<SessionSaveSummary, SessionError> {
        let snap = self.capture_session_snapshot();
        let bytes = encode_session(&snap)?;
        save_bytes_atomic(path, &bytes)?;
        Ok(SessionSaveSummary {
            workspaces: snap.workspaces.len(),
            panes: snap.workspaces.iter().map(|ws| ws.panes.len()).sum(),
            scrollback_lines: snap
                .workspaces
                .iter()
                .flat_map(|ws| ws.panes.iter().map(|pane| pane.scrollback.len()))
                .sum(),
            bytes: bytes.len(),
        })
    }

    /// Atomically persists the session to the default XDG state path.
    pub fn save_session_to_default_path(&self) -> Result<SessionSaveSummary, SessionError> {
        let path = session_file().ok_or(SessionError::NoStateDir)?;
        self.save_session_to_path(&path)
    }

    /// Loads, decodes, and applies the session at `path` (fail-closed: any
    /// error leaves the runtime untouched).
    pub fn load_session_from_path(
        &mut self,
        path: &Path,
    ) -> Result<SessionRestoreSummary, SessionError> {
        let bytes = load_bytes_capped(path)?;
        let snap = decode_session(&bytes)?;
        self.apply_session_snapshot(&snap)
    }

    /// Startup restore with injected environment roots (hermetic twin for
    /// tests; production uses [`Runtime::restore_session_on_startup`]).
    ///
    /// `safe_mode` short-circuits before any filesystem access: recovery
    /// startup never reads session state. A missing file is a quiet clean
    /// start ([`SessionStartupOutcome::Fresh`]); any other failure keeps
    /// the clean runtime and reports a content-free warning.
    pub fn restore_session_on_startup_with_env(
        &mut self,
        safe_mode: bool,
        xdg_state_home: Option<&str>,
        home: Option<&str>,
    ) -> SessionStartupOutcome {
        if safe_mode {
            return SessionStartupOutcome::SkippedSafeMode;
        }
        let Some(path) = session_file_for(xdg_state_home, home) else {
            return SessionStartupOutcome::Fresh;
        };
        if !path.exists() {
            return SessionStartupOutcome::Fresh;
        }
        match self.load_session_from_path(&path) {
            Ok(summary) => SessionStartupOutcome::Restored(summary),
            Err(err) => SessionStartupOutcome::FreshWithWarning(err),
        }
    }

    /// Startup restore from the live environment (see the `_with_env` twin
    /// for the contract).
    pub fn restore_session_on_startup(&mut self, safe_mode: bool) -> SessionStartupOutcome {
        self.restore_session_on_startup_with_env(
            safe_mode,
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// Best-effort exit save to the default XDG state path: bounded capture
    /// plus one atomic write, no retries. Never panics, never blocks shutdown;
    /// failures collapse to content-free outcomes for a one-line warning.
    pub fn save_session_on_exit(&self) -> SessionExitSaveOutcome {
        match self.save_session_to_default_path() {
            Ok(summary) => SessionExitSaveOutcome::Saved(summary),
            Err(SessionError::NoStateDir | SessionError::NotFound) => {
                SessionExitSaveOutcome::SkippedNoStateDir
            }
            Err(err) => SessionExitSaveOutcome::Warned(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
    }

    #[test]
    fn field_escaping_round_trips_backslash_and_newline() {
        for raw in [
            "plain",
            "back\\slash",
            "line\nbreak",
            "cr\rhere",
            "  spaces  ",
        ] {
            assert_eq!(unescape_field(&escape_field(raw)).unwrap(), raw);
        }
        assert!(unescape_field("dangling\\").is_err());
        assert!(unescape_field("bad\\escape").is_err());
    }

    #[test]
    fn layout_codec_round_trips_split_and_stack() {
        let tree = LayoutNode::split(
            SplitAxis::Vertical,
            0.3,
            LayoutNode::stack(vec![leaf(1), leaf(2)]),
            leaf(3),
        );
        let back = decode_layout(&encode_layout(&tree)).expect("decode own encoding");
        assert_eq!(tree, back);
    }

    #[test]
    fn layout_capture_strips_overlays_and_presentation_state() {
        let base = LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2));
        let over = LayoutNode::overlay(base.clone(), leaf(9), bitty_ui::Rect::new(5, 5, 20, 10));
        let stripped = strip_overlays(&over);
        assert_eq!(stripped, base, "overlays must not survive capture");
        // A reflowed leaf (origin assigned) normalizes to identity+geometry.
        let mut moved = leaf(7);
        moved.reflow(bitty_ui::Rect::new(3, 4, 80, 24));
        let LayoutNode::Leaf(view) = strip_overlays(&moved) else {
            panic!("leaf must stay a leaf");
        };
        assert_eq!(view, View::new(ViewId::new(7), 80, 24));
    }

    #[test]
    fn layout_decode_rejects_unknown_nodes_and_trailing_tokens() {
        assert!(decode_layout("(leaf 1 80 24) (leaf 2 80 24)").is_err());
        assert!(decode_layout("(workspace 1 80 24)").is_err());
        assert!(decode_layout("(leaf 1 0 24)").is_err());
        assert!(decode_layout("(split x 1 (leaf 1 80 24) (leaf 2 80 24))").is_err());
        assert!(decode_layout("").is_err());
    }

    #[test]
    fn snapshot_validation_rejects_non_leaf_focus_and_bad_mru() {
        let ws = WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: leaf(1),
            focus: Some(ViewId::new(99)),
            panes: vec![PaneSnapshot {
                view: ViewId::new(1),
                cwd: None,
                scrollback: Vec::new(),
            }],
        };
        let snap = SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces: vec![ws],
            active: 0,
            mru: vec![0],
        };
        assert!(validate_snapshot(&snap).is_err());
        let mut snap2 = SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                seq: 1,
                name: "ws1".to_string(),
                layout: leaf(1),
                focus: Some(ViewId::new(1)),
                panes: vec![PaneSnapshot {
                    view: ViewId::new(1),
                    cwd: None,
                    scrollback: Vec::new(),
                }],
            }],
            active: 0,
            mru: vec![1],
        };
        assert!(validate_snapshot(&snap2).is_err());
        snap2.mru = vec![0];
        validate_snapshot(&snap2).expect("fixed snapshot validates");
    }

    #[test]
    fn scrollback_tail_keeps_newest_within_cap() {
        let mut state = State::with_scrollback_lines(1000);
        let rows: Vec<String> = (0..10).map(|i| format!("row {i}")).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        state.restore_scrollback_text(&refs);
        let tail = scrollback_tail_text(&state, 3);
        assert_eq!(
            tail,
            vec![
                "row 7".to_string(),
                "row 8".to_string(),
                "row 9".to_string()
            ]
        );
    }

    #[test]
    fn restore_scrollback_text_handles_wide_and_control_scalars() {
        let mut state = State::with_scrollback_lines(100);
        let pushed = state.restore_scrollback_text(&["hi\u{0007}中"]);
        assert_eq!(pushed, 1);
        let line = state.scrollback_line(0).expect("line stored");
        assert_eq!(line.cells.len(), state.width());
        // BEL degraded to U+FFFD, CJK kept atomic (lead + spacer).
        let glyphs: Vec<char> = line
            .cells
            .iter()
            .filter(|c| !c.spacer)
            .map(|c| c.glyph)
            .collect();
        assert!(glyphs.contains(&'\u{FFFD}'));
        assert!(glyphs.contains(&'中'));
        assert!(state.check_invariants().is_ok());
    }
}
