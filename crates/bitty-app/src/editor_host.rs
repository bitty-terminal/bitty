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
//! Security properties (mirroring the blocking
//! [`edit_externally`](bitty_rich::composer::edit_externally) path):
//!
//! - the environment-driven program always resolves through
//!   [`resolve_editor`](bitty_rich::composer::resolve_editor): only the bare
//!   `nvim`/`vim`/`vi` names run, a hostile `$VISUAL`/`$EDITOR` is denied
//!   before any side effect, and denied values are never echoed;
//! - `program_override` bypasses that allowlist so tests can host
//!   short-lived fake editors; production always passes `None` (the single
//!   `EditorRequested` arm), so the bypass never reaches real sessions;
//! - the draft travels through
//!   [`write_composer_temp`](bitty_rich::composer::write_composer_temp)
//!   (`0600`, bounded) and
//!   [`read_composer_back`](bitty_rich::composer::read_composer_back)
//!   (bounded, UTF-8); the [`TempComposerFile`](bitty_rich::composer::TempComposerFile)
//!   guard deletes it on every path, including panic past the frame;
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

use bitty_rich::composer::{
    TempComposerFile, read_composer_back, resolve_editor, write_composer_temp,
};
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

/// At most one pending editor session (a second `Alt+E` warns and keeps the
/// existing one, so hosted editors stay bounded at one leaf plus one file).
#[derive(Debug, Default)]
pub(crate) struct ExternalEditorHost {
    pending: Option<ExternalEditorSession>,
}

impl ExternalEditorHost {
    /// Empty host (no pending editor).
    pub(crate) fn new() -> Self {
        Self { pending: None }
    }

    /// Whether an editor leaf is currently hosted.
    pub(crate) fn is_hosting(&self) -> bool {
        self.pending.is_some()
    }

    /// View of the hosted editor leaf, if any.
    pub(crate) fn pending_view(&self) -> Option<ViewId> {
        self.pending.as_ref().map(|session| session.view)
    }

