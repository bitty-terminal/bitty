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
/// A future RFC may add row anchoring; this draft keeps the headless log
/// view and groups by ordinal sequence only.
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
    #[must_use]
    pub fn command_regions(state: &State) -> Vec<CommandRegion> {
        let mut regions: Vec<CommandRegion> = Vec::new();
        let mut current: Option<CommandRegion> = None;

        for record in state.zones().copied() {
            match record.kind {
                ZoneKind::PromptStart => {
                    if let Some(region) = current.take() {
                        if !region.is_empty() {
                            regions.push(region);
                        }
                    }
                    current = Some(CommandRegion {
                        prompt_start: Some(record.ordinal),
                        input_start: None,
                        output_start: None,
                        output_end: None,
                        exit_code: None,
                    });
                }
                ZoneKind::InputStart => {
                    let entry = current.get_or_insert(CommandRegion {
                        prompt_start: None,
                        input_start: None,
                        output_start: None,
                        output_end: None,
                        exit_code: None,
                    });
                    if entry.input_start.is_none() {
                        entry.input_start = Some(record.ordinal);
                    }
                }
                ZoneKind::OutputStart => {
                    let entry = current.get_or_insert(CommandRegion {
                        prompt_start: None,
                        input_start: None,
                        output_start: None,
                        output_end: None,
                        exit_code: None,
                    });
                    if entry.output_start.is_none() {
                        entry.output_start = Some(record.ordinal);
                    }
                }
                ZoneKind::OutputEnd => {
                    let entry = current.get_or_insert(CommandRegion {
                        prompt_start: None,
                        input_start: None,
                        output_start: None,
                        output_end: None,
                        exit_code: None,
                    });
                    if entry.output_end.is_none() {
                        entry.output_end = Some(record.ordinal);
                        entry.exit_code = record.exit_code;
                    }
                }
            }
        }
        if let Some(region) = current {
            if !region.is_empty() {
                regions.push(region);
            }
        }
        regions
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
}
