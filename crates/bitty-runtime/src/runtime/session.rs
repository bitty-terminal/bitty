//! Atomic session save/restore (CTX-0393, issue #649).
//!
//! Browser-like restore: reopening bitty recreates workspaces, the layout
//! tree, focus, scrollback history, and per-pane working directories.
//! Like a browser tab restore, layout and history come back but dead
//! processes do not: shells respawn fresh in the captured `OSC 7` cwd
//! (fail-open to the platform default when the report is missing, stale, or
//! no longer a directory) and scrollback rehydrates as immutable history.
//! Startup spawns only the active workspace's shells; a restored inactive
//! workspace arrives with layout plus pending history and respawns its
//! shells lazily on the first switch to it (bounded, best-effort).
//!
//! # Format (v2, hand-rolled, no new dependencies)
//!
//! A UTF-8 text file, `\n`-separated, magic first line `bitty-session v2`:
//!
//! ```text
//! bitty-session v2
//! workspaces <n> active <a> mru <m0,m1,...>
//! workspace <seq> <focus-id|none>
//! name <escaped workspace name>
//! layout <s-expr: (leaf id cols rows) | (split h|v ratio-bits first second) | (stack children...)>
//! pane <view-id> <cols> <rows> <k> <cwd:0|1> <attach> <route> <mode>
//! <escaped cwd, iff cwd == 1>
//! <k escaped scrollback lines, oldest first>
//! end-pane
//! end-workspace
//! end-session
//! ```
//!
//! The v2 pane header appends three fixed tokens: `<attach>` is the live
//! attachment map (`primary` = owned the primary grid, `session` = owned a
//! private pane session, `detached` = session-less leaf with no state);
//! `<route>` is the content route (only `terminal` is live — the reserved
//! extension point for panel/rich activities); `<mode>` is the requested
//! per-leaf [`PresentationMode`](bitty_ui::PresentationMode)
//! (`tiled`/`floating`/`fullscreen`/`scratchpad`). The layout S-expression
//! still carries identity plus geometry only; the mode token stamps the
//! restored leaf at decode so the live tree keeps it.
//!
//! # Versioning and migration (CW-16)
//!
//! | File version | Decoder behavior |
//! |---|----------------------------------------------------------------------------|
//! | v1 | Migrated in memory (unspecified attachment, terminal route, tiled mode); re-encoding writes v2 (one-way on disk). |
//! | v2 | Native: all three pane tokens parsed and validated. |
//! | anything else | [`SessionError::UnsupportedVersion`]: whole file rejected before any other parsing, runtime untouched. |
//!
//! Migration normalizes at decode: downstream (validation, apply,
//! re-encode) only ever sees [`SESSION_FORMAT_VERSION`] snapshots.
//! Unknown `<attach>`/`<route>`/`<mode>` tokens in a v2 file are
//! [`SessionError::Corrupt`], never defaulted — a future activity route or
//! mode must arrive with its own format version, not smuggled into v2.
//! At most one pane per file may claim `primary`; a `detached` pane
//! carrying a cwd or scrollback is corrupt (capture never emits it).
//!
//! # Rehydration rules (CW-15, under the accepted WS-INV-25/26 contract)
//!
//! * Fresh identities: a restore never resurrects a live PTY or
//!   `RuntimeId`. Every restored shell spawns fresh; rehydrated scrollback
//!   is immutable history in that fresh grid, never live PTY state.
//! * One live session per view: a snapshot view colliding with a live pane
//!   session is rejected fail-closed before any mutation (the
//!   runtime-level counterpart of the registry's `PersistentIdInUse`).
//! * Primary routing follows the startup recipe: the primary shell
//!   re-attaches at the focused leaf on every launch, so the derived
//!   startup owner (active workspace focus, else first leaf) hydrates the
//!   primary grid — a recorded `primary` on any other leaf is downgraded
//!   to a pending respawn carrying its own history and cwd, never mixed
//!   into the shared grid.
//! * Detached leaves restore empty: no grid write, no pending entry, no
//!   respawn on switch or startup. Only attached (`session`) leaves earn
//!   fresh shells.
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
//! the file is never partial. Stale `<file>.tmp.*` siblings are swept
//! best-effort *before* writing — never after the rename, so a concurrent
//! saver's live temp is never deleted. Oversize snapshots fail closed
//! *before* touching the filesystem, so a failed save never truncates a
//! good session.
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
//! Unix the temp file is created mode `0600` from the first byte —
//! create-time mode plus an immediate pre-write chmod, both covered by the
//! single `sync_all`, so no crash window ever exposes it at `0644` (a mode
//! failure aborts the save rather than leaving a readable file). Session
//! contents are never written
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

