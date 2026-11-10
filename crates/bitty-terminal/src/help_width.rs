//! Rendered help-text width gate (CTX-1788, PR #3109 round-2 F7).
//!
//! The gate measures **rendered** text, never source lines. The per-command
//! help bodies are `\`-continued literals whose indentation is carried by
//! trailing spaces, so a 60-column source line can render 126 columns and a
//! source-line check would pass while the terminal wraps.
//!
//! Every subcommand help source is listed once, in both tiers, so a new
//! command or a new over-wide line fails here instead of shipping. This module
//! is test-only: it adds no runtime behavior and no production API.

use crate::component::component_help_text;
use crate::ctl::ctl_help_text;
use crate::dev::dev_help_text;
use crate::inspect::inspect_help_text;
use crate::list::list_help_text;
use crate::plugin::plugin_help_text;

/// Widest column count a rendered help line may occupy.
///
/// 100 is the documented readability bound for `bitty`'s help: it leaves room
/// for an 80-column terminal plus a scrollback gutter and keeps every command's
/// body on one screen at the default window width.
pub(crate) const HELP_MAX_COLUMNS: usize = 100;

/// Column count of `line`, counting each scalar as one column.
///
/// Help text is ASCII apart from the em dash in each title, which is one
/// column wide, so scalar counting is exact here.
fn columns(line: &str) -> usize {
    line.chars().count()
}

/// `(1-based line number, columns)` for every rendered line over
/// [`HELP_MAX_COLUMNS`].
pub(crate) fn over_wide_lines(text: &str) -> Vec<(usize, usize)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, columns(line)))
        .filter(|(_, width)| *width > HELP_MAX_COLUMNS)
        .collect()
}

/// Widest rendered line, in columns (`0` for empty text).
pub(crate) fn max_columns(text: &str) -> usize {
    text.lines().map(columns).max().unwrap_or(0)
}

/// Panics when `text` renders a line wider than [`HELP_MAX_COLUMNS`].
///
/// The panic names the command and every offending line with its width, so a
/// regression reads as "which line" instead of "help too wide".
pub(crate) fn assert_help_width(label: &str, text: &str) {
    let over = over_wide_lines(text);
    assert!(
        over.is_empty(),
        "{label} renders {} line(s) over {HELP_MAX_COLUMNS} columns: {over:?}",
        over.len()
    );
}

/// One help source under test: label, compact text, verbose text.
fn sources() -> Vec<(&'static str, String, String)> {
    vec![
        ("ctl", ctl_help_text(false), ctl_help_text(true)),
        ("plugin", plugin_help_text(false), plugin_help_text(true)),
        (
            "extension",
            component_help_text(false),
            component_help_text(true),
        ),
        (
            "list",
            list_help_text("list", false),
            list_help_text("list", true),
        ),
        ("dev", dev_help_text(false), dev_help_text(true)),
        ("inspect", inspect_help_text(false), inspect_help_text(true)),
    ]
}

#[test]
fn every_help_tier_fits_the_documented_width() {
    for (label, compact, verbose) in sources() {
        assert_help_width(label, &compact);
        assert_help_width(&format!("{label} -v"), &verbose);
        // The bound must actually be exercised, not just satisfied by short
        // text: every tier is expected to fill at least half the budget.
        assert!(
            max_columns(&compact) > HELP_MAX_COLUMNS / 2,
            "{label} help looks truncated: widest line is {} columns",
            max_columns(&compact)
        );
    }
}

#[test]
fn the_width_gate_rejects_an_over_wide_line() {
    // Proves the gate can fail: without this, a green suite proves nothing
    // about the 100-column rule.
    let exactly_at_bound = "x".repeat(HELP_MAX_COLUMNS);
    assert!(over_wide_lines(&exactly_at_bound).is_empty());
    let one_over = "x".repeat(HELP_MAX_COLUMNS + 1);
    assert_eq!(
        over_wide_lines(&format!("ok\n{one_over}\n")),
        vec![(2, HELP_MAX_COLUMNS + 1)]
    );
    let panicked = std::panic::catch_unwind(|| assert_help_width("probe", &one_over)).is_err();
    assert!(
        panicked,
        "assert_help_width must panic on an over-wide line"
    );
}

#[test]
fn verbose_tier_differs_and_carries_the_contract() {
    // F2: `bitty <cmd> --help -v` must not be byte-identical to `--help`, and
    // the extra bytes must be the contract block a script author needs.
    for (label, compact, verbose) in sources() {
        assert_ne!(compact, verbose, "{label}: -v must add the contract tier");
        assert!(
            verbose.starts_with(&compact),
            "{label}: -v must extend the compact body, not replace it"
        );
        let added = &verbose[compact.len()..];
        assert!(
            added.contains("\ncontract:\n"),
            "{label}: -v must open a contract section, got {added:?}"
        );
        assert!(
            added.lines().count() >= 6,
            "{label}: contract tier looks empty: {added:?}"
        );
    }
}

#[test]
fn each_contract_tier_states_its_own_boundary() {
    // One distinguishing sentence per command: the tier is command-specific
    // prose, not a shared boilerplate paragraph.
    let markers = [
        ("ctl", "Consent issuance is never reachable"),
        ("plugin", "Profiles preselect only"),
        (
            "extension",
            "the user tier shadows the read-only system tier",
        ),
        ("list", "Local kinds"),
        ("dev", "`dev-tools` build feature"),
        ("inspect", "Read-only by construction"),
    ];
    for (label, compact, verbose) in sources() {
        let marker = markers
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, marker)| *marker)
            .expect("every source has a marker");
        assert!(
            verbose.contains(marker),
            "{label}: contract tier must state {marker:?}"
        );
        assert!(
            !compact.contains(marker),
            "{label}: {marker:?} belongs to the -v tier only"
        );
    }
}
