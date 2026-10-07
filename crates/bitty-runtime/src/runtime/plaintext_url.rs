//! Plaintext URL detection wiring (issue #1760, OQ-004).
//!
//! Scanner side lives in the dependency-free `bitty-url-detector` crate
//! (`detect_urls`, byte offsets, `forbid(unsafe)`); this module maps grid
//! lines to detector matches and feeds them into the same
//! [`ValidatedUrl`](bitty_platform::ValidatedUrl) + [`ActivationGesture`](super::ActivationGesture)
//! pipeline as OSC 8.
//!
//! Coordination with #1759 (PR #1771, R-005): OSC 8 hover/activation lives
//! there; this module owns only the plaintext path with a distinct hover
//! field (`hovered_plaintext_url`) so the two PRs do not duplicate logic.
//! Both paths share validation (`validate_url`) and the single-use gesture.
//!
//! Mapping rule: the detector returns byte offsets into the reconstructed
//! line text. Byte offsets equal columns only for pure ASCII; a preceding
//! multi-byte scalar (e.g. `caf\u{e9}`) shifts bytes past columns. The map
//! below converts byte offsets to lead columns via char indices, mirroring
//! `bitty-term-state::search` (`text[..byte_off].chars().count()` +
//! per-char lead columns). Detector matches are ASCII-only, so each match
//! char occupies exactly one column; wide trailing spacers are never
//! emitted (the lead carries the column).

#![forbid(unsafe_code)]

use bitty_term_state::{Cell, Snapshot};

/// One validated plaintext URL span on a single row.
///
/// Spans never cross rows. Columns are owner-grid lead columns, inclusive
/// on both ends (matching `HoveredHyperlink` in #1771 for future merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaintextUrlSpan {
    /// Safe target URI (`http`/`https`/`mailto` only; `git://` is detected
    /// by the scanner but rejected by `validate_url`, fail-closed).
    pub uri: String,
    /// Owner-grid row of the span.
    pub row: usize,
    /// Inclusive owner-grid start column.
    pub col_start: usize,
    /// Inclusive owner-grid end column.
    pub col_end: usize,
}

/// Plaintext URL span under the pointer, with its owning view.
///
/// Presentation-only hover state (issue #1760): the owner-grid span backing
/// the pointer cursor and the underline highlight. Rows/cols are owner-grid
/// cells (translated to frame space at paint through `owner_row_window_start`,
/// like the selection highlight); spans never cross rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoveredPlaintextUrl {
    /// View owning the grid the span was resolved in.
    pub view: crate::ViewId,
    /// Safe target URI.
    pub uri: String,
    /// Owner-grid row of the span.
    pub row: usize,
    /// Inclusive owner-grid start column.
    pub col_start: usize,
    /// Inclusive owner-grid end column.
    pub col_end: usize,
}

/// Reconstructs line text plus per-char lead-column map from grid cells.
///
/// Mirrors `bitty-term-state::search::line_text_and_map`: skips wide
/// trailing spacers (the lead's glyph is emitted once), emits `' '` for
/// blanks, and appends combining marks onto their base cell's column.
/// Returns `(text, col_map)` where `col_map[i]` is the lead column of
/// `text` char `i`.
fn line_text_and_map(cells: &[Cell]) -> (String, Vec<usize>) {
    let mut text = String::with_capacity(cells.len());
    let mut map = Vec::new();
    let mut col = 0usize;
    while col < cells.len() {
        let cell = &cells[col];
        if cell.spacer {
            col += 1;
            continue;
        }
        if cell.is_blank() {
            text.push(' ');
            map.push(col);
        } else {
            text.push(cell.glyph);
            map.push(col);
            for mark in &cell.zerowidth {
                text.push(*mark);
                map.push(col);
            }
        }
        if cell.width == 2 {
            col += 2;
        } else {
            col += 1;
        }
    }
    (text, map)
}

/// Whether `uri` passes the shared safety gate (issue #1760).
///
/// Same pipeline as OSC 8: `http`/`https`/`mailto` through
/// [`validate_url`](bitty_platform::validate_url). `git://` matches are
/// detected but never presented (the allowlist rejects them); `file:`,
/// `javascript:`, and friends are likewise fail-closed.
#[must_use]
pub fn is_safe_plaintext_uri(uri: &str) -> bool {
    bitty_platform::validate_url(uri).is_ok()
}

