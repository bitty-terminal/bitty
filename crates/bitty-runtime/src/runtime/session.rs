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
//! # Format (v3, hand-rolled, no new dependencies)
//!
//! A UTF-8 text file, `\n`-separated, magic first line `bitty-session v3`:
//!
//! ```text
//! bitty-session v3
//! workspaces <n> active <a> mru <m0,m1,...>
//! workspace <seq> <focus-id|none>
//! name <escaped workspace name>
//! layout <s-expr: (leaf id cols rows) | (split h|v ratio-bits first second) | (stack children...)>
//! pane <view-id> <cols> <rows> <k> <cwd:0|1> <attach> <route> <mode>
//! <escaped cwd, iff cwd == 1>
//! <k escaped scrollback lines, oldest first>
//! end-pane
//! end-workspace
//! pinned <p>
//! pin <view-id> <cols> <rows> <k> <cwd:0|1> <attach> <anchor-id|none> <after:0|1>
//! <escaped cwd, iff cwd == 1>
//! <k escaped scrollback lines, oldest first>
//! end-pin
//! end-session
//! ```
//!
//! The v3 file appends one window-global `pinned` block after the workspace
//! blocks: every leaf parked in the [`PinnedStore`](bitty_ui::PinnedStore)
//! at capture, in pin order (present-time paint order). A `pin` record
//! mirrors the v2 `pane` record minus the `<route>`/`<mode>` tokens — pinned
//! leaves are floating terminal panels by construction, so the mode is
//! always `Floating` and the route always terminal; a future route or mode
//! arrives with its own format version, never smuggled into v3. The trailing
//! `<anchor-id|none> <after:0|1>` pair carries the unpin restore anchor
//! (the neighboring leaf id at pin time and which side the leaf returns
//! to); a missing anchor at unpin falls back to docking beside the first
//! live leaf, exactly like a live unpin whose anchor closed meanwhile.
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
//! | v1 | Migrated in memory (unspecified attachment, terminal route, tiled mode, empty pinned store); capture always writes v3. |
//! | v2 | Migrated in memory (empty pinned store); the workspace blocks decode exactly like v3. |
//! | v3 | Native: workspace blocks plus the window-global pinned block. |
//! | anything else | [`SessionError::UnsupportedVersion`]: whole file rejected before any other parsing, runtime untouched. |
//!
//! Migration normalizes at decode: downstream (validation, apply,
//! re-encode) only ever sees [`SESSION_FORMAT_VERSION`] snapshots. A v1/v2
//! snapshot carrying pinned entries is corrupt (those versions carry no
//! pinned block by construction). Unknown `<attach>`/`<route>`/`<mode>`
//! tokens in a v2/v3 file are
//! [`SessionError::Corrupt`], never defaulted — a future activity route or
//! mode must arrive with its own format version, not smuggled into v3.
//! At most one layout pane per file may claim `primary` (pinned entries
//! never hydrate the shared grid, so they are excluded from the tally and
//! route to pending instead); a `detached` pane
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
//! * Pinned floats restore into the window-global pinned store (CTX-1082):
//!   the parked leaf (identity, geometry, `Floating` mode), its unpin
//!   anchor, and its cwd plus scrollback history come back in pin order, so
//!   the composited scene presents identically. Attached pinned leaves
//!   (`primary`/`session`) wait pending like attached layout panes — the
//!   entry drains into the fresh grid on the next successful spawn of that
//!   view; `detached` pinned leaves restore empty with no entry. The
//!   Alt+drag re-anchor offset is presentation-only and resets: a restored
//!   pin presents at its cascade anchor, exactly like a fresh pin (unpin
//!   already drops the offset, so the lifecycle stays consistent).
//!
//! Field escaping is backslash-only (`\\` → `\`, `\n` → LF, `\r` → CR);
//! whole-line fields (name, cwd, scrollback) may contain spaces. Transient
//! overlays (help popup, palette) are never persisted: capture strips them
//! to the base tree. Split ratios persist as `f32` bits so the value
//! round-trips exactly and re-clamps through [`LayoutNode::split`].
//!
//! # Atomicity
//!
//! The injected [`SessionFileBackend`] owns the commit mechanics: a
//! per-writer unique temp sibling, `write_all` +
//! [`File::sync_all`](std::fs::File::sync_all), then an atomic rename onto
//! the final name plus a best-effort parent-dir sync. A crash can only
//! leave a temp file behind: temps are never read, so a partial write is
//! always ignored (the previous complete session stays authoritative).
//! Concurrent savers are last-writer-wins; each rename is still atomic, so
//! the file is never partial. Crashed-save litter is swept best-effort
//! *before* writing — never after the rename, so a concurrent saver's live
//! temp is never deleted. Oversize snapshots fail closed *before* touching
//! the filesystem, so a failed save never truncates a good session.
//!
//! # Bounds (fail-closed: any violation rejects the whole file)
//!
//! | Bound | Value | Source |
//! |---|---|---|
//! | File bytes | [`MAX_SESSION_FILE_BYTES`] (1 MiB) | issue: bounded size |
//! | Line bytes | [`MAX_SESSION_LINE_BYTES`] (4096) | parser bound parity |
//! | Workspaces | [`MAX_SESSION_WORKSPACES`] (16) | `MAX_WORKSPACES` |
//! | Panes / workspace | [`MAX_SESSION_PANES_PER_WORKSPACE`] (32) | registry bound |
//! | Panes total (layout panes plus pinned entries) | [`MAX_SESSION_PANES_TOTAL`] (128) | decode-CPU guard |
//! | Pinned entries | counted in the panes total (no separate cap) | live-leaf population |
//! | Scrollback lines / pane | [`MAX_SESSION_SCROLLBACK_LINES_PER_PANE`] (200) | issue: cap restored lines |
//! | Scrollback line bytes | [`MAX_SESSION_LINE_BYTES`] (4096) | parser bound parity |
//! | Cwd bytes | [`MAX_SESSION_CWD_BYTES`] (4096) | `bitty_rich::shell::SHELL_CWD_MAX_BYTES` |
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
//! XDG fallback `$HOME/.local/state/...`; resolution runs inside the
//! injected [`SessionFileBackend`] from injected environment values so
//! tests stay hermetic. Resolution returns `None` (fail-closed, never a
//! panic) when neither root is usable, and with no backend injected every
//! durable path fails closed without touching the filesystem.
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
use std::sync::Arc;

