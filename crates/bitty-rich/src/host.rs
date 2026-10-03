//! Typed Core host operations behind the Composer boundary, phase B
//! (CTX-0929, W-103 S-1b).
//!
//! Accepted contract:
//! `bitty-terminal-docs/specifications/composer-architecture.md` (`W-82`).
//! This module implements the Core side of two Composer-facing host
//! operations with the accepted spellings (DEC-W103-3):
//!
//! - `terminal.submit` (`bitty.terminal.submit(text) -> outcome`): bounded
//!   submission framing with panel-lease and per-plugin-budget gates. The
//!   byte-exact bracketed-paste frame (`ESC[200~` content `ESC[201~` `CR`)
//!   stays host-side (W-82 requires host framing); the extension passes raw
//!   text and never frames bytes itself.
//! - `process.editor` (`bitty.process.editor.start(path) -> outcome`):
//!   allowlisted external-editor round trip with the Bitty-owned temp policy,
//!   a minimized child environment, closed-stdin default, a bounded wait with
//!   owned-tree kill, and typed outcomes.
//!
//! Phase-B scope only: no overlay/capture API (`ui.overlay.focus` and
//! `bitty.overlay.*` stay with CTX-0941 per DEC-W103-2), no SDK binding
//! (`W-120`), no Lua plugin, no composer/chrome deletion, no modal rewiring
//! (cutover is later). The legacy [`crate::composer`] mechanics and the
//! Core-internal `CwComposerFeed` routing are untouched; this module is the
//! typed API boundary those mechanics grow into (G-4).
//!
//! Gaps closed (W-82 "Current implementation status", W-103 plan section 4):
//!
//! - G-1 ([`owned_temp_root`]): the editor temp file lives in a Bitty-owned
//!   `0700` root instead of the ambient OS temp directory. Creation is
//!   exclusive and validated (symlink/dir/mode checked, fail-closed), and
//!   [`sweep_crashed_temps`] removes dead-owner leftovers on the next start
//!   after a crash.
//! - G-2 ([`build_hosted_env`], [`minimized_env_removals`],
//!   [`run_editor_hosted`]): the blocking editor child starts with a
//!   minimized environment (exact keep-allowlist, no ambient credentials)
//!   and closed stdin. The interactive PTY leaf is the explicit exception
//!   and keeps its PTY stdio; it receives the same minimized environment
//!   through `PtyBuilder::env_remove`.
//! - G-3 ([`run_editor_hosted`]): the spawned child is recorded through the
//!   execution boundary's reviewed tracker ([`bitty_pty::OwnedTree`]) and a
//!   timeout kills the whole recorded tree, not just the direct child.
//!   Platforms without an owned-tree backend keep direct-child semantics
//!   (documented, fail-closed).
//! - G-4 ([`SubmitDeny`], [`check_terminal_submit`], [`EditorOutcome`]):
//!   typed host outcomes at the API boundary. Denials name the violated rule,
//!   never the payload.
//!
//! Bounds (carried values per DEC-W103-4, no invented bounds):
//!
//! | Dimension | Bound | Source |
//! |---|---|---|
//! | buffer / temp payload | [`COMPOSER_MAX_BYTES`] (64 KiB) | `composer.rs` |
//! | submit frame | content `+ 13` bytes, fail-closed past cap | `frame_submit` |
//! | editor wait | 120 s default, 300 s ceiling | `EDITOR_TIMEOUT_*` |
//! | editor program | `nvim`/`vim`/`vi` bare names | `EDITOR_ALLOWLIST` |
//! | per-plugin submit window | caller-supplied cap | Isolation/Resource RFC lane (not invented here) |
//!
//! No I/O except the editor round trip and temp-root management, no
//! wall-clock except the editor timeout poll loop, no randomness for content
//! (temp names mix pid + nanos + counter only), no unsafe.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::composer::{
    COMPOSER_MAX_BYTES, CommandBuffer, EDITOR_TIMEOUT_MAX, EditorError, TEMP_PREFIX,
    editor_round_trip, frame_submit, resolve_editor, truncate_err,
};

// ---------------------------------------------------------------------------
// G-1: Bitty-owned 0700 temp root
// ---------------------------------------------------------------------------

/// Directory name of the Bitty-owned editor temp root inside its base.
pub const OWNED_TEMP_DIR_NAME: &str = "bitty-composer";

/// Returns the Bitty-owned editor temp root, creating it when absent.
///
/// The root lives under `$XDG_RUNTIME_DIR` when that variable names an
/// absolute directory, else under [`std::env::temp_dir`]. Creation is
/// exclusive (`create_dir`, never `create_dir_all` on the leaf) and the
/// result is validated before use: the path must resolve to a real directory
/// (never a symlink), and on Unix its mode must be exactly `0700`. Any
/// deviation fails closed with [`EditorError::WriteFailed`] rather than
/// using a squatted or drifted directory; a squat can at worst deny service,
/// never disclose a draft.
///
/// # Errors
///
/// [`EditorError::WriteFailed`] when the root cannot be created or fails
/// validation. The message names the failure class, never a temp path.
pub fn owned_temp_root() -> Result<PathBuf, EditorError> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    owned_temp_root_under(&base)
}

