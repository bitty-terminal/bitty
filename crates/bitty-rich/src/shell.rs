//! OSC 133 shell integration / semantic zones (bounded, headless-testable).
//!
//! Terminal truth records each `OSC 133` disposition as a [`ZoneRecord`]
//! stamped with a monotonic [`ZoneRecord::ordinal`] and a [`ZoneKind`]
//! (`PromptStart`/`InputStart`/`OutputStart`/`OutputEnd`), bounded at
//! [`ZONE_RECORDS_MAX`] (1024) with oldest eviction. This module interprets
//! that ordered log as optional prompt/command/output regions without ever
//! parsing prompt text (grep-audited: no heuristics exist here).

use bitty_term_state::{State, ZONE_RECORDS_MAX, ZoneKind, ZoneRecord};

/// Re-exported bound (mirrors `bitty-term-state`).
pub const SHELL_ZONE_MAX: usize = ZONE_RECORDS_MAX;

/// One logical command region derived from the ordered zone log.
///
/// A perfect cycle for a shell that emits all four marks is
/// `PromptStart -> InputStart -> OutputStart -> OutputEnd`. Real shells
/// may emit subsets; fields are `Option` to capture partial signals. Each
/// region is bounded by the ordinals that produced it, not by grid rows.
/// `OutputEnd` may carry an exit code (`OSC 133;D;code`).
///
/// Row anchors (M1-18, CTX-0665): each `*_row` resolves the corresponding
/// marker through [`State::zone_buffer_row`] at build time, anchoring the
/// command block to stable scrollback identity for prompt-jump and fold.
/// A row is `None` when its marker no longer resolves (pruned, cleared,
/// resized, or recorded on the other screen) while the ordinal is still
/// retained — ordinal presence and row presence are independent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandRegion {
    /// Ordinal of the `PromptStart` (`A`) that opened this command, if seen.
    pub prompt_start: Option<u64>,
    /// Ordinal of the `InputStart` (`B`) that opened user input, if seen.
    pub input_start: Option<u64>,
    /// Ordinal of the `OutputStart` (`C`) that opened command output, if seen.
    pub output_start: Option<u64>,
    /// Ordinal of the `OutputEnd` (`D`) that closed command output, if seen.
    pub output_end: Option<u64>,
    /// Exit status for the command, if reported via `OSC 133;D;code`.
    pub exit_code: Option<i32>,
    /// Current buffer row of the `PromptStart` marker, if resolvable.
    pub prompt_row: Option<usize>,
    /// Current buffer row of the `InputStart` marker, if resolvable.
    pub input_row: Option<usize>,
    /// Current buffer row of the `OutputStart` marker, if resolvable.
    pub output_row: Option<usize>,
    /// Current buffer row of the `OutputEnd` marker, if resolvable.
    pub output_end_row: Option<usize>,
}

impl CommandRegion {
    /// Whether this region contains at least one zone marker.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prompt_start.is_none()
            && self.input_start.is_none()
            && self.output_start.is_none()
            && self.output_end.is_none()
    }
}

/// Headless shell-integration view over a `State`'s zone log.
///
/// All methods are pure and read only `State::zones`; no mutation or I/O
/// occurs. Output is bounded: at most `zone_count` regions, each at most
/// four markers.
#[derive(Debug, Clone, Copy)]
pub struct ShellIntegration;

impl ShellIntegration {
    /// Returns the ordered zone log oldest first (bounded slice).
    #[must_use]
    pub fn zones(state: &State) -> Vec<ZoneRecord> {
        state.zones().copied().collect()
    }

    /// Number of retained zone records.
    #[must_use]
    pub fn zone_count(state: &State) -> usize {
        state.zone_len()
    }

    /// Most recent zone marker, if any.
    #[must_use]
    pub fn last_zone(state: &State) -> Option<ZoneRecord> {
        state.zones().copied().last()
    }

    /// Filters zones by kind (bounded, headless).
    #[must_use]
    pub fn zones_of_kind(state: &State, kind: ZoneKind) -> Vec<ZoneRecord> {
        state
            .zones()
            .copied()
            .filter(|record| record.kind == kind)
            .collect()
    }

