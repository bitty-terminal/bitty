//! `Runtime` — keyboard-driven copy mode (CTX-0384, issue #640).
//!
//! Vi-style modal copy mode with visual selection plus yank, built on the
//! CTX-0385 multi-click seams (`SelectionKind`, `word_drag`/`line_drag`,
//! `block_text` plus `text` dispatch, `PersistentSelection` kind
//! preservation). No new dependencies.
//!
//! Design (scoped, fail-closed, bounded `O(1)` state):
//! - `CopyModeState` holds a live-grid cursor plus an optional visual anchor
//!   and kind. The cursor lives in snapshot grid coordinates and is always
//!   clamped plus wide-snapped, so every motion is total over all inputs.
//! - While copy mode is active the runtime consumes all non-modifier key
//!   presses (no PTY bytes, no viewport snap-to-live, no mouse-selection
//!   clearing). Mouse selection and SGR wheel forwarding are suppressed;
//!   viewport paging via keyboard still scrolls for history viewing.
//! - Visual `v` selects [`SelectionKind::Simple`], `V` selects
//!   [`SelectionKind::Line`] (via [`Selection::line_drag`]), `Ctrl+V`
//!   selects [`SelectionKind::Block`]. Yank (`y`/`Enter`) copies through the
//!   existing clipboard plus primary paths, then exits.
//! - Live-grid scope: the cursor and visual selection address the live
//!   snapshot (the same grid the mouse path selects). `PgUp`/`PgDn` scroll
//!   the focused viewport for history viewing; selecting scrolled-off
//!   history rows is a follow-up (buffer-anchored copy).
use super::*;
use bitty_ui::is_word_char;

/// Bounded copy-mode state (`O(1)`: three small values, no heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyModeState {
    /// Live-grid cursor (clamped plus wide-snapped).
    pub cursor: CellPos,
    /// Visual anchor when a visual selection is active.
    pub anchor: Option<CellPos>,
    /// Visual kind when active (`None` means cursor-only, no selection).
    pub visual_kind: Option<SelectionKind>,
}

impl CopyModeState {
    /// New cursor-only state at `cursor`.
    #[must_use]
    pub const fn new(cursor: CellPos) -> Self {
        Self {
            cursor,
            anchor: None,
            visual_kind: None,
        }
    }

    /// Whether a visual selection is active.
    #[must_use]
    pub const fn is_visual(self) -> bool {
        self.anchor.is_some() && self.visual_kind.is_some()
    }
}

impl Runtime {
    /// Whether keyboard copy mode is active.
    #[must_use]
    pub fn is_copy_mode(&self) -> bool {
        self.copy_mode.is_some()
    }

    /// Copy-mode cursor in live-grid coordinates, if active.
    #[must_use]
    pub fn copy_mode_cursor(&self) -> Option<CellPos> {
        self.copy_mode.map(|c| c.cursor)
    }

    /// Visual kind when a visual selection is active (`None` otherwise).
    ///
    /// Reuses the CTX-0385 [`SelectionKind`] vocabulary so copy-mode visuals
    /// stay typed without new selection algebra.
    #[must_use]
    pub fn copy_mode_visual_kind(&self) -> Option<SelectionKind> {
        self.copy_mode.and_then(|c| c.visual_kind)
    }

