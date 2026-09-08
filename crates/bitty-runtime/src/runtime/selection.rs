//! `Runtime` — Selection, clipboard, paste gating, and truncation helpers.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::input::key_inspect_label;
use super::*;

pub(super) fn clamp_cell_pos(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    let max_row = snapshot.height.saturating_sub(1) as u16;
    let max_col = snapshot.width.saturating_sub(1) as u16;
    CellPos::new(pos.row.min(max_row), pos.col.min(max_col))
}

fn truncate_paste_text(text: String) -> String {
    const MAX_BYTES: usize = bitty_platform::clipboard::CLIPBOARD_MAX_BYTES;
    if text.len() <= MAX_BYTES {
        return text;
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Truncate `s` to at most `max_bytes` at a char boundary (CTX-0186 summary).
fn truncate_str_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Runtime {
    /// Current selection, if any (read-only).
    #[must_use]
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Whether a drag is in progress.
    #[must_use]
    pub fn is_selection_dragging(&self) -> bool {
        self.selection_dragging
    }

    /// Whether a selection currently exists and is non-empty.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    /// Clears the current selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_dragging = false;
        self.pending_full_redraw = true;
    }

    /// Directly sets the selection (headless test seam).
    pub fn set_selection(&mut self, selection: Selection) {
        let snap = self.state.snapshot();
        let clamped = selection.clamped(&snap).snapped(Some(&snap));
        self.selection = Some(clamped);
        self.selection_dragging = clamped.active;
        self.pending_full_redraw = true;
    }

    /// Starts a new selection at `pos` (mouse down).
    pub fn start_selection(&mut self, pos: CellPos) {
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        self.selection = Some(Selection {
            anchor: snapped,
            focus: snapped,
            kind: SelectionKind::Simple,
            active: true,
        });
        self.selection_dragging = true;
        self.pending_full_redraw = true;
    }

    /// Updates the current selection's focus to `pos` (mouse drag).
    pub fn update_selection(&mut self, pos: CellPos) {
        let Some(mut sel) = self.selection else {
            return;
        };
        if !self.selection_dragging {
            return;
        }
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        sel.focus = snapped;
        sel.active = true;
        self.selection = Some(sel);
        self.pending_full_redraw = true;
    }

    /// Ends the selection at `pos` (mouse up) and leaves it active for copy.
    pub fn end_selection(&mut self, pos: CellPos) {
        let Some(mut sel) = self.selection else {
            return;
        };
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        sel.focus = snapped;
        sel.active = false;
        self.selection_dragging = false;
        // Keep zero-length selections as None to avoid empty copies.
        if sel.anchor == sel.focus {
            self.selection = None;
        } else {
            self.selection = Some(sel);
        }
        self.pending_full_redraw = true;
    }

    /// Returns selected text for the current selection, if any.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let snap = self.state.snapshot();
        let text = sel.text(&snap);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Copies the current selection to the system clipboard (via the
    /// Wayland-first platform backend with headless fallback, which
    /// best-effort syncs the primary selection on Linux). Returns the copied
    /// text on success, `None` when no selection exists.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the OS reports an error,
    /// returns `PlatformError::ClipboardOperation` but still updates the
    /// headless buffer so headless tests can observe the value.
    pub fn copy_selection_to_clipboard(
        &mut self,
    ) -> Result<Option<String>, bitty_platform::PlatformError> {
        let Some(text) = self.selection_text() else {
            return Ok(None);
        };
        self.clipboard.set_text(text.clone())?;
        Ok(Some(text))
    }

    /// Best-effort copy that never returns an error (drops system errors).
    pub fn copy_selection_lossy(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        self.clipboard.set_text_lossy(text.clone());
        Some(text)
    }

    /// Current contents of the platform primary (selection) clipboard buffer.
    ///
    /// Headless-first observation seam for tests: on a live Wayland/X11
    /// desktop this mirrors the last primary write through
    /// `bitty-platform::Clipboard`, so unit tests stay deterministic by
    /// forcing the headless clipboard first (`force_headless_clipboard`).
    #[must_use]
    pub fn primary_contents(&self) -> &str {
        self.clipboard.primary_contents()
    }

    /// Last clipboard failure observed on the mouse-paste path, if any.
    ///
    /// Mouse paste stays fail-soft (no bytes, no panic), but read/write
    /// failures from the platform clipboard are recorded here instead of
    /// swallowed, so the embedder can surface them (PR #259 review). A
    /// subsequent successful clipboard operation clears the slot. Cloned
    /// because [`Runtime`] is not `Sync`-friendly to borrow across frames.
    #[must_use]
    pub fn last_clipboard_error(&self) -> Option<bitty_platform::PlatformError> {
        self.last_clipboard_error.clone()
    }

    /// Records a platform clipboard failure for later surfacing.
    pub(super) fn record_clipboard_error(&mut self, err: bitty_platform::PlatformError) {
        self.last_clipboard_error = Some(err);
    }

    /// Clears the recorded clipboard failure after a successful operation.
    pub(super) fn clear_clipboard_error(&mut self) {
        self.last_clipboard_error = None;
    }

    /// Directly sets the platform primary clipboard (headless test seam).
    ///
    /// Routes through `bitty-platform::Clipboard::set_primary` (Wayland
    /// primary selection where supported, headless buffer otherwise).
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] but the headless buffer is still
    /// updated by the platform layer, so headless tests stay deterministic.
    pub fn set_primary_text(&mut self, text: String) {
        if let Err(err) = self.clipboard.set_primary(text) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
    }

    /// Copies the current selection to the platform primary clipboard.
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] while the void return keeps the
    /// historic call shape.
    pub fn copy_selection_to_primary(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        if let Err(err) = self.clipboard.set_primary(text.clone()) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
        Some(text)
    }

    /// Ghostty `setSelectionAndCopy` equivalent (CTX-0158): copies the
    /// current selection to the standard clipboard, which the platform layer
    /// best-effort syncs to the primary selection on Linux (CTX-0160).
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system clipboard error is recorded for
    /// [`Self::last_clipboard_error`] while the headless buffers always
    /// update, and headless tests never touch the real clipboard.
    ///
    /// Called automatically on left-release only when
    /// `RuntimeConfig::selection_auto_copy` is `true` (CTX-0191); the explicit
    /// `copy_to_clipboard` chord calls the same path regardless of the toggle.
    pub fn auto_copy_selection(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        match self.clipboard.set_text(text.clone()) {
            Ok(()) => self.clear_clipboard_error(),
            Err(err) => self.record_clipboard_error(err),
        }
        Some(text)
    }

    /// Pastes from the platform primary selection (middle-click /
    /// `wl-paste --primary`) through the same suspicious-paste inspection
    /// gate as clipboard input. Returns `None` when the primary selection is
    /// empty, otherwise `Some(true)` when the paste requires confirmation or
    /// `Some(false)` when delivered immediately.
    ///
    /// Fail-soft with a surfaced error: a platform read failure pastes
    /// nothing but is recorded for [`Self::last_clipboard_error`] instead of
    /// swallowed (PR #259 review); a successful read clears the slot.
    pub fn paste_from_primary(&mut self) -> Option<bool> {
        let text = match self.clipboard.get_primary() {
            Ok(text) => {
                self.clear_clipboard_error();
                text
            }
            Err(err) => {
                self.record_clipboard_error(err);
                return None;
            }
        };
        if text.is_empty() {
            return None;
        }
        Some(self.request_paste(text))
    }

    /// Whether the pending paste requires confirmation, if one exists.
    #[must_use]
    pub fn pending_paste_inspection(&self) -> Option<bool> {
        self.pending_paste
            .as_ref()
            .map(|p| p.inspection.needs_confirmation())
    }

    /// Whether a paste is awaiting confirmation.
    #[must_use]
    pub fn has_pending_paste(&self) -> bool {
        self.pending_paste.is_some()
    }

    /// Current pending paste text, if any.
    #[must_use]
    pub fn pending_paste_text(&self) -> Option<&str> {
        self.pending_paste.as_ref().map(|p| p.text.as_str())
    }

    /// Bounded human-readable summary of the pending paste, if any (CTX-0186,
    /// compacted CTX-0192).
    ///
    /// A gated paste is never silent: while [`Self::has_pending_paste`] holds,
    /// this returns `Some` single line of the form
    /// `Paste 2 lines, 11B [newline] "line1\nline2" (repeat=confirm Esc=cancel)`.
    ///
    /// Bounded and deterministic: the input is already capped at
    /// `CLIPBOARD_MAX_BYTES` (8192), reasons are at most 7 static tokens, and
    /// the preview keeps the first 32 chars escaped (`escape_debug`) and cut
    /// to 48 bytes at a char boundary. Total length stays well under 256
    /// bytes, single-line (no raw `\n`). `O(n)` with `n ≤ 8192`.
    #[must_use]
    pub fn pending_paste_summary(&self) -> Option<String> {
        let pending = self.pending_paste.as_ref()?;
        let lines = pending.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let bytes = pending.text.len();
        let reasons = pending.inspection.reasons().join(", ");
        let preview: String = pending.text.chars().take(32).collect();
        let preview = preview.escape_debug().to_string();
        let preview = truncate_str_to_bytes(&preview, 48);
        Some(format!(
            "Paste {lines} lines, {bytes}B [{reasons}] \"{preview}\" (repeat=confirm Esc=cancel)"
        ))
    }

    /// Whether the banner has collapsed to the minimal flash at `now`
    /// (CTX-0192). `None` when no paste pends.
    #[must_use]
    pub fn paste_banner_collapsed_at(&self, now: std::time::Instant) -> Option<bool> {
        self.pending_paste.as_ref()?;
        let since = self.pending_paste_since?;
        Some(now.saturating_duration_since(since) >= PASTE_BANNER_FULL_DURATION)
    }

    /// Visible banner text at `now` (CTX-0192): compact summary while fresh,
    /// [`PASTE_BANNER_FLASH_TEXT`] after [`PASTE_BANNER_FULL_DURATION`].
    /// Always `Some` while [`Self::has_pending_paste`] holds (never-silent),
    /// bounded, single-line, overlay-only.
    #[must_use]
    pub fn paste_banner_text_at(&self, now: std::time::Instant) -> Option<String> {
        if !self.has_pending_paste() {
            return None;
        }
        match self.paste_banner_collapsed_at(now) {
            Some(true) => Some(PASTE_BANNER_FLASH_TEXT.to_string()),
            _ => self.pending_paste_summary(),
        }
    }

    /// Visible banner text now (CTX-0192). See [`Self::paste_banner_text_at`].
    #[must_use]
    pub fn paste_banner_text(&self) -> Option<String> {
        self.paste_banner_text_at(std::time::Instant::now())
    }

    /// Pastes text from the system clipboard (or headless buffer) and routes
    /// it as terminal input via the bounded pending path. Returns
    /// `Err(PlatformError)` when clipboard acquisition fails, `Ok(None)` when
    /// the clipboard is empty, and `Ok(Some(true))` when confirmation is
    /// required or `Ok(Some(false))` when the text is delivered immediately.
    ///
    /// The right-click mouse path records `Err` for
    /// [`Self::last_clipboard_error`] instead of dropping it (PR #259
    /// review); direct callers match on the `Result` themselves.
    ///
    /// Suspicious-paste inspection (P0-AC-008): every paste is inspected for
    /// C0/NUL/ESC/CR/newline/Unicode BiDi controls. Clean text is delivered
    /// immediately; suspicious text is stored as a pending paste that requires
    /// explicit confirmation — `confirm_pending_paste(true)`, repeating the
    /// identical paste while pending (CTX-0186 second chord/right-click press
    /// with unchanged clipboard), or `Esc` to cancel. The pending paste stays
    /// visible via [`Self::pending_paste_summary`]: there is no silent
    /// delivery path and no silent drop. Bracketed paste (`?2004`) is
    /// defense-in-depth only and wraps confirmed delivery when enabled in
    /// terminal state.
    ///
    /// Paste is bounded to `CLIPBOARD_MAX_BYTES` (8192) via the clipboard
    /// primitive before the scan, so untrusted clipboard content cannot grow
    /// the heap without limit (T-01).
    pub fn paste_from_clipboard(&mut self) -> Result<Option<bool>, bitty_platform::PlatformError> {
        let text = self.clipboard.get_text()?;
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.request_paste(text)))
    }

    /// Pastes from a given string via the inspection gate (headless helper).
    /// Returns `true` when the submitted paste requires confirmation and
    /// `false` when it is delivered immediately. Re-submitting the identical
    /// pending text confirms and delivers (CTX-0186); different suspicious
    /// content while pending preserves the first paste and returns `true`.
    pub fn paste_text_via_gate(&mut self, text: String) -> bool {
        self.request_paste(text)
    }

    /// Pastes the given text through the suspicious-paste inspection gate.
    ///
    /// This string-input seam is safe for production callers because it uses
    /// the same pending confirmation path as clipboard input.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.request_paste(text.to_owned());
    }

    /// Core paste entry: bounds and inspects `text`, stores a pending paste
    /// when suspicious, otherwise delivers immediately. Returns `true` when
    /// confirmation is required and `false` when delivery is immediate. A
    /// different suspicious request while another paste is pending is rejected,
    /// which preserves the first pending paste for explicit confirmation or
    /// cancel.
    ///
    /// CTX-0186 explicit repeat-to-confirm: re-submitting the identical
    /// (post-truncation) text while it is pending is the user's confirmation
    /// gesture — the second chord/right-click press with an unchanged
    /// clipboard delivers (bracketed when `?2004` is on) and clears pending.
    /// Different content while pending preserves the first paste (TOCTOU-safe:
    /// a swapped clipboard cannot smuggle new bytes through confirmation).
    ///
    /// No silent delivery path exists for `needs_confirmation() == true`.
    pub fn request_paste(&mut self, text: String) -> bool {
        // CTX-0243: paste is typing — snap to live so the pending banner and
        // the eventual echo land on the visible window (delivery via
        // `write_input` also snaps; explicit here for the pending-confirm path
        // with no bytes yet).
        self.snap_focused_to_live();
        let text = truncate_paste_text(text);
        // Explicit confirmation: identical re-paste while pending delivers.
        if let Some(pending) = self.pending_paste.as_ref() {
            if pending.text == text {
                let pending = self.pending_paste.take().expect("checked above");
                self.deliver_paste_bytes_bracketed(&pending.text);
                self.pending_paste_since = None;
                self.paste_banner_collapsed = false;
                self.pending_full_redraw = true;
                return false;
            }
        }
        let inspection = crate::paste::inspect_paste(&text);
        if inspection.needs_confirmation() {
            if self.pending_paste.is_some() {
                return true;
            }
            self.pending_paste = Some(crate::paste::PendingPaste::new(text, inspection.clone()));
            // CTX-0192 transient banner starts full now.
            self.pending_paste_since = Some(std::time::Instant::now());
            self.paste_banner_collapsed = false;
            self.pending_full_redraw = true;
            return true;
        }
        self.deliver_paste_bytes(text.as_bytes());
        false
    }

    /// Confirm or cancel the pending paste. `confirm == true` delivers the
    /// pending text (bracketed when `?2004` is enabled); `false` drops it.
    ///
    /// Returns `true` when a pending paste existed and was handled.
    pub fn confirm_pending_paste(&mut self, confirm: bool) -> bool {
        // CTX-0243: confirming/cancelling is user intent — snap to live
        // (confirm delivers via `write_input` which also snaps; cancel has
        // no bytes so needs the explicit snap).
        self.snap_focused_to_live();
        let Some(pending) = self.pending_paste.take() else {
            return false;
        };
        if confirm {
            self.deliver_paste_bytes_bracketed(&pending.text);
        }
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        true
    }

    /// Cancel any pending paste without delivery.
    pub fn cancel_pending_paste(&mut self) -> bool {
        self.confirm_pending_paste(false)
    }

    /// Consume an `Esc` press while a paste is pending (CTX-0186).
    ///
    /// Returns `true` when the event was an `Esc` press with a pending paste:
    /// the pending paste is dropped without delivery, a redraw is requested so
    /// any pending indicator clears, and the caller must not forward the key
    /// to the PTY. Returns `false` otherwise (no pending paste, not `Esc`, or
    /// not a press), leaving existing key routing untouched.
    ///
    /// CTX-0257: the same press also cancels a pending workspace-close arm
    /// (kill-confirm gate). Either cancellation consumes the `Esc`; both are
    /// dropped together when both pend (loud, no partial state).
    pub(super) fn cancel_pending_on_escape(&mut self, event: &KeyEvent) -> bool {
        if event.state != PressState::Pressed {
            return false;
        }
        if !matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(bitty_platform::NamedKey::Escape)
        ) {
            return false;
        }
        let mut cancelled = false;
        if self.pending_ws_close.is_some() {
            // CTX-0243: Esc-cancel is user intent — snap to live (key handler
            // already snapped; idempotent).
            self.snap_focused_to_live();
            cancelled = self.cancel_pending_ws_close();
        }
        if self.pending_paste.is_none() {
            return cancelled;
        }
        // CTX-0243: Esc-cancel is user intent — snap to live (key handler
        // already snapped; idempotent).
        self.snap_focused_to_live();
        self.pending_paste = None;
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            Some(true),
        );
        self.publish_inspect_snapshot();
        true
    }

    pub(super) fn deliver_paste_bytes(&mut self, bytes: &[u8]) {
        self.write_input(bytes);
    }

    pub(super) fn deliver_paste_bytes_bracketed(&mut self, text: &str) {
        let bracketed = self.state.modes().bracketed_paste;
        let bytes = crate::paste::bracketed_wrap(text, bracketed);
        self.write_input(&bytes);
    }

    /// Selects all cells in the current snapshot (Ctrl+Shift+A / triple-click equivalent).
    pub fn select_all(&mut self) {
        let snap = self.state.snapshot();
        if snap.width == 0 || snap.height == 0 {
            self.selection = None;
            self.pending_full_redraw = true;
            return;
        }
        let start = CellPos::new(0, 0);
        let end = CellPos::new((snap.height - 1) as u16, (snap.width - 1) as u16);
        let sel = Selection {
            anchor: start,
            focus: bitty_ui::snap_to_leading(&snap, end),
            kind: SelectionKind::Simple,
            active: false,
        };
        self.selection = Some(sel);
        self.selection_dragging = false;
        self.pending_full_redraw = true;
    }

    /// Owned clipboard handle (mutable) for advanced use (e.g. OSC 52 tests).
    pub fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// Owned clipboard handle (read-only).
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// Forces the clipboard into headless mode (test helper, deterministic).
    ///
    /// Replaces the handle (clearing both the standard and primary headless
    /// buffers) and drops any recorded clipboard error, so tests start from
    /// a clean seam and never touch the real clipboard or primary.
    pub fn force_headless_clipboard(&mut self) {
        self.clipboard = Clipboard::new_headless();
        self.last_clipboard_error = None;
    }

    /// Allow or deny OSC 52 clipboard writes (capability-gated, default false).
    pub fn set_osc_clipboard_write_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_write_allowed = allowed;
    }

    /// Allow or deny OSC 52 clipboard reads / queries (consent-gated, default false).
    pub fn set_osc_clipboard_read_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_read_allowed = allowed;
    }

    /// Whether OSC 52 writes are currently allowed.
    #[must_use]
    pub fn osc_clipboard_write_allowed(&self) -> bool {
        self.osc_clipboard_write_allowed
    }

    /// Whether OSC 52 reads are currently allowed (consent-gated).
    #[must_use]
    pub fn osc_clipboard_read_allowed(&self) -> bool {
        self.osc_clipboard_read_allowed
    }

    /// Count of OSC 52 writes rejected for invalid base64 (CTX-0212).
    ///
    /// Monotonic (wrapping); each rejection leaves the clipboard unchanged
    /// and emits a loud `eprintln!` warn.
    #[must_use]
    pub fn osc52_rejected_writes(&self) -> u64 {
        self.osc52_rejected_writes
    }
}