/// [`owned_temp_root`] against an explicit base (hermetic seam for tests).
///
/// # Errors
///
/// As [`owned_temp_root`].
pub(crate) fn owned_temp_root_under(base: &Path) -> Result<PathBuf, EditorError> {
    let root = base.join(OWNED_TEMP_DIR_NAME);
    match std::fs::create_dir(&root) {
        Ok(()) => {
            // Freshly created by this process: assert owner-only before use.
            // (`create_dir` honors umask, so the mode is not 0700 yet.
            // Non-Unix has no safe-std ACL API; the per-user base-directory
            // ACL is the guarantee there, same residual as the temp file.)
            #[cfg(unix)]
            if let Err(e) = restrict_dir_owner_only(&root) {
                let _ = std::fs::remove_dir(&root);
                return Err(EditorError::WriteFailed(truncate_err(format!(
                    "owned temp root chmod failed: {e}"
                ))));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(EditorError::WriteFailed(truncate_err(format!(
                "owned temp root unavailable: {e}"
            ))));
        }
    }
    validate_owned_temp_root(&root)?;
    Ok(root)
}

/// Rejects symlink escapes, non-directories, and wrong modes (fail-closed).
fn validate_owned_temp_root(root: &Path) -> Result<(), EditorError> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| {
        EditorError::WriteFailed(truncate_err(format!("owned temp root unreadable: {e}")))
    })?;
    if meta.file_type().is_symlink() {
        return Err(EditorError::WriteFailed(String::from(
            "owned temp root is a symlink",
        )));
    }
    if !meta.is_dir() {
        return Err(EditorError::WriteFailed(String::from(
            "owned temp root is not a directory",
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(EditorError::WriteFailed(String::from(
                "owned temp root is not owner-only (0700)",
            )));
        }
    }
    Ok(())
}

/// Sets `0700` on a directory this process just created.
///
/// Non-Unix has no safe-std ACL API (same residual as the temp file itself,
/// W-82 documented): the per-user base-directory ACL is the guarantee there.
#[cfg(unix)]
fn restrict_dir_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

// ---------------------------------------------------------------------------
// G-1 continued: crash-restart sweep
// ---------------------------------------------------------------------------

/// Removes crashed-owner editor temps from the owned root.
///
/// On the next start after a crash, temp files whose creating process is
/// provably dead are unlinked. A file is removed only when its name carries
/// the [`TEMP_PREFIX`] shape with a parsed pid that is dead by
/// [`temp_pid_is_dead`]; live-owner files, foreign names, and unprovable
/// files are always kept (a sweep that cannot prove death must not delete).
/// Returns the number of files removed. Best-effort and infallible: an
/// unreadable root sweeps nothing.
#[must_use]
pub fn sweep_crashed_temps() -> usize {
    let Ok(root) = owned_temp_root() else {
        return 0;
    };
    sweep_crashed_temps_under(&root)
}

/// [`sweep_crashed_temps`] against an explicit root (hermetic seam).
#[must_use]
pub(crate) fn sweep_crashed_temps_under(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let current = std::process::id();
    let mut removed = 0_usize;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(pid) = temp_file_pid(&name.to_string_lossy()) else {
            continue;
        };
        if pid == current || !temp_pid_is_dead(pid) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// Parses the creating pid out of a `bitty-composer-<pid>-<nanos>-<seq>` name.
fn temp_file_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(TEMP_PREFIX)?;
    rest.split('-').next()?.parse::<u32>().ok()
}

/// Whether `pid` provably names no live process.
///
/// Pid `0` never owns a file (our names never embed it). On Linux/Android a
/// live pid owns `/proc/<pid>`; a vanished entry means death. (A recycled pid
/// errs toward keeping: the safe direction is disk use, never disclosure.)
/// Other platforms have no safe-std dead-pid probe, so only pid `0` counts
/// as dead there and every other file is kept.
fn temp_pid_is_dead(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        std::fs::symlink_metadata(format!("/proc/{pid}")).is_err()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// G-2: minimized editor environment
// ---------------------------------------------------------------------------

/// Exact environment keys a hosted editor child keeps.
///
/// Everything else in the ambient environment is removed before spawn, so
/// credentials, tokens, and shell-startup hijack vectors never reach the
/// editor. Functional terminal keys stay (`PATH` for bare-name lookup,
/// `HOME` for editor config, locale, `TMPDIR` for editor swap files);
/// graphics/session markers need no listing because removal is
/// allowlist-based. `TERM`/`COLORTERM`/`TERM_PROGRAM` are (re-)applied by
/// the spawn path defaults.
pub const HOSTED_ENV_KEEP: &[&str] = &[
    "PATH",
    "HOME",
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "TZ",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "EDITOR",
    "VISUAL",
];

/// Whether an environment key survives minimization.
#[must_use]
pub fn hosted_env_keeps(key: &str) -> bool {
    HOSTED_ENV_KEEP.contains(&key)
}

/// Builds the minimized environment for the blocking editor child: the
/// ambient variables whose keys are in [`HOSTED_ENV_KEEP`], nothing else.
#[must_use]
pub fn build_hosted_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| hosted_env_keeps(&key.to_string_lossy()))
        .collect()
}