    /// Status indicator for the chrome layer (`COPY`, `COPY VISUAL`,
    /// `COPY LINE`, `COPY BLOCK`; `None` when inactive).
    #[must_use]
    pub fn copy_mode_label(&self) -> Option<&'static str> {
        let mode = self.copy_mode?;
        match mode.visual_kind {
            None => Some("COPY"),
            Some(SelectionKind::Simple) => Some("COPY VISUAL"),
            Some(SelectionKind::Word) => Some("COPY VISUAL"),
            Some(SelectionKind::Line) => Some("COPY LINE"),
            Some(SelectionKind::Block) => Some("COPY BLOCK"),
        }
    }

    /// Enters keyboard copy mode (fail-closed: no-op when already active).
    ///
    /// The cursor starts at the live terminal cursor (clamped plus
    /// wide-snapped). Any existing mouse selection is cleared so the copy
    /// cursor owns the highlight; no PTY bytes are produced.
    pub fn enter_copy_mode(&mut self) {
        if self.copy_mode.is_some() {
            return;
        }
        // CTX-0383: modals stay exclusive — entering copy exits search.
        if self.search_mode {
            self.exit_search_mode();
        }
        let snap = self.state.snapshot();
        let term_cursor = snap.cursor.position;
        let raw = CellPos::new(term_cursor.row, term_cursor.col);
        let cursor = bitty_ui::snap_to_leading(&snap, clamp_copy_pos(&snap, raw));
        self.clear_selection();
        self.copy_mode = Some(CopyModeState::new(cursor));
        self.pending_full_redraw = true;
    }

    /// Exits copy mode and clears any copy-driven selection.
    ///
    /// Yank paths call [`Self::copy_mode_yank`] instead (copy first, then
    /// exit). Plain exit (Esc) copies nothing.
    pub fn exit_copy_mode(&mut self) {
        if self.copy_mode.is_none() {
            return;
        }
        self.copy_mode = None;
        self.selection = None;
        self.selection_dragging = false;
        self.selection_anchor_press = None;
        self.pending_full_redraw = true;
    }

    /// Yanks the current copy-mode visual selection to the clipboard plus
    /// the primary selection, then exits copy mode.
    ///
    /// Reuses the existing fail-soft clipboard paths
    /// ([`Self::copy_selection_to_clipboard`] plus
    /// [`Self::copy_selection_to_primary`]) so headless determinism and
    /// error recording stay identical to the mouse path. Returns the yanked
    /// text, or `None` when no non-empty visual selection exists (stays in
    /// copy mode so the user can adjust).
    pub fn copy_mode_yank(&mut self) -> Option<String> {
        self.copy_mode?;
        let text = self.selection_text()?;
        let _ = self.copy_selection_to_clipboard();
        let _ = self.copy_selection_to_primary();
        self.exit_copy_mode();
        Some(text)
    }

    /// Handles one key event while copy mode is active.
    ///
    /// Returns `true` when the key was consumed (the caller must not forward
    /// to the PTY). Total over all inputs: unknown keys are consumed as
    /// no-ops so typing can never leak into the shell while modal.
    pub(super) fn handle_copy_mode_key(&mut self, event: &KeyEvent) -> bool {
        use bitty_platform::{LogicalKey, NamedKey, PressState};
        if self.copy_mode.is_none() {
            return false;
        }
        // Releases and synthetic events produce no PTY bytes anyway; consume
        // them so copy mode stays modal until an explicit exit/yank.
        if event.state != PressState::Pressed || event.is_synthetic {
            return true;
        }
        // Modifier-only keys never drive copy motions; consume without effect
        // (modifier tracking already ran in the caller).
        if matches!(
            &event.logical_key,
            LogicalKey::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::AltGraph
                    | NamedKey::Super
                    | NamedKey::Meta
            )
        ) {
            return true;
        }
        match &event.logical_key {
            LogicalKey::Named(NamedKey::Escape) => {
                self.exit_copy_mode();
                true
            }
            LogicalKey::Named(NamedKey::Enter) => {
                // Enter yanks like `y` (tmux parity); with no selection it is
                // a no-op that stays active.
                let _ = self.copy_mode_yank();
                true
            }
            LogicalKey::Named(named) => {
                self.handle_copy_mode_named(*named);
                true
            }
            LogicalKey::Character(text) => {
                self.handle_copy_mode_char(text);
                true
            }
            LogicalKey::Dead(_) | LogicalKey::Unidentified => true,
        }
    }

    /// Named-key motions for copy mode (arrows, pages, home/end).
    fn handle_copy_mode_named(&mut self, named: bitty_platform::NamedKey) {
        use bitty_platform::NamedKey;
        // See `handle_copy_mode_char`: Alt-held named keys are chrome-owned.
        if self.alt_pressed {
            return;
        }
        match named {
            NamedKey::ArrowLeft => self.copy_mode_move(-1, 0),
            NamedKey::ArrowRight => self.copy_mode_move(1, 0),
            NamedKey::ArrowUp => self.copy_mode_move(0, -1),
            NamedKey::ArrowDown => self.copy_mode_move(0, 1),
            NamedKey::PageUp => self.copy_mode_page(-1),
            NamedKey::PageDown => self.copy_mode_page(1),
            NamedKey::Home => self.copy_mode_line_start(),
            NamedKey::End => self.copy_mode_line_end(),
            _ => {}
        }
    }

    /// Character-key dispatch for copy mode (motions, visuals, yank, exit).
    ///
    /// Single-character matching is fail-closed: `Ctrl` held only enables
    /// the `Ctrl+V` block-visual gesture; every other `Ctrl` chord is
    /// consumed as a no-op so shell control bytes never leak while modal.
    fn handle_copy_mode_char(&mut self, text: &str) {
        // Alt-held chords are chrome-owned (or unbound chrome candidates):
        // consume without moving so `Alt+H` never drives the copy cursor
        // while modal (the app captures bound Alt chords first; this guards
        // headless/direct runtime callers).
        if self.alt_pressed {
            return;
        }
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return;
        };
        if chars.next().is_some() {
            // Multi-char composition (IME shape): no-op while modal.
            return;
        }
        // `Ctrl+V` (block visual) wins over the plain `v`/`V` arms below.
        // The platform reports `Ctrl+V` either as control-held `v` or as
        // `0x16`; both map here through the caller's modifier mirror.
        if self.control_pressed && (first == 'v' || first == 'V' || first == '\u{16}') {
            self.copy_mode_toggle_visual(SelectionKind::Block);
            return;
        }
        // Any other Ctrl-held chord is consumed without effect (no SIGINT,
        // no control bytes to the PTY while modal).
        if self.control_pressed {
            return;
        }
        match first {
            'h' => self.copy_mode_move(-1, 0),
            'l' => self.copy_mode_move(1, 0),
            'j' => self.copy_mode_move(0, 1),
            'k' => self.copy_mode_move(0, -1),
            'v' => self.copy_mode_toggle_visual(SelectionKind::Simple),
            'V' => self.copy_mode_toggle_visual(SelectionKind::Line),
            'y' | 'Y' => {
                let _ = self.copy_mode_yank();
            }
            'g' => self.copy_mode_top(),
            'G' => self.copy_mode_bottom(),
            '0' => self.copy_mode_line_start(),
            '$' => self.copy_mode_line_end(),
            'w' => self.copy_mode_word_forward(),
            'b' => self.copy_mode_word_backward(),
            'e' => self.copy_mode_word_end(),
            // `H` (shift+h) is not a motion here: plain `h` already moves and
            // shift-tracking varies by platform, so consume without effect
            // rather than aliasing to an unexpected jump.
            'H' => {}
            _ => {}
        }
    }

    /// Toggles a visual selection of `kind` anchored at the cursor.
    ///
    /// Pressing the active kind again clears the visual (cursor-only).
    /// Switching kinds re-anchors at the cursor. Entering a line visual
    /// immediately covers the cursor row via [`Selection::line_drag`].
    fn copy_mode_toggle_visual(&mut self, kind: SelectionKind) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        if mode.visual_kind == Some(kind) {
            self.copy_mode = Some(CopyModeState::new(mode.cursor));
            self.selection = None;
            self.selection_dragging = false;
            self.selection_anchor_press = None;
            self.pending_full_redraw = true;
            return;
        }
        let snap = self.state.snapshot();
        let cursor = bitty_ui::snap_to_leading(&snap, clamp_copy_pos(&snap, mode.cursor));
        let selection = match kind {
            SelectionKind::Line => Selection::line_drag(&snap, cursor, cursor),
            SelectionKind::Block => Selection::block(cursor, cursor),
            SelectionKind::Simple | SelectionKind::Word => Selection::simple(cursor, cursor),
        };
        self.copy_mode = Some(CopyModeState {
            cursor,
            anchor: Some(cursor),
            visual_kind: Some(kind),
        });
        // A collapsed visual is not a selection yet (mirrors mouse press
        // before drag); store it so rendering can show the cursor anchor
        // without exposing empty text through `selection_text`.
        if selection.anchor == selection.focus {
            self.selection = None;
        } else {
            self.selection = Some(selection);
        }
        self.selection_dragging = false;
        self.selection_anchor_press = None;
        self.pending_full_redraw = true;
    }

    /// Moves the cursor by (`dx`, `dy`) cells with wide-aware clamping.
    ///
    /// When a visual is active the live `selection` follows via the CTX-0385
    /// seams: `Simple` tracks anchor to cursor, `Line` covers whole lines
    /// via `line_drag`, `Block` tracks the rectangle corner.
    fn copy_mode_move(&mut self, dx: isize, dy: isize) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let next = step_copy_cursor(&snap, mode.cursor, dx, dy);
        self.copy_mode_apply_cursor(next);
    }

    /// Applies `next` as the copy cursor and refreshes the visual selection.
    fn copy_mode_apply_cursor(&mut self, next: CellPos) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let cursor = bitty_ui::snap_to_leading(&snap, clamp_copy_pos(&snap, next));
        let Some(anchor) = mode.anchor else {
            self.copy_mode = Some(CopyModeState::new(cursor));
            // Cursor-only motion clears any stale highlight.
            self.selection = None;
            self.pending_full_redraw = true;
            return;
        };
        let Some(kind) = mode.visual_kind else {
            self.copy_mode = Some(CopyModeState::new(cursor));
            self.selection = None;
            self.pending_full_redraw = true;
            return;
        };
        let selection = match kind {
            SelectionKind::Simple | SelectionKind::Word => Selection::simple(anchor, cursor),
            SelectionKind::Line => Selection::line_drag(&snap, anchor, cursor),
            SelectionKind::Block => Selection::block(anchor, cursor),
        };
        self.copy_mode = Some(CopyModeState {
            cursor,
            anchor: Some(anchor),
            visual_kind: Some(kind),
        });
        if selection.anchor == selection.focus {
            self.selection = None;
        } else {
            let clamped = selection.clamped(&snap).snapped(Some(&snap));
            self.selection = Some(clamped);
        }
        self.selection_dragging = false;
        self.pending_full_redraw = true;
    }

    /// Pages the cursor plus the focused viewport (`PgUp`/`PgDn`).
    ///
    /// The viewport scroll keeps history viewing keyboard-only; the cursor
    /// page keeps the motion total even with no scrollback.
    fn copy_mode_page(&mut self, dir: isize) {
        let page = self.copy_mode_page_rows().max(1) as isize;
        let max = self.state.scrollback_len();
        if let Some(id) = self.focused_view() {
            if let Some(view) = self.layout.find_leaf_mut(id) {
                view.scroll_by(dir * page, max);
            }
        }
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let row = (mode.cursor.row as isize + dir * page)
            .clamp(0, snap.height.saturating_sub(1) as isize) as u16;
        self.copy_mode_apply_cursor(CellPos::new(row, mode.cursor.col));
        self.pending_full_redraw = true;
    }

    /// Page size: focused view rows when available, else snapshot height.
    fn copy_mode_page_rows(&self) -> usize {
        if let Some(id) = self.focused_view() {
            if let Some(view) = self.layout.find_leaf(id) {
                let rows = usize::from(view.rows());
                if rows > 0 {
                    return rows;
                }
            }
        }
        self.state.snapshot().height.max(1)
    }

    /// Jump to the first row, first column.
    fn copy_mode_top(&mut self) {
        self.copy_mode_apply_cursor(CellPos::new(0, 0));
    }

    /// Jump to the last row, first column.
    fn copy_mode_bottom(&mut self) {
        let snap = self.state.snapshot();
        let row = snap.height.saturating_sub(1) as u16;
        self.copy_mode_apply_cursor(CellPos::new(row, 0));
    }

    /// Jump to the first column of the cursor row.
    fn copy_mode_line_start(&mut self) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        self.copy_mode_apply_cursor(CellPos::new(mode.cursor.row, 0));
    }

    /// Jump to the last column of the cursor row (wide-snapped).
    fn copy_mode_line_end(&mut self) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let last = snap.width.saturating_sub(1) as u16;
        self.copy_mode_apply_cursor(CellPos::new(mode.cursor.row, last));
    }

    /// Vi `w`: next word start on the cursor row (same-row, clamped).
    fn copy_mode_word_forward(&mut self) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let next = copy_word_forward(&snap, mode.cursor);
        self.copy_mode_apply_cursor(next);
    }

    /// Vi `b`: previous word start on the cursor row (same-row, clamped).
    fn copy_mode_word_backward(&mut self) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let next = copy_word_backward(&snap, mode.cursor);
        self.copy_mode_apply_cursor(next);
    }

    /// Vi `e`: next word end on the cursor row (same-row, clamped).
    fn copy_mode_word_end(&mut self) {
        let Some(mode) = self.copy_mode else {
            return;
        };
        let snap = self.state.snapshot();
        let next = copy_word_end(&snap, mode.cursor);
        self.copy_mode_apply_cursor(next);
    }
}

