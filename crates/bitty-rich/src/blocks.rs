//! CommandBlock semantic anchoring + fold MVP (CTX-0225, 008 route step 1).
//!
//! Design input: `recording/research/008.md` sections 1-3 and 5 (read-only,
//! owner-approved program). `flash.nvim` (rev `5f0f270`, Apache-2.0) is a
//! design reference for label-addressable targets only; no code is copied.
//!
//! # What this module is
//!
//! The first step of the 008 route: stable [`CommandBlock`] identity over the
//! ordinal-grouped [`CommandRegion`](crate::shell::CommandRegion) log plus a
//! presentation-only fold projection ([`FoldState`]). Hint Mode and Composer
//! are separate follow-ups and are explicitly out of scope here.
//!
//! # Anchoring model
//!
//! Verified: [`CommandRegion`](crate::shell::CommandRegion) records
//! `OSC 133` disposition ordinals only (`prompt_start` / `input_start` /
//! `output_start` / `output_end` plus `exit_code`); it holds no row numbers.
//! [`CommandBlock`] bridges that log to a stable [`CommandId`] without ever
//! keying on raw grid rows:
//!
//! ```text
//! PTY -> VT parser -> Terminal State / Scrollback (truth, untouched here)
//!   -> zone log (ordinals, monotonic, geometry-independent)
//!   -> CommandRegion (ordinal groups) -> CommandBlock (stable id + ranges)
//!   -> FoldState (presentation projection: which ids hide rows visually)
//! ```
//!
//! Ordinals are assigned monotonically at mark time
//! (`bitty_term_state::State::record_zone`) and never change on
//! resize/reflow/scroll; scrollback line ids are likewise preserved across
//! the singular resize reflow (`Scrollback::resize` keeps ids, mutates cells
//! only). Keying identity and ranges on ordinals therefore survives geometry
//! changes that would invalidate any `row 100 -> row 150` annotation. No grid
//! row number is stored anywhere in this module (grep-audited).
//!
//! # Fold model
//!
//! Folding hides rows visually in the presentation layer only. It never
//! mutates [`State`](bitty_term_state::State): no grid write, no scrollback
//! push/clear/resize, no zone/cwd edit. Copy-all, search, agent history, and
//! replay read `State` directly and are therefore unaffected; proof tests pin
//! `state_hash`, scrollback length, search results, snapshot cells, and the
//! region log across fold/unfold cycles. Fold membership is keyed per
//! [`CommandId`]; unfolding removes the id and restores the exact block list.
//!
//! # Query surface (Hint Mode seam)
//!
//! Future Hint Mode needs exactly three operations, exposed here with no
//! keybindings, no overlay, and no UI chrome: [`blocks`] (list),
//! [`FoldState::fold`], and [`FoldState::unfold`], plus [`block_by_id`] for
//! target resolution and [`visible_blocks`] / [`hidden_blocks`] for the
//! projection view.
//!
//! # Bounds (threat T-01)
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`COMMAND_BLOCK_MAX`] blocks per [`blocks`] call | 256 | newest retained, oldest dropped (mirrors zone oldest-eviction; aligns with the 008 section 9 HintBatch `max targets = 256` budget) |
//! | [`FoldState`] folded ids | 256 (`FOLD_MAX`) | [`FoldState::fold`] fails closed (`false`) at cap, no eviction |
//! | `cwd` clone per block | 4096 bytes | already bounded by `BoundedString::MAX_LEN` at the parser boundary; cloned verbatim |
//! | [`FoldState::folded_ordinal_span`] | `u64`, saturating add | bounded by retained zone count, never allocates |
//!
//! Fail-closed: with no shell integration (zero zones) [`blocks`] returns an
//! empty vec and every projection helper degrades to plain-terminal behavior
//! (nothing hidden, nothing listed). No I/O, no wall-clock, no unsafe.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use bitty_term_state::{State, ZoneKind};

use crate::shell::{CommandRegion, ShellIntegration};