use bitty_ui::{Focus, LayoutNode, PresentationMode, SplitAxis, View, ViewId};

use super::panes::osc7_cwd_path;
use super::workspaces::WorkspaceSlot;
use super::*;

/// Session file format version written by this slice.
///
/// v2 extends the v1 pane record with the live attachment map
/// ([`PaneAttachment`]), the content route ([`PaneRoute`]), and the
/// per-leaf presentation mode. See the module docs for the v1/v2 record
/// shapes and the migration rules.
pub const SESSION_FORMAT_VERSION: u32 = 2;

/// Earliest format version the decoder still migrates. v1 files carry the
/// short pane record (`pane <id> <cols> <rows> <k> <cwd:0|1>`); migration
/// leaves the attachment unspecified (resolved through
/// [`derive_startup_owner`] exactly like the pre-v2 restore), defaults the
/// route to [`PaneRoute::Terminal`], and defaults the mode to
/// [`PresentationMode::Tiled`].
pub const SESSION_MIN_DECODE_VERSION: u32 = 1;

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

/// One pane's live attachment at capture: which binding backed the leaf.
///
/// CW-15 (F-2): the runtime half of the accepted attach/visibility
/// contract (at most one live session per view; rehydration mints fresh
/// PTYs under the same identity, never a second live terminal). It records
/// which leaf owned the primary grid, which owned private pane sessions
/// (live PTYs), and which were session-less.
///
/// `None` in memory marks a v1-legacy pane whose attachment was never
/// recorded. Apply and encode resolve it through [`derive_startup_owner`]
/// (the active workspace's focused leaf, else its first leaf),
/// reproducing the pre-v2 restore exactly: the derived owner hydrates the
/// primary grid, every other pane waits pending for its respawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAttachment {
    /// The leaf owned the primary grid at capture.
    Primary,
    /// The leaf owned a private pane session (live PTY) at capture.
    Session,
    /// Session-less leaf: no grid, no PTY; persists no history and earns
    /// no respawn (stays empty until the user spawns into it).
    Detached,
}

impl PaneAttachment {
    /// Canonical file token (`"primary" | "session" | "detached"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Session => "session",
            Self::Detached => "detached",
        }
    }

    /// Parses a file token; `None` on anything else (fail-closed, never a
    /// silent alias).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "primary" => Some(Self::Primary),
            "session" => Some(Self::Session),
            "detached" => Some(Self::Detached),
            _ => None,
        }
    }
}

impl std::fmt::Display for PaneAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Content route serving a leaf: which activity backend the pane restores
/// against.
///
/// CW-16: only [`Self::Terminal`] is live — every runtime leaf is
/// terminal-backed (primary grid or pane session). The token reserves the
/// extension point the candidate Panel/Activity direction names (rich,
/// browser, panel activities hosted by a panel): a future version adds
/// variants here, and this version's decoder rejects them fail-closed
/// rather than misrouting a pane onto the wrong backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRoute {
    /// Terminal-backed leaf (primary grid or pane session).
    Terminal,
}

impl PaneRoute {
    /// Canonical file token (`"terminal"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
        }
    }

    /// Parses a file token; `None` on anything else (fail-closed: a future
    /// activity route in a v2 file is rejected, never defaulted).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