/// Clamps `pos` into `snapshot` bounds (total over all inputs).
fn clamp_copy_pos(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return CellPos::new(0, 0);
    }
    let row = (pos.row as usize).min(snapshot.height.saturating_sub(1)) as u16;
    let col = (pos.col as usize).min(snapshot.width.saturating_sub(1)) as u16;
    CellPos::new(row, col)
}

/// Wide-aware single step from `pos` by (`dx`, `dy`).
///
/// Moving right off a wide lead jumps two cells (over the spacer);
/// every landing snaps to its leading half so wide pairs never split.
fn step_copy_cursor(snapshot: &Snapshot, pos: CellPos, dx: isize, dy: isize) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return CellPos::new(0, 0);
    }
    let max_row = snapshot.height.saturating_sub(1) as isize;
    let max_col = snapshot.width.saturating_sub(1) as isize;
    let mut row = (pos.row as isize + dy).clamp(0, max_row);
    let mut col = pos.col as isize;
    if dx > 0 {
        for _ in 0..dx {
            let cur = CellPos::new(row as u16, col.clamp(0, max_col) as u16);
            let step = if is_wide_lead(snapshot, cur) { 2 } else { 1 };
            col = (col + step).min(max_col);
        }
    } else if dx < 0 {
        for _ in 0..(-dx) {
            col = (col - 1).max(0);
            // Landing on a spacer snaps one more left to its leader.
            let probe = CellPos::new(row as u16, col.clamp(0, max_col) as u16);
            if is_spacer(snapshot, probe) && col > 0 {
                col -= 1;
            }
        }
    }
    row = row.clamp(0, max_row);
    col = col.clamp(0, max_col);
    bitty_ui::snap_to_leading(snapshot, CellPos::new(row as u16, col as u16))
}