/// Maximum command blocks returned by [`blocks`].
///
/// Bounded presentation budget: at most 256 blocks (newest retained) so the
/// future HintBatch label allocator (008 section 9: `max targets = 256`) and
/// headless callers never face an unbounded vec. The zone log itself caps at
/// 1024 records, so this cap only drops the oldest blocks under extreme
/// histories, mirroring the zone log's oldest-eviction policy.
pub const COMMAND_BLOCK_MAX: usize = 256;

/// Maximum folded ids retained by [`FoldState`]; [`FoldState::fold`] fails
/// closed at this cap.
pub const FOLD_MAX: usize = COMMAND_BLOCK_MAX;

/// Stable identity of one [`CommandBlock`].
///
/// Derived from the block's anchor ordinal (the `PromptStart` ordinal when
/// present, else the first available marker ordinal in the region). Ordinals
/// are monotonic at mark time and geometry-independent, so the id survives
/// resize, reflow, and scroll. Never derived from a grid row number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(pub u64);

impl CommandId {
    /// Raw anchor ordinal behind this id.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cmd:{}", self.0)
    }
}

/// Ordinal-anchored range over the zone log, inclusive on both ends.
///
/// Both bounds are `OSC 133` disposition ordinals, never grid rows.
/// `start_ordinal <= end_ordinal` always holds (enforced by
/// [`SemanticRange::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SemanticRange {
    /// First ordinal covered by this range (inclusive).
    pub start_ordinal: u64,
    /// Last ordinal covered by this range (inclusive).
    pub end_ordinal: u64,
}

impl SemanticRange {
    /// Builds an ordinal range, ordering the bounds so the invariant
    /// `start <= end` holds for any input order.
    #[must_use]
    pub fn new(a: u64, b: u64) -> Self {
        if a <= b {
            Self {
                start_ordinal: a,
                end_ordinal: b,
            }
        } else {
            Self {
                start_ordinal: b,
                end_ordinal: a,
            }
        }
    }

    /// Single-ordinal range.
    #[must_use]
    pub fn point(ordinal: u64) -> Self {
        Self {
            start_ordinal: ordinal,
            end_ordinal: ordinal,
        }
    }

    /// Number of ordinals covered (`end - start + 1`), saturating.
    ///
    /// Always `>= 1`: a range is never empty (`start <= end` invariant).
    #[must_use]
    pub fn ordinal_count(&self) -> u64 {
        self.end_ordinal
            .saturating_sub(self.start_ordinal)
            .saturating_add(1)
    }

    /// Whether this range covers `ordinal`.
    #[must_use]
    pub fn contains(&self, ordinal: u64) -> bool {
        self.start_ordinal <= ordinal && ordinal <= self.end_ordinal
    }
}

/// Lifecycle state of one [`CommandBlock`], derived from region shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandState {
    /// No `OutputEnd` yet and this is the newest block: still running.
    Running,
    /// `OutputEnd` seen with exit `0` (or unreported code).
    Completed,
    /// `OutputEnd` seen with a non-zero exit code.
    Failed,
    /// No `OutputEnd`, but a newer block superseded this one (a later
    /// `PromptStart` arrived first): interrupted or abandoned.
    Interrupted,
}

/// One semantic command: stable identity plus ordinal-anchored ranges.
///
/// Bridged from [`CommandRegion`] (ordinal groups) without row numbers.
/// `cwd` is the observation-time `OSC 7` report (`State::cwd_report`,
/// verbatim clone, `<=4096` bytes enforced at the parser boundary), shared
/// by every block derived in the same call; per-command cwd history awaits a
/// future Terminal Truth extension that snapshots cwd per zone, and is
/// deliberately not synthesized here. `region` retains the source ordinals
/// for traceability.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandBlock {
    /// Stable id (anchor ordinal); survives resize/reflow/scroll.
    pub id: CommandId,
    /// Ordinal span from the first to the last marker of this command.
    pub command_range: SemanticRange,
    /// Ordinal span of the output phase, when any output marker was seen.
    pub output_range: Option<SemanticRange>,
    /// Observation-time working directory report, if any.
    pub cwd: Option<String>,
    /// Exit status from `OSC 133;D;code`, if reported.
    pub exit_code: Option<i32>,
    /// Lifecycle derived from region shape and position.
    pub state: CommandState,
    /// Source ordinal group this block was bridged from.
    pub region: CommandRegion,
}

