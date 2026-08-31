//! OSC 8 hyperlink presentation (bounded, headless-testable).
//!
//! Terminal truth owns the hyperlink table (`bitty_term_state::State`),
//! bounded at [`HYPERLINK_TABLE_MAX`] (1024) per threat T-01; new distinct
//! links beyond the cap degrade to no link. This module does **not** mutate
//! that table. It interprets snapshot cells (each carries an optional
//! [`HyperlinkId`]) against the table to produce spans, hit tests, and
//! headless overlay geometry.

use bitty_platform::{validate_file_url, validate_url};
use bitty_term_state::{HyperlinkId, Snapshot, State};

use crate::geometry::{CellMetrics, RectPx};

/// Re-exported bound from `bitty-term-state`.
pub const HYPERLINK_TABLE_MAX: usize = bitty_term_state::HYPERLINK_TABLE_MAX;

/// Maximum sizes accepted at the hyperlink presentation boundary.
pub const HYPERLINK_URI_MAX: usize = bitty_platform::URL_MAX_LEN;
pub const HYPERLINK_ID_MAX: usize = bitty_vt::BoundedString::MAX_LEN;

/// Validates a terminal-provided URI before it reaches an OS URL handler.
///
/// This intentionally does not normalize or invoke a shell. Exact, lowercase
/// schemes and one-layer percent-encoding checks prevent scheme obfuscation.
#[must_use]
pub fn is_safe_hyperlink_uri(uri: &str) -> bool {
    if uri.starts_with("file:") {
        validate_file_url(uri).is_ok()
    } else {
        validate_url(uri).is_ok()
    }
}

/// Resolved hyperlink target plus its optional `id=` parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HyperlinkInfo {
    /// Opaque hyperlink id; stable for the lifetime of the owning `State`.
    pub hyperlink_id: HyperlinkId,
    /// Optional `id=` parameter from OSC 8 (`id=foo` in `OSC 8 ;id=foo;uri`).
    pub id_param: Option<String>,
    /// Target URI (`https://example.dev`).
    pub uri: String,
}

/// One contiguous hyperlink span on a single row.
///
/// Spans never cross rows and never include wide-character trailing spacers
/// (those are skipped; the leading half already carries the hyperlink). The
/// same uri/id may appear in multiple spans on different rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HyperlinkSpan {
    /// Which hyperlink identity this span belongs to.
    pub hyperlink_id: HyperlinkId,
    /// Target URI.
    pub uri: String,
    /// Optional `id=` parameter.
    pub id_param: Option<String>,
    /// Row index (0-based).
    pub row: usize,
    /// Inclusive start column (leading cell index).
    pub col_start: usize,
    /// Inclusive end column (leading cell index; wide cells occupy one
    /// column in this coordinate, not two).
    pub col_end: usize,
}

/// Resolves `id` against `state`'s hyperlink table; `None` when the id is
/// stale (e.g. capped table degraded to no link, or snapshot from another
/// generation).
#[must_use]
pub fn hyperlink_info(state: &State, id: HyperlinkId) -> Option<HyperlinkInfo> {
    let (id_param, uri) = state.hyperlink_entry(id)?;
    if !is_safe_hyperlink_uri(uri) || id_param.is_some_and(|value| value.len() > HYPERLINK_ID_MAX) {
        return None;
    }
    Some(HyperlinkInfo {
        hyperlink_id: id,
        id_param: id_param.map(ToOwned::to_owned),
        uri: uri.to_owned(),
    })
}

/// Hit-tests `snapshot` at `(row, col)`; returns the hyperlink under the
/// cursor when present, otherwise `None`.
///
/// Returns `None` for out-of-bounds coordinates and for trailing spacer
/// halves (the leading half carries the link; hit-testing the spacer
/// returns the same link as the leading half for ergonomics).
#[must_use]
pub fn hyperlink_at(
    snapshot: &Snapshot,
    state: &State,
    row: usize,
    col: usize,
) -> Option<HyperlinkInfo> {
    if row >= snapshot.height || col >= snapshot.width {
        return None;
    }
    let idx = row.checked_mul(snapshot.width)?.checked_add(col)?;
    let cell = snapshot.cells.get(idx)?;
    if let Some(id) = cell.hyperlink {
        return hyperlink_info(state, id);
    }
    // Ergonomic: if the requested cell is a trailing spacer, report the
    // hyperlink of its leading half (wide char pair). This matches the
    // visual expectation that the whole wide glyph is clickable.
    if cell.spacer && col > 0 {
        let lead_idx = row.checked_mul(snapshot.width)?.checked_add(col - 1)?;
        let lead = snapshot.cells.get(lead_idx)?;
        if lead.width == 2 {
            if let Some(id) = lead.hyperlink {
                return hyperlink_info(state, id);
            }
        }
    }
    None
}