/// Collects validated plaintext URL spans for one grid row.
///
/// Runs [`detect_urls`](bitty_url_detector::detect_urls) on the
/// reconstructed line text, maps byte offsets to lead columns, and keeps
/// only spans that pass [`is_safe_plaintext_uri`]. Row-major order,
/// bounded by the detector output (at most one entry per URL).
#[must_use]
pub fn plaintext_spans_for_row(cells: &[Cell], row: usize) -> Vec<PlaintextUrlSpan> {
    let (text, map) = line_text_and_map(cells);
    if text.is_empty() || map.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for (byte_start, byte_end, url) in bitty_url_detector::detect_urls(&text) {
        if !is_safe_plaintext_uri(&url) {
            continue;
        }
        let char_start = text.get(..byte_start).map_or(0, |s| s.chars().count());
        let url_chars = url.chars().count();
        if url_chars == 0 || char_start >= map.len() {
            continue;
        }
        let last_char_idx = match char_start.checked_add(url_chars.saturating_sub(1)) {
            Some(idx) => idx,
            None => continue,
        };
        if last_char_idx >= map.len() {
            continue;
        }
        let col_start = map[char_start];
        let last_col = map[last_char_idx];
        // ASCII-only matches are width-1; look up the lead width
        // defensively so a future alphabet change cannot mis-anchor.
        let width = usize::from(cells.get(last_col).map_or(1, |c| c.width.max(1)));
        let col_end = last_col.saturating_add(width.saturating_sub(1));
        // `byte_end` is exclusive; silence the unused binding by asserting
        // the slice round-trips (detector contract: `input[s..e] == url`).
        debug_assert!(text.get(byte_start..byte_end) == Some(url.as_str()));
        spans.push(PlaintextUrlSpan {
            uri: url,
            row,
            col_start,
            col_end,
        });
    }
    spans
}

/// Hit-tests `snapshot` at `(row, col)` for a validated plaintext URL.
///
/// Returns the span under the cursor when present, otherwise `None`.
/// Out-of-bounds coordinates yield `None`; trailing-spacer columns resolve
/// through the lead (wide glyphs are wholly clickable).
#[must_use]
pub fn plaintext_span_at(snapshot: &Snapshot, row: usize, col: usize) -> Option<PlaintextUrlSpan> {
    if row >= snapshot.height || col >= snapshot.width {
        return None;
    }
    let base = row.checked_mul(snapshot.width)?;
    let width = snapshot.width;
    let start = base;
    let end = base.checked_add(width)?;
    let cells = snapshot.cells.get(start..end)?;
    // Snap trailing spacers to their lead, mirroring the OSC 8 hit test.
    let lead_col = if cells.get(col).is_some_and(|c| c.spacer) && col > 0 {
        let lead = col - 1;
        if cells.get(lead).is_some_and(|c| c.width == 2) {
            lead
        } else {
            col
        }
    } else {
        col
    };
    plaintext_spans_for_row(cells, row)
        .into_iter()
        .find(|span| lead_col >= span.col_start && lead_col <= span.col_end)
}

use super::Runtime;
use bitty_platform::{CursorIcon, CursorPosition};

impl Runtime {
    /// Whether the plaintext gesture modifier is held (issue #1760).
    ///
    /// `Ctrl` on Linux/Windows, `Cmd` on macOS (arrives as
    /// [`Self::super_pressed`] via the named-key latch). Either authorizes
    /// click-to-open and the hover affordance. Mirrors #1759's
    /// `hyperlink_activation_modifier_held` without duplicating it: when
    /// PR #1771 lands the two helpers unify.
    #[must_use]
    pub fn plaintext_activation_modifier_held(&self) -> bool {
        self.control_pressed || self.super_pressed
    }