/// Inherited keys the PTY-leaf editor child must NOT receive.
///
/// The caller applies these through `PtyBuilder::env_remove` (which runs
/// after the graphics-fingerprint strip and before explicit overrides, so a
/// caller-explicit entry still wins). Same policy as [`build_hosted_env`],
/// expressed as removals for the PTY spawn path that inherits by default.
#[must_use]
pub fn minimized_env_removals() -> Vec<OsString> {
    std::env::vars_os()
        .filter_map(|(key, _)| {
            if hosted_env_keeps(&key.to_string_lossy()) {
                None
            } else {
                Some(key)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// terminal.submit: framing gate + per-plugin budget + typed denial
// ---------------------------------------------------------------------------

/// Maximum plugin-id length accepted by [`SubmitBudget`] (bounded attribution).
pub const SUBMIT_PLUGIN_ID_MAX: usize = 128;

/// Per-plugin submit byte budget (P0-AC-014 attribution).
///
/// The window cap is caller-supplied: the numeric budget policy belongs to
/// the Isolation/Resource lane, which this operation must not invent
/// (DEC-W103-4). The operation only enforces and attributes: [`try_charge`](Self::try_charge)
/// fails closed once the window is exhausted, and the plugin id is
/// length-bounded at construction.
#[derive(Debug, Clone)]
pub struct SubmitBudget {
    plugin: String,
    used: u64,
    cap: u64,
}

impl SubmitBudget {
    /// Creates a budget for `plugin` with a `window_cap_bytes` byte window.
    #[must_use]
    pub fn new(plugin: &str, window_cap_bytes: u64) -> Self {
        let mut id = plugin.to_string();
        if id.len() > SUBMIT_PLUGIN_ID_MAX {
            id.truncate(SUBMIT_PLUGIN_ID_MAX);
        }
        Self {
            plugin: id,
            used: 0,
            cap: window_cap_bytes,
        }
    }

    /// Charges `bytes` against the window (fails closed, usage unchanged on
    /// refusal).
    pub fn try_charge(&mut self, bytes: u64) -> bool {
        let wanted = self.used.saturating_add(bytes);
        if wanted > self.cap {
            return false;
        }
        self.used = wanted;
        true
    }

    /// Attributed plugin id (length-bounded, never payload).
    #[must_use]
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Bytes charged so far in this window.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Window cap in bytes.
    #[must_use]
    pub fn cap(&self) -> u64 {
        self.cap
    }
}

/// Why `terminal.submit` refused a submission (fail-closed, nothing emitted).
///
/// Denials name the violated rule, never the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitDeny {
    /// Framed payload would exceed [`COMPOSER_MAX_BYTES`] plus framing.
    TooLarge {
        /// Bytes the content had.
        wanted: usize,
    },
    /// The panel lease write rule refused (not the holder or tenure lapsed).
    LeaseDenied,
    /// The plugin's submit byte window is exhausted.
    BudgetExceeded {
        /// Bytes already charged in this window.
        used: u64,
        /// Window cap in bytes.
        cap: u64,
    },
}

impl std::fmt::Display for SubmitDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { wanted } => write!(
                f,
                "submit too large ({wanted} bytes, max {COMPOSER_MAX_BYTES})"
            ),
            Self::LeaseDenied => f.write_str("submit denied: panel lease write refused"),
            Self::BudgetExceeded { used, cap } => {
                write!(
                    f,
                    "submit denied: plugin submit budget exceeded ({used}/{cap} bytes)"
                )
            }
        }
    }
}

impl std::error::Error for SubmitDeny {}