impl std::fmt::Display for PaneRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One pane's persisted state: cwd report plus scrollback history.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneSnapshot {
    /// Leaf the history belongs to.
    pub view: ViewId,
    /// Captured `OSC 7` report, if the pane had one.
    pub cwd: Option<String>,
    /// Scrollback lines oldest-first, trimmed, bounded per pane.
    pub scrollback: Vec<String>,
    /// Live attachment at capture; `None` for v1-legacy panes (resolved
    /// through [`derive_startup_owner`] on apply and encode).
    pub attach: Option<PaneAttachment>,
    /// Content route serving the leaf (always [`PaneRoute::Terminal`]).
    pub route: PaneRoute,
    /// Requested per-leaf display mode at capture.
    pub mode: PresentationMode,
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
    // P3-5: leaf ids are runtime-global — one id backing panes in two
    // workspaces would alias two histories onto one session.
    let mut all_views = std::collections::BTreeSet::new();
    for ws in &snap.workspaces {
        if !seqs.insert(ws.seq) {
            return Err(SessionError::Corrupt("workspace seq"));
        }
        if ws.name.chars().count() > MAX_SESSION_NAME_CHARS {
            return Err(SessionError::Corrupt("workspace name"));
        }
        let leaves = ws.layout.leaf_ids();
        // P3-6: every workspace restores at least one live leaf; an empty
        // stack would otherwise apply as a dead workspace.
        if leaves.is_empty() {
            return Err(SessionError::Corrupt("empty workspace"));
        }
        for leaf in &leaves {
            if !all_views.insert(*leaf) {
                return Err(SessionError::Corrupt("duplicate pane"));
            }
        }
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
                // CW-16: a session-less leaf persists no state by
                // construction (capture emits no cwd and no history for
                // it), so a file claiming otherwise is corrupt.
                if pane.attach == Some(PaneAttachment::Detached)
                    && (pane.cwd.is_some() || !pane.scrollback.is_empty())
                {
                    return Err(SessionError::Corrupt("detached pane state"));
                }
            }
        }
        total_panes += ws.panes.len();
        if total_panes > MAX_SESSION_PANES_TOTAL {
            return Err(SessionError::Corrupt("too many panes"));
        }
    }
    // CW-16: the primary grid is single — at most one leaf may claim it
    // across the whole file. (Whether the recorded owner actually hydrates
    // the grid is decided at apply: the startup recipe re-pins the primary
    // shell at the focused leaf on every launch, so a recorded owner
    // elsewhere is downgraded to a pending respawn with its own history.)
    let primaries = snap
        .workspaces
        .iter()
        .flat_map(|ws| ws.panes.iter())
        .filter(|pane| pane.attach == Some(PaneAttachment::Primary))
        .count();
    if primaries > 1 {
        return Err(SessionError::Corrupt("duplicate primary"));
    }
    Ok(())
}

/// Startup-owner derivation shared by v1 migration and legacy (`None`)
/// attachments: the active workspace's focused leaf, else its first leaf.
///
/// Total on validated snapshots (every workspace carries at least one
/// leaf; `active` indexes a live workspace; focus is a leaf or absent).
/// This is the leaf the startup recipe binds the primary shell to, so it
/// is the only leaf whose history hydrates straight into the primary
/// grid; every other pane waits pending for its own fresh shell.
fn derive_startup_owner(snap: &SessionSnapshot) -> ViewId {
    let ws = &snap.workspaces[snap.active];
    ws.focus
        .filter(|focus| ws.layout.leaf_ids().contains(focus))
        .unwrap_or_else(|| ws.layout.leaf_ids()[0])
}

/// Resolves one pane's recorded attachment for encode.
///
/// Unspecified (`None`, v1-legacy) attachments resolve through
/// [`derive_startup_owner`]: the derived owner records as
/// [`PaneAttachment::Primary`], every other pane as
/// [`PaneAttachment::Session`]. Recorded attachments always write
/// faithfully — the stale-owner downgrade is an apply-time routing rule
/// (see [`Runtime::apply_session_snapshot`]), never an encode rewrite, so
/// capture/encode/decode round-trips byte-identically.
fn resolve_attachment(
    attach: Option<PaneAttachment>,
    view: ViewId,
    owner: ViewId,
) -> PaneAttachment {
    attach.unwrap_or(if view == owner {
        PaneAttachment::Primary
    } else {
        PaneAttachment::Session
    })
}

// ---------------------------------------------------------------------------
// Text codec (pure, bounded, content-free errors)
// ---------------------------------------------------------------------------

