//! Panel-hosted external-editor session (CTX-0731, issue #982).
//!
//! [`ExternalEditorHost`] owns at most one pending `$EDITOR` round trip:
//! opening resolves the allowlisted editor, snapshots the live composer
//! draft into a `0600` temp file, hosts the editor as a plain PTY grid leaf
//! (a `new_split`-shaped leaf plus
//! [`Runtime::spawn_shell_for_view`](bitty_runtime::Runtime::spawn_shell_for_view)),
//! and polling finishes the round trip once the child exits (bounded
//! read-back, draft apply, leaf teardown, focus restore).
//!
//! A hosted editor is an ordinary terminal leaf, not a non-terminal panel:
//! it needs no compositor sub-surface or Scene path, so the OQ-051 placed
//! contract (gating #985/#990) does not block this flow.
//!
//! Phase B host-operation policy (CTX-0929, W-103 S-1b; accepted contract
//! `bitty-terminal-docs/specifications/composer-architecture.md`, W-82):
//!
//! - the environment-driven program always resolves through
//!   [`resolve_editor`](bitty_rich::composer::resolve_editor): only the bare
//!   `nvim`/`vim`/`vi` names run, a hostile `$VISUAL`/`$EDITOR` is denied
//!   before any side effect, and denied values are never echoed;
//! - `program_override` bypasses that allowlist so tests can host
//!   short-lived fake editors (`/bin/true`, marker scripts). Production always
//!   passes `None` (the single `EditorRequested` arm), so the bypass never
//!   reaches real sessions;
//! - the draft travels through the Bitty-owned `0700` root
//!   ([`owned_temp_root`](bitty_rich::host::owned_temp_root)) via
//!   [`write_composer_temp`](bitty_rich::composer::write_composer_temp)
//!   (`0600`, bounded) and
//!   [`read_composer_back`](bitty_rich::composer::read_composer_back)
//!   (bounded, UTF-8); the [`TempComposerFile`](bitty_rich::composer::TempComposerFile)
//!   guard deletes it on every path, including panic past the frame;
//! - the editor leaf spawns with the minimized environment
//!   ([`minimized_env_removals`](bitty_rich::host::minimized_env_removals)):
//!   ambient credentials never reach the child. Leaf stdio is the PTY
//!   itself (the execution boundary's explicit interactive exception);
//! - the hosted wait is bounded (120 s default, 300 s ceiling, carried
//!   values). Expiry kills the recorded owned process tree
//!   ([`Runtime::kill_pane_tree`](bitty_runtime::Runtime::kill_pane_tree)),
//!   not just the direct child, and reports a typed timeout;
//! - every open/poll step reports a typed outcome
//!   ([`EditorOpenOutcome`]/[`EditorOutcome`](bitty_rich::host::EditorOutcome))
//!   at the API boundary; the loud-warning wrappers preserve the existing
//!   messages for current callers;
//! - the editor child runs with the temp path as its only argv element
//!   (direct argv, no shell); a non-zero exit discards the edits and keeps
//!   the old draft, matching the blocking path;
//! - the event loop never blocks: exit is polled once per pump tick via
//!   [`Runtime::pane_try_wait`](bitty_runtime::Runtime::pane_try_wait).
//!
//! While hosted, the composer overlay stays closed (draft preserved) so its
//! modal input routing cannot steal keystrokes from the editor leaf; every
//! finish path reopens it.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_rich::composer::{
    EDITOR_TIMEOUT_DEFAULT, EDITOR_TIMEOUT_MAX, EditorError, TempComposerFile, read_composer_back,
    resolve_editor, write_composer_temp,
};
use bitty_rich::host::{EditorDeny, EditorOutcome, minimized_env_removals, owned_temp_root};
use bitty_runtime::ViewId;

use crate::chrome_keys::{close_focused_leaf, split_dir_to_axis, split_focused_leaf};
use crate::terminal_app::TerminalApp;

/// One in-flight panel-hosted editor: the leaf running it, the draft temp
/// file (RAII-deleted), and the view to refocus when it finishes.
#[derive(Debug)]
pub(crate) struct ExternalEditorSession {
    /// Leaf running the editor child.
    pub(crate) view: ViewId,
    /// Draft snapshot the editor edits; deleted on drop in all cases.
    pub(crate) temp: TempComposerFile,
    /// Focused view before the editor opened; restored when it still exists.
    pub(crate) return_focus: ViewId,
}