/// Checks and frames a `terminal.submit` (`bitty.terminal.submit`) request.
///
/// Order (all fail-closed, nothing emitted on refusal): byte-cap check, then
/// the panel-lease gate (`lease_granted` is the caller's evaluated lease
/// write rule for the focused panel), then the per-plugin budget charge.
/// On success returns the byte-exact bracketed-paste frame
/// (`ESC[200~` content `ESC[201~` `CR`) for one PTY write and the budget is
/// charged; on refusal the budget is untouched.
///
/// # Errors
///
/// [`SubmitDeny::TooLarge`], [`SubmitDeny::LeaseDenied`], or
/// [`SubmitDeny::BudgetExceeded`].
pub fn check_terminal_submit(
    text: &str,
    lease_granted: bool,
    budget: &mut SubmitBudget,
) -> Result<Vec<u8>, SubmitDeny> {
    if text.len() > COMPOSER_MAX_BYTES {
        return Err(SubmitDeny::TooLarge { wanted: text.len() });
    }
    if !lease_granted {
        return Err(SubmitDeny::LeaseDenied);
    }
    // `frame_submit` re-checks the cap; unreachable after the pre-check, so
    // the defensive map can never misreport a size.
    let frame = frame_submit(text).map_err(|_| SubmitDeny::TooLarge { wanted: text.len() })?;
    let bytes = frame.len() as u64;
    if !budget.try_charge(bytes) {
        return Err(SubmitDeny::BudgetExceeded {
            used: budget.used(),
            cap: budget.cap(),
        });
    }
    Ok(frame)
}

// ---------------------------------------------------------------------------
// process.editor: typed outcomes (G-4)
// ---------------------------------------------------------------------------

/// Why `process.editor` denied a launch before any side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorDeny {
    /// Neither `$VISUAL` nor `$EDITOR` names an editor.
    NoEditor,
    /// The chosen program is not in the exact bare-name allowlist.
    NotAllowed,
}

impl std::fmt::Display for EditorDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEditor => f.write_str("no editor: set $VISUAL or $EDITOR"),
            Self::NotAllowed => f.write_str("editor not allowed: not in the bare-name allowlist"),
        }
    }
}

/// Typed `process.editor` (`bitty.process.editor.start`) outcome (W-82 G-4).
///
/// The accepted minimum (accepted/edited/cancelled/denied/timeout/
/// spawn-failed/non-zero/unavailable) plus `Signal` for hosted children
/// killed by a signal. Denials name the rule, never the payload or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorOutcome {
    /// Edited content returned (blocking) or applied to the draft (hosted).
    Edited(String),
    /// The round trip was cancelled (hosted leaf closed before exit).
    Cancelled,
    /// Launch denied before any temp file or child.
    Denied(EditorDeny),
    /// The bounded wait expired; the recorded tree was killed.
    Timeout,
    /// The editor could not be spawned (bounded OS detail).
    SpawnFailed(String),
    /// The editor exited non-zero (code when known).
    NonZeroExit(Option<i32>),
    /// The hosted editor died by signal (signal name, bounded).
    Signal(String),
    /// The operation cannot proceed (static reason: temp/read-back/apply
    /// unavailability; never a path or payload).
    Unavailable(&'static str),
}

impl std::fmt::Display for EditorOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Edited(content) => write!(f, "editor returned {} bytes", content.len()),
            Self::Cancelled => f.write_str("editor cancelled"),
            Self::Denied(d) => write!(f, "editor denied: {d}"),
            Self::Timeout => f.write_str("editor timed out and its tree was killed"),
            Self::SpawnFailed(e) => write!(f, "editor spawn failed: {e}"),
            Self::NonZeroExit(code) => match code {
                Some(c) => write!(f, "editor exited with status {c}"),
                None => f.write_str("editor exited with unknown failure"),
            },
            Self::Signal(s) => write!(f, "editor killed by signal {s}"),
            Self::Unavailable(r) => write!(f, "editor unavailable: {r}"),
        }
    }
}

impl From<EditorError> for EditorOutcome {
    /// Maps the blocking error contract onto typed host outcomes.
    ///
    /// Authorization failures become `Denied`; the deadline becomes
    /// `Timeout`; spawn/exit map 1:1; temp-file and read-back failures become
    /// `Unavailable` with a static reason (never a path or payload).
    /// The buffer is preserved on every error path by construction.
    fn from(err: EditorError) -> Self {
        match err {
            EditorError::NoEditor => Self::Denied(EditorDeny::NoEditor),
            EditorError::NotAllowed => Self::Denied(EditorDeny::NotAllowed),
            EditorError::Timeout => Self::Timeout,
            EditorError::SpawnFailed(e) => Self::SpawnFailed(e),
            EditorError::NonZeroExit(c) => Self::NonZeroExit(c),
            EditorError::WriteFailed(_) => Self::Unavailable("composer temp unavailable"),
            EditorError::ReadFailed(_) => Self::Unavailable("edited file unreadable"),
            EditorError::TooLarge { .. } => Self::Unavailable("edited file too large"),
            EditorError::InvalidUtf8 => Self::Unavailable("edited file is not valid UTF-8"),
            EditorError::WaitFailed(_) => Self::Unavailable("editor wait unavailable"),
        }
    }
}

// ---------------------------------------------------------------------------
// process.editor: hosted blocking spawn (G-2 + G-3)
// ---------------------------------------------------------------------------