    /// Groups the ordered log into logical command regions.
    ///
    /// Deterministic: same ordinal sequence always yields same grouping.
    /// A new `PromptStart` (`A`) starts a new region; if the previous
    /// region has no `PromptStart` it is still emitted before starting the
    /// next one. Markers that do not start a region (`B`/`C`/`D`) attach
    /// to the current region, or to a fresh implicit region when no region
    /// is open.
    ///
    /// Row anchors resolve at build time through
    /// [`State::zone_buffer_row`]: regions are a snapshot of where each
    /// marker sits *now*, so callers re-query after scroll or prune
    /// rather than caching rows.
    #[must_use]
    pub fn command_regions(state: &State) -> Vec<CommandRegion> {
        /// Working region holding the winning record per slot (first wins,
        /// mirroring the ordinal grouping below).
        struct Work {
            prompt: Option<ZoneRecord>,
            input: Option<ZoneRecord>,
            output: Option<ZoneRecord>,
            output_end: Option<ZoneRecord>,
        }

        impl Work {
            fn is_empty(&self) -> bool {
                self.prompt.is_none()
                    && self.input.is_none()
                    && self.output.is_none()
                    && self.output_end.is_none()
            }

            fn finish(self, state: &State) -> Option<CommandRegion> {
                if self.is_empty() {
                    return None;
                }
                let row = |r: Option<ZoneRecord>| r.and_then(|rec| state.zone_buffer_row(&rec));
                let ord = |r: Option<ZoneRecord>| r.map(|rec| rec.ordinal);
                Some(CommandRegion {
                    prompt_start: ord(self.prompt),
                    input_start: ord(self.input),
                    output_start: ord(self.output),
                    output_end: ord(self.output_end),
                    exit_code: self.output_end.and_then(|rec| rec.exit_code),
                    prompt_row: row(self.prompt),
                    input_row: row(self.input),
                    output_row: row(self.output),
                    output_end_row: row(self.output_end),
                })
            }
        }

        let mut regions: Vec<CommandRegion> = Vec::new();
        let mut current: Option<Work> = None;

        for record in state.zones().copied() {
            match record.kind {
                ZoneKind::PromptStart => {
                    if let Some(work) = current.take() {
                        regions.extend(work.finish(state));
                    }
                    current = Some(Work {
                        prompt: Some(record),
                        input: None,
                        output: None,
                        output_end: None,
                    });
                }
                ZoneKind::InputStart => {
                    let entry = current.get_or_insert(Work {
                        prompt: None,
                        input: None,
                        output: None,
                        output_end: None,
                    });
                    if entry.input.is_none() {
                        entry.input = Some(record);
                    }
                }
                ZoneKind::OutputStart => {
                    let entry = current.get_or_insert(Work {
                        prompt: None,
                        input: None,
                        output: None,
                        output_end: None,
                    });
                    if entry.output.is_none() {
                        entry.output = Some(record);
                    }
                }
                ZoneKind::OutputEnd => {
                    let entry = current.get_or_insert(Work {
                        prompt: None,
                        input: None,
                        output: None,
                        output_end: None,
                    });
                    if entry.output_end.is_none() {
                        entry.output_end = Some(record);
                    }
                }
            }
        }
        if let Some(work) = current {
            regions.extend(work.finish(state));
        }
        regions
    }

    /// Command-block row spans for prompt-jump and fold (M1-18, CTX-0665).
    ///
    /// Returns one `(start, end)` buffer-row span per resolvable
    /// `PromptStart`, oldest first: `start` is the prompt's resolved row
    /// and `end` is the next prompt's row, or the buffer end
    /// (`scrollback_len + height`) for the last block. Unresolvable
    /// prompts contribute no span; an empty prompt log yields no spans.
    /// Bounded (at most one span per retained prompt) and deterministic.
    #[must_use]
    pub fn command_block_rows(state: &State) -> Vec<(usize, usize)> {
        let mut starts: Vec<usize> = state
            .zones()
            .copied()
            .filter(|record| record.kind == ZoneKind::PromptStart)
            .filter_map(|record| state.zone_buffer_row(&record))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let end = state.scrollback_len() + state.height();
        starts
            .iter()
            .enumerate()
            .map(|(i, start)| {
                let stop = starts.get(i + 1).copied().unwrap_or(end);
                (*start, stop.max(*start))
            })
            .collect()
    }