/// True when `pos` addresses a wide leading cell.
fn is_wide_lead(snapshot: &Snapshot, pos: CellPos) -> bool {
    let row = pos.row as usize;
    let col = pos.col as usize;
    if row >= snapshot.height || col >= snapshot.width {
        return false;
    }
    let idx = row * snapshot.width + col;
    snapshot
        .cells
        .get(idx)
        .is_some_and(|c| c.width == 2 && !c.spacer)
}

/// True when `pos` addresses a wide spacer half.
fn is_spacer(snapshot: &Snapshot, pos: CellPos) -> bool {
    let row = pos.row as usize;
    let col = pos.col as usize;
    if row >= snapshot.height || col >= snapshot.width {
        return false;
    }
    let idx = row * snapshot.width + col;
    snapshot.cells.get(idx).is_some_and(|c| c.spacer)
}

/// Glyph at `pos`, if in bounds.
fn glyph_at(snapshot: &Snapshot, pos: CellPos) -> Option<char> {
    let row = pos.row as usize;
    let col = pos.col as usize;
    if row >= snapshot.height || col >= snapshot.width {
        return None;
    }
    let idx = row * snapshot.width + col;
    snapshot.cells.get(idx).map(|c| c.glyph)
}

/// Blank (padding) at `pos`: missing, blank-flagged, or a space.
fn is_blank_at(snapshot: &Snapshot, pos: CellPos) -> bool {
    let row = pos.row as usize;
    let col = pos.col as usize;
    if row >= snapshot.height || col >= snapshot.width {
        return true;
    }
    let idx = row * snapshot.width + col;
    match snapshot.cells.get(idx) {
        Some(c) if c.spacer => true,
        Some(c) if c.is_blank() => true,
        Some(c) if c.glyph == ' ' => true,
        Some(_) => false,
        None => true,
    }
}