    /// Records a freshly spawned editor leaf. Returns `false` (session
    /// untouched) when one is already hosted.
    pub(crate) fn begin(&mut self, session: ExternalEditorSession) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(session);
        true
    }

    /// Takes the pending session for finishing or cancelling.
    pub(crate) fn take(&mut self) -> Option<ExternalEditorSession> {
        self.pending.take()
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
        if self.chrome.editor.is_hosting() {
            eprintln!(
                "warning: composer external editor already hosted — finish it (exit the editor) before opening another"
            );
            return;
        }
        let program = match program_override {
            Some(candidate) => {
                if candidate.trim().is_empty() {
                    eprintln!(
                        "warning: composer external editor refused (empty program) — draft kept, session stays open"
                    );
                    return;
                }
                candidate.trim().to_string()
            }
            None => match resolve_editor(visual, editor) {
                Ok(program) => program,
                Err(err) => {
                    eprintln!(
                        "warning: composer external editor refused ({err}) — draft kept, session stays open"
                    );
                    return;
                }
            },
        };
        let draft = self.runtime.cw_composer_content().to_owned();
        let temp = match write_composer_temp(&draft, &std::env::temp_dir()) {
            Ok(temp) => temp,
            Err(err) => {
                eprintln!(
                    "warning: composer external editor temp failed ({err}) — draft kept, session stays open"
                );
                return;
            }
        };
        let focused = match self.runtime.focused_view() {
            Some(view) => view,
            None => {
                eprintln!(
                    "warning: composer external editor has no focused pane — draft kept, session stays open"
                );
                return;
            }
        };
        self.restore_zoom();
        // CTX-0378: the id comes from the runtime-wide allocator, never the
        // live layout's max + 1 (mirrors the `new_split` arm).
        let new_id = self.runtime.next_view_id_global();
        // CTX-0343 first match (mirrors the `new_split` arm): fail the
        // creation closed before the layout commits it.
        if let Err(err) = self.runtime.validate_new_view_appearance(new_id) {
            eprintln!("warning: composer external editor refused: {err} — draft kept");
            return;
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
            eprintln!(
                "warning: composer external editor found no focused pane — draft kept, session stays open"
            );
            return;
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
        if let Err(err) =
            self.runtime
                .spawn_shell_for_view(new_id, &program, &[temp_arg.as_str()], cols, rows)
        {
            // Roll the leaf back (unlike `new_split`, which keeps an empty
            // pane on spawn failure): the open failed, so nothing changes.
            let mut layout = self.runtime.layout().clone();
            if close_focused_leaf(&mut layout, new_id) {
                self.runtime.set_layout_closing(layout, new_id);
            }
            eprintln!(
                "warning: composer external editor spawn failed ({err}) — draft kept, session stays open"
            );
            return;
        }
        // CTX-0364: focus follows the fresh pane (mirrors `new_split`).
        self.runtime.set_focus(new_id);
        if !self.chrome.editor.begin(ExternalEditorSession {
            view: new_id,
            temp,
            return_focus: focused,
        }) {
            // Unreachable single-threaded (busy was checked above); tear the
            // leaf down rather than leak an untracked editor.
            let mut layout = self.runtime.layout().clone();
            if close_focused_leaf(&mut layout, new_id) {
                self.runtime.set_layout_closing(layout, new_id);
            }
            self.runtime.close_pane_session(&new_id);
            eprintln!(
                "warning: composer external editor already hosted — draft kept, session stays open"
            );
            return;
        }
        // Only now: close the overlay (draft preserved) so its modal routing
        // cannot steal keystrokes from the editor leaf. Every finish path in
        // `poll_external_editor` reopens it.
        self.runtime.cw_composer_close();
        eprintln!(
            "bitty: external editor '{program}' opened in pane {new_id:?} — exit the editor to apply its buffer to the composer draft"
        );
    }

    /// Advances the hosted editor, if any. Called once per pump tick; never
    /// blocks (exit is a non-blocking
    /// [`pane_try_wait`](bitty_runtime::Runtime::pane_try_wait) poll).
    ///
    /// Still running (or nothing hosted): no-op. Otherwise the round trip
    /// finishes: on a zero exit the edited file is read back bounded and
    /// applied to the draft (fail-closed past the cap, old content kept); a
    /// non-zero exit, a vanished leaf, or any I/O failure keeps the old
    /// draft. Every terminal path tears the editor leaf down, restores the
    /// prior focus when it still exists, and reopens the composer overlay.
    pub(crate) fn poll_external_editor(&mut self) {
        let view = match self.chrome.editor.pending_view() {
            Some(view) => view,
            None => return,
        };
        if !self.runtime.layout().leaf_ids().contains(&view) {
            // The editor leaf went away without us (e.g. a manual
            // `close_view`): drop the session — the RAII temp goes with it —
            // and reopen the overlay over the kept draft.
            drop(self.chrome.editor.take());
            self.runtime.cw_composer_open();
            eprintln!(
                "warning: composer external editor pane closed — draft kept, session reopened"
            );
            return;
        }
        let status = match self.runtime.pane_try_wait(&view) {
            Some(status) => status,
            None => return,
        };
        let Some(session) = self.chrome.editor.take() else {
            return;
        };
        if status.is_success() {
            match read_composer_back(session.temp.path()) {
                Ok(content) => {
                    let bytes = content.len();
                    match self.runtime.cw_composer_apply_external(&content) {
                        Ok(()) => eprintln!(
                            "bitty: external editor applied {bytes} bytes to the composer draft"
                        ),
                        Err(err) => eprintln!(
                            "warning: composer external editor result refused ({err}) — draft kept"
                        ),
                    }
                }
                Err(err) => eprintln!(
                    "warning: composer external editor read-back failed ({err}) — draft kept"
                ),
            }
        } else if let Some(signal) = status.signal() {
            eprintln!(
                "warning: composer external editor killed by signal {signal} — draft kept, edits discarded"
            );
        } else {
            eprintln!(
                "warning: composer external editor exited with code {} — draft kept, edits discarded",
                status.code()
            );
        }
        self.finish_external_editor(session);
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