use bitty_ui::{Focus, LayoutNode, PinnedStore, PresentationMode, View, ViewId};

use super::panes::osc7_cwd_path;
use super::workspaces::WorkspaceSlot;
use super::*;

/// Session file format version written by this slice.
///
/// v3 extends the v2 snapshot with the window-global pinned block (one
/// [`PinnedSnapshot`] per parked leaf, in pin order). See the module docs
/// for the v3 record shape and the v1/v2 migration rules.
pub const SESSION_FORMAT_VERSION: u32 = 3;

/// Earliest format version the decoder still migrates. v1 files carry the
/// short pane record (`pane <id> <cols> <rows> <k> <cwd:0|1>`); migration
/// leaves the attachment unspecified (resolved through
/// [`derive_startup_owner`] exactly like the pre-v2 restore), defaults the
/// route to [`PaneRoute::Terminal`], and defaults the mode to
/// [`PresentationMode::Tiled`]. v1 and v2 files carry no pinned block, so
/// migration yields an empty pinned store.
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

/// Maximum bytes of a persisted cwd report (derived from
/// `bitty_rich::shell::SHELL_CWD_MAX_BYTES`, itself the parser bound).
pub const MAX_SESSION_CWD_BYTES: usize = bitty_rich::shell::SHELL_CWD_MAX_BYTES;

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

/// One pinned leaf's persisted state (CTX-1082): the parked view plus its
/// restore anchor and its cwd plus scrollback history.
///
/// A pinned leaf is detached from every workspace layout, so unlike
/// [`PaneSnapshot`] this carries the full stripped [`View`] (identity,
/// geometry, `Floating` mode — origin dropped as presentation-only, exactly
/// like [`strip_overlays`]) instead of referencing a layout leaf. The mode
/// is always [`PresentationMode::Floating`] (pinning stamps it through the
/// CW-08 gate); the route is always terminal, so neither travels as a file
/// token. `anchor`/`after` are the unpin restore hint recorded at pin time
/// (see [`PinnedStore`](bitty_ui::PinnedStore)); a stale anchor falls back
/// to docking beside the first live leaf at unpin.
#[derive(Debug, Clone, PartialEq)]
pub struct PinnedSnapshot {
    /// Parked leaf (stripped: identity, geometry, floating mode).
    pub view: View,
    /// Captured `OSC 7` report, if the pinned leaf had a live grid.
    pub cwd: Option<String>,
    /// Scrollback lines oldest-first, trimmed, bounded like panes.
    pub scrollback: Vec<String>,
    /// Live attachment at capture (`primary`/`session` earn a pending
    /// restore; `detached` restores empty). Always recorded: pinned entries
    /// are v3-born and have no legacy unspecified form.
    pub attach: PaneAttachment,
    /// Neighboring leaf id recorded at pin time, if any.
    pub anchor: Option<ViewId>,
    /// Which side of the anchor the leaf returns to (`false` restores
    /// before the anchor).
    pub after: bool,
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

/// A persistable session: all workspaces plus active index, MRU order, and
/// the window-global pinned store.
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
    /// Parked pinned leaves in pin order (empty for migrated v1/v2
    /// snapshots, which carry no pinned block).
    pub pinned: Vec<PinnedSnapshot>,
}