/// Typed `process.editor` open outcome (W-82 G-4 at the editor boundary).
///
/// Denials name the rule, never the payload or path; unavailability names
/// the missing precondition. The loud-warning open wrappers map each
/// variant back to the existing messages, so current callers observe no
/// behavior change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditorOpenOutcome {
    /// The editor leaf owns the session.
    Hosted {
        /// Leaf running the editor child.
        view: ViewId,
    },
    /// A session is already hosted; the existing one is kept.
    Busy,
    /// Launch denied before any side effect.
    Denied(EditorDeny),
    /// No focused pane to anchor the leaf or restore to.
    NoFocusedPane,
    /// The owned temp root or draft snapshot failed.
    TempUnavailable,
    /// The editor leaf could not be prepared.
    LeafUnavailable,
    /// The editor child could not be spawned.
    SpawnFailed,
}

/// Why a typed open failed.
///
/// The `warning` carries the exact loud message current callers observe
/// (byte-for-byte preserved from the pre-typed implementation); the typed
/// API maps `outcome` and never surfaces temp paths or denied program
/// values beyond what the existing messages already contain.
#[derive(Debug)]
struct OpenError {
    // Read by the typed cutover seam (`open_external_editor_typed`, test-
    // gated until cutover rewires the modal to this outcome). Allowed while
    // dormant so the loud-warning path stays the only live reader.
    #[cfg_attr(not(test), allow(dead_code))]
    outcome: EditorOpenOutcome,
    warning: String,
}

impl OpenError {
    fn warn(outcome: EditorOpenOutcome, warning: String) -> Self {
        let mut warning = warning;
        if warning.len() > 512 {
            warning.truncate(512);
        }
        Self { outcome, warning }
    }
}

/// At most one pending editor session (a second `Alt+E` warns and keeps the
/// existing one, so hosted editors stay bounded at one leaf plus one file).
#[derive(Debug)]
pub(crate) struct ExternalEditorHost {
    pending: Option<ExternalEditorSession>,
    /// Bounded-wait deadline for the pending session (W-103 G-3): set at
    /// `begin`, cleared at `take`. `None` exactly when nothing is hosted.
    deadline: Option<(Instant, Duration)>,
}

impl ExternalEditorHost {
    /// Empty host (no pending editor).
    pub(crate) fn new() -> Self {
        Self {
            pending: None,
            deadline: None,
        }
    }

    /// Whether an editor leaf is currently hosted.
    pub(crate) fn is_hosting(&self) -> bool {
        self.pending.is_some()
    }

    /// View of the hosted editor leaf, if any.
    pub(crate) fn pending_view(&self) -> Option<ViewId> {
        self.pending.as_ref().map(|session| session.view)
    }

    /// Records a freshly spawned editor leaf with the default bounded wait.
    /// Returns `false` (session untouched) when one is already hosted.
    ///
    /// Test seam: production records through [`Self::begin_with_timeout`].
    #[cfg(test)]
    pub(crate) fn begin(&mut self, session: ExternalEditorSession) -> bool {
        self.begin_with_timeout(session, EDITOR_TIMEOUT_DEFAULT)
    }

    /// Records a freshly spawned editor leaf with an explicit bounded wait
    /// (clamped to [`EDITOR_TIMEOUT_MAX`]). Returns `false` (session
    /// untouched) when one is already hosted.
    pub(crate) fn begin_with_timeout(
        &mut self,
        session: ExternalEditorSession,
        timeout: Duration,
    ) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(session);
        self.deadline = Some((Instant::now(), timeout.min(EDITOR_TIMEOUT_MAX)));
        true
    }

    /// Whether the pending session outlived its bounded wait.
    fn deadline_expired(&self) -> bool {
        match self.deadline {
            Some((started, timeout)) => started.elapsed() >= timeout,
            None => false,
        }
    }

    /// Takes the pending session for finishing or cancelling.
    pub(crate) fn take(&mut self) -> Option<ExternalEditorSession> {
        self.deadline = None;
        self.pending.take()
    }
}

impl Default for ExternalEditorHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalApp {
    /// Opens the `$VISUAL`/`$EDITOR` program on the live composer draft in
    /// a new PTY leaf (the `EditorRequested` arm).
    ///
    /// Every failure warns loudly and leaves the layout, focus, and draft
    /// untouched; the composer overlay stays open on failure and closes
    /// (draft preserved) only once the editor leaf owns the session.
    pub(crate) fn open_external_editor(&mut self) {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        self.open_external_editor_with(visual.as_deref(), editor.as_deref(), None);
    }