/// Word char at `pos` (blank counts as delimiter, never a word char).
fn is_word_at(snapshot: &Snapshot, pos: CellPos) -> bool {
    if is_blank_at(snapshot, pos) {
        return false;
    }
    match glyph_at(snapshot, pos) {
        Some(ch) => is_word_char(ch),
        None => false,
    }
}

/// Leader-column cursor for `pos` (spacers resolve left first).
fn leader_col(snapshot: &Snapshot, row: u16, col: usize) -> usize {
    let snapped = bitty_ui::snap_to_leading(snapshot, CellPos::new(row, col as u16));
    snapped.col as usize
}

/// Next leaders after `col` on `row` (each wide glyph visited once).
fn next_leader(snapshot: &Snapshot, row: u16, col: usize) -> Option<usize> {
    let width = snapshot.width;
    let mut c = col + 1;
    while c < width {
        let pos = CellPos::new(row, c as u16);
        let snapped = bitty_ui::snap_to_leading(snapshot, pos);
        let lc = snapped.col as usize;
        if lc <= col {
            c += 1;
            continue;
        }
        return Some(lc);
    }
    None
}

/// Previous leader before `col` on `row`, if any.
fn prev_leader(snapshot: &Snapshot, row: u16, col: usize) -> Option<usize> {
    if col == 0 {
        return None;
    }
    let pos = CellPos::new(row, (col - 1) as u16);
    Some(bitty_ui::snap_to_leading(snapshot, pos).col as usize)
}