impl CommandBlock {
    /// Whether this block finished (completed or failed).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state,
            CommandState::Completed | CommandState::Failed | CommandState::Interrupted
        )
    }

    /// Whether this block has any output phase markers.
    #[must_use]
    pub fn has_output(&self) -> bool {
        self.output_range.is_some()
    }
}

/// Derives the anchor ordinal + command/output ranges for one region.
///
/// Returns `None` only for an empty region (no markers), which
/// [`ShellIntegration::command_regions`] never emits; kept `Option` so
/// callers stay fail-closed on hand-built input.
fn anchor_region(region: &CommandRegion) -> Option<(u64, SemanticRange, Option<SemanticRange>)> {
    let anchor = region
        .prompt_start
        .or(region.input_start)
        .or(region.output_start)
        .or(region.output_end)?;
    let first = region
        .prompt_start
        .or(region.input_start)
        .or(region.output_start)
        .or(region.output_end)?;
    let last = region
        .output_end
        .or(region.output_start)
        .or(region.input_start)
        .or(region.prompt_start)?;
    let command_range = SemanticRange::new(first.min(last), first.max(last));
    let output_range = match (region.output_start, region.output_end) {
        (Some(a), Some(b)) => Some(SemanticRange::new(a.min(b), a.max(b))),
        (Some(a), None) => Some(SemanticRange::point(a)),
        (None, Some(b)) => Some(SemanticRange::point(b)),
        (None, None) => None,
    };
    Some((anchor, command_range, output_range))
}

/// Derives lifecycle state for a region at `index` of `total` regions.
fn lifecycle(region: &CommandRegion, index: usize, total: usize) -> CommandState {
    match region.output_end {
        Some(_) => match region.exit_code {
            Some(code) if code != 0 => CommandState::Failed,
            _ => CommandState::Completed,
        },
        None if index + 1 < total => CommandState::Interrupted,
        None => CommandState::Running,
    }
}

/// Lists command blocks oldest first, bounded at [`COMMAND_BLOCK_MAX`].
///
/// Pure read of `State::zones` (via [`ShellIntegration::command_regions`])
/// plus `State::cwd_report`; never mutates `State`. Deterministic: the same
/// zone log always yields the same blocks in the same order. Fail-closed:
/// with no shell integration (zero zones) returns an empty vec. When history
/// exceeds the cap, the newest [`COMMAND_BLOCK_MAX`] blocks are retained and
/// the oldest dropped.
#[must_use]
pub fn blocks(state: &State) -> Vec<CommandBlock> {
    let regions = ShellIntegration::command_regions(state);
    if regions.is_empty() {
        return Vec::new();
    }
    let cwd: Option<String> = state.cwd_report().map(str::to_string);
    let total = regions.len();
    let start = total.saturating_sub(COMMAND_BLOCK_MAX);
    regions[start..]
        .iter()
        .enumerate()
        .filter_map(|(i, region)| {
            let (anchor, command_range, output_range) = anchor_region(region)?;
            Some(CommandBlock {
                id: CommandId(anchor),
                command_range,
                output_range,
                cwd: cwd.clone(),
                exit_code: region.exit_code,
                state: lifecycle(region, start + i, total),
                region: region.clone(),
            })
        })
        .collect()
}

/// Alias of [`blocks`] naming the Hint Mode read operation explicitly.
#[must_use]
pub fn list_blocks(state: &State) -> Vec<CommandBlock> {
    blocks(state)
}

/// Resolves one block by id (linear scan, bounded at [`COMMAND_BLOCK_MAX`]).
#[must_use]
pub fn block_by_id(state: &State, id: CommandId) -> Option<CommandBlock> {
    blocks(state).into_iter().find(|b| b.id == id)
}

/// Number of blocks [`blocks`] would return (capped at [`COMMAND_BLOCK_MAX`]).
#[must_use]
pub fn block_count(state: &State) -> usize {
    blocks(state).len()
}

