//! Bounded scrollback search (CTX-0060).
//!
//! This module provides headless, deterministic search over the scrollback
//! buffer and the live grid. Search is a pure function of `(State, pattern,
//! options)`: no I/O, no wall-clock, no platform variance. It is bounded
//! (T-01) on pattern length and result count, and never panics on empty or
//! truncated inputs.

#![forbid(unsafe_code)]

use crate::cell::Cell;
use crate::state::State;

/// Maximum bytes for a search pattern before truncation (char-boundary
/// preserved). Mirrors clipboard and cold-queue bounds philosophy (T-01).
pub const SEARCH_MAX_PATTERN_LEN: usize = 256;

/// Hard cap on returned matches per call (bounded heap). `SearchOptions::max_results`
/// is clamped to this limit.
pub const SEARCH_MAX_RESULTS: usize = 1000;

/// Options controlling a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// When `false`, ASCII case-insensitive matching is performed (`A-Za-z` folding).
    /// Unicode case folding beyond ASCII is intentionally not performed to keep
    /// the match offset → column mapping stable (deterministic headless).
    pub case_sensitive: bool,
    /// Maximum matches to return; clamped to [`SEARCH_MAX_RESULTS`].
    pub max_results: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: true,
            max_results: SEARCH_MAX_RESULTS,
        }
    }
}

impl SearchOptions {
    /// Creates options with explicit fields, clamping `max_results` to the hard cap.
    #[must_use]
    pub fn new(case_sensitive: bool, max_results: usize) -> Self {
        Self {
            case_sensitive,
            max_results: max_results.min(SEARCH_MAX_RESULTS),
        }
    }
}

/// One Search match, anchored to the combined scrollback + live buffer.
///
/// The buffer is a linearisation oldest scrollback `0` .. newest scrollback `sb_len-1`
/// followed by live grid rows `sb_len .. sb_len+height-1`. `buffer_row` is the
/// zero-based index in that combined view. `line_id` is `Some` for scrollback
/// lines (stable id assigned at push) and `None` for live grid rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// Combined buffer row index (0 = oldest retained scrollback).
    pub buffer_row: usize,
    /// Scrollback line id when `is_scrollback` else `None`.
    pub line_id: Option<u64>,
    /// Starting cell column (lead column) of the match, inclusive.
    pub col_start: usize,
    /// Ending cell column (lead column of last char + width-1) inclusive.
    pub col_end: usize,
    /// Matched substring (bounded by pattern length).
    pub matched_text: String,
}

impl SearchMatch {
    /// Whether the match lives in scrollback history.
    #[must_use]
    pub fn is_scrollback(&self) -> bool {
        self.line_id.is_some()
    }
}

/// Truncates `pattern` to [`SEARCH_MAX_PATTERN_LEN`] bytes at a char boundary.
///
/// Returns an owned String (maybe truncated). Empty string stays empty.
fn truncate_pattern(pattern: &str) -> String {
    if pattern.len() <= SEARCH_MAX_PATTERN_LEN {
        return pattern.to_string();
    }
    // Find last char boundary within cap.
    let mut end = SEARCH_MAX_PATTERN_LEN;
    while end > 0 && !pattern.is_char_boundary(end) {
        end -= 1;
    }
    pattern[..end].to_string()
}

/// Extracts line text and per-char col mapping from a slice of `Cell`.
///
/// Returns `(text, col_map)` where `text` concatenates glyphs (skipping spacers,
/// `' '` for blanks) and `col_map[i]` is the lead column for `text` char `i`.
/// Wide spacers are never emitted; their leading half's glyph is emitted once.
fn line_text_and_map(cells: &[Cell]) -> (String, Vec<usize>) {
    let mut text = String::with_capacity(cells.len());
    let mut map = Vec::new();
    let mut col = 0usize;
    while col < cells.len() {
        let cell = &cells[col];
        if cell.spacer {
            // Spacer should have been skipped via its lead's width==2 handling.
            // But if we land on a spacer directly (malformed), skip.
            col += 1;
            continue;
        }
        if cell.is_blank() {
            text.push(' ');
        } else {
            text.push(cell.glyph);
        }
        map.push(col);
        if cell.width == 2 {
            // Expect spacer at col+1; advance by 2, but map only for lead.
            // The spacer col is width extension, not a separate char.
            col += 2;
        } else {
            col += 1;
        }
    }
    (text, map)
}

