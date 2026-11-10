//! Rendered help-text width gate (CTX-1788, PR #3109 round-2 F7).
//!
//! The gate measures **rendered** text, never source lines. The per-command
//! help bodies are `\`-continued literals whose indentation is carried by
//! trailing spaces, so a 60-column source line can render 126 columns and a
//! source-line check would pass while the terminal wraps.
//!
//! Coverage is derived from the CLI's own command table
//! ([`crate::cli::SUBCOMMANDS`]), never from a hand-picked list: every
//! subcommand the parser accepts must have a surface registered here, so a
//! new command fails this gate until its help is measured. The alias table is
//! checked too, so `bitty ls --help` is the same surface as `bitty list`.
//!
//! This module is test-only: it adds no runtime behavior and no production
//! API.

use crate::cli::SUBCOMMANDS;
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

/// The render-time wrap budget must stay inside the measured bound: text that
/// is wrapped by `cli::push_wrapped_line` (catalog rows, interpolated command
/// lists) has to pass the same gate as the hand-wrapped bodies.
const _: () = assert!(
    crate::cli::HELP_WRAP_COLUMNS <= HELP_MAX_COLUMNS,
    "cli::HELP_WRAP_COLUMNS must not exceed help_width::HELP_MAX_COLUMNS"
);

/// One rendered help surface: label, compact (`--help`) text, and the
/// optional `-v` tier.
///
/// `verbose` is `None` for the subcommands that ship no second tier, so the
/// verbose assertions cover exactly the commands that have one instead of
/// silently assuming every command has two.
struct HelpSurface {
    label: &'static str,
    compact: String,
    verbose: Option<String>,
}

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

/// Rendered help for one canonical subcommand name.
///
/// The argument is a name from [`SUBCOMMANDS`]; `None` means the surface was
/// never registered, which [`sources`] turns into a hard failure rather than a
/// silently skipped command.
fn surface(name: &'static str) -> Option<HelpSurface> {
    let (compact, verbose) = match name {
        "run" => (crate::run::run_help_text(), None),
        "ctl" => (ctl_help_text(false), Some(ctl_help_text(true))),
        "config" => (crate::config_cli::config_usage(), None),
        "init" => (crate::init::init_usage(), None),
        "doctor" => (crate::doctor::doctor_usage(), None),
        "upgrade" => (crate::upgrade::upgrade_usage(), None),
        "reload" => (crate::reload::reload_help_text(), None),
        "list" => (
            list_help_text("list", false),
            Some(list_help_text("list", true)),
        ),
        "inspect" => (inspect_help_text(false), Some(inspect_help_text(true))),
        "dev" => (dev_help_text(false), Some(dev_help_text(true))),
        "plugin" => (plugin_help_text(false), Some(plugin_help_text(true))),
        "extension" => (component_help_text(false), Some(component_help_text(true))),
        "version" => (crate::version::version_help_text(), None),
        "completion" => (crate::completion::completion_help_text("completion"), None),
        "cmd" => (crate::cmd::cmd_help_text(), None),
        "x" => (crate::x::x_help_text(), None),
        "shell-init" => (crate::shell_init::shell_init_help_text(), None),
        "help" => (crate::cli::help_help_text(), None),
        _ => return None,
    };
    Some(HelpSurface {
        label: name,
        compact,
        verbose,
    })
}

/// Every subcommand surface, in [`SUBCOMMANDS`] order.
///
/// Panics when a table entry has no registered surface: the gate must fail on
/// a command it does not measure, not quietly measure the rest.
fn sources() -> Vec<HelpSurface> {
    SUBCOMMANDS
        .iter()
        .map(|(name, _)| {
            surface(name).unwrap_or_else(|| {
                panic!("help_width: no rendered help surface registered for `bitty {name}`")
            })
        })
        .collect()
}