/// Counts from a successful restore (no contents, safe to log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRestoreSummary {
    /// Workspaces rebuilt.
    pub workspaces: usize,
    /// Panes rebuilt (layout leaves plus restored pinned entries).
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
    /// Panes persisted (layout leaves plus pinned entries).
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
// Durable-commit seam (DEC-W146-2: Core-owned backend trait)
// ---------------------------------------------------------------------------

/// Core-owned session persistence backend (W-146 integration seam).
///
/// Validation-before-mutation stays in Core: [`Runtime::save_session_to_path`]
/// validates the captured snapshot before any backend call, and
/// [`Runtime::apply_session_snapshot`] re-validates before any mutation.
/// The backend implements only the byte mechanics (file codec, atomic
/// temp-plus-rename commit, capped load, XDG path resolution) behind that
/// gate, preserving every [`MAX_SESSION_*`](MAX_SESSION_FILE_BYTES) ceiling,
/// user-only file permissions, and content-free errors.
///
/// The dependency is one-way: Core defines this trait and never imports the
/// extension crate. The application wiring crate implements it with the
/// extracted storage mechanics and injects it via
/// [`Runtime::set_session_backend`]. With no backend injected every path
/// fails closed: saves report a content-free error, loads behave as a
/// missing file (clean start), and the previous on-disk state is untouched.
pub trait SessionFileBackend: Send + Sync + std::fmt::Debug {
    /// Encodes a Core-validated snapshot to file bytes (fails closed).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] (kinds and counts only) when the snapshot
    /// violates a bound or cannot be represented.
    fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError>;

    /// Decodes file bytes to a snapshot; any violation rejects the whole file.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] (kinds and counts only) on corrupt,
    /// oversize, or version-mismatched input. Callers validate again via
    /// [`Runtime::apply_session_snapshot`] before mutating anything.
    fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError>;

    /// Atomically commits `bytes` to `path` (temp write, fsync, rename).
    ///
    /// A failed commit leaves the previous destination untouched; partial
    /// writes are never observable.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] (kinds and counts only) on over-ceiling
    /// payloads or filesystem failures.
    fn commit_session_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SessionError>;

    /// Reads a session file with a hard size cap.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] when no file exists (quiet clean
    /// start); over-ceiling or unreadable files fail closed.
    fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError>;

    /// Session file path from injected env values (`None` fails closed).
    fn session_file_for(&self, xdg_state_home: Option<&str>, home: Option<&str>)
    -> Option<PathBuf>;

    /// Live-environment session file path (`None` fails closed).
    fn session_file(&self) -> Option<PathBuf>;
}

// ---------------------------------------------------------------------------
// Capture helpers (bounds live here; the file codec lives behind
// [`SessionFileBackend`])
// ---------------------------------------------------------------------------

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
// Snapshot validation (shared by encode and apply: fail-closed pre-mutation)
// ---------------------------------------------------------------------------