    /// Full owner-grid span of the safe plaintext URL under `pos`.
    ///
    /// Same hit-test rule as OSC 8 `hyperlink_uri_at` (in-frame only,
    /// fail-closed on history scroll): resolves through the View under the
    /// pointer, maps the frame cell to the owner grid, then hit-tests the
    /// reconstructed line. Unsafe schemes never resolve because
    /// [`plaintext_spans_for_row`] already gates on [`is_safe_plaintext_uri`].
    fn plaintext_hover_at_pos(&self, pos: CursorPosition) -> Option<HoveredPlaintextUrl> {
        let frames = self.present_frames();
        let (view, local) = self.present_cell_in(&frames, pos)?;
        let rows = frames
            .iter()
            .find(|frame| frame.view == view)
            .map_or(0, |frame| frame.rows);
        let cell = self.frame_cell_to_owner_cell(view, rows, local)?;
        if self.view_scroll_offset(view) != 0 {
            return None;
        }
        let state = self.live_view_state(view)?;
        let snapshot = state.snapshot();
        let (row, col) = (usize::from(cell.row), usize::from(cell.col));
        // OSC 8 takes precedence where both claim a cell: an explicit link
        // owns the cell even when hostile (its path already fails closed),
        // so plaintext never second-guesses it.
        let index = row.checked_mul(snapshot.width)?.checked_add(col)?;
        if snapshot.cells.get(index)?.hyperlink.is_some() {
            return None;
        }
        let span = plaintext_span_at(&snapshot, row, col)?;
        Some(HoveredPlaintextUrl {
            view,
            uri: span.uri,
            row: span.row,
            col_start: span.col_start,
            col_end: span.col_end,
        })
    }

    /// Safe plaintext URI under `pos`, if any (issue #1760).
    ///
    /// Shared by the press interception and the gesture mint so the two can
    /// never disagree about which patterns are clickable.
    pub(super) fn safe_plaintext_url_at(&self, pos: CursorPosition) -> Option<String> {
        self.plaintext_hover_at_pos(pos).map(|hover| hover.uri)
    }

    /// Owner-grid span of the safe plaintext URL under `pos`, if any.
    ///
    /// Public for tests and the present path; `None` when the cell holds an
    /// OSC 8 id, is out of frame, or is scrolled into history.
    #[must_use]
    pub fn plaintext_hover_at_cursor(&self, pos: CursorPosition) -> Option<HoveredPlaintextUrl> {
        self.plaintext_hover_at_pos(pos)
    }

    /// Refreshes the plaintext hover from `pos` (issue #1760).
    ///
    /// Ctrl-gated: without the gesture modifier the hover clears (no stale
    /// underline/pointer when `Ctrl` is released). A change in either
    /// direction forces a full redraw; steady hover costs one bounded hit
    /// test and no redraw.
    pub(super) fn update_plaintext_hover(&mut self, pos: CursorPosition) {
        let next = if self.hover_suppressed_by_overlay {
            None
        } else if self.plaintext_activation_modifier_held() {
            self.plaintext_hover_at_pos(pos)
        } else {
            None
        };
        if next != self.hovered_plaintext_url {
            self.hovered_plaintext_url = next;
            self.pending_full_redraw = true;
        }
    }

    /// Clears the plaintext hover (pointer left the window).
    pub(super) fn clear_plaintext_hover(&mut self) {
        if self.hovered_plaintext_url.is_some() {
            self.hovered_plaintext_url = None;
            self.pending_full_redraw = true;
        }
    }

    /// Re-resolves the hover against live grid truth (once per tick).
    ///
    /// Terminal output can move or evict the hovered URL under a stationary
    /// pointer; only a re-resolve keeps the highlight honest. Modifier-gated
    /// like [`Self::update_plaintext_hover`].
    pub(super) fn revalidate_plaintext_hover(&mut self) {
        if self.hover_suppressed_by_overlay {
            self.clear_plaintext_hover();
            return;
        }
        match self.last_cursor {
            Some(pos) => self.update_plaintext_hover(pos),
            None => self.clear_plaintext_hover(),
        }
    }

    /// Suppresses hyperlink hover affordances while an overlay capture holds.
    ///
    /// Set per tick by the app; while true, revalidates clear instead of
    /// re-arming and paints skip, so a modal never shows underlying URL
    /// affordances.
    pub fn set_hover_suppressed_by_overlay(&mut self, suppressed: bool) {
        self.hover_suppressed_by_overlay = suppressed;
    }

    /// Owner-grid span of the hovered plaintext URL, if any.
    #[must_use]
    pub fn hovered_plaintext_span(&self) -> Option<HoveredPlaintextUrl> {
        self.hovered_plaintext_url.clone()
    }