/// Collects contiguous hyperlink spans from `snapshot` against `state`.
///
/// The output is bounded by `snapshot.width * snapshot.height` (max
/// 1920 for 80×24) and preserves row-major order. Wide trailing spacers
/// are not emitted as separate spans.
#[must_use]
pub fn hyperlink_spans(snapshot: &Snapshot, state: &State) -> Vec<HyperlinkSpan> {
    let mut spans = Vec::new();
    for row in 0..snapshot.height {
        let mut col = 0;
        while col < snapshot.width {
            let Some(idx) = row
                .checked_mul(snapshot.width)
                .and_then(|base| base.checked_add(col))
            else {
                break;
            };
            let Some(cell) = snapshot.cells.get(idx) else {
                col += 1;
                continue;
            };
            if cell.spacer {
                col += 1;
                continue;
            }
            let Some(id) = cell.hyperlink else {
                col += 1;
                continue;
            };
            let Some(info) = hyperlink_info(state, id) else {
                col += 1;
                continue;
            };
            let uri = info.uri.as_str();
            let start = col;
            let mut end = col;
            // Extend while the next leading cell carries the same hyperlink id.
            while end.checked_add(1).is_some_and(|next| next < snapshot.width) {
                let next_col = end + 1;
                let Some(next_idx) = row
                    .checked_mul(snapshot.width)
                    .and_then(|base| base.checked_add(next_col))
                else {
                    break;
                };
                let Some(next_cell) = snapshot.cells.get(next_idx) else {
                    break;
                };
                if next_cell.spacer {
                    // Spacer cannot start a new hyperlink; skip it and peek
                    // at the following cell, but do not extend over it
                    // because wide links already span both columns via the
                    // leading half. For deterministic grouping, break after
                    // a spacer — the next leading cell begins a new run.
                    break;
                }
                if next_cell.hyperlink != Some(id) {
                    break;
                }
                // Verify the same uri/id still resolves (defensive; ids are
                // stable but the map could have been capped).
                if state
                    .hyperlink_entry(next_cell.hyperlink.unwrap())
                    .map(|(_, u)| u)
                    != Some(uri)
                {
                    break;
                }
                end += 1;
            }
            spans.push(HyperlinkSpan {
                hyperlink_id: id,
                uri: uri.to_owned(),
                id_param: info.id_param,
                row,
                col_start: start,
                col_end: end,
            });
            let Some(next_col) = end.checked_add(1) else {
                break;
            };
            col = next_col;
        }
    }
    spans
}