/// Encodes a validated snapshot to file bytes (fails closed before I/O).
pub fn encode_session(snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
    validate_snapshot(snap)?;
    // Unspecified (v1-legacy) attachments resolve through the same
    // startup-owner derivation apply uses, so hand-built snapshots encode
    // to the v2 record apply would have restored them through.
    let owner = derive_startup_owner(snap);
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
            // CW-16 v2 record: attachment map, content route, and
            // presentation mode ride the pane header. Fixed tokens, still
            // one line per pane, still covered by the line-bytes cap below.
            let attach = resolve_attachment(pane.attach, pane.view, owner);
            out.push_str(&format!(
                "pane {} {} {} {} {cwd_flag} {attach} {} {}\n",
                pane.view.0,
                leaf.cols(),
                leaf.rows(),
                pane.scrollback.len(),
                pane.route,
                pane.mode,
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
    // P3-9 self-compatibility: escaped whole-line fields (cwd, scrollback)
    // can exceed the decode line cap while the raw text stays within its
    // own bound (e.g. 3000 backslashes escape to 6000 bytes). Reject here,
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
///
/// Versioning (CW-16): v1 files migrate in memory — pane records gain an
/// unspecified attachment (resolved through [`derive_startup_owner`] like
/// the pre-v2 restore), [`PaneRoute::Terminal`], and
/// [`PresentationMode::Tiled`]. The returned snapshot always carries
/// [`SESSION_FORMAT_VERSION`]; anything outside
/// `SESSION_MIN_DECODE_VERSION..=SESSION_FORMAT_VERSION` is
/// [`SessionError::UnsupportedVersion`] before any other parsing.
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
    if !(SESSION_MIN_DECODE_VERSION..=SESSION_FORMAT_VERSION).contains(&version) {
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
        let ws = parse_workspace(&mut parser, &mut total_panes, version)?;
        workspaces.push(ws);
    }
    parser.expect("end-session")?;
    if parser.pos != parser.lines.len() {
        return Err(SessionError::Corrupt("trailing"));
    }
    // Migration normalizes here: downstream (validate, apply, re-encode)
    // only ever sees the current model version.
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
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
    version: u32,
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
    let mut layout = decode_layout(
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
        let pane = parse_pane(parser, line, &layout, version)?;
        // CW-16: the layout S-expression carries identity plus geometry
        // only; the v2 mode token stamps the restored leaf so the live
        // tree keeps the requested display mode (the solver ignores it,
        // transitions stay gated by `can_transition`).
        if let Some(leaf) = layout.find_leaf_mut(pane.view) {
            leaf.set_presentation(pane.mode);
        }
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
///
/// v1 headers carry five fields (`pane <id> <cols> <rows> <k> <cwd:0|1>`)
/// and migrate with an unspecified attachment, the terminal route, and a
/// tiled mode. v2 headers append `<attach> <route> <mode>`; the token
/// count is exact per version so a v2 record can never hide inside a v1
/// file (or vice versa).
fn parse_pane(
    parser: &mut SessionParser<'_>,
    head: &str,
    layout: &LayoutNode,
    version: u32,
) -> Result<PaneSnapshot, SessionError> {
    let head = head
        .strip_prefix("pane ")
        .ok_or(SessionError::Corrupt("pane"))?;
    let parts: Vec<&str> = head.split(' ').collect();
    let (id_raw, cols_raw, rows_raw, count_raw, cwd_raw, attach, route, mode) = match version {
        1 => {
            let [id_raw, cols_raw, rows_raw, count_raw, cwd_raw] = parts.as_slice() else {
                return Err(SessionError::Corrupt("pane"));
            };
            (
                *id_raw,
                *cols_raw,
                *rows_raw,
                *count_raw,
                *cwd_raw,
                None,
                PaneRoute::Terminal,
                PresentationMode::Tiled,
            )
        }
        _ => {
            let [
                id_raw,
                cols_raw,
                rows_raw,
                count_raw,
                cwd_raw,
                attach_raw,
                route_raw,
                mode_raw,
            ] = parts.as_slice()
            else {
                return Err(SessionError::Corrupt("pane"));
            };
            (
                *id_raw,
                *cols_raw,
                *rows_raw,
                *count_raw,
                *cwd_raw,
                Some(PaneAttachment::parse(attach_raw).ok_or(SessionError::Corrupt("attach"))?),
                PaneRoute::parse(route_raw).ok_or(SessionError::Corrupt("route"))?,
                PresentationMode::parse(mode_raw).ok_or(SessionError::Corrupt("mode"))?,
            )
        }
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
    let cwd = match cwd_raw {
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
        attach,
        route,
        mode,
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
/// geometry plus the requested presentation mode. Origin and scroll offset
/// are presentation-only solver output and must not affect round-trip
/// equality; the mode is session truth (CW-16 persists it per pane) and
/// survives the strip.
fn strip_overlays(node: &LayoutNode) -> LayoutNode {
    match node {
        LayoutNode::Leaf(view) => LayoutNode::leaf(View::with_presentation(
            view.id(),
            usize::from(view.cols()),
            usize::from(view.rows()),
            view.presentation(),
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
///
/// P2-1: the temp file is `0600` from the first byte — create-time mode
/// plus an immediate pre-write chmod — so the single `sync_all` covers
/// data and mode together and no crash window exposes the secret-capable
/// file at `0644`. A mode failure aborts the save.
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
    // P3-3: sweep crashed-save litter BEFORE writing, never after the
    // rename — a post-rename sweep would delete a concurrent saver's live
    // temp sibling in another process.
    clean_temp_siblings(path);
    let temp = temp_sibling_for(path);
    let _ = std::fs::remove_file(&temp);
    let write_result = (|| -> Result<(), std::io::Error> {
        use std::io::Write as _;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&temp)?;
        #[cfg(unix)]
        {
            // Defense in depth: the mode is already 0600 from create;
            // re-assert before the first byte so the sync below covers both.
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
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
    Ok(())
}

/// Reads a session file with a hard size cap (missing → [`SessionError::NotFound`]).
///
/// P3-4: a `symlink_metadata` pre-check avoids an unbounded allocation
/// against a hostile file; the post-read length check stays as the TOCTOU
/// backstop (the file may grow between the check and the read).
fn load_bytes_capped(path: &Path) -> Result<Vec<u8>, SessionError> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.len() > MAX_SESSION_FILE_BYTES as u64 {
            return Err(SessionError::TooLarge {
                what: "session file",
                actual: usize::try_from(meta.len()).unwrap_or(usize::MAX),
                limit: MAX_SESSION_FILE_BYTES,
            });
        }
    }
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

/// Maps a session-load result to a startup outcome (pure, for tests).
///
/// P3-7: `NotFound` — including a delete raced between the exists-probe and
/// the read — is a quiet clean start, never a warning.
fn startup_outcome_from_load(
    result: Result<SessionRestoreSummary, SessionError>,
) -> SessionStartupOutcome {
    match result {
        Ok(summary) => SessionStartupOutcome::Restored(summary),
        Err(SessionError::NotFound) => SessionStartupOutcome::Fresh,
        Err(err) => SessionStartupOutcome::FreshWithWarning(err),
    }
}

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
                // CW-15/16: record the live attachment map (primary owner,
                // pane session, or session-less), the content route, and
                // the requested presentation mode for every leaf.
                let attach = if self.primary_view == Some(*view) {
                    PaneAttachment::Primary
                } else if self.pane_sessions.contains_key(view) {
                    PaneAttachment::Session
                } else {
                    PaneAttachment::Detached
                };
                let mode = layout
                    .find_leaf(*view)
                    .map(|leaf| leaf.presentation())
                    .unwrap_or_default();
                panes.push(PaneSnapshot {
                    view: *view,
                    cwd,
                    scrollback,
                    attach: Some(attach),
                    route: PaneRoute::Terminal,
                    mode,
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
        // CW-15 (F-2, WS-INV-26): a snapshot view colliding with a live
        // pane session fails closed before any mutation. Rehydration mints
        // fresh PTYs for restored leaves; merging snapshot history into a
        // live grid would alias two histories onto one session, the exact
        // second-live-terminal hazard the registry rejects with
        // `PersistentIdInUse`. The runtime is left untouched.
        for ws in &snap.workspaces {
            for pane in &ws.panes {
                if self.pane_sessions.contains_key(&pane.view) {
                    return Err(SessionError::Corrupt("attachment in use"));
                }
            }
        }
        // CTX-0567 (#992): fold the outgoing live layout plus every stashed
        // slot into the monotonic high-water before a restore replaces them.
        // An id installed through the `layout_mut` escape never hit an
        // allocation funnel, so a restore whose snapshot carries lower ids
        // would otherwise let the allocator reissue it.
        self.raise_view_id_high_water();
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
        // CTX-0536 (#923): a restored snapshot installs ids directly; raise
        // the monotonic high-water so a later allocation never reuses one.
        self.raise_view_id_high_water();
        // CTX-0532: a restore load is a focus transition; attribute the
        // input-mode caches to the restored focus before any input arrives.
        self.sync_mode_caches_to_focus();
        // CW-15/16: the startup recipe binds the primary shell at the
        // focused leaf on every launch, so the primary owner is always the
        // derived startup owner (a recorded owner elsewhere is downgraded
        // to a pending respawn by `resolve_attachment` below).
        let owner = derive_startup_owner(snap);
        self.primary_view = Some(owner);
        self.pending_ws_close = None;
        self.session_pending.clear();
        self.session_primary_cwd = None;
        // CW-15: marks the world restored so startup pane spawn respawns
        // only attached leaves (pending restores). Session-less
        // (`Detached`) leaves stay empty by design instead of gaining
        // shells they never had.
        self.session_restored = true;

        let mut panes = 0usize;
        let mut scrollback_lines = 0usize;
        for ws in &snap.workspaces {
            for pane in &ws.panes {
                panes += 1;
                // A recorded `Primary` away from the derived owner is
                // downgraded to a pending respawn: the startup recipe
                // re-pins the primary shell at the focused leaf on every
                // launch, so honoring a stale owner would mix two histories
                // into the one global grid. The downgraded pane keeps its
                // own history and cwd — contents preserved, bindings fresh.
                match resolve_attachment(pane.attach, pane.view, owner) {
                    PaneAttachment::Primary if pane.view == owner => {
                        scrollback_lines += self.rehydrate_primary(pane);
                    }
                    PaneAttachment::Primary | PaneAttachment::Session => {
                        self.stage_pending_restore(pane);
                    }
                    // Detached: no grid, no pending entry, no respawn.
                    // The leaf restores as an empty pane.
                    PaneAttachment::Detached => {}
                }
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

    /// Rehydrates the startup owner's pane straight into the primary grid
    /// (which has no shell yet) and stages its captured cwd for the real
    /// restart attach. Returns the rehydrated line count.
    ///
    /// The collision guard in [`Runtime::apply_session_snapshot`] guarantees
    /// no live pane session backs this view, so the grid write always lands
    /// on a fresh grid — never merged into a live PTY (WS-INV-26).
    fn rehydrate_primary(&mut self, pane: &PaneSnapshot) -> usize {
        let lines: Vec<&str> = pane.scrollback.iter().map(String::as_str).collect();
        // CTX-0585: the primary owner has no shell yet (its history goes
        // straight into the grid), but the real restart attach spawns
        // through `spawn_shell_with_args`, which cannot see the pending
        // map. Stash the captured cwd so that path can seed the spawn cwd.
        self.session_primary_cwd = pane.cwd.clone().map(|cwd| (pane.view, cwd));
        self.state.restore_scrollback_text(&lines)
    }

    /// Stages one pane's captured history and cwd in the pending map for
    /// the next successful spawn of its leaf.
    fn stage_pending_restore(&mut self, pane: &PaneSnapshot) {
        self.session_pending.insert(
            pane.view,
            PendingPaneRestore {
                cwd: pane.cwd.clone(),
                scrollback: pane.scrollback.clone(),
            },
        );
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

    /// Whether `view` has a pending restore awaiting its shell spawn.
    ///
    /// CW-15: startup pane spawn consults this so a restored session
    /// respawns only attached leaves; session-less (`Detached`) leaves
    /// carry no entry and stay empty by design.
    #[must_use]
    pub fn session_pending_contains(&self, view: &ViewId) -> bool {
        self.session_pending.contains_key(view)
    }

    /// Whether a session restore populated this runtime (set by a
    /// successful [`Runtime::apply_session_snapshot`]).
    ///
    /// CW-15: distinguishes "fresh start, every leaf needs a shell" from
    /// "restored world, only pending leaves need one" for the startup
    /// spawn path. Never logged beyond the bit itself.
    #[must_use]
    pub fn session_restored(&self) -> bool {
        self.session_restored
    }

    /// Validated spawn directory from a pending restore: the captured
    /// `OSC 7` URL decoded to a local path that must still exist.
    /// Fail-open (`None`) on any doubt — the caller keeps its default.
    ///
    /// CTX-0585: the primary owner has no pending-map entry (its history went
    /// straight into the grid), so its captured cwd is read from
    /// `session_primary_cwd` — keyed to the owner so a later pane is never
    /// seeded from a foreign leaf's report.
    #[must_use]
    pub fn session_pending_cwd(&self, view: &ViewId) -> Option<PathBuf> {
        let report = match self.session_pending.get(view) {
            Some(pending) => pending.cwd.as_deref()?,
            None => match &self.session_primary_cwd {
                Some((owner, report)) if owner == view => report.as_str(),
                _ => return None,
            },
        };
        let path = osc7_cwd_path(report)?;
        path.is_dir().then_some(path)
    }

    /// Spawns shells for the active workspace's still-pending leaves (P2-2).
    ///
    /// Startup spawns only the active layout's shells, so a restored
    /// session's inactive workspaces arrive with layout plus pending history
    /// but no live shells. The first [`Runtime::workspace_switch`] onto such
    /// a workspace calls here: every active leaf that still has a pending
    /// restore and no live session gets a fresh shell replaying the primary
    /// attach recipe (startup parity with `workspace_new`), which hydrates
    /// the pending scrollback and seeds the spawn cwd from the captured
    /// `OSC 7` report. Best-effort with loud warnings; leaves already owning
    /// a session are untouched. No-op before any successful primary attach
    /// (no recipe to replay) or with nothing pending.
    pub(super) fn spawn_session_pending_for_active(&mut self) {
        let Some((program, args)) = self.primary_spawn.clone() else {
            return;
        };
        let targets: Vec<ViewId> = self
            .layout
            .leaf_ids()
            .into_iter()
            .filter(|view| {
                self.session_pending.contains_key(view)
                    && !self.pane_sessions.contains_key(view)
                    && Some(*view) != self.primary_view
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let frames = self.present_frames();
        let tail: Vec<&str> = args.iter().map(String::as_str).collect();
        for view in targets {
            let (cols, rows) = frames
                .iter()
                .find(|frame| frame.view == view)
                .map(|frame| (frame.cols.max(1), frame.rows.max(1)))
                .unwrap_or((
                    self.cols.min(u16::MAX as usize) as u16,
                    self.rows.min(u16::MAX as usize) as u16,
                ));
            if let Err(err) = self.spawn_shell_for_view(view, &program, &tail, cols, rows) {
                // Rate-limited (CTX-0473): session restore replays spawns for
                // every view; a broken recipe must not flood stderr.
                if let Some(suppressed) = self.spawn_log.admit_now() {
                    eprintln!(
                        "warning: workspace switch pane {view:?} shell spawn failed ({err}) — pane stays empty{}",
                        log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
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
        startup_outcome_from_load(self.load_session_from_path(&path))
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
            // Nit: only a missing state dir is silent. Any other failure
            // (including a `NotFound` surfaced mid-save) warns loudly.
            Err(SessionError::NoStateDir) => SessionExitSaveOutcome::SkippedNoStateDir,
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
    fn layout_capture_strips_overlays_but_keeps_modes() {
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
        // CW-16: the requested presentation mode is session truth and
        // survives the strip (origin/scroll do not).
        let floating = LayoutNode::leaf(View::with_presentation(
            ViewId::new(8),
            80,
            24,
            PresentationMode::Floating,
        ));
        let LayoutNode::Leaf(kept) = strip_overlays(&floating) else {
            panic!("leaf must stay a leaf");
        };
        assert_eq!(kept.presentation(), PresentationMode::Floating);
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
    fn startup_outcome_maps_raced_not_found_to_quiet_fresh() {
        // P3-7: a delete raced between the exists-probe and the read is a
        // quiet clean start, never a warning; anything else still warns.
        assert!(matches!(
            startup_outcome_from_load(Err(SessionError::NotFound)),
            SessionStartupOutcome::Fresh
        ));
        assert!(matches!(
            startup_outcome_from_load(Err(SessionError::NoStateDir)),
            SessionStartupOutcome::FreshWithWarning(_)
        ));
        assert!(matches!(
            startup_outcome_from_load(Err(SessionError::Corrupt("marker"))),
            SessionStartupOutcome::FreshWithWarning(_)
        ));
        let summary = SessionRestoreSummary {
            workspaces: 1,
            panes: 1,
            scrollback_lines: 0,
            pending: 0,
        };
        assert!(matches!(
            startup_outcome_from_load(Ok(summary)),
            SessionStartupOutcome::Restored(_)
        ));
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
                attach: None,
                route: PaneRoute::Terminal,
                mode: PresentationMode::Tiled,
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
                    attach: None,
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
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

    fn v2_pane(id: u64, attach: Option<PaneAttachment>) -> PaneSnapshot {
        PaneSnapshot {
            view: ViewId::new(id),
            cwd: None,
            scrollback: Vec::new(),
            attach,
            route: PaneRoute::Terminal,
            mode: PresentationMode::Tiled,
        }
    }

    fn two_pane_snapshot(attach_a: Option<PaneAttachment>) -> SessionSnapshot {
        SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                seq: 1,
                name: "ws1".to_string(),
                layout: LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.5,
                    LayoutNode::leaf(View::with_presentation(
                        ViewId::new(1),
                        80,
                        24,
                        PresentationMode::Floating,
                    )),
                    leaf(2),
                ),
                focus: Some(ViewId::new(1)),
                panes: vec![
                    PaneSnapshot {
                        mode: PresentationMode::Floating,
                        ..v2_pane(1, attach_a)
                    },
                    v2_pane(2, Some(PaneAttachment::Session)),
                ],
            }],
            active: 0,
            mru: vec![0],
        }
    }

    #[test]
    fn attach_route_mode_tokens_round_trip() {
        assert_eq!(
            PaneAttachment::parse("primary"),
            Some(PaneAttachment::Primary)
        );
        assert_eq!(
            PaneAttachment::parse("session"),
            Some(PaneAttachment::Session)
        );
        assert_eq!(
            PaneAttachment::parse("detached"),
            Some(PaneAttachment::Detached)
        );
        assert_eq!(PaneAttachment::parse("Primary"), None);
        assert_eq!(PaneAttachment::parse(""), None);
        assert_eq!(PaneRoute::parse("terminal"), Some(PaneRoute::Terminal));
        assert_eq!(PaneRoute::parse("panel"), None);
        assert_eq!(PaneRoute::parse(""), None);
    }

    #[test]
    fn v2_encode_decode_preserves_attach_route_mode() {
        let snap = two_pane_snapshot(Some(PaneAttachment::Primary));
        let bytes = encode_session(&snap).expect("v2 snapshot encodes");
        assert!(
            bytes.starts_with(b"bitty-session v2\n"),
            "encoder writes the v2 magic"
        );
        let back = decode_session(&bytes).expect("decode own v2 encoding");
        assert_eq!(snap, back, "v2 round trip must preserve everything");
        assert_eq!(
            back.workspaces[0].layout.leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2)]
        );
        let mode = back.workspaces[0]
            .layout
            .find_leaf(ViewId::new(1))
            .expect("leaf present")
            .presentation();
        assert_eq!(mode, PresentationMode::Floating);
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
        let snap = decode_session(raw.as_bytes()).expect("v1 must migrate");
        assert_eq!(snap.version, SESSION_FORMAT_VERSION);
        let pane = &snap.workspaces[0].panes[0];
        assert_eq!(pane.attach, None, "v1 carries no attachment record");
        assert_eq!(pane.route, PaneRoute::Terminal);
        assert_eq!(pane.mode, PresentationMode::Tiled);
        assert_eq!(pane.scrollback, vec!["migrated line".to_string()]);
        // The migrated snapshot re-encodes as v2 and decodes back.
        let bytes = encode_session(&snap).expect("migrated snapshot encodes");
        assert!(bytes.starts_with(b"bitty-session v2\n"));
        let back = decode_session(&bytes).expect("v2 re-decode works");
        assert_eq!(
            back.workspaces[0].panes[0].attach,
            Some(PaneAttachment::Primary),
            "unspecified attachment resolves through the startup-owner derivation"
        );
    }

    #[test]
    fn v1_record_cannot_hide_v2_tokens() {
        // An 8-token pane header in a v1 file is corrupt (exact token
        // count per version), not a silent upgrade.
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
        assert!(decode_session(raw.as_bytes()).is_err());
    }

    #[test]
    fn v2_rejects_unknown_attach_route_mode() {
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
            let err = decode_session(pane_with(header).as_bytes())
                .expect_err(&format!("bad {label} must fail"));
            assert!(
                !format!("{err}").contains("ws1"),
                "errors must never echo session contents"
            );
        }
    }

    #[test]
    fn validation_rejects_double_primary_and_stateful_detached() {
        let mut snap = two_pane_snapshot(Some(PaneAttachment::Primary));
        snap.workspaces[0].panes[1].attach = Some(PaneAttachment::Primary);
        let err = validate_snapshot(&snap).expect_err("two primaries must fail");
        assert_eq!(format!("{err}"), "session file corrupt (duplicate primary)");

        let mut snap = two_pane_snapshot(Some(PaneAttachment::Primary));
        snap.workspaces[0].panes[1].attach = Some(PaneAttachment::Detached);
        snap.workspaces[0].panes[1].scrollback = vec!["stowaway".to_string()];
        let err = validate_snapshot(&snap).expect_err("stateful detached must fail");
        assert_eq!(
            format!("{err}"),
            "session file corrupt (detached pane state)"
        );
    }

    #[test]
    fn startup_owner_derivation_prefers_focus_else_first_leaf() {
        let snap = two_pane_snapshot(None);
        assert_eq!(derive_startup_owner(&snap), ViewId::new(1));
        let mut unfocused = snap.clone();
        unfocused.workspaces[0].focus = None;
        assert_eq!(derive_startup_owner(&unfocused), ViewId::new(1));

        assert_eq!(
            resolve_attachment(None, ViewId::new(1), ViewId::new(1)),
            PaneAttachment::Primary
        );
        assert_eq!(
            resolve_attachment(None, ViewId::new(2), ViewId::new(1)),
            PaneAttachment::Session
        );
        // Recorded attachments resolve faithfully at encode/decode; the
        // stale-owner downgrade is an apply-time routing rule (pinned by
        // `recorded_primary_elsewhere_downgrades_to_pending`).
        assert_eq!(
            resolve_attachment(
                Some(PaneAttachment::Primary),
                ViewId::new(2),
                ViewId::new(1)
            ),
            PaneAttachment::Primary
        );
        assert_eq!(
            resolve_attachment(
                Some(PaneAttachment::Detached),
                ViewId::new(2),
                ViewId::new(1)
            ),
            PaneAttachment::Detached
        );
    }
}