    /// URI of the hovered plaintext URL, if any.
    #[must_use]
    pub fn hovered_plaintext_uri(&self) -> Option<&str> {
        self.hovered_plaintext_url
            .as_ref()
            .map(|hover| hover.uri.as_str())
    }

    /// OS pointer shape for the plaintext hover state.
    ///
    /// [`CursorIcon::Pointer`] over a valid plaintext span with `Ctrl` held,
    /// [`CursorIcon::Text`] elsewhere. The app applies this with change
    /// detection. When PR #1771 lands, the app unifies this with
    /// `hyperlink_cursor_icon` (either pointer wins).
    #[must_use]
    pub fn plaintext_cursor_icon(&self) -> CursorIcon {
        if self.hovered_plaintext_url.is_some() {
            CursorIcon::Pointer
        } else {
            CursorIcon::Text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{State, TerminalAction};
    use bitty_vt::GraphemeCell;

    fn feed(state: &mut State, text: &str) {
        for ch in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    fn snapshot_for_line(line: &str) -> Snapshot {
        let mut state = State::new();
        feed(&mut state, line);
        state.snapshot()
    }

    #[test]
    fn plain_text_yields_no_spans() {
        let snap = snapshot_for_line("just some words 12345");
        assert!(plaintext_spans_for_row(&snap.cells[..snap.width], 0).is_empty());
        assert!(plaintext_span_at(&snap, 0, 0).is_none());
    }

    #[test]
    fn http_span_maps_to_columns() {
        let snap = snapshot_for_line("see http://example.com here");
        let spans = plaintext_spans_for_row(&snap.cells[..snap.width], 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].uri, "http://example.com");
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].col_start, 4);
        assert_eq!(spans[0].col_end, 4 + "http://example.com".len() - 1);
        assert_eq!(
            plaintext_span_at(&snap, 0, 4)
                .as_ref()
                .map(|s| s.uri.as_str()),
            Some("http://example.com")
        );
        assert!(plaintext_span_at(&snap, 0, 0).is_none());
    }

    #[test]
    fn unicode_prefix_shifts_bytes_past_columns() {
        // `caf\u{e9} ` is 6 bytes but 5 columns; the URL must anchor at
        // column 5, not byte offset 6.
        let snap = snapshot_for_line("caf\u{e9} https://example.com/x");
        let spans = plaintext_spans_for_row(&snap.cells[..snap.width], 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_start, 5);
        assert_eq!(
            plaintext_span_at(&snap, 0, 5)
                .as_ref()
                .map(|s| s.uri.as_str()),
            Some("https://example.com/x")
        );
        assert!(plaintext_span_at(&snap, 0, 4).is_none());
    }

    #[test]
    fn wide_char_lead_and_spacer_both_hit() {
        let snap = snapshot_for_line("\u{4e2d} https://example.com/a");
        let spans = plaintext_spans_for_row(&snap.cells[..snap.width], 0);
        assert_eq!(spans.len(), 1);
        // Wide lead occupies cols 0-1, space at col 2, URL starts at col 3.
        assert_eq!(spans[0].col_start, 3);
        assert!(
            plaintext_span_at(&snap, 0, 3).is_some(),
            "lead column of the URL must hit"
        );
        assert!(
            plaintext_span_at(&snap, 0, 0).is_none(),
            "wide glyph is not a URL"
        );
    }

    #[test]
    fn git_scheme_is_detected_but_not_presented() {
        let snap = snapshot_for_line("clone git://git.example.com/repo.git now");
        // Scanner finds it, but the safety gate rejects it.
        assert!(
            !bitty_url_detector::detect_urls("clone git://git.example.com/repo.git now").is_empty()
        );
        assert!(
            plaintext_spans_for_row(&snap.cells[..snap.width], 0).is_empty(),
            "git:// must stay fail-closed through validate_url"
        );
    }

    #[test]
    fn hostile_payload_never_spans() {
        for line in [
            "see javascript:alert(1) here",
            "go https://example.test/`id` end",
            "x https://example.test/$(id) y",
        ] {
            let snap = snapshot_for_line(line);
            // Either the scanner misses it or the gate rejects it; either
            // way no clickable span survives.
            let spans = plaintext_spans_for_row(&snap.cells[..snap.width], 0);
            for span in &spans {
                assert!(
                    is_safe_plaintext_uri(&span.uri),
                    "unsafe span survived: {span:?}"
                );
            }
        }
    }
}