/// Poll interval while waiting for the hosted editor child (carried value:
/// same 5 ms as the legacy blocking primitive, DEC-W103-4).
const HOST_EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Spawns `editor` on `path` with the hosted child policy and waits up to
/// `timeout` (clamped to [`EDITOR_TIMEOUT_MAX`]).
///
/// Differences from the legacy [`crate::composer::run_editor`]:
/// minimized environment ([`build_hosted_env`], no ambient credentials),
/// closed stdin (execution-boundary default; the interactive PTY leaf is the
/// explicit exception and does not use this path), and owned-tree tracking:
/// the child is recorded through [`bitty_pty::OwnedTree`] and a timeout
/// signals the whole recorded tree ([`bitty_pty::TreeSignal::Kill`]).
/// Platforms without an owned-tree backend keep direct-child kill semantics.
///
/// Like the legacy primitive this function does **not** apply the editor
/// allowlist; the environment-driven path ([`process_editor_start`]) resolves
/// through [`resolve_editor`] first. No shell: the program travels unsplit
/// and the temp path is one argv element.
///
/// # Errors
///
/// [`EditorError::NoEditor`] on blank input; [`EditorError::SpawnFailed`],
/// [`EditorError::Timeout`] (tree killed), [`EditorError::NonZeroExit`],
/// [`EditorError::WaitFailed`].
pub fn run_editor_hosted(editor: &str, path: &Path, timeout: Duration) -> Result<(), EditorError> {
    let program = editor.trim();
    if program.is_empty() {
        return Err(EditorError::NoEditor);
    }
    let timeout = timeout.min(EDITOR_TIMEOUT_MAX);
    let mut command = std::process::Command::new(program);
    command.arg(path);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());
    command.env_clear();
    for (key, value) in build_hosted_env() {
        command.env(key, value);
    }
    bitty_pty::OwnedTree::prepare_command(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| EditorError::SpawnFailed(truncate_err(e.to_string())))?;
    // Adopt immediately (on Windows this also resumes the prepared child, on
    // every path). `None` on unsupported platforms: direct-child fallback.
    let tree = bitty_pty::OwnedTree::adopt_prepared(child.id()).ok();
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let outcome = if status.success() {
                    Ok(())
                } else {
                    Err(EditorError::NonZeroExit(status.code()))
                };
                // The leader is reaped; retire the tracker so no later
                // signal can hit the recycled id.
                if let Some(tree) = tree {
                    tree.retire(|| ());
                }
                return outcome;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    match tree {
                        Some(tree) => {
                            let _ = tree.signal(bitty_pty::TreeSignal::Kill);
                            let _ = tree.retire(|| child.wait());
                        }
                        None => {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                    return Err(EditorError::Timeout);
                }
                std::thread::sleep(HOST_EDITOR_POLL_INTERVAL);
            }
            Err(e) => {
                match tree {
                    Some(tree) => {
                        let _ = tree.signal(bitty_pty::TreeSignal::Kill);
                        let _ = tree.retire(|| child.wait());
                    }
                    None => {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                return Err(EditorError::WaitFailed(truncate_err(e.to_string())));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// process.editor: hosted blocking round trip with typed outcome
// ---------------------------------------------------------------------------

/// Full `process.editor` round trip against `buffer` with a typed outcome.
///
/// 1. resolves and allowlists `$VISUAL`/`$EDITOR` ([`resolve_editor`];
///    a hostile value is [`EditorOutcome::Denied`] before any side effect),
/// 2. writes a `0600` temp file in the Bitty-owned root ([`owned_temp_root`]),
/// 3. spawns the editor with the hosted child policy ([`run_editor_hosted`]),
/// 4. reads the file back (bounded, UTF-8),
/// 5. deletes the temp file in all cases (RAII),
/// 6. installs the result into `buffer` (fail-closed past the cap, old
///    content kept).
///
/// The hermetic seam (explicit temp dir) is [`process_editor_start_in`].
pub fn process_editor_start(
    buffer: &mut CommandBuffer,
    visual: Option<&str>,
    editor: Option<&str>,
    timeout: Duration,
) -> EditorOutcome {
    let program = match resolve_editor(visual, editor) {
        Ok(program) => program,
        Err(EditorError::NotAllowed) => return EditorOutcome::Denied(EditorDeny::NotAllowed),
        Err(_) => return EditorOutcome::Denied(EditorDeny::NoEditor),
    };
    let root = match owned_temp_root() {
        Ok(root) => root,
        Err(_) => return EditorOutcome::Unavailable("composer temp unavailable"),
    };
    match editor_round_trip(buffer, &program, timeout, &root, run_editor_hosted) {
        Ok(content) => EditorOutcome::Edited(content),
        Err(e) => EditorOutcome::from(e),
    }
}

/// [`process_editor_start`] against an explicit temp dir (hermetic seam for
/// tests; production always uses the owned root).
#[cfg(test)]
pub(crate) fn process_editor_start_in(
    buffer: &mut CommandBuffer,
    visual: Option<&str>,
    editor: Option<&str>,
    timeout: Duration,
    dir: &Path,
) -> EditorOutcome {
    let program = match resolve_editor(visual, editor) {
        Ok(program) => program,
        Err(EditorError::NotAllowed) => return EditorOutcome::Denied(EditorDeny::NotAllowed),
        Err(_) => return EditorOutcome::Denied(EditorDeny::NoEditor),
    };
    match editor_round_trip(buffer, &program, timeout, dir, run_editor_hosted) {
        Ok(content) => EditorOutcome::Edited(content),
        Err(e) => EditorOutcome::from(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use bitty_test_support::require_pty;

    fn workdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("bitty-host-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&base).expect("test workdir");
        base
    }

    // -- G-1: owned root ------------------------------------------------------

    #[test]
    fn owned_root_is_created_owner_only_and_idempotent() {
        let base = workdir();
        // Point the root under an isolated base via the hermetic seam.
        let root = owned_temp_root_under(&base).expect("owned root");
        assert!(root.is_dir());
        assert_no_symlink(&root);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&root)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
        // Second call reuses the validated root.
        let again = owned_temp_root_under(&base).expect("owned root again");
        assert_eq!(root, again);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn owned_root_rejects_wrong_mode() {
        let base = workdir();
        let root = base.join(OWNED_TEMP_DIR_NAME);
        std::fs::create_dir_all(&root).expect("pre-create");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
                .expect("chmod 0755");
            let err = owned_temp_root_under(&base).expect_err("wrong mode must fail");
            assert!(matches!(err, EditorError::WriteFailed(_)));
        }
        #[cfg(not(unix))]
        {
            assert!(owned_temp_root_under(&base).is_ok());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn owned_root_rejects_symlink() {
        let base = workdir();
        let target = base.join("real");
        std::fs::create_dir_all(&target).expect("target");
        std::os::unix::fs::symlink(&target, base.join(OWNED_TEMP_DIR_NAME)).expect("symlink");
        let err = owned_temp_root_under(&base).expect_err("symlink must fail");
        assert!(matches!(err, EditorError::WriteFailed(_)));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn assert_no_symlink(path: &Path) {
        assert!(
            !std::fs::symlink_metadata(path)
                .expect("symlink_metadata")
                .file_type()
                .is_symlink()
        );
    }

    // -- G-1: crash-restart sweep ----------------------------------------------

    fn stale_name(pid: u32) -> String {
        format!("{TEMP_PREFIX}{pid}-1-2")
    }

    #[test]
    fn sweep_parses_pids_and_keeps_live_and_foreign() {
        assert_eq!(temp_file_pid(&stale_name(123)), Some(123));
        assert_eq!(temp_file_pid("unrelated.txt"), None);
        assert_eq!(temp_file_pid(TEMP_PREFIX), None);
        // Live owner is never dead, on any platform.
        assert!(!temp_pid_is_dead(std::process::id()));
        // Pid 0 never owns a file.
        assert!(temp_pid_is_dead(0));
    }

    #[test]
    fn sweep_removes_only_dead_owners() {
        let base = workdir();
        let live = base.join(stale_name(std::process::id()));
        let foreign = base.join("notes.txt");
        let dead = base.join(stale_name(u32::MAX));
        std::fs::write(&live, "live").expect("live");
        std::fs::write(&foreign, "foreign").expect("foreign");
        std::fs::write(&dead, "dead").expect("dead");
        let removed = sweep_crashed_temps_under(&base);
        assert!(live.exists(), "live owner kept");
        assert!(foreign.exists(), "foreign name kept");
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            assert_eq!(removed, 1);
            assert!(!dead.exists(), "dead owner swept on Linux");
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            // No safe-std dead-pid probe there: keep, never delete unprovable.
            assert_eq!(removed, 0);
            assert!(dead.exists(), "unprovable death keeps the file");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // -- G-2: minimized environment ----------------------------------------------

    #[test]
    fn hosted_env_keeps_functional_keys_and_drops_credentials() {
        for keep in [
            "PATH", "HOME", "TERM", "LANG", "LC_ALL", "TZ", "TMPDIR", "SHELL",
        ] {
            assert!(hosted_env_keeps(keep), "{keep} must survive minimization");
        }
        for drop in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "SUPER_SECRET",
            "MY_API_KEY",
            "DB_PASSWORD",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "CLOUDFLARE_API_TOKEN",
        ] {
            assert!(!hosted_env_keeps(drop), "{drop} must not reach the child");
        }
    }

    #[test]
    fn built_hosted_env_has_no_credential_shaped_keys() {
        // Read-only iteration over the ambient environment (no mutation, so
        // parallel-safe): whatever this process inherited, the minimized
        // form must carry no credential/token/secret/password key.
        for (key, _) in build_hosted_env() {
            let text = key.to_string_lossy().to_ascii_uppercase();
            assert!(
                !text.contains("TOKEN")
                    && !text.contains("SECRET")
                    && !text.contains("PASSWORD")
                    && !text.contains("CREDENTIALS"),
                "credential-shaped key leaked into hosted env: {text}"
            );
        }
    }

    #[test]
    fn minimized_removals_are_the_complement_of_the_keep_list() {
        for removed in minimized_env_removals() {
            assert!(
                !hosted_env_keeps(&removed.to_string_lossy()),
                "removal must not name a kept key"
            );
        }
    }

    // -- terminal.submit (T-1, T-2) ------------------------------------------------

    fn test_budget() -> SubmitBudget {
        SubmitBudget::new("composer.test", 1024 * 1024)
    }

    #[test]
    fn submit_frame_is_byte_exact_and_charges_budget() {
        let mut budget = test_budget();
        let frame = check_terminal_submit("cargo test", true, &mut budget).expect("submit");
        assert_eq!(frame, b"\x1b[200~cargo test\x1b[201~\r".to_vec());
        assert_eq!(budget.used(), frame.len() as u64);
    }

    #[test]
    fn submit_over_cap_fails_closed_without_charge() {
        let mut budget = test_budget();
        let big = "q".repeat(COMPOSER_MAX_BYTES + 1);
        let err = check_terminal_submit(&big, true, &mut budget).expect_err("over cap");
        assert!(matches!(err, SubmitDeny::TooLarge { wanted } if wanted == big.len()));
        assert_eq!(budget.used(), 0, "denial must not charge");
    }

    #[test]
    fn submit_lease_denied_emits_nothing_without_charge() {
        let mut budget = test_budget();
        let err = check_terminal_submit("echo hi", false, &mut budget).expect_err("lease");
        assert_eq!(err, SubmitDeny::LeaseDenied);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn submit_budget_exhausted_emits_nothing() {
        let mut budget = SubmitBudget::new("composer.test", 4);
        let err = check_terminal_submit("echo hi", true, &mut budget).expect_err("budget");
        assert!(matches!(err, SubmitDeny::BudgetExceeded { .. }));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn submit_budget_id_is_bounded() {
        let budget = SubmitBudget::new(&"p".repeat(1000), 8);
        assert_eq!(budget.plugin().len(), SUBMIT_PLUGIN_ID_MAX);
    }

    // -- process.editor denial before side effects (T-3) ----------------------------

    #[test]
    fn hostile_editor_denied_before_any_temp_file() {
        let dir = workdir();
        let before = std::fs::read_dir(&dir).expect("readdir").count();
        let mut buf = CommandBuffer::with_content("untouched").expect("seed");
        let outcome = process_editor_start_in(
            &mut buf,
            Some("sh"),
            Some("vim"),
            Duration::from_secs(5),
            &dir,
        );
        assert_eq!(outcome, EditorOutcome::Denied(EditorDeny::NotAllowed));
        assert_eq!(buf.as_str(), "untouched");
        let after = std::fs::read_dir(&dir).expect("readdir").count();
        assert_eq!(before, after, "denial must not create temp files");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_editor_denied_before_any_temp_file() {
        let dir = workdir();
        let mut buf = CommandBuffer::with_content("untouched").expect("seed");
        let outcome = process_editor_start_in(&mut buf, None, None, Duration::from_secs(5), &dir);
        assert_eq!(outcome, EditorOutcome::Denied(EditorDeny::NoEditor));
        assert_eq!(buf.as_str(), "untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- blocking round trip + non-zero discard (T-5) ---------------------------------

    #[cfg(unix)]
    fn fake_editor_script(dir: &Path, body: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = dir.join(format!(
            "fake-host-editor-{}-{}.sh",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut file = std::fs::File::create(&path).expect("create fake editor");
        std::io::Write::write_all(&mut file, body.as_bytes()).expect("write fake editor");
        file.sync_all().expect("fsync fake editor");
        drop(file);
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        path
    }

    #[cfg(unix)]
    fn hosted_round_trip(
        buf: &mut CommandBuffer,
        editor: &str,
        timeout: Duration,
        dir: &Path,
    ) -> Result<String, EditorError> {
        // Retry transient ETXTBSY spawns of just-written fake editors (same
        // environment race the composer tests already handle).
        let mut last: Option<EditorError> = None;
        for _ in 0..20 {
            match editor_round_trip(buf, editor, timeout, dir, run_editor_hosted) {
                Ok(out) => return Ok(out),
                Err(EditorError::SpawnFailed(msg)) if msg.contains("Text file busy") => {
                    last = Some(EditorError::SpawnFailed(msg));
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(other) => return Err(other),
            }
        }
        Err(last.expect("retry loop always sets last on ETXTBSY"))
    }

    #[test]
    #[cfg(unix)]
    fn hosted_round_trip_applies_edits_with_minimized_env() {
        require_pty!();
        let dir = workdir();
        let envfile = dir.join("child-env-keys");
        // The fake editor records every environment key it received. Closed
        // stdin is proven by `/dev/stdin` resolving to the same file as
        // `/dev/null` (POSIX `-ef`, no blocking read).
        let script = fake_editor_script(
            &dir,
            &format!(
                "#!/bin/sh\n\
                 if [ ! /dev/stdin -ef /dev/null ]; then exit 43; fi\n\
                 env | sed 's/=.*//' | sort -u > \"{envf}\"\n\
                 printf 'hosted-edited' > \"$1\"\n",
                envf = envfile.to_string_lossy()
            ),
        );
        let mut buf = CommandBuffer::with_content("original").expect("seed");
        let out = hosted_round_trip(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_secs(10),
            &dir,
        )
        .expect("hosted round trip");
        assert_eq!(out, "hosted-edited");
        assert_eq!(buf.as_str(), "hosted-edited");
        // Every key the child observed is either allowlisted or shell-set by
        // the script interpreter itself (`_`, `SHLVL`, `PWD` are assigned by
        // `/bin/sh` at startup, never inherited through the cleared env).
        // Continuation lines of multiline values are not keys and are
        // skipped by the identifier-shape gate.
        let recorded = std::fs::read_to_string(&envfile).expect("child env recorded");
        assert!(!recorded.is_empty(), "child must record its environment");
        let mut saw_path = false;
        for line in recorded.lines() {
            let key = line.trim();
            if key.is_empty() || !is_env_name(key) {
                continue;
            }
            if key == "PATH" {
                saw_path = true;
            }
            assert!(
                hosted_env_keeps(key) || ["_", "SHLVL", "PWD", "OLDPWD"].contains(&key),
                "unallowlisted key reached the hosted child: {key}"
            );
            let upper = key.to_ascii_uppercase();
            assert!(
                !upper.contains("TOKEN")
                    && !upper.contains("SECRET")
                    && !upper.contains("PASSWORD"),
                "credential-shaped key reached the hosted child: {key}"
            );
        }
        assert!(saw_path, "kept PATH must reach the hosted child");
        assert_no_temp_leftovers(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn is_env_name(s: &str) -> bool {
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    #[test]
    #[cfg(unix)]
    fn hosted_nonzero_exit_discards_and_cleans_temp() {
        require_pty!();
        let dir = workdir();
        let script = fake_editor_script(&dir, "#!/bin/sh\nexit 3\n");
        let mut buf = CommandBuffer::with_content("precious").expect("seed");
        let err = hosted_round_trip(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_secs(10),
            &dir,
        )
        .expect_err("must fail");
        assert!(matches!(err, EditorError::NonZeroExit(Some(3))));
        assert_eq!(buf.as_str(), "precious");
        assert_eq!(
            EditorOutcome::from(err),
            EditorOutcome::NonZeroExit(Some(3))
        );
        assert_no_temp_leftovers(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn assert_no_temp_leftovers(dir: &Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp file cleaned up");
    }

    // -- timeout kills the recorded tree (T-6) -----------------------------------------

    #[test]
    #[cfg(unix)]
    fn hosted_timeout_kills_recorded_tree() {
        require_pty!();
        let dir = workdir();
        let pidfile = dir.join("grandchild.pid");
        // Parent script backgrounds a grandchild sleeper (recording its pid
        // via `$!`, the background-job pid — `$$` would record the parent),
        // then sleeps foreground: a direct-child-only kill would orphan the
        // grandchild, while the owned-tree kill takes both.
        let script = fake_editor_script(
            &dir,
            &format!(
                "#!/bin/sh\n\
                 ( exec sleep 60 ) & \n\
                 printf '%s' \"$!\" > \"{pid}\" \n\
                 sleep 60\n",
                pid = pidfile.to_string_lossy()
            ),
        );
        let mut buf = CommandBuffer::with_content("waiting").expect("seed");
        let err = hosted_round_trip(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_millis(300),
            &dir,
        )
        .expect_err("must time out");
        assert_eq!(err, EditorError::Timeout);
        assert_eq!(buf.as_str(), "waiting");
        assert_eq!(EditorOutcome::from(err), EditorOutcome::Timeout);
        assert_no_temp_leftovers(&dir);
        // The grandchild must be gone: only an owned-tree kill reaches it.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let pid_text = std::fs::read_to_string(&pidfile).expect("grandchild pid recorded");
            let grandchild: u32 = pid_text.trim().parse().expect("pid parses");
            assert!(
                grandchild_gone(grandchild),
                "grandchild {grandchild} survived the timeout kill"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether a pid is fully gone on Linux: absent from `/proc`, or a zombie
    /// awaiting reaping (a SIGKILLed child reparents to init, which reaps
    /// promptly; poll briefly before concluding survival).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn grandchild_gone(pid: u32) -> bool {
        for _ in 0..100 {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) => {
                    // State follows the last ')' (comm may contain spaces).
                    let state = stat.rsplit(')').next().unwrap_or("").trim_start();
                    if !state.starts_with('Z') {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    return true;
                }
            }
        }
        false
    }
}