/// Finds all non-overlapping byte offsets of `needle` in `haystack`.
///
/// If `case_sensitive` is false, folding is ASCII only (A-Z -> a-z) to keep
/// offsets stable. Returns byte offsets in the original haystack.
fn find_all_occurrences(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<usize> {
    if needle.is_empty() || haystack.is_empty() {
        return Vec::new();
    }
    let (hay_cmp, needle_cmp): (String, String) = if case_sensitive {
        (haystack.to_string(), needle.to_string())
    } else {
        (haystack.to_ascii_lowercase(), needle.to_ascii_lowercase())
    };
    let mut offsets = Vec::new();
    let mut start = 0usize;
    while start <= hay_cmp.len() {
        if let Some(rel) = hay_cmp[start..].find(&needle_cmp) {
            let byte_off = start + rel;
            // Map byte offset in lowercased version to original byte offset.
            // For ASCII case folding, byte lengths are identical, so this is exact.
            // For Unicode folding that changes length we avoid folding, so identical.
            offsets.push(byte_off);
            start = byte_off + needle_cmp.len();
            if start > hay_cmp.len() {
                break;
            }
            // Prevent infinite loop on zero-length needle (already excluded).
        } else {
            break;
        }
        if offsets.len() >= SEARCH_MAX_RESULTS {
            break;
        }
    }
    offsets
}

impl State {
    /// Searches scrollback and live grid for `pattern`.
    ///
    /// The pattern is truncated to [`SEARCH_MAX_PATTERN_LEN`] at a char
    /// boundary before searching. An empty pattern returns no matches.
    /// Results are ordered oldest scrollback first, then live grid top to bottom,
    /// and within a line left to right. At most `options.max_results` matches
    /// are returned, clamped to [`SEARCH_MAX_RESULTS`].
    ///
    /// This is headless, deterministic, and bounded: no allocation beyond
    /// `max_results` * line length, pattern truncated, no I/O.
    #[must_use]
    pub fn search(&self, pattern: &str, options: SearchOptions) -> Vec<SearchMatch> {
        let pat = truncate_pattern(pattern);
        if pat.is_empty() {
            return Vec::new();
        }
        let max_results = options.max_results.min(SEARCH_MAX_RESULTS);
        if max_results == 0 {
            return Vec::new();
        }
        let pat_chars = pat.chars().count();
        if pat_chars == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let sb_len = self.scrollback_len();
        // Scrollback lines
        for idx in 0..sb_len {
            if out.len() >= max_results {
                break;
            }
            if let Some(line) = self.scrollback_line(idx) {
                let (text, map) = line_text_and_map(&line.cells);
                let occ = find_all_occurrences(&text, &pat, options.case_sensitive);
                for byte_off in occ {
                    if out.len() >= max_results {
                        break;
                    }
                    let char_idx = text[..byte_off].chars().count();
                    if char_idx >= map.len() {
                        continue;
                    }
                    let col_start = map[char_idx];
                    // Determine col_end via width of last char in match.
                    let last_char_idx = char_idx + pat_chars - 1;
                    if last_char_idx >= map.len() {
                        continue;
                    }
                    let last_col = map[last_char_idx];
                    // Look up width of last char's cell (lead)
                    let width = if last_col < line.cells.len() {
                        let w = line.cells[last_col].width as usize;
                        if w == 0 { 1 } else { w }
                    } else {
                        1
                    };
                    let col_end = last_col + width - 1;
                    // Extract matched_text as slice of original text (char-correct)
                    let matched_text: String =
                        text.chars().skip(char_idx).take(pat_chars).collect();
                    out.push(SearchMatch {
                        buffer_row: idx,
                        line_id: Some(line.id),
                        col_start,
                        col_end,
                        matched_text,
                    });
                }
            }
        }
        if out.len() >= max_results {
            out.truncate(max_results);
            return out;
        }
        // Live grid rows
        let snap = self.snapshot();
        if snap.width == 0 || snap.height == 0 {
            return out;
        }
        for row in 0..snap.height {
            if out.len() >= max_results {
                break;
            }
            let start = row * snap.width;
            let end = start + snap.width;
            if end > snap.cells.len() {
                break;
            }
            let cells = &snap.cells[start..end];
            let (text, map) = line_text_and_map(cells);
            let occ = find_all_occurrences(&text, &pat, options.case_sensitive);
            for byte_off in occ {
                if out.len() >= max_results {
                    break;
                }
                let char_idx = text[..byte_off].chars().count();
                if char_idx >= map.len() {
                    continue;
                }
                let col_start = map[char_idx];
                let last_char_idx = char_idx + pat_chars - 1;
                if last_char_idx >= map.len() {
                    continue;
                }
                let last_col = map[last_char_idx];
                let width = if last_col < cells.len() {
                    let w = cells[last_col].width as usize;
                    if w == 0 { 1 } else { w }
                } else {
                    1
                };
                let col_end = last_col + width - 1;
                let matched_text: String = text.chars().skip(char_idx).take(pat_chars).collect();
                out.push(SearchMatch {
                    buffer_row: sb_len + row,
                    line_id: None,
                    col_start,
                    col_end,
                    matched_text,
                });
            }
        }
        if out.len() > max_results {
            out.truncate(max_results);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalAction;
    use crate::state::State;
    use bitty_vt::{ControlChar, GraphemeCell};

    fn prints(state: &mut State, text: &str) {
        for c in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
        }
    }

    fn feed_line(state: &mut State, text: &str) {
        prints(state, text);
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    #[test]
    fn search_empty_pattern_returns_empty() {
        let s = State::new();
        let opts = SearchOptions::default();
        assert!(s.search("", opts).is_empty());
        assert!(s.search("   ", SearchOptions::new(true, 0)).is_empty());
    }

    #[test]
    fn search_finds_in_live_grid() {
        let mut s = State::new();
        prints(&mut s, "hello world");
        let m = s.search("world", SearchOptions::default());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].matched_text, "world");
        assert_eq!(m[0].col_start, 6);
        assert_eq!(m[0].line_id, None);
    }

    #[test]
    fn search_finds_in_scrollback() {
        let mut s = State::new();
        for i in 0..(s.height() + 2) {
            feed_line(&mut s, &format!("line{i:02} needle"));
        }
        // At least two scrollback lines with needle
        let m = s.search("needle", SearchOptions::default());
        assert!(m.len() >= 2, "should find needle in scrollback");
        assert!(
            m.iter().any(|mm| mm.is_scrollback()),
            "at least one match should be in scrollback"
        );
        for mm in &m {
            assert_eq!(mm.matched_text, "needle");
        }
        // Buffer rows are ordered
        for w in m.windows(2) {
            assert!(w[0].buffer_row <= w[1].buffer_row);
        }
    }

    #[test]
    fn search_case_sensitive_vs_insensitive() {
        let mut s = State::new();
        prints(&mut s, "Hello");
        let cs = s.search("hello", SearchOptions::new(true, 10));
        assert!(cs.is_empty(), "case sensitive should miss");
        let ci = s.search("hello", SearchOptions::new(false, 10));
        assert_eq!(ci.len(), 1);
        assert_eq!(ci[0].matched_text, "Hello");
    }

    #[test]
    fn search_truncates_long_pattern_and_bounds_results() {
        let mut s = State::new();
        prints(&mut s, "aaa aaa aaa");
        let long = "a".repeat(SEARCH_MAX_PATTERN_LEN + 50);
        let m = s.search(&long, SearchOptions::default());
        // Long pattern truncated but not found (too long vs line)
        assert!(m.is_empty());
        // Bounding: many occurrences but cap
        let mut s2 = State::new();
        // Fill scrollback with many "x" lines
        for _ in 0..10 {
            feed_line(&mut s2, "xxxxxxxxxx");
        }
        prints(&mut s2, "xxxxxxxxxx");
        let m2 = s2.search("x", SearchOptions::new(true, 5));
        assert_eq!(m2.len(), 5);
        assert!(m2.len() <= SEARCH_MAX_RESULTS);
    }

    #[test]
    fn search_wide_char_col_mapping() {
        let mut s = State::new();
        // Row: A 中 B
        prints(&mut s, "A中B");
        let m = s.search("中", SearchOptions::default());
        assert_eq!(m.len(), 1);
        // 中 at col 1, width 2 => col_start 1 col_end 2
        assert_eq!(m[0].col_start, 1);
        assert_eq!(m[0].col_end, 2);
        let m2 = s.search("A中", SearchOptions::default());
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].col_start, 0);
        assert_eq!(m2[0].col_end, 2);
    }

    #[test]
    fn search_is_deterministic_and_headless() {
        let mut a = State::new();
        let mut b = State::new();
        for s in [&mut a, &mut b] {
            feed_line(s, "deterministic needle");
            prints(s, "live needle here");
        }
        let opts = SearchOptions::new(false, 100);
        let ma = a.search("needle", opts);
        let mb = b.search("needle", opts);
        assert_eq!(ma, mb);
    }
}