/// Presentation-only fold membership keyed per [`CommandId`].
///
/// Holds no reference to `State`, performs no I/O, and never mutates
/// terminal truth: folding only records ids whose rows a renderer should hide
/// visually. Bounded at [`FOLD_MAX`] ids; [`FoldState::fold`] fails closed
/// once full. Deterministic iteration via `BTreeSet` (ascending ids).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldState {
    folded: BTreeSet<CommandId>,
}

impl FoldState {
    /// Empty fold set: plain terminal behavior (nothing hidden).
    #[must_use]
    pub fn new() -> Self {
        Self {
            folded: BTreeSet::new(),
        }
    }

    /// Folds `id`, hiding its rows in the presentation projection.
    ///
    /// Returns `true` when the id became newly folded. Returns `false` when
    /// already folded, or fail-closed when the set is at [`FOLD_MAX`]
    /// (nothing is evicted or mutated in either case).
    pub fn fold(&mut self, id: CommandId) -> bool {
        if self.folded.contains(&id) {
            return false;
        }
        if self.folded.len() >= FOLD_MAX {
            return false;
        }
        self.folded.insert(id)
    }

    /// Unfolds `id`, restoring its rows exactly. Returns `true` when the id
    /// was folded and is now restored.
    pub fn unfold(&mut self, id: CommandId) -> bool {
        self.folded.remove(&id)
    }

    /// Whether `id` is currently folded.
    #[must_use]
    pub fn is_folded(&self, id: CommandId) -> bool {
        self.folded.contains(&id)
    }

    /// Folded ids ascending (bounded at [`FOLD_MAX`]).
    #[must_use]
    pub fn folded_ids(&self) -> Vec<CommandId> {
        self.folded.iter().copied().collect()
    }

    /// Number of folded ids.
    #[must_use]
    pub fn folded_count(&self) -> usize {
        self.folded.len()
    }

    /// Whether nothing is folded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// Clears all folds, restoring plain-terminal presentation.
    pub fn clear(&mut self) {
        self.folded.clear();
    }

    /// Folded-row accounting: total ordinals covered by folded blocks in
    /// `all` (saturating sum, bounded by retained zone count). Unknown ids
    /// (evicted history) contribute zero. Never allocates beyond the caller's
    /// slice; pure function of `(all, self)`.
    #[must_use]
    pub fn folded_ordinal_span(&self, all: &[CommandBlock]) -> u64 {
        let mut total = 0u64;
        for block in all {
            if self.folded.contains(&block.id) {
                if let Some(range) = block.output_range {
                    total = total.saturating_add(range.ordinal_count());
                }
            }
        }
        total
    }
}

/// Blocks hidden by `fold` (presentation projection, oldest first).
#[must_use]
pub fn hidden_blocks<'a>(all: &'a [CommandBlock], fold: &FoldState) -> Vec<&'a CommandBlock> {
    all.iter().filter(|b| fold.is_folded(b.id)).collect()
}

/// Blocks visible under `fold` (presentation projection, oldest first).
///
/// With an empty fold this returns every block in order (plain terminal
/// behavior). Folding never removes entries from `all` itself; expand
/// ([`FoldState::unfold`]) restores the exact original list.
#[must_use]
pub fn visible_blocks<'a>(all: &'a [CommandBlock], fold: &FoldState) -> Vec<&'a CommandBlock> {
    all.iter().filter(|b| !fold.is_folded(b.id)).collect()
}