/// Vi `w` on one row: next word start, else clamped line end.
fn copy_word_forward(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return CellPos::new(0, 0);
    }
    let row = pos.row.min(snapshot.height.saturating_sub(1) as u16);
    let mut col = leader_col(snapshot, row, pos.col as usize);
    // Skip the rest of the current word (when on one).
    if is_word_at(snapshot, CellPos::new(row, col as u16)) {
        while let Some(nc) = next_leader(snapshot, row, col) {
            if !is_word_at(snapshot, CellPos::new(row, nc as u16)) {
                col = nc;
                break;
            }
            col = nc;
        }
        // Ran off the word to its end: advance one past it when possible.
        if is_word_at(snapshot, CellPos::new(row, col as u16)) {
            match next_leader(snapshot, row, col) {
                Some(nc) => col = nc,
                None => {
                    return CellPos::new(row, snapshot.width.saturating_sub(1) as u16);
                }
            }
        }
    }
    // Skip delimiters to the next word start.
    loop {
        if is_word_at(snapshot, CellPos::new(row, col as u16)) {
            return CellPos::new(row, col as u16);
        }
        match next_leader(snapshot, row, col) {
            Some(nc) => col = nc,
            None => return CellPos::new(row, col as u16),
        }
    }
}

/// Vi `b` on one row: previous word start, else clamped line start.
fn copy_word_backward(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return CellPos::new(0, 0);
    }
    let row = pos.row.min(snapshot.height.saturating_sub(1) as u16);
    let mut col = leader_col(snapshot, row, pos.col as usize);
    // Step one glyph left first (so `b` on a word start moves to the
    // previous word instead of staying).
    let Some(prev) = prev_leader(snapshot, row, col) else {
        return CellPos::new(row, 0);
    };
    col = prev;
    // Skip delimiters leftwards to the previous word.
    loop {
        if is_word_at(snapshot, CellPos::new(row, col as u16)) {
            // Walk to that word's start.
            loop {
                match prev_leader(snapshot, row, col) {
                    Some(pc) if is_word_at(snapshot, CellPos::new(row, pc as u16)) => {
                        col = pc;
                    }
                    _ => return CellPos::new(row, col as u16),
                }
            }
        }
        match prev_leader(snapshot, row, col) {
            Some(pc) => col = pc,
            None => return CellPos::new(row, 0),
        }
    }
}

/// Vi `e` on one row: next word end (stays on a mid-word end run).
fn copy_word_end(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    if snapshot.width == 0 || snapshot.height == 0 {
        return CellPos::new(0, 0);
    }
    let row = pos.row.min(snapshot.height.saturating_sub(1) as u16);
    let mut col = leader_col(snapshot, row, pos.col as usize);
    // Step one glyph right first when on a word end already (so repeat `e`
    // advances instead of sticking).
    let on_word = is_word_at(snapshot, CellPos::new(row, col as u16));
    let at_end = on_word
        && next_leader(snapshot, row, col)
            .is_none_or(|nc| !is_word_at(snapshot, CellPos::new(row, nc as u16)));
    if at_end {
        match next_leader(snapshot, row, col) {
            Some(nc) => col = nc,
            None => return CellPos::new(row, col as u16),
        }
    }
    // Skip delimiters to the next word.
    loop {
        if is_word_at(snapshot, CellPos::new(row, col as u16)) {
            // Walk to that word's end.
            loop {
                match next_leader(snapshot, row, col) {
                    Some(nc) if is_word_at(snapshot, CellPos::new(row, nc as u16)) => {
                        col = nc;
                    }
                    _ => return CellPos::new(row, col as u16),
                }
            }
        }
        match next_leader(snapshot, row, col) {
            Some(nc) => col = nc,
            None => return CellPos::new(row, col as u16),
        }
    }
}
