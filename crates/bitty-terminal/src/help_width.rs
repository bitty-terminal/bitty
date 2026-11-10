//! Rendered help-text width gate (CTX-1788, PR #3109 round-2 F7).
//!
//! The gate measures **rendered** text, never source lines. The per-command
//! help bodies are `\`-continued literals whose indentation is carried by
//! trailing spaces, so a 60-column source line can render 126 columns and a
//! source-line check would pass while the terminal wraps.
//!
//! Every subcommand in [`crate::cli::SUBCOMMANDS`] is enumerated here, in both
//! tiers: a new command fails this gate until it has a registered source, and
//! an over-wide line fails with its label, line number, and width. This module
//! is test-only: it adds no runtime behavior and no production API.

use crate::cli::{SUBCOMMANDS, help_help_text};

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

/// One help surface under test.
struct HelpSurface {
    /// Canonical subcommand name, as listed in [`SUBCOMMANDS`].
    name: &'static str,
    /// Default (`bitty <cmd> --help`) text.
    compact: String,
    /// `-v` text; `Some` only for the commands that implement the F2 tier.
    verbose: Option<String>,
}

/// Compact and `-v` help text for one canonical subcommand.
///
/// `None` means "no help source registered". [`sources`] turns that into a
/// panic naming the command, so adding a subcommand to [`SUBCOMMANDS`] without
/// registering its help fails the gate instead of silently narrowing it.
fn help_text_for(name: &str) -> Option<(String, Option<String>)> {
    let tiers = |compact: String, verbose: String| Some((compact, Some(verbose)));
    let flat = |text: String| Some((text, None));
    match name {
        "run" => flat(crate::run::run_help_text()),
        "ctl" => tiers(
            crate::ctl::ctl_help_text(false),
            crate::ctl::ctl_help_text(true),
        ),
        "config" => flat(crate::config_cli::config_usage()),
        "init" => flat(crate::init::init_usage()),
        "doctor" => flat(crate::doctor::doctor_usage()),
        "upgrade" => flat(crate::upgrade::upgrade_usage()),
        "reload" => flat(crate::reload::reload_help_text()),
        "list" => tiers(
            crate::list::list_help_text("list", false),
            crate::list::list_help_text("list", true),
        ),
        "inspect" => tiers(
            crate::inspect::inspect_help_text(false),
            crate::inspect::inspect_help_text(true),
        ),
        "dev" => tiers(
            crate::dev::dev_help_text(false),
            crate::dev::dev_help_text(true),
        ),
        "plugin" => tiers(
            crate::plugin::plugin_help_text(false),
            crate::plugin::plugin_help_text(true),
        ),
        "extension" => tiers(
            crate::component::component_help_text(false),
            crate::component::component_help_text(true),
        ),
        "version" => flat(crate::version::version_help_text()),
        "completion" => flat(crate::completion::completion_help_text("completion")),
        "cmd" => flat(crate::cmd::cmd_help_text()),
        // The bundled catalog, not the store-enumerating variant: the gate must
        // not depend on the developer's installed plugins. `x.rs` has its own
        // test for a synthetic over-long store row.
        "x" => flat(crate::x::x_help_text()),
        "shell-init" => flat(crate::shell_init::shell_init_help_text()),
        "help" => flat(help_help_text()),
        _ => None,
    }
}

/// Every help surface, driven by the CLI's own subcommand table.
fn sources() -> Vec<HelpSurface> {
    SUBCOMMANDS
        .iter()
        .map(|(name, _aliases)| {
            let (compact, verbose) = help_text_for(name).unwrap_or_else(|| {
                panic!("help_width has no source for `{name}`: register it in help_text_for")
            });
            HelpSurface {
                name,
                compact,
                verbose,
            }
        })
        .collect()
}

#[test]
fn every_help_tier_fits_the_documented_width() {
    // Collect every offender before failing: one run must name every over-wide
    // surface, not just the first, or fixing them becomes a guessing loop.
    let mut offenders: Vec<String> = Vec::new();
    for surface in sources() {
        assert!(
            max_columns(&surface.compact) > HELP_MAX_COLUMNS / 2,
            "{} help looks truncated: widest line is {} columns",
            surface.name,
            max_columns(&surface.compact)
        );
        for (label, text) in [
            (surface.name.to_string(), surface.compact.as_str()),
            (
                format!("{} -v", surface.name),
                surface.verbose.as_deref().unwrap_or(""),
            ),
        ] {
            let over = over_wide_lines(text);
            if !over.is_empty() {
                offenders.push(format!(
                    "{label} renders {} line(s) over {HELP_MAX_COLUMNS} columns: {over:?}",
                    over.len()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "help surfaces over {HELP_MAX_COLUMNS} columns:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_subcommand_has_a_help_source() {
    // F7: the gate must cover the CLI, not a hand-picked subset. Driving
    // `sources()` from SUBCOMMANDS is only honest if every entry resolves, so
    // assert the enumeration is complete and one-to-one.
    let surfaces = sources();
    let names: Vec<&str> = surfaces.iter().map(|surface| surface.name).collect();
    let expected: Vec<&str> = SUBCOMMANDS.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names, expected,
        "help_width must enumerate every SUBCOMMANDS entry, in order"
    );
    for surface in &surfaces {
        assert!(
            !surface.compact.trim().is_empty(),
            "{}: registered help source is empty",
            surface.name
        );
    }
}

#[test]
fn the_verbose_tier_covers_exactly_the_documented_commands() {
    // F2 landed a `-v` tier for six commands. This pins the set so a silently
    // dropped (or silently added) tier is visible here rather than in a diff.
    let mut with_tier: Vec<&str> = sources()
        .iter()
        .filter(|surface| surface.verbose.is_some())
        .map(|surface| surface.name)
        .collect();
    with_tier.sort_unstable();
    assert_eq!(
        with_tier,
        ["ctl", "dev", "extension", "inspect", "list", "plugin"]
    );
}

#[test]
fn the_wrap_budget_matches_the_gate() {
    // Production builders wrap to `cli::HELP_WRAP_COLUMNS`; the gate measures
    // against its own constant. They must not drift apart.
    assert_eq!(crate::cli::HELP_WRAP_COLUMNS, HELP_MAX_COLUMNS);
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
    for surface in sources().into_iter().filter(|s| s.verbose.is_some()) {
        let label = surface.name;
        let compact = &surface.compact;
        let verbose = surface.verbose.as_deref().expect("filtered to Some");
        assert_ne!(compact, verbose, "{label}: -v must add the contract tier");
        assert!(
            verbose.starts_with(compact),
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
    for surface in sources().into_iter().filter(|s| s.verbose.is_some()) {
        let verbose = surface.verbose.as_deref().expect("filtered to Some");
        let marker = markers
            .iter()
            .find(|(name, _)| *name == surface.name)
            .map(|(_, marker)| *marker)
            .expect("every verbose source has a marker");
        assert!(
            verbose.contains(marker),
            "{}: contract tier must state {marker:?}",
            surface.name
        );
        assert!(
            !surface.compact.contains(marker),
            "{}: {marker:?} belongs to the -v tier only",
            surface.name
        );
    }
}