/// Validates every bound without touching runtime state; content-free errors.
fn validate_snapshot(snap: &SessionSnapshot) -> Result<(), SessionError> {
    if snap.version < SESSION_MIN_DECODE_VERSION || snap.version > SESSION_FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion(snap.version));
    }
    // CTX-1082: only v3 carries a pinned block; an older version claiming
    // pinned entries never came from a decoder (decode migrates to an empty
    // store) and is corrupt.
    if snap.version < SESSION_FORMAT_VERSION && !snap.pinned.is_empty() {
        return Err(SessionError::Corrupt("pinned version"));
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
    // CTX-1082: pinned entries share the decode-CPU total with layout
    // panes (same per-entry history profile) and must name live ids
    // outside every layout tree; the mode is always `Floating` (pinning
    // stamps it) and a session-less pinned leaf carries no state, exactly
    // like a `detached` layout pane.
    for pin in &snap.pinned {
        if !all_views.insert(pin.view.id()) {
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
    total_panes += snap.pinned.len();
    if total_panes > MAX_SESSION_PANES_TOTAL {
        return Err(SessionError::Corrupt("too many panes"));
    }
    // CW-16: the primary grid is single — at most one layout leaf may claim
    // it across the whole file. Pinned entries are excluded from the tally:
    // they never hydrate the shared grid (apply routes every attached pinned
    // leaf to a pending respawn, the stale-owner downgrade by construction),
    // so a recorded pinned `primary` is not a second grid claim.
    // Count resolved attachments, not recorded ones:
    // a `None` (v1-legacy) pane resolves through `derive_startup_owner` at
    // encode, so one explicit primary off-owner plus a `None` on the derived
    // owner would encode to two primaries that the next load rejects.
    // Deriving here is total: `active` indexes a live workspace, every
    // workspace carries a live leaf, and focus is a leaf or absent (all
    // validated above). Resolved primaries subsume explicit ones, so this
    // one check covers both. (Whether the recorded owner actually hydrates
    // the grid is decided at apply: the startup recipe re-pins the primary
    // shell at the focused leaf on every launch, so a recorded owner
    // elsewhere is downgraded to a pending respawn with its own history.)
    let owner = derive_startup_owner(snap);
    let primaries = snap
        .workspaces
        .iter()
        .flat_map(|ws| ws.panes.iter())
        .filter(|pane| resolve_attachment(pane.attach, pane.view, owner) == PaneAttachment::Primary)
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
        LayoutNode::Leaf(view) => {
            let mut fresh = View::with_presentation(
                view.id(),
                usize::from(view.cols()),
                usize::from(view.rows()),
                view.presentation(),
            );
            // CTX-1079: the pseudo flag is layout truth like the mode stamp
            // (solver ignores it, present recomputes geometry), so the strip
            // carries it forward without a format bump.
            fresh.set_pseudo_constraint(view.pseudo_constraint());
            LayoutNode::leaf(fresh)
        }
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
// Runtime integration (capture / apply / startup / exit)
//
// Durable byte mechanics (codec, atomic commit, capped load, path
// resolution) live behind the injected [`SessionFileBackend`]; every method
// below validates in Core before any backend call and fails closed.
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
        // CTX-1082: the window-global pinned store is layout-detached, so
        // the slot loop above never sees it — capture every parked leaf in
        // pin order (present-time paint order) with its live history and
        // its unpin anchor. The stored view rebuilds stripped (identity,
        // geometry, floating mode) so origin never persists, exactly like
        // `strip_overlays`; the Alt+drag re-anchor offset is
        // presentation-only and is never captured (restore resets to the
        // cascade anchor).
        let mut pinned = Vec::with_capacity(self.pinned.len());
        for id in self.pinned.ids() {
            let Some(stored) = self.pinned.get(id).cloned() else {
                continue;
            };
            let mut view = View::with_presentation(
                id,
                usize::from(stored.cols()),
                usize::from(stored.rows()),
                PresentationMode::Floating,
            );
            view.set_pseudo_constraint(stored.pseudo_constraint());
            let state = self.session_state_for(id);
            let (cwd, scrollback) = match state {
                Some(state) => (
                    state.cwd_report().map(str::to_owned),
                    scrollback_tail_text(state, MAX_SESSION_SCROLLBACK_LINES_PER_PANE),
                ),
                None => (None, Vec::new()),
            };
            let attach = if self.primary_view == Some(id) {
                PaneAttachment::Primary
            } else if self.pane_sessions.contains_key(&id) {
                PaneAttachment::Session
            } else {
                PaneAttachment::Detached
            };
            let (anchor, after) = self.pinned.anchor_of(id).unwrap_or((None, true));
            pinned.push(PinnedSnapshot {
                view,
                cwd,
                scrollback,
                attach,
                anchor,
                after,
            });
        }
        SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces,
            active: self.active_workspace,
            mru: self.workspace_mru.iter().copied().collect(),
            pinned,
        }
    }

    /// Applies a validated snapshot: rebuilds workspaces, the live
    /// layout/focus pair, the window-global pinned store, and the primary
    /// owner, rehydrates scrollback into
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
        // `PersistentIdInUse`. The runtime is left untouched. Pinned views
        // collide the same way: their pending restores also drain into
        // fresh grids.
        for ws in &snap.workspaces {
            for pane in &ws.panes {
                if self.pane_sessions.contains_key(&pane.view) {
                    return Err(SessionError::Corrupt("attachment in use"));
                }
            }
        }
        for pin in &snap.pinned {
            if self.pane_sessions.contains_key(&pin.view.id()) {
                return Err(SessionError::Corrupt("attachment in use"));
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
        // CTX-1082: a restore installs a whole new world, so the pinned
        // store is replaced wholesale in snapshot pin order (present-time
        // paint order) with the recorded unpin anchors. The Alt+drag
        // re-anchor offsets reset: they are presentation-only
        // container-derived geometry, and unpin already drops them, so a
        // restored pin presents at its cascade anchor exactly like a fresh
        // pin. Installed before the high-water raise below so restored
        // pinned ids are covered like every other installed id.
        self.pinned = PinnedStore::new();
        self.pinned_offsets.clear();
        for pin in &snap.pinned {
            self.pinned.restore(pin.view.clone(), pin.anchor, pin.after);
        }
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
        // CTX-0803 (#1476): a restore installs a whole new world (layout,
        // focus, primary owner, grids). Any live selection, copy mode, or
        // search addresses the pre-restore grids, so all are dropped
        // (CTX-0805).
        self.drop_all_view_bindings();
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
        // CTX-1082: attached pinned leaves wait pending like attached
        // layout panes — pinned views never own the primary grid (the
        // startup recipe binds it at the derived layout owner), so even a
        // recorded `primary` routes to a pending respawn and keeps its own
        // history and cwd. `Detached` pinned leaves restore empty with no
        // entry, and the entry drains on the next successful spawn of that
        // view.
        for pin in &snap.pinned {
            panes += 1;
            match pin.attach {
                PaneAttachment::Primary | PaneAttachment::Session => {
                    self.stage_pending(pin.view.id(), pin.cwd.clone(), pin.scrollback.clone());
                }
                PaneAttachment::Detached => {}
            }
        }
        // CTX-0873: a restore can change the workspace count across one,
        // which reserves or releases the bar band.
        self.refresh_chrome_band();
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
        self.stage_pending(pane.view, pane.cwd.clone(), pane.scrollback.clone());
    }

    /// Stages one view's captured history and cwd in the pending map for
    /// the next successful spawn of that view (shared by layout panes and
    /// pinned entries, which drain through the same spawn paths).
    fn stage_pending(&mut self, view: ViewId, cwd: Option<String>, scrollback: Vec<String>) {
        self.session_pending
            .insert(view, PendingPaneRestore { cwd, scrollback });
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
    /// `OSC 7` report. Restored attached pins are window-global (in no
    /// layout) yet presented in every workspace scene, so pending ones
    /// respawn here too — covering a startup spawn that failed, or an
    /// apply that landed after startup. Unpinned ids rejoin the layout set
    /// above, so no special path is needed after unpin. Best-effort with
    /// loud warnings; leaves already owning a session are untouched. No-op
    /// before any successful primary attach (no recipe to replay) or with
    /// nothing pending.
    pub(super) fn spawn_session_pending_for_active(&mut self) {
        let Some((program, args)) = self.primary_spawn.clone() else {
            return;
        };
        let still_pending = |view: &ViewId| {
            self.session_pending.contains_key(view)
                && !self.pane_sessions.contains_key(view)
                && Some(*view) != self.primary_view
        };
        let mut targets: Vec<ViewId> = self
            .layout
            .leaf_ids()
            .into_iter()
            .filter(|view| still_pending(view))
            .collect();
        // CTX-1082: pinned ids live in no layout — keep them out of the
        // layout filter above and cover them here, in stable pin order.
        targets.extend(
            self.pinned
                .ids()
                .into_iter()
                .filter(|view| still_pending(view)),
        );
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

    /// Installs (or clears with `None`) the durable-commit backend.
    ///
    /// The application wiring injects the storage-backed implementation at
    /// startup; tests inject a stub or leave it absent to prove fail-closed
    /// behavior. With no backend every durable path fails closed (saves
    /// report a content-free error, loads behave as a missing file) and the
    /// previous on-disk state is untouched.
    pub fn set_session_backend(&mut self, backend: Option<Arc<dyn SessionFileBackend>>) {
        self.session_backend = backend;
    }

    /// Captures, validates, encodes, and atomically persists the session.
    ///
    /// Core validation runs before any backend call, so an invalid snapshot
    /// never reaches the filesystem; the backend enforces the byte ceilings
    /// again on the way through.
    pub fn save_session_to_path(&self, path: &Path) -> Result<SessionSaveSummary, SessionError> {
        let backend = self
            .session_backend
            .clone()
            .ok_or_else(|| SessionError::Io(String::from("no session backend")))?;
        let snap = self.capture_session_snapshot();
        validate_snapshot(&snap)?;
        let bytes = backend.encode_snapshot(&snap)?;
        backend.commit_session_bytes(path, &bytes)?;
        Ok(SessionSaveSummary {
            workspaces: snap.workspaces.len(),
            panes: snap
                .workspaces
                .iter()
                .map(|ws| ws.panes.len())
                .sum::<usize>()
                + snap.pinned.len(),
            scrollback_lines: snap
                .workspaces
                .iter()
                .flat_map(|ws| ws.panes.iter().map(|pane| pane.scrollback.len()))
                .sum::<usize>()
                + snap
                    .pinned
                    .iter()
                    .map(|pin| pin.scrollback.len())
                    .sum::<usize>(),
            bytes: bytes.len(),
        })
    }

    /// Atomically persists the session to the default XDG state path.
    ///
    /// With no backend injected (or no usable state root) this resolves to
    /// [`SessionError::NoStateDir`]: no durable store is configured, so there
    /// is nothing to write and the previous state stays intact.
    pub fn save_session_to_default_path(&self) -> Result<SessionSaveSummary, SessionError> {
        let path = self
            .session_backend
            .clone()
            .and_then(|backend| backend.session_file())
            .ok_or(SessionError::NoStateDir)?;
        self.save_session_to_path(&path)
    }

    /// Loads, decodes, and applies the session at `path` (fail-closed: any
    /// error leaves the runtime untouched).
    ///
    /// With no backend injected this behaves as a missing file (quiet clean
    /// start); the whole snapshot is validated again by
    /// [`Runtime::apply_session_snapshot`] before any mutation.
    pub fn load_session_from_path(
        &mut self,
        path: &Path,
    ) -> Result<SessionRestoreSummary, SessionError> {
        let backend = self.session_backend.clone().ok_or(SessionError::NotFound)?;
        let bytes = backend.load_session_bytes(path)?;
        let snap = backend.decode_snapshot(&bytes)?;
        self.apply_session_snapshot(&snap)
    }

    /// Startup restore with injected environment roots (hermetic twin for
    /// tests; production uses [`Runtime::restore_session_on_startup`]).
    ///
    /// `safe_mode` short-circuits before any filesystem or backend access:
    /// recovery startup never reads session state. A missing backend, a
    /// missing state root, or a missing file is a quiet clean start
    /// ([`SessionStartupOutcome::Fresh`]); any other failure keeps
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
        let Some(path) = self
            .session_backend
            .clone()
            .and_then(|backend| backend.session_file_for(xdg_state_home, home))
        else {
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
    use bitty_ui::SplitAxis;
    use std::sync::{Arc, Mutex};

    /// Recording stub: proves Core validates before any backend call.
    #[derive(Debug, Default)]
    struct RecordingBackend {
        calls: Mutex<Vec<&'static str>>,
    }

    impl SessionFileBackend for RecordingBackend {
        fn encode_snapshot(&self, _snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
            self.calls.lock().expect("stub lock").push("encode");
            Ok(Vec::new())
        }

        fn decode_snapshot(&self, _bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
            self.calls.lock().expect("stub lock").push("decode");
            Err(SessionError::Corrupt("stub"))
        }

        fn commit_session_bytes(&self, _path: &Path, _bytes: &[u8]) -> Result<(), SessionError> {
            self.calls.lock().expect("stub lock").push("commit");
            Ok(())
        }

        fn load_session_bytes(&self, _path: &Path) -> Result<Vec<u8>, SessionError> {
            self.calls.lock().expect("stub lock").push("load");
            Err(SessionError::NotFound)
        }

        fn session_file_for(
            &self,
            _xdg_state_home: Option<&str>,
            _home: Option<&str>,
        ) -> Option<PathBuf> {
            self.calls.lock().expect("stub lock").push("paths");
            None
        }

        fn session_file(&self) -> Option<PathBuf> {
            self.calls.lock().expect("stub lock").push("paths");
            None
        }
    }

    #[test]
    fn save_validates_before_touching_backend() {
        let backend = Arc::new(RecordingBackend::default());
        let mut rt = Runtime::with_defaults().expect("defaults build");
        // Reach an invalid snapshot only through module-visible state: no
        // public API can empty the workspace list, so any production capture
        // is valid and this path exercises the fail-closed order, never a
        // reachable state.
        rt.workspaces.clear();
        rt.set_session_backend(Some(backend.clone()));
        let err = rt
            .save_session_to_path(Path::new("/tmp/should-never-be-touched"))
            .expect_err("empty workspace list must fail validation");
        assert!(
            matches!(err, SessionError::Corrupt(_)),
            "validation rejects before the backend: {err}"
        );
        assert!(
            backend.calls.lock().expect("stub lock").is_empty(),
            "no backend call may precede validation"
        );
    }

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
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
            pinned: Vec::new(),
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
            pinned: Vec::new(),
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
            pinned: Vec::new(),
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
    fn validation_counts_resolved_primaries_not_just_recorded() {
        // CTX-0972 F4 round-trip break: one explicit primary off the derived
        // owner (leaf 2) plus a legacy `None` on the owner (leaf 1) records
        // a single primary, but encode resolves the `None` to a second
        // primary that the next load rejects.
        let mut ambiguous = two_pane_snapshot(None);
        ambiguous.workspaces[0].panes[1].attach = Some(PaneAttachment::Primary);
        assert_eq!(
            ambiguous.workspaces[0]
                .panes
                .iter()
                .filter(|pane| pane.attach == Some(PaneAttachment::Primary))
                .count(),
            1,
            "the old explicit-only check would accept this file"
        );
        let err = validate_snapshot(&ambiguous).expect_err("ambiguous ownership must fail");
        assert_eq!(format!("{err}"), "session file corrupt (duplicate primary)");

        // 0-explicit still loads and resolves to exactly one primary.
        let legacy = two_pane_snapshot(None);
        validate_snapshot(&legacy).expect("legacy Nones must validate");
        let owner = derive_startup_owner(&legacy);
        let resolved = legacy.workspaces[0]
            .panes
            .iter()
            .filter(|pane| {
                resolve_attachment(pane.attach, pane.view, owner) == PaneAttachment::Primary
            })
            .count();
        assert_eq!(
            resolved, 1,
            "legacy Nones must resolve to exactly one primary"
        );

        // 1-explicit-on-owner plus Nones elsewhere loads.
        let mut on_owner = SessionSnapshot {
            version: SESSION_FORMAT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                seq: 1,
                name: "ws1".to_string(),
                layout: LayoutNode::stack(vec![leaf(1), leaf(2), leaf(3)]),
                focus: Some(ViewId::new(1)),
                panes: vec![
                    v2_pane(1, Some(PaneAttachment::Primary)),
                    v2_pane(2, None),
                    v2_pane(3, None),
                ],
            }],
            active: 0,
            mru: vec![0],
            pinned: Vec::new(),
        };
        validate_snapshot(&on_owner).expect("primary on the owner must validate");
        on_owner.workspaces[0].panes[1].attach = Some(PaneAttachment::Session);
        validate_snapshot(&on_owner).expect("explicit session off-owner must validate");
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

    fn pinned_entry(id: u64, attach: PaneAttachment) -> PinnedSnapshot {
        PinnedSnapshot {
            view: View::with_presentation(ViewId::new(id), 80, 24, PresentationMode::Floating),
            cwd: None,
            scrollback: Vec::new(),
            attach,
            anchor: Some(ViewId::new(1)),
            after: false,
        }
    }

    #[test]
    fn pinned_validation_rejects_duplicates_and_old_version_claims() {
        // A pinned id colliding with a layout leaf aliases two histories
        // onto one session.
        let mut colliding = two_pane_snapshot(Some(PaneAttachment::Primary));
        colliding
            .pinned
            .push(pinned_entry(2, PaneAttachment::Session));
        let err = validate_snapshot(&colliding).expect_err("pinned/layout alias must fail");
        assert_eq!(format!("{err}"), "session file corrupt (duplicate pane)");

        // Two pinned entries sharing one id collide the same way.
        let mut doubled = two_pane_snapshot(Some(PaneAttachment::Primary));
        doubled
            .pinned
            .push(pinned_entry(9, PaneAttachment::Session));
        doubled
            .pinned
            .push(pinned_entry(9, PaneAttachment::Detached));
        let err = validate_snapshot(&doubled).expect_err("pinned/pinned alias must fail");
        assert_eq!(format!("{err}"), "session file corrupt (duplicate pane)");

        // v1/v2 carry no pinned block by construction: an older version
        // claiming pinned entries never came from a decoder.
        let mut backdated = two_pane_snapshot(Some(PaneAttachment::Primary));
        backdated.version = 2;
        backdated
            .pinned
            .push(pinned_entry(9, PaneAttachment::Session));
        let err = validate_snapshot(&backdated).expect_err("v2 with pinned must fail");
        assert_eq!(format!("{err}"), "session file corrupt (pinned version)");

        // A session-less pinned leaf carrying state is corrupt, mirroring
        // the layout `detached` rule.
        let mut stowaway = two_pane_snapshot(Some(PaneAttachment::Primary));
        let mut smuggled = pinned_entry(9, PaneAttachment::Detached);
        smuggled.scrollback = vec!["stowaway".to_string()];
        stowaway.pinned.push(smuggled);
        let err = validate_snapshot(&stowaway).expect_err("stateful detached pin must fail");
        assert_eq!(
            format!("{err}"),
            "session file corrupt (detached pane state)"
        );

        // Pinned views are floating by construction; anything else is corrupt.
        let mut tiled = two_pane_snapshot(Some(PaneAttachment::Primary));
        let mut wrong_mode = pinned_entry(9, PaneAttachment::Session);
        wrong_mode.view = View::new(ViewId::new(9), 80, 24);
        tiled.pinned.push(wrong_mode);
        let err = validate_snapshot(&tiled).expect_err("non-floating pin must fail");
        assert_eq!(format!("{err}"), "session file corrupt (pinned mode)");
    }

    #[test]
    fn pinned_entries_share_the_total_pane_bound() {
        let mut snap = two_pane_snapshot(Some(PaneAttachment::Primary));
        snap.pinned.push(pinned_entry(9, PaneAttachment::Session));
        validate_snapshot(&snap).expect("one pinned entry validates");
        // Fill to the total with distinct pinned ids, then tip over it.
        for id in 10..(10 + (MAX_SESSION_PANES_TOTAL - 3) as u64) {
            snap.pinned.push(pinned_entry(id, PaneAttachment::Detached));
        }
        validate_snapshot(&snap).expect("exactly at the total validates");
        snap.pinned
            .push(pinned_entry(1000, PaneAttachment::Detached));
        let err = validate_snapshot(&snap).expect_err("over the total must fail");
        assert_eq!(format!("{err}"), "session file corrupt (too many panes)");
    }

    #[test]
    fn capture_reads_pinned_live_grids_with_attachment() {
        // CTX-1082: capture reads a pinned leaf's live grid exactly like a
        // layout leaf's (the store detaches the leaf, never its session).
        // Hermetic: the test parks the leaf directly and points the primary
        // grid at it — pinning through the public path would hand primary
        // ownership to the surviving leaf first.
        let mut rt = Runtime::with_defaults().expect("defaults build");
        // Feed past the viewport so history enters the scrollback ring
        // (a single visible line is grid, not scrollback).
        for i in 0..30 {
            rt.handle_pty_bytes(format!("pinned-history {i:02}\r\n").as_bytes());
        }
        assert!(rt.state().scrollback_len() > 0, "must have scrollback");
        let parked = View::with_presentation(ViewId::new(9), 80, 24, PresentationMode::Floating);
        rt.pinned.restore(parked, Some(ViewId::new(1)), false);
        rt.primary_view = Some(ViewId::new(9));

        let snap = rt.capture_session_snapshot();
        assert_eq!(snap.pinned.len(), 1);
        let pin = &snap.pinned[0];
        assert_eq!(pin.view.id(), ViewId::new(9));
        assert_eq!(pin.attach, PaneAttachment::Primary);
        assert_eq!(pin.anchor, Some(ViewId::new(1)));
        assert!(!pin.after);
        assert!(
            pin.scrollback
                .iter()
                .any(|line| line.contains("pinned-history")),
            "pinned history captures from the live grid"
        );

        // The captured entry applies: a recorded pinned primary routes to
        // pending (it never hydrates the shared grid).
        let mut fresh = Runtime::with_defaults().expect("defaults build");
        let summary = fresh.apply_session_snapshot(&snap).expect("apply valid");
        assert_eq!(fresh.pinned_views(), vec![ViewId::new(9)]);
        assert!(fresh.session_pending_contains(&ViewId::new(9)));
        assert_eq!(summary.pending, 1);
    }

    #[test]
    fn old_version_snapshot_without_pinned_still_validates() {
        // CTX-1082 compat: a v2 snapshot (no pinned block) validates as-is;
        // decode migrates it to an empty pinned store.
        let mut legacy = two_pane_snapshot(Some(PaneAttachment::Primary));
        legacy.version = 2;
        validate_snapshot(&legacy).expect("v2 without pinned validates");
        let mut ancient = two_pane_snapshot(None);
        ancient.version = SESSION_MIN_DECODE_VERSION;
        validate_snapshot(&ancient).expect("v1 without pinned validates");
        let mut future = two_pane_snapshot(Some(PaneAttachment::Primary));
        future.version = SESSION_FORMAT_VERSION + 1;
        assert!(matches!(
            validate_snapshot(&future),
            Err(SessionError::UnsupportedVersion(_))
        ));
    }
}
