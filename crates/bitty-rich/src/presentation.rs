//! Aggregated rich presentation view (headless, bounded).
//!
//! This module composes the four rich concerns — hyperlinks (`OSC 8`),
//! clipboard (`OSC 52`), shell integration (`OSC 133`), and kitty graphics
//! stub — into a single owned snapshot that can be driven headlessly from
//! `State` + `Snapshot` without touching GPU, clipboard, or the parser path.

use bitty_term_state::{Snapshot, State};

use crate::RectPx;
use crate::geometry::CellMetrics;
use crate::hyperlink::{HyperlinkSpan, hyperlink_overlay_rects, hyperlink_spans};
use crate::kitty::KittyGraphicsStub;
use crate::shell::{CommandRegion, ShellIntegration};

/// An owned, headless rich presentation derived from one `Snapshot`.
///
/// Bounded: hyperlink spans ≤ grid cells (≤ 1920), shell regions ≤ zone
/// count (≤ 1024), clipboard history ≤ 16, kitty placeholders ≤ 64.
/// Deterministic for fixed `(state, snapshot, metrics, stub)` inputs.
#[derive(Debug, Clone)]
pub struct RichPresentation {
    /// Snapshot generation this presentation was derived from.
    pub generation: u64,
    /// Grid dimensions of the snapshot.
    pub width: usize,
    /// Grid dimensions of the snapshot.
    pub height: usize,
    /// Hyperlink spans for the visible grid.
    pub hyperlink_spans: Vec<HyperlinkSpan>,
    /// Headless overlay rects for those spans (one per span).
    pub hyperlink_rects: Vec<RectPx>,
    /// Command regions grouped from the zone log.
    pub command_regions: Vec<CommandRegion>,
    /// Total retained zone count (for instrumentation).
    pub zone_count: usize,
    /// Whether the kitty stub currently holds any placeholders.
    pub kitty_len: usize,
}

impl RichPresentation {
    /// Derives a presentation from `state` + `snapshot` + `metrics` + `kitty`.
    ///
    /// Pure, headless, and allocation-bounded: every vector is bounded as
    /// documented in the crate root. No I/O, no unsafe, no wall-clock.
    #[must_use]
    pub fn from_snapshot(
        state: &State,
        snapshot: &Snapshot,
        metrics: CellMetrics,
        kitty: &KittyGraphicsStub,
    ) -> Self {
        let spans = hyperlink_spans(snapshot, state);
        let rects = hyperlink_overlay_rects(snapshot, state, metrics);
        let regions = ShellIntegration::command_regions(state);
        let zone_count = ShellIntegration::zone_count(state);
        Self {
            generation: snapshot.generation,
            width: snapshot.width,
            height: snapshot.height,
            hyperlink_spans: spans,
            hyperlink_rects: rects,
            command_regions: regions,
            zone_count,
            kitty_len: kitty.len(),
        }
    }

    /// Whether any rich annotation is present (hyperlink, zone, or kitty).
    #[must_use]
    pub fn has_rich_content(&self) -> bool {
        !self.hyperlink_spans.is_empty() || self.zone_count > 0 || self.kitty_len > 0
    }

    /// Number of hyperlink spans.
    #[must_use]
    pub fn hyperlink_span_count(&self) -> usize {
        self.hyperlink_spans.len()
    }

    /// Number of command regions.
    #[must_use]
    pub fn command_region_count(&self) -> usize {
        self.command_regions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::CellMetrics;
    use crate::kitty::KittyGraphicsStub;
    use bitty_term_state::{State, TerminalAction, ZoneKind};
    use bitty_vt::{BoundedString, GraphemeCell, Hyperlink};

    fn print(state: &mut State, s: &str) {
        for ch in s.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    #[test]
    fn empty_presentation_has_no_content() {
        let state = State::new();
        let snap = state.snapshot();
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let kitty = KittyGraphicsStub::new();
        let rich = RichPresentation::from_snapshot(&state, &snap, metrics, &kitty);
        assert!(!rich.has_rich_content());
        assert_eq!(rich.hyperlink_span_count(), 0);
        assert_eq!(rich.command_region_count(), 0);
    }

    #[test]
    fn hyperlink_and_zone_together() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("https://example.dev"),
            }),
        });
        print(&mut state, "hi");
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::PromptStart,
        });
        let snap = state.snapshot();
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let kitty = KittyGraphicsStub::new();
        let rich = RichPresentation::from_snapshot(&state, &snap, metrics, &kitty);
        assert!(rich.has_rich_content());
        assert_eq!(rich.hyperlink_span_count(), 1);
        assert_eq!(rich.command_region_count(), 1);
        assert_eq!(rich.zone_count, 1);
        assert_eq!(rich.hyperlink_rects.len(), 1);
    }

    #[test]
    fn kitty_len_reflected() {
        let state = State::new();
        let snap = state.snapshot();
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let mut kitty = KittyGraphicsStub::new();
        kitty.ingest(b"payload", None);
        let rich = RichPresentation::from_snapshot(&state, &snap, metrics, &kitty);
        assert_eq!(rich.kitty_len, 1);
        assert!(rich.has_rich_content());
    }

    #[test]
    fn deterministic() {
        let mut s1 = State::new();
        let mut s2 = State::new();
        for state in [&mut s1, &mut s2] {
            state.apply(&TerminalAction::OscHyperlink {
                link: Some(Hyperlink {
                    id: None,
                    uri: BoundedString::new("https://example.dev"),
                }),
            });
            print(state, "x");
        }
        let snap1 = s1.snapshot();
        let snap2 = s2.snapshot();
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let kitty = KittyGraphicsStub::new();
        let r1 = RichPresentation::from_snapshot(&s1, &snap1, metrics, &kitty);
        let r2 = RichPresentation::from_snapshot(&s2, &snap2, metrics, &kitty);
        assert_eq!(r1.hyperlink_spans, r2.hyperlink_spans);
        assert_eq!(r1.command_regions, r2.command_regions);
    }
}