    /// [`Self::open_external_editor`] with injectable inputs.
    ///
    /// `program_override` names the editor program directly, bypassing the
    /// allowlist; it exists so tests can host short-lived fake editors
    /// (`/bin/true`, marker scripts). Production always passes `None`.
    pub(crate) fn open_external_editor_with(
        &mut self,
        visual: Option<&str>,
        editor: Option<&str>,
        program_override: Option<&str>,
    ) {
        self.open_external_editor_with_timeout(
            visual,
            editor,
            program_override,
            EDITOR_TIMEOUT_DEFAULT,
        );
    }

    /// [`Self::open_external_editor_with`] with an explicit bounded wait
    /// (clamped to [`EDITOR_TIMEOUT_MAX`]).
    pub(crate) fn open_external_editor_with_timeout(
        &mut self,
        visual: Option<&str>,
        editor: Option<&str>,
        program_override: Option<&str>,
        timeout: Duration,
    ) {
        self.open_external_editor_full(visual, editor, program_override, timeout);
    }

    /// Typed `process.editor` open: the same policy as
    /// [`Self::open_external_editor_with`], reporting [`EditorOpenOutcome`]
    /// instead of warning loudly. No modal is rewired; the overlay still
    /// closes (draft preserved) only once the editor leaf owns the session.
    ///
    /// Test and cutover seam: the production open path stays on the
    /// loud-warning wrapper until cutover rewires the modal to this outcome.
    #[cfg(test)]
    pub(crate) fn open_external_editor_typed(
        &mut self,
        visual: Option<&str>,
        editor: Option<&str>,
        program_override: Option<&str>,
        timeout: Duration,
    ) -> EditorOpenOutcome {
        match self.open_inner(visual, editor, program_override, timeout) {
            Ok(opened) => opened.outcome,
            Err(err) => err.outcome,
        }
    }

    /// Loud-warning open wrapper: preserves the existing messages for
    /// current callers (byte-for-byte).
    fn open_external_editor_full(
        &mut self,
        visual: Option<&str>,
        editor: Option<&str>,
        program_override: Option<&str>,
        timeout: Duration,
    ) {
        match self.open_inner(visual, editor, program_override, timeout) {
            Ok(opened) => {
                let EditorOpenOutcome::Hosted { view } = opened.outcome else {
                    unreachable!("open_inner reports Hosted on success");
                };
                eprintln!(
                    "bitty: external editor '{}' opened in pane {:?} — exit the editor to apply its buffer to the composer draft",
                    opened.program, view
                );
            }
            Err(err) => eprintln!("{}", err.warning),
        }
    }