#[test]
fn every_subcommand_help_surface_is_covered() {
    // The coverage claim is derived from the CLI's own table, so this fails the
    // moment a subcommand is added without a surface (the panic in `sources`),
    // and it names the count so a table that silently shrinks is visible too.
    let covered = sources();
    assert_eq!(
        covered.len(),
        SUBCOMMANDS.len(),
        "every SUBCOMMANDS entry must be measured"
    );
    // Aliases must resolve to a covered canonical name, so `bitty ls --help`
    // cannot reach an unmeasured surface.
    for (name, aliases) in SUBCOMMANDS {
        for alias in *aliases {
            let canonical = crate::cli::help_canonical_cmd(alias)
                .unwrap_or_else(|| panic!("alias {alias:?} does not resolve"));
            assert_eq!(
                canonical, *name,
                "alias {alias:?} must resolve to its table entry"
            );
            assert!(
                surface(canonical).is_some(),
                "alias {alias:?} resolves to an unmeasured surface {canonical:?}"
            );
        }
    }
    // The hidden `component` alias is parser-only but still renders the
    // extension help, so it must map onto a measured surface as well.
    assert_eq!(
        crate::cli::help_canonical_cmd("component"),
        Some("extension"),
        "the hidden `component` alias renders the extension surface"
    );
}

#[test]
fn every_help_tier_fits_the_documented_width() {
    for surface in sources() {
        assert_help_width(surface.label, &surface.compact);
        if let Some(verbose) = surface.verbose.as_deref() {
            assert_help_width(&format!("{} -v", surface.label), verbose);
        }
        // The bound must actually be exercised, not just satisfied by short
        // text: every tier is expected to fill at least half the budget.
        assert!(
            max_columns(&surface.compact) > HELP_MAX_COLUMNS / 2,
            "{} help looks truncated: widest line is {} columns",
            surface.label,
            max_columns(&surface.compact)
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
    // the extra bytes must be the contract block a script author needs. Only
    // the commands that ship a `-v` tier are in scope; the rest are disclosed
    // in the PR body rather than silently assumed to have one.
    let mut tiers = 0usize;
    for surface in sources() {
        let Some(verbose) = surface.verbose.as_deref() else {
            continue;
        };
        tiers += 1;
        let compact = &surface.compact;
        assert_ne!(
            compact, verbose,
            "{}: -v must add the contract tier",
            surface.label
        );
        assert!(
            verbose.starts_with(compact),
            "{}: -v must extend the compact body, not replace it",
            surface.label
        );
        let added = &verbose[compact.len()..];
        assert!(
            added.contains("\ncontract:\n"),
            "{}: -v must open a contract section, got {added:?}",
            surface.label
        );
        assert!(
            added.lines().count() >= 6,
            "{}: contract tier looks empty: {added:?}",
            surface.label
        );
    }
    assert_eq!(tiers, 6, "six subcommands ship the documented -v tier");
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
    let surfaces = sources();
    for (label, marker) in markers {
        let surface = surfaces
            .iter()
            .find(|surface| surface.label == label)
            .unwrap_or_else(|| panic!("{label}: surface missing"));
        let verbose = surface
            .verbose
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: -v tier missing"));
        assert!(
            verbose.contains(marker),
            "{label}: contract tier must state {marker:?}"
        );
        assert!(
            !surface.compact.contains(marker),
            "{label}: {marker:?} belongs to the -v tier only"
        );
    }
}

#[test]
fn the_x_catalog_wraps_at_the_documented_width() {
    // `bitty x --help` lists installed plugins and their commands. The row used
    // to be one unbounded line (`id (version) — cmd, cmd, ...`), which rendered
    // 455 columns in a store with many commands. A synthetic wide row proves
    // the wrapping is real and that the catalog is inside this gate.
    let row = crate::x::XPluginRow {
        id: "bitty-featured.catalogue".to_string(),
        short: "catalogue".to_string(),
        version: "0.0.1".to_string(),
        commands: (0..40)
            .map(|index| format!("catalogue-command-{index:02}"))
            .collect(),
    };
    let text = crate::x::x_help_text_for(std::slice::from_ref(&row));
    assert_help_width("x (wide catalog row)", &text);
    // The wrapping must not drop or rename a command.
    for command in &row.commands {
        assert!(
            text.contains(command.as_str()),
            "wrapped catalog lost command {command:?}"
        );
    }
}