    /// Convenience: prompt markers (`A`) oldest first.
    #[must_use]
    pub fn prompt_starts(state: &State) -> Vec<ZoneRecord> {
        Self::zones_of_kind(state, ZoneKind::PromptStart)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{State, TerminalAction, ZoneKind};

    fn mark(state: &mut State, kind: ZoneKind) {
        state.apply(&TerminalAction::OscPromptMark {
            kind,
            exit_code: None,
        });
    }

    fn mark_with_exit(state: &mut State, code: i32) {
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputEnd,
            exit_code: Some(code),
        });
    }

    #[test]
    fn empty_state_has_no_zones() {
        let state = State::new();
        assert_eq!(ShellIntegration::zone_count(&state), 0);
        assert!(ShellIntegration::zones(&state).is_empty());
        assert!(ShellIntegration::command_regions(&state).is_empty());
    }

    #[test]
    fn zones_are_ordered_and_bounded() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::InputStart);
        let zones = ShellIntegration::zones(&state);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].kind, ZoneKind::PromptStart);
        assert_eq!(zones[1].kind, ZoneKind::InputStart);
        assert_eq!(zones[0].ordinal + 1, zones[1].ordinal);
    }

    #[test]
    fn command_regions_group_full_cycle() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::InputStart);
        mark(&mut state, ZoneKind::OutputStart);
        mark(&mut state, ZoneKind::OutputEnd);
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1);
        let region = &regions[0];
        assert!(region.prompt_start.is_some());
        assert!(region.input_start.is_some());
        assert!(region.output_start.is_some());
        assert!(region.output_end.is_some());
    }

    #[test]
    fn multiple_commands_split_on_prompt_start() {
        let mut state = State::new();
        for _ in 0..2 {
            mark(&mut state, ZoneKind::PromptStart);
            mark(&mut state, ZoneKind::InputStart);
            mark(&mut state, ZoneKind::OutputStart);
            mark(&mut state, ZoneKind::OutputEnd);
        }
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 2);
        assert_ne!(regions[0].prompt_start, regions[1].prompt_start);
    }

    #[test]
    fn partial_markers_still_group() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::OutputEnd);
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1);
        assert!(regions[0].prompt_start.is_some());
        assert!(regions[0].output_end.is_some());
        assert!(regions[0].input_start.is_none());
    }

    #[test]
    fn zones_of_kind_filters() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::InputStart);
        let prompts = ShellIntegration::zones_of_kind(&state, ZoneKind::PromptStart);
        assert_eq!(prompts.len(), 2);
    }

    #[test]
    fn bounded_oldest_evicted() {
        let mut state = State::new();
        for _ in 0..ZONE_RECORDS_MAX + 5 {
            mark(&mut state, ZoneKind::PromptStart);
        }
        assert_eq!(ShellIntegration::zone_count(&state), ZONE_RECORDS_MAX);
        let zones = ShellIntegration::zones(&state);
        // First 5 ordinals evicted, so oldest retained is 6.
        assert_eq!(zones.first().unwrap().ordinal, 6);
    }

    #[test]
    fn exit_code_captured_on_output_end() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark_with_exit(&mut state, 42);
        let zones = ShellIntegration::zones(&state);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[1].kind, ZoneKind::OutputEnd);
        assert_eq!(zones[1].exit_code, Some(42));
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].exit_code, Some(42));
    }

    #[test]
    fn prompt_start_exit_code_always_none() {
        let mut state = State::new();
        // Even if we try to set exit_code on PromptStart, State discards it.
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::PromptStart,
            exit_code: Some(99),
        });
        let zones = ShellIntegration::zones(&state);
        assert_eq!(zones[0].exit_code, None);
    }

    // M1-18 row anchors (CTX-0665).
    use bitty_vt::{ControlChar, GraphemeCell};

    fn feed_shell_line(state: &mut State, text: &str) {
        for ch in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    #[test]
    fn regions_carry_live_row_anchors() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        for ch in "cmd".chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
        mark(&mut state, ZoneKind::InputStart);
        mark(&mut state, ZoneKind::OutputStart);
        mark_with_exit(&mut state, 3);
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1);
        let region = &regions[0];
        assert!(region.prompt_start.is_some());
        assert_eq!(region.exit_code, Some(3));
        // All four marks landed on the live cursor row: rows resolve and
        // agree with each other.
        let rows = [
            region.prompt_row,
            region.input_row,
            region.output_row,
            region.output_end_row,
        ];
        assert!(rows.iter().all(|r| r.is_some()), "rows: {rows:?}");
        assert!(rows.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn regions_track_prompts_into_scrollback() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        feed_shell_line(&mut state, "first command");
        mark(&mut state, ZoneKind::PromptStart);
        feed_shell_line(&mut state, "second command");
        let h = state.height();
        for i in 0..(h + 2) {
            feed_shell_line(&mut state, &format!("filler{i:02}"));
        }
        assert!(state.scrollback_len() > 0);
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 2);
        let first = regions[0].prompt_row.expect("first row");
        let second = regions[1].prompt_row.expect("second row");
        assert!(first < state.scrollback_len(), "first prompt is history");
        assert!(first < second, "prompts stay ordered");
        // Block spans partition the buffer in order for jump/fold.
        let blocks = ShellIntegration::command_block_rows(&state);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, first);
        assert_eq!(blocks[0].1, second);
        assert_eq!(blocks[1].0, second);
        assert_eq!(blocks[1].1, state.scrollback_len() + state.height());
        for pair in blocks.windows(2) {
            assert!(pair[0].0 < pair[1].0);
        }
    }

    #[test]
    fn pruned_prompt_keeps_ordinal_but_loses_row() {
        let mut state = State::with_scrollback_lines(2);
        mark(&mut state, ZoneKind::PromptStart);
        feed_shell_line(&mut state, "doomed command");
        let h = state.height();
        for i in 0..(h + 8) {
            feed_shell_line(&mut state, &format!("churn{i:02}"));
        }
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1, "ordinal log still groups");
        assert!(
            regions[0].prompt_start.is_some(),
            "ordinal retained (zone cap)"
        );
        assert_eq!(
            regions[0].prompt_row, None,
            "pruned line resolves to no row"
        );
        assert!(
            ShellIntegration::command_block_rows(&state).is_empty(),
            "no span without a resolvable prompt"
        );
    }

    #[test]
    fn empty_log_has_no_regions_or_blocks() {
        let state = State::new();
        assert!(ShellIntegration::command_regions(&state).is_empty());
        assert!(ShellIntegration::command_block_rows(&state).is_empty());
    }
}