    /// Shared open implementation: allowlist resolve, owned-root temp
    /// snapshot, PTY-leaf spawn with minimized env, single-session record.
    fn open_inner(
        &mut self,
        visual: Option<&str>,
        editor: Option<&str>,
        program_override: Option<&str>,
        timeout: Duration,
    ) -> Result<OpenedEditor, OpenError> {
        if self.chrome.editor.is_hosting() {
            return Err(OpenError::warn(
                EditorOpenOutcome::Busy,
                String::from(
                    "warning: composer external editor already hosted — finish it (exit the editor) before opening another",
                ),
            ));
        }
        let program = match program_override {
            Some(candidate) => {
                if candidate.trim().is_empty() {
                    return Err(OpenError::warn(
                        EditorOpenOutcome::Denied(EditorDeny::NoEditor),
                        String::from(
                            "warning: composer external editor refused (empty program) — draft kept, session stays open",
                        ),
                    ));
                }
                candidate.trim().to_string()
            }
            None => match resolve_editor(visual, editor) {
                Ok(program) => program,
                Err(resolve) => {
                    let outcome = match resolve {
                        EditorError::NotAllowed => {
                            EditorOpenOutcome::Denied(EditorDeny::NotAllowed)
                        }
                        _ => EditorOpenOutcome::Denied(EditorDeny::NoEditor),
                    };
                    return Err(OpenError::warn(
                        outcome,
                        format!(
                            "warning: composer external editor refused ({resolve}) — draft kept, session stays open"
                        ),
                    ));
                }
            },
        };
        let draft = self.runtime.cw_composer_content().to_owned();
        // G-1: the draft snapshot lives in the Bitty-owned 0700 root, never
        // the ambient temp dir.
        let root = owned_temp_root().map_err(|err| {
            OpenError::warn(
                EditorOpenOutcome::TempUnavailable,
                format!(
                    "warning: composer external editor temp failed ({err}) — draft kept, session stays open"
                ),
            )
        })?;
        let temp = write_composer_temp(&draft, &root).map_err(|err| {
            OpenError::warn(
                EditorOpenOutcome::TempUnavailable,
                format!(
                    "warning: composer external editor temp failed ({err}) — draft kept, session stays open"
                ),
            )
        })?;
        let focused = match self.runtime.focused_view() {
            Some(view) => view,
            None => {
                return Err(OpenError::warn(
                    EditorOpenOutcome::NoFocusedPane,
                    String::from(
                        "warning: composer external editor has no focused pane — draft kept, session stays open",
                    ),
                ));
            }
        };
        self.restore_zoom();
        // CTX-0378: the id comes from the runtime-wide allocator, never the
        // live layout's max + 1 (mirrors the `new_split` arm).
        let new_id = self.runtime.next_view_id_global();
        // CTX-0343 first match (mirrors the `new_split` arm): fail the
        // creation closed before the layout commits it.
        if let Err(err) = self.runtime.validate_new_view_appearance(new_id) {
            return Err(OpenError::warn(
                EditorOpenOutcome::LeafUnavailable,
                format!("warning: composer external editor refused: {err} — draft kept"),
            ));
        }
        // Editor below the focused pane (`Down` geometry, new leaf second).
        let mut layout = self.runtime.layout().clone();
        if !split_focused_leaf(
            &mut layout,
            focused,
            split_dir_to_axis(bitty_config::SplitDir::Down),
            new_id,
            false,
        ) {
            return Err(OpenError::warn(
                EditorOpenOutcome::LeafUnavailable,
                String::from(
                    "warning: composer external editor found no focused pane — draft kept, session stays open",
                ),
            ));
        }
        self.runtime.set_layout(layout);
        let (cols, rows) = self
            .runtime
            .layout_allocations()
            .iter()
            .find(|(id, _)| *id == new_id)
            .map(|(_, rect)| (rect.width.max(1), rect.height.max(1)))
            .unwrap_or((80, 24));
        let temp_arg = temp.path().to_string_lossy().into_owned();
        // G-2: the leaf spawns without the ambient credentials (minimized
        // env); stdio is the PTY itself (explicit interactive exception).
        if let Err(err) = self.runtime.spawn_shell_for_view_scrubbed(
            new_id,
            &program,
            &[temp_arg.as_str()],
            cols,
            rows,
            &minimized_env_removals(),
        ) {
            // Roll the leaf back (unlike `new_split`, which keeps an empty
            // pane on spawn failure): the open failed, so nothing changes.
            let mut layout = self.runtime.layout().clone();
            if close_focused_leaf(&mut layout, new_id) {
                self.runtime.set_layout_closing(layout, new_id);
            }
            return Err(OpenError::warn(
                EditorOpenOutcome::SpawnFailed,
                format!(
                    "warning: composer external editor spawn failed ({err}) — draft kept, session stays open"
                ),
            ));
        }
        // CTX-0364: focus follows the fresh pane (mirrors `new_split`).
        self.runtime.set_focus(new_id);
        if !self.chrome.editor.begin_with_timeout(
            ExternalEditorSession {
                view: new_id,
                temp,
                return_focus: focused,
            },
            timeout,
        ) {
            // Unreachable single-threaded (busy was checked above); tear the
            // leaf down rather than leak an untracked editor.
            let mut layout = self.runtime.layout().clone();
            if close_focused_leaf(&mut layout, new_id) {
                self.runtime.set_layout_closing(layout, new_id);
            }
            self.runtime.close_pane_session(&new_id);
            return Err(OpenError::warn(
                EditorOpenOutcome::Busy,
                String::from(
                    "warning: composer external editor already hosted — draft kept, session stays open",
                ),
            ));
        }
        // Only now: close the overlay (draft preserved) so its modal routing
        // cannot steal keystrokes from the editor leaf. Every finish path in
        // `poll_external_editor` reopens it.
        self.runtime.cw_composer_close();
        Ok(OpenedEditor {
            outcome: EditorOpenOutcome::Hosted { view: new_id },
            program,
        })
    }