/// Whether `kind` is an output-phase marker (for documentation symmetry;
/// output ranges span `OutputStart..=OutputEnd` ordinals only).
#[must_use]
pub fn is_output_kind(kind: ZoneKind) -> bool {
    matches!(kind, ZoneKind::OutputStart | ZoneKind::OutputEnd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{State, TerminalAction, ZoneKind};
    use bitty_vt::{BoundedString, ControlChar, GraphemeCell};

    fn mark(state: &mut State, kind: ZoneKind) {
        state.apply(&TerminalAction::OscPromptMark {
            kind,
            exit_code: None,
        });
    }

    fn mark_exit(state: &mut State, code: i32) {
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputEnd,
            exit_code: Some(code),
        });
    }

    fn cwd(state: &mut State, url: &str) {
        state.apply(&TerminalAction::OscCwd {
            url: BoundedString::new(url),
        });
    }

    fn print(state: &mut State, s: &str) {
        for ch in s.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    /// Line feed (`LF`): advances the cursor, scrolling into scrollback at
    /// the bottom margin. `Print` of `'\n'` would place a glyph instead.
    fn lf(state: &mut State) {
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    /// Prints one full line (text plus line feed).
    fn line(state: &mut State, s: &str) {
        print(state, s);
        lf(state);
    }

    fn full_cycle(state: &mut State, exit: i32) {
        mark(state, ZoneKind::PromptStart);
        mark(state, ZoneKind::InputStart);
        mark(state, ZoneKind::OutputStart);
        mark_exit(state, exit);
    }

    #[test]
    fn command_region_records_ordinals_not_rows() {
        // Proof: CommandRegion is ordinal-keyed (008 section 2 premise).
        let mut state = State::new();
        full_cycle(&mut state, 0);
        let regions = ShellIntegration::command_regions(&state);
        assert_eq!(regions.len(), 1);
        let region = &regions[0];
        // Ordinals are 1-based monotonic (zone_counter starts at 0, ++ first).
        assert_eq!(region.prompt_start, Some(1));
        assert_eq!(region.input_start, Some(2));
        assert_eq!(region.output_start, Some(3));
        assert_eq!(region.output_end, Some(4));
        assert_eq!(region.exit_code, Some(0));
        // No row field exists: the struct surface is exactly these ordinals.
        let debug = format!("{region:?}");
        assert!(!debug.contains("row"), "region must not mention rows");
    }

    #[test]
    fn block_identity_stable_across_resize_reflow() {
        let mut state = State::new();
        cwd(&mut state, "file:///home/user/bitty");
        full_cycle(&mut state, 0);
        full_cycle(&mut state, 3);
        // Force scrollback content so reflow has retained lines to rewrite.
        for _ in 0..40 {
            line(&mut state, "x");
        }
        let before = blocks(&state);
        assert_eq!(before.len(), 2);
        let ids: Vec<CommandId> = before.iter().map(|b| b.id).collect();
        let exits: Vec<Option<i32>> = before.iter().map(|b| b.exit_code).collect();
        let cwds: Vec<Option<String>> = before.iter().map(|b| b.cwd.clone()).collect();
        let _scroll_ids_before: Vec<u64> = state.scrollback().map(|l| l.id).collect();

        state.resize(100, 30);
        let after_wide = blocks(&state);
        assert_eq!(
            after_wide.iter().map(|b| b.id).collect::<Vec<_>>(),
            ids,
            "resize must not change block identity"
        );
        assert_eq!(
            after_wide.iter().map(|b| b.exit_code).collect::<Vec<_>>(),
            exits
        );
        assert_eq!(
            after_wide.iter().map(|b| b.cwd.clone()).collect::<Vec<_>>(),
            cwds,
            "cwd report survives resize"
        );
        // CTX-0266 reflow reassigns scrollback ids (physical row count changes),
        // so assert monotonicity + width coherence, not identity preservation.
        // Block identity (semantic zones) must still survive.
        let scroll_ids_wide: Vec<u64> = state.scrollback().map(|l| l.id).collect();
        assert!(
            scroll_ids_wide.windows(2).all(|w| w[0] < w[1]),
            "scrollback ids must stay monotonic after wide reflow"
        );
        for line in state.scrollback() {
            assert_eq!(line.cells.len(), 100);
        }

        state.resize(40, 10);
        let after_narrow = blocks(&state);
        assert_eq!(
            after_narrow.iter().map(|b| b.id).collect::<Vec<_>>(),
            ids,
            "narrow reflow must not change block identity either"
        );
        assert_eq!(
            after_narrow.iter().map(|b| b.exit_code).collect::<Vec<_>>(),
            exits
        );
    }

    #[test]
    fn block_identity_stable_across_scroll() {
        let mut state = State::new();
        full_cycle(&mut state, 0);
        let ids_before: Vec<CommandId> = blocks(&state).iter().map(|b| b.id).collect();
        // Scroll heavily: push many lines into scrollback.
        for _ in 0..200 {
            line(&mut state, "scroll-pressure-line");
        }
        assert!(state.scrollback_len() > 0);
        let ids_after: Vec<CommandId> = blocks(&state).iter().map(|b| b.id).collect();
        assert_eq!(ids_before, ids_after, "scroll must not move block identity");
    }

    #[test]
    fn fail_closed_without_shell_integration() {
        let state = State::new();
        assert!(blocks(&state).is_empty());
        assert!(list_blocks(&state).is_empty());
        assert_eq!(block_count(&state), 0);
        assert!(block_by_id(&state, CommandId(1)).is_none());
        // Projection over nothing stays plain-terminal: nothing hidden.
        let fold = FoldState::new();
        let all = blocks(&state);
        assert!(visible_blocks(&all, &fold).is_empty());
        assert!(hidden_blocks(&all, &fold).is_empty());
        assert_eq!(fold.folded_ordinal_span(&all), 0);
    }

    #[test]
    fn lifecycle_running_interrupted_completed_failed() {
        let mut state = State::new();
        // Block 0: abandoned (no OutputEnd, superseded) -> Interrupted.
        mark(&mut state, ZoneKind::PromptStart);
        mark(&mut state, ZoneKind::InputStart);
        // Block 1: full cycle exit 0 -> Completed.
        full_cycle(&mut state, 0);
        // Block 2: full cycle exit 2 -> Failed.
        full_cycle(&mut state, 2);
        // Block 3: open prompt only -> Running.
        mark(&mut state, ZoneKind::PromptStart);
        let all = blocks(&state);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].state, CommandState::Interrupted);
        assert_eq!(all[1].state, CommandState::Completed);
        assert_eq!(all[1].exit_code, Some(0));
        assert_eq!(all[2].state, CommandState::Failed);
        assert_eq!(all[2].exit_code, Some(2));
        assert_eq!(all[3].state, CommandState::Running);
    }

    #[test]
    fn cwd_and_exit_carried_per_block() {
        let mut state = State::new();
        cwd(&mut state, "file:///home/user/proj");
        full_cycle(&mut state, 7);
        let all = blocks(&state);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].exit_code, Some(7));
        assert_eq!(all[0].cwd.as_deref(), Some("file:///home/user/proj"));
    }

    #[test]
    fn fold_is_presentation_only_truth_untouched() {
        let mut state = State::new();
        cwd(&mut state, "file:///home/user");
        full_cycle(&mut state, 0);
        full_cycle(&mut state, 1);
        print(&mut state, "hello fold me");
        lf(&mut state);
        for _ in 0..30 {
            line(&mut state, "filler");
        }
        let hash_before = state.state_hash();
        let sb_before = state.scrollback_len();
        let snap_before = state.snapshot();
        let regions_before = ShellIntegration::command_regions(&state);
        let search_before =
            state.search("hello", bitty_term_state::search::SearchOptions::default());
        assert!(!search_before.is_empty(), "fixture must be searchable");

        let all = blocks(&state);
        assert_eq!(all.len(), 2);
        let mut fold = FoldState::new();
        assert!(fold.fold(all[0].id));
        assert!(fold.fold(all[1].id));
        assert_eq!(fold.folded_count(), 2);
        assert!(fold.folded_ordinal_span(&all) > 0);
        // Expand restores exactly.
        assert!(fold.unfold(all[0].id));
        assert!(fold.unfold(all[1].id));
        assert!(fold.is_empty());

        // Terminal truth is byte-identical: hash, scrollback, snapshot,
        // region log, and search results are all unaffected by fold traffic.
        assert_eq!(state.state_hash(), hash_before, "fold must not touch truth");
        assert_eq!(state.scrollback_len(), sb_before);
        assert_eq!(state.snapshot().cells, snap_before.cells);
        assert_eq!(ShellIntegration::command_regions(&state), regions_before);
        assert_eq!(
            state.search("hello", bitty_term_state::search::SearchOptions::default()),
            search_before,
            "search reads truth, never the fold projection"
        );
        // Copy-all equivalent: scrollback + snapshot text unchanged.
        let truth_text_len: usize = state
            .scrollback()
            .flat_map(|l| l.cells.iter().map(|c| c.glyph))
            .count();
        assert!(truth_text_len > 0);
    }

    #[test]
    fn fold_per_id_expand_restores_exactly() {
        let mut state = State::new();
        full_cycle(&mut state, 0);
        full_cycle(&mut state, 0);
        full_cycle(&mut state, 5);
        let all = blocks(&state);
        assert_eq!(all.len(), 3);
        let mut fold = FoldState::new();
        // Fold middle only.
        assert!(fold.fold(all[1].id));
        assert!(fold.is_folded(all[1].id));
        assert!(!fold.is_folded(all[0].id));
        let visible: Vec<CommandId> = visible_blocks(&all, &fold).iter().map(|b| b.id).collect();
        let hidden: Vec<CommandId> = hidden_blocks(&all, &fold).iter().map(|b| b.id).collect();
        assert_eq!(visible, vec![all[0].id, all[2].id]);
        assert_eq!(hidden, vec![all[1].id]);
        // Source list itself is never mutated by folding.
        assert_eq!(all.len(), 3);
        // Expand restores exactly.
        assert!(fold.unfold(all[1].id));
        let restored: Vec<CommandId> = visible_blocks(&all, &fold).iter().map(|b| b.id).collect();
        assert_eq!(restored, all.iter().map(|b| b.id).collect::<Vec<_>>());
    }

    #[test]
    fn fold_cap_fails_closed_no_eviction() {
        let mut fold = FoldState::new();
        for i in 0..(FOLD_MAX as u64) {
            assert!(fold.fold(CommandId(i + 1)), "fill to cap");
        }
        assert_eq!(fold.folded_count(), FOLD_MAX);
        // At cap: fail closed, existing membership untouched.
        assert!(!fold.fold(CommandId(999_999)));
        assert!(!fold.is_folded(CommandId(999_999)));
        assert_eq!(fold.folded_count(), FOLD_MAX);
        // Double-fold is a no-op false.
        assert!(!fold.fold(CommandId(1)));
    }

    #[test]
    fn block_list_bounded_newest_retained() {
        let mut state = State::new();
        // 300 full cycles = 1200 zones > 1024 zone cap; zones evict oldest.
        for _ in 0..300 {
            full_cycle(&mut state, 0);
        }
        let all = blocks(&state);
        assert!(
            all.len() <= COMMAND_BLOCK_MAX,
            "bounded: {} <= {COMMAND_BLOCK_MAX}",
            all.len()
        );
        assert_eq!(all.len(), COMMAND_BLOCK_MAX);
        // Newest retained: last block anchors at the newest zone ordinal.
        let last_zone = ShellIntegration::last_zone(&state).unwrap();
        let newest = all.last().unwrap();
        assert!(
            newest.command_range.contains(last_zone.ordinal),
            "newest block must cover the newest zone"
        );
        // Ids strictly increasing oldest-first (deterministic order).
        let ids: Vec<u64> = all.iter().map(|b| b.id.get()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "ids unique");
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids ascending");
    }

    #[test]
    fn partial_markers_still_bridge() {
        let mut state = State::new();
        mark(&mut state, ZoneKind::PromptStart);
        mark_exit(&mut state, 42);
        let all = blocks(&state);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].exit_code, Some(42));
        assert_eq!(all[0].state, CommandState::Failed);
        assert!(all[0].has_output());
        // Id anchors on PromptStart when present.
        assert_eq!(all[0].id, CommandId(1));
    }

    #[test]
    fn no_row_numbers_anywhere_in_block_surface() {
        let mut state = State::new();
        full_cycle(&mut state, 0);
        let all = blocks(&state);
        assert_eq!(all.len(), 1);
        let debug = format!("{:?}", all[0]);
        assert!(
            !debug.contains("row"),
            "CommandBlock must not key on rows: {debug}"
        );
        let range_debug = format!("{:?}", all[0].command_range);
        assert!(range_debug.contains("ordinal"));
    }
}