/// Headless overlay geometry for hyperlink underlines.
///
/// Each span maps to exactly one [`RectPx`] covering its columns at the
/// row's baseline decoration area. The underline geometry is
/// deliberately simple in this draft (one solid bar per span, height =
/// `cell.height / 8` clamped to 1..2, y = `row*height + height - thickness*2`);
/// a future rich-block RFC may replace this with per-cell style-aware
/// underlines. Callers that already have a `GridRenderer` may prefer to
/// let that layer emit decorations; this helper exists so headless tests
/// can assert span→pixel mapping without pulling `bitty-render`.
#[must_use]
pub fn hyperlink_overlay_rects(
    snapshot: &Snapshot,
    state: &State,
    metrics: CellMetrics,
) -> Vec<RectPx> {
    let spans = hyperlink_spans(snapshot, state);
    let thickness = (metrics.height / 8).clamp(1, 2);
    let mut rects = Vec::with_capacity(spans.len());
    for span in spans {
        let cols = (span.col_end - span.col_start + 1) as u64;
        let width = (cols * u64::from(metrics.width)) as u32;
        let x = (span.col_start as u64 * u64::from(metrics.width)) as i32;
        let y = (span.row as u64 * u64::from(metrics.height)
            + u64::from(metrics.height).saturating_sub(u64::from(thickness) * 2))
            as i32;
        rects.push(RectPx::new(x, y, width, thickness));
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{State, TerminalAction};
    use bitty_vt::{BoundedString, GraphemeCell, Hyperlink};

    fn print(state: &mut State, s: &str) {
        for ch in s.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    #[test]
    fn no_link_when_no_osc8() {
        let mut state = State::new();
        print(&mut state, "hello");
        let snap = state.snapshot();
        assert!(hyperlink_spans(&snap, &state).is_empty());
        assert!(hyperlink_at(&snap, &state, 0, 0).is_none());
    }

    #[test]
    fn single_link_span_grouped() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("https://example.dev"),
            }),
        });
        print(&mut state, "click");
        state.apply(&TerminalAction::OscHyperlink { link: None });
        print(&mut state, " here");

        let snap = state.snapshot();
        let spans = hyperlink_spans(&snap, &state);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].uri, "https://example.dev");
        assert_eq!(spans[0].row, 0);
        assert_eq!(spans[0].col_start, 0);
        assert_eq!(spans[0].col_end, 4);
        assert_eq!(
            hyperlink_at(&snap, &state, 0, 2).unwrap().uri,
            "https://example.dev"
        );
        assert!(hyperlink_at(&snap, &state, 0, 6).is_none());
    }

    #[test]
    fn link_with_id_param() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: Some(BoundedString::new("myid")),
                uri: BoundedString::new("https://example.dev/abc"),
            }),
        });
        print(&mut state, "x");
        let snap = state.snapshot();
        let info = hyperlink_at(&snap, &state, 0, 0).unwrap();
        assert_eq!(info.id_param.as_deref(), Some("myid"));
        assert_eq!(info.uri, "https://example.dev/abc");
    }

    #[test]
    fn wide_char_link_is_single_span() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("https://example.dev"),
            }),
        });
        print(&mut state, "\u{4E2D}"); // wide char occupies 2 columns but 1 logical cell
        let snap = state.snapshot();
        let spans = hyperlink_spans(&snap, &state);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_start, 0);
        assert_eq!(spans[0].col_end, 0); // one logical cell
        // Hit test on trailing spacer should still resolve.
        assert!(hyperlink_at(&snap, &state, 0, 1).is_some());
    }

    #[test]
    fn table_bound_degrades_to_no_link() {
        let mut state = State::new();
        for i in 0..bitty_term_state::HYPERLINK_TABLE_MAX + 5 {
            state.apply(&TerminalAction::OscHyperlink {
                link: Some(Hyperlink {
                    id: Some(BoundedString::new(format!("id{i}"))),
                    uri: BoundedString::new(format!("https://example.dev/{i}")),
                }),
            });
            // Force distinct entries by immediately ending link so next
            // `Print` uses the just-registered id.
            state.apply(&TerminalAction::OscHyperlink { link: None });
        }
        // Now table is at cap; next distinct link should degrade.
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: Some(BoundedString::new("overflow")),
                uri: BoundedString::new("https://overflow.dev"),
            }),
        });
        print(&mut state, "x");
        let snap = state.snapshot();
        // The overflow link was degraded -> cell has no hyperlink.
        assert!(snap.cells[0].hyperlink.is_none() || hyperlink_at(&snap, &state, 0, 0).is_none());
    }

    #[test]
    fn overlay_rects_match_spans() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("https://example.dev"),
            }),
        });
        print(&mut state, "ab");
        let snap = state.snapshot();
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let rects = hyperlink_overlay_rects(&snap, &state, metrics);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].x, 0);
        assert_eq!(rects[0].width, 16);
        assert_eq!(rects[0].height, 2);
    }

    #[test]
    fn out_of_bounds_hit_is_none() {
        let state = State::new();
        let snap = state.snapshot();
        assert!(hyperlink_at(&snap, &state, 99, 0).is_none());
        assert!(hyperlink_at(&snap, &state, 0, 99).is_none());
    }

    #[test]
    fn adversarial_uri_corpus_is_rejected() {
        for uri in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java%73cript:alert(1)",
            "https://example.test/%3Btouch%20/tmp/x",
            "https://example.test;touch /tmp/x",
            "https://example.test/`id`",
            "https://example.test/$(id)",
            "https://example.test/$HOME",
            "https://example.test/\\x",
            "https://example.test/\nnext",
            "file:///tmp/a|b",
            "https://example.test/%",
        ] {
            assert!(!is_safe_hyperlink_uri(uri), "accepted hostile URI: {uri:?}");
        }
    }

    #[test]
    fn supported_uri_corpus_is_accepted() {
        for uri in [
            "https://example.test/path?q=a%20b",
            "http://127.0.0.1:8080/",
            "mailto:user@example.test",
            "file:///tmp/report.txt",
        ] {
            assert!(
                is_safe_hyperlink_uri(uri),
                "rejected supported URI: {uri:?}"
            );
        }
    }

    #[test]
    fn unsafe_table_entries_are_not_presented() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("javascript:alert(1)"),
            }),
        });
        print(&mut state, "x");
        let snapshot = state.snapshot();
        assert!(hyperlink_at(&snapshot, &state, 0, 0).is_none());
        assert!(hyperlink_spans(&snapshot, &state).is_empty());
    }

    #[test]
    fn hyperlink_at_overflow_is_none() {
        let state = State::new();
        let snapshot = Snapshot {
            width: usize::MAX,
            height: usize::MAX,
            cells: Vec::new().into(),
            ..State::new().snapshot()
        };
        assert!(hyperlink_at(&snapshot, &state, usize::MAX, usize::MAX).is_none());
    }
}