    /// Advances the hosted editor, if any. Called once per pump tick; never
    /// blocks (exit is a non-blocking
    /// [`pane_try_wait`](bitty_runtime::Runtime::pane_try_wait) poll).
    ///
    /// Still running (and within its bounded wait): no-op, returns `None`.
    /// Otherwise the round trip finishes with a typed [`EditorOutcome`]: on
    /// a zero exit the edited file is read back bounded and applied to the
    /// draft (fail-closed past the cap, old content kept); a non-zero exit,
    /// a signal death, a vanished leaf, a read-back failure, or an expired
    /// bounded wait (recorded tree killed) keeps the old draft. Every
    /// terminal path tears the editor leaf down, restores the prior focus
    /// when it still exists, and reopens the composer overlay.
    pub(crate) fn poll_external_editor(&mut self) -> Option<EditorOutcome> {
        let view = self.chrome.editor.pending_view()?;
        if !self.runtime.layout().leaf_ids().contains(&view) {
            // The editor leaf went away without us (e.g. a manual
            // `close_view`): drop the session — the RAII temp goes with it —
            // and reopen the overlay over the kept draft.
            drop(self.chrome.editor.take());
            self.runtime.cw_composer_open();
            eprintln!(
                "warning: composer external editor pane closed — draft kept, session reopened"
            );
            return Some(EditorOutcome::Cancelled);
        }
        let status = match self.runtime.pane_try_wait(&view) {
            Some(status) => status,
            None => {
                // Still running: enforce the bounded wait (W-103 G-3). Expiry
                // kills the recorded owned tree (not just the direct child)
                // before the normal teardown reaps the leader.
                if !self.chrome.editor.deadline_expired() {
                    return None;
                }
                let _ = self.runtime.kill_pane_tree(&view);
                let session = self.chrome.editor.take()?;
                eprintln!(
                    "warning: composer external editor timed out and its tree was killed — draft kept, edits discarded"
                );
                self.finish_external_editor(session);
                return Some(EditorOutcome::Timeout);
            }
        };
        let session = self.chrome.editor.take()?;
        if status.is_success() {
            match read_composer_back(session.temp.path()) {
                Ok(content) => {
                    let bytes = content.len();
                    match self.runtime.cw_composer_apply_external(&content) {
                        Ok(()) => {
                            eprintln!(
                                "bitty: external editor applied {bytes} bytes to the composer draft"
                            );
                            self.finish_external_editor(session);
                            Some(EditorOutcome::Edited(content))
                        }
                        Err(err) => {
                            eprintln!(
                                "warning: composer external editor result refused ({err}) — draft kept"
                            );
                            self.finish_external_editor(session);
                            Some(EditorOutcome::Unavailable("draft apply refused"))
                        }
                    }
                }
                Err(err) => {
                    eprintln!(
                        "warning: composer external editor read-back failed ({err}) — draft kept"
                    );
                    let outcome = EditorOutcome::from(err);
                    self.finish_external_editor(session);
                    Some(outcome)
                }
            }
        } else if let Some(signal) = status.signal() {
            eprintln!(
                "warning: composer external editor killed by signal {signal} — draft kept, edits discarded"
            );
            // Signal names are short platform words; bound defensively.
            let signal: String = signal.chars().take(32).collect();
            self.finish_external_editor(session);
            Some(EditorOutcome::Signal(signal))
        } else {
            eprintln!(
                "warning: composer external editor exited with code {} — draft kept, edits discarded",
                status.code()
            );
            let code = i32::try_from(status.code()).ok();
            self.finish_external_editor(session);
            Some(EditorOutcome::NonZeroExit(code))
        }
    }

    /// Tears the finished editor leaf down, restores focus, and reopens the
    /// composer overlay over the (kept or applied) draft.
    fn finish_external_editor(&mut self, session: ExternalEditorSession) {
        // No `view_close_request` confirm gate here: the child already
        // exited, so there is no running foreground job left to protect.
        self.restore_zoom();
        let mut layout = self.runtime.layout().clone();
        if close_focused_leaf(&mut layout, session.view) {
            // CTX-0359: an explicit close is the only layout change allowed
            // to re-home the primary owner.
            self.runtime.set_layout_closing(layout, session.view);
        }
        // CTX-0176: tear down the closed leaf's shell (drop kills + reaps
        // the child; no-op when it never owned one).
        if self.runtime.close_pane_session(&session.view) {
            eprintln!("bitty: external editor pane {:?} torn down", session.view);
        }
        if self
            .runtime
            .layout()
            .leaf_ids()
            .contains(&session.return_focus)
        {
            self.runtime.set_focus(session.return_focus);
        }
        self.runtime.cw_composer_open();
        // `session.temp` drops here: the temp file is unlinked in all cases.
        drop(session);
    }
}

/// A successfully opened editor leaf: its typed success outcome plus the
/// program name for the loud open notice.
struct OpenedEditor {
    outcome: EditorOpenOutcome,
    program: String,
}
