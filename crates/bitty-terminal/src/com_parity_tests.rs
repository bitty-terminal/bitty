//! CLI grammar parity: `bitty.com`'s classifier vs. the real parser (bitty#3224).
//!
//! `crates/bitty-com` decides from argv alone whether an invocation is a console
//! command (wait, return the child's exit code) or a desktop launch (detach).
//! That decision duplicates a slice of [`crate::cli::parse_args`], so this module
//! is the drift gate: one corpus is pushed through both sides and any
//! disagreement fails the suite.
//!
//! # What is compared
//!
//! The expectation is derived from the *real* parse ([`expected_mode`]): an
//! invocation is a console command exactly when the parser recorded something
//! that ends the process without a window (a subcommand word, a `+action`,
//! help/version/headless/test-mode, an unknown flag, or a CLI value error).
//! Because the corpus also pins the expected answer per row, the two
//! implementations cannot drift *together* into a wrong classification.
//!
//! # Scope
//!
//! Only argv is compared, plus the `BITTY_HEADLESS` fallback both sides read.
//! The four flags whose *values* the real binary rejects before any window
//! exists are part of that argv decision, so [`expected_mode`] runs the same
//! validators: `--layout` at the startup layout build, `--focus` at the focus
//! check, and `--font-size`/`--opacity` at the config merge. Failures that
//! depend on the user's config file, profile chain or display environment are
//! file and machine state rather than argv, and stay owned by the real binary.
//! (`--theme` names are resolved against the preset registry, which this
//! dependency-free crate does not duplicate.)

use crate::cli::{Args, SUBCOMMANDS, help_canonical_cmd, parse_args};
use crate::plus::PlusAction;

/// The mode the real parser implies for one parsed invocation.
#[cfg(test)]
fn expected_mode(args: &Args) -> bitty_com::Mode {
    let console_word = args.config_word
        || args.init_word
        || args.doctor_word
        || args.upgrade_word
        || args.run_word
        || args.ctl_word
        || args.reload_word
        || args.list_word
        || args.inspect_word
        || args.dev_word
        || args.plugin_word
        || args.component_word
        || args.version_word
        || args.completion_word
        || args.cmd_word
        || args.x_word
        || args.shell_init_word
        || args.help_word
        || args.show_font
        || args.plus_action.is_some();
    if console_word
        || args.help
        || args.version
        || args.headless
        || args.test_mode
        || args.unknown_flag.is_some()
        || args.cli_value_error.is_some()
        || pre_window_value_error(args)
    {
        bitty_com::Mode::WaitCli
    } else {
        bitty_com::Mode::DetachGui
    }
}

/// True when a value the parser stores without validating is rejected by the
/// startup check the real binary runs before any window exists.
///
/// The parser records `--layout`/`--focus`/`--font-size`/`--opacity` as raw
/// text; the failures happen in `apply_startup_layout_and_focus` (layout build,
/// `is_valid_focus_spec`) and in the config merge
/// (`CliOverrides::validate_appearance_overrides`). This mirrors those call
/// sites, so the corpus expectation comes from real code rather than a
/// hand-written table.
#[cfg(test)]
fn pre_window_value_error(args: &Args) -> bool {
    if crate::layout_cmd::try_build_layout(args, 80, 24).is_err() {
        return true;
    }
    if let Some(spec) = args.focus.as_deref() {
        if !crate::layout_cmd::is_valid_focus_spec(spec) {
            return true;
        }
    }
    if let Some(raw) = args.font_size.as_deref() {
        if cli_font_size_rejected(raw) {
            return true;
        }
    }
    if let Some(raw) = args.opacity.as_deref() {
        if cli_opacity_rejected(raw) {
            return true;
        }
    }
    false
}

/// The `--font-size` check inside
/// `CliOverrides::validate_appearance_overrides`: a blank raw is not an
/// override, everything else must parse as a finite number and validate as
/// `font.size`.
#[cfg(test)]
fn cli_font_size_rejected(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    match trimmed.parse::<f32>() {
        Ok(size) => bitty_config::types::FontConfig {
            family: String::from("cli"),
            size,
            ..Default::default()
        }
        .validate()
        .is_err(),
        Err(_) => true,
    }
}

/// The `--opacity` check inside `validate_appearance_overrides`: a blank raw is
/// not an override, everything else must parse as a finite number and validate
/// as `window.opacity`.
#[cfg(test)]
fn cli_opacity_rejected(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    match trimmed.parse::<f32>() {
        Ok(opacity) => bitty_config::types::WindowConfig {
            opacity,
            ..Default::default()
        }
        .validate()
        .is_err(),
        Err(_) => true,
    }
}

/// One argv tail (everything after `argv[0]`).
#[cfg(test)]
fn classify_tail(tail: &[&str]) -> (bitty_com::Mode, bitty_com::Mode) {
    let mut raw: Vec<String> = Vec::with_capacity(tail.len() + 1);
    raw.push(String::from("bitty"));
    raw.extend(tail.iter().map(|token| String::from(*token)));
    let tail_owned: Vec<String> = tail.iter().map(|token| String::from(*token)).collect();
    let headless_env = bitty_com::headless_requested_by_env();
    // The hidden completion helpers (`+list-profiles`, `+list-plugins
    // --installed`) run before `parse_args` and end the process themselves, so
    // the parser never sees them. They are console commands either way; this is
    // the one pre-parse step the comparison has to model.
    let parsed = if crate::candidates::parse_hidden(&raw).is_some() {
        bitty_com::Mode::WaitCli
    } else {
        expected_mode(&parse_args(&raw))
    };
    (
        parsed,
        bitty_com::classify_with_env(&tail_owned, headless_env),
    )
}

/// (argv tail, expected mode, why). The expected mode is the *intent*; the
/// assertion below also proves the real parser agrees with it.
#[cfg(test)]
const CORPUS: &[(&[&str], bitty_com::Mode, &str)] = &[
    // --- Desktop launches ---------------------------------------------------
    (
        &[],
        bitty_com::Mode::DetachGui,
        "bare launch opens the window",
    ),
    (
        &["--split"],
        bitty_com::Mode::DetachGui,
        "--split with no axis is still a launch",
    ),
    (
        &["--split", "v"],
        bitty_com::Mode::DetachGui,
        "--split swallows its axis",
    ),
    (
        &["--split=h:0.3"],
        bitty_com::Mode::DetachGui,
        "--split= accepts an axis and ratio",
    ),
    (
        &["--split="],
        bitty_com::Mode::DetachGui,
        "an empty --split= falls back to horizontal",
    ),
    (&["--stack"], bitty_com::Mode::DetachGui, "layout request"),
    (&["--overlay"], bitty_com::Mode::DetachGui, "layout request"),
    (
        &["--layout", "single"],
        bitty_com::Mode::DetachGui,
        "--layout takes a value",
    ),
    (
        &["--layout=stack:2"],
        bitty_com::Mode::DetachGui,
        "--layout= takes a value",
    ),
    (
        &["--layout", "stack:2"],
        bitty_com::Mode::DetachGui,
        "--layout accepts a stack count",
    ),
    (
        &["--layout=split:h:0.3"],
        bitty_com::Mode::DetachGui,
        "--layout= accepts a split spec",
    ),
    (
        &["--focus", "next"],
        bitty_com::Mode::DetachGui,
        "--focus takes a value",
    ),
    (
        &["--focus=1"],
        bitty_com::Mode::DetachGui,
        "--focus= takes a numeric id",
    ),
    (
        &["--theme", "dark"],
        bitty_com::Mode::DetachGui,
        "--theme takes a value",
    ),
    (
        &["--theme=dark"],
        bitty_com::Mode::DetachGui,
        "--theme= takes a value",
    ),
    (
        &["--theme", "init"],
        bitty_com::Mode::DetachGui,
        "a swallowed value is never a subcommand word",
    ),
    (
        &["--config", "init"],
        bitty_com::Mode::DetachGui,
        "a swallowed value is never a subcommand word",
    ),
    (
        &["--config", "init.lua"],
        bitty_com::Mode::DetachGui,
        "--config takes a path",
    ),
    (
        &["--profile", "work"],
        bitty_com::Mode::DetachGui,
        "--profile takes a name",
    ),
    (
        &["--font-family", "JetBrains Mono"],
        bitty_com::Mode::DetachGui,
        "--font-family takes a name",
    ),
    (
        &["--font-size", "13"],
        bitty_com::Mode::DetachGui,
        "--font-size takes a size",
    ),
    (
        &["--font-size=12.5"],
        bitty_com::Mode::DetachGui,
        "--font-size= takes a size",
    ),
    (
        &["--opacity", "0.9"],
        bitty_com::Mode::DetachGui,
        "--opacity takes a value",
    ),
    (
        &["--opacity=0.5"],
        bitty_com::Mode::DetachGui,
        "--opacity= takes a value",
    ),
    (&["--safe"], bitty_com::Mode::DetachGui, "safe-mode launch"),
    (&["-v"], bitty_com::Mode::DetachGui, "verbose launch"),
    (&["--verbose"], bitty_com::Mode::DetachGui, "verbose launch"),
    (
        &["--fail-loud"],
        bitty_com::Mode::DetachGui,
        "fail-loud launch",
    ),
    (
        &["--log-level", "debug"],
        bitty_com::Mode::DetachGui,
        "--log-level takes a known level",
    ),
    (
        &["--log-level=warn"],
        bitty_com::Mode::DetachGui,
        "--log-level= takes a known level",
    ),
    (&["--no-color"], bitty_com::Mode::DetachGui, "color toggle"),
    (
        &["--dry-run"],
        bitty_com::Mode::DetachGui,
        "upgrade-only flag",
    ),
    (
        &["--check"],
        bitty_com::Mode::DetachGui,
        "upgrade-only flag",
    ),
    (
        &["--rollback"],
        bitty_com::Mode::DetachGui,
        "upgrade-only flag",
    ),
    (&["--yes"], bitty_com::Mode::DetachGui, "init/upgrade flag"),
    (&["--force"], bitty_com::Mode::DetachGui, "init-only flag"),
    (&["--migrate"], bitty_com::Mode::DetachGui, "init-only flag"),
    (
        &["--no-plugins"],
        bitty_com::Mode::DetachGui,
        "init-only flag",
    ),
    (
        &["--with-components"],
        bitty_com::Mode::DetachGui,
        "init-only flag",
    ),
    (
        &["--no-with-components"],
        bitty_com::Mode::DetachGui,
        "init-only flag",
    ),
    (
        &["--no-components"],
        bitty_com::Mode::DetachGui,
        "init-only flag alias",
    ),
    (
        &["--without-components"],
        bitty_com::Mode::DetachGui,
        "init-only flag alias",
    ),
    (
        &["--print-shell-init"],
        bitty_com::Mode::DetachGui,
        "init-only flag",
    ),
    (
        &["--no-print-shell-init"],
        bitty_com::Mode::DetachGui,
        "init-only flag",
    ),
    (
        &["--no-shell-init"],
        bitty_com::Mode::DetachGui,
        "init-only flag alias",
    ),
    (
        &["--format", "json"],
        bitty_com::Mode::DetachGui,
        "--format takes a shape",
    ),
    (
        &["--format=jsonl"],
        bitty_com::Mode::DetachGui,
        "--format= takes a shape",
    ),
    (
        &["--socket", "/tmp/bitty.sock"],
        bitty_com::Mode::DetachGui,
        "--socket takes a path",
    ),
    (
        &["--instance", "abc123"],
        bitty_com::Mode::DetachGui,
        "--instance takes an id",
    ),
    (
        &["--scrollback", "10000"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--close-confirm", "never"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--gaps-in", "2"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--gaps-out", "2"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--border", "1"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--radius", "4"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--plugins", "a,b"],
        bitty_com::Mode::DetachGui,
        "init value flag",
    ),
    (
        &["--profile"],
        bitty_com::Mode::DetachGui,
        "a missing profile name only warns",
    ),
    (
        &["--theme"],
        bitty_com::Mode::DetachGui,
        "a missing theme only warns",
    ),
    (
        &["--format"],
        bitty_com::Mode::DetachGui,
        "a missing format only warns",
    ),
    (
        &["--split-ratio", "0.5"],
        bitty_com::Mode::DetachGui,
        "finite ratio",
    ),
    (
        &["--split-ratio=0.5"],
        bitty_com::Mode::DetachGui,
        "finite ratio",
    ),
    (
        &["--split-ratio", "-0.5"],
        bitty_com::Mode::DetachGui,
        "a negative ratio is a value",
    ),
    (&["htop"], bitty_com::Mode::DetachGui, "program to spawn"),
    (
        &["--", "htop"],
        bitty_com::Mode::DetachGui,
        "the separator ends the grammar",
    ),
    (
        &["--", "init"],
        bitty_com::Mode::DetachGui,
        "after -- a word is a program",
    ),
    (
        &["htop", "--bogus"],
        bitty_com::Mode::DetachGui,
        "program argv is never ours",
    ),
    (
        &["+not-an-action"],
        bitty_com::Mode::DetachGui,
        "an unknown +token keeps the program-spawn behavior",
    ),
    // --- Console commands ---------------------------------------------------
    (&["init"], bitty_com::Mode::WaitCli, "subcommand word"),
    (
        &["init", "--yes"],
        bitty_com::Mode::WaitCli,
        "subcommand word with flags",
    ),
    (
        &["init", "--force", "--no-plugins"],
        bitty_com::Mode::WaitCli,
        "subcommand word with flags",
    ),
    (&["cfg", "show"], bitty_com::Mode::WaitCli, "alias spelling"),
    (
        &["config", "show"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["ctl", "status"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["run", "--", "htop"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["list", "themes"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["ls", "themes"],
        bitty_com::Mode::WaitCli,
        "alias spelling",
    ),
    (
        &["inspect", "config", "theme"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["dev", "trace"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (&["doctor"], bitty_com::Mode::WaitCli, "subcommand word"),
    (
        &["upgrade", "--check"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (&["reload"], bitty_com::Mode::WaitCli, "subcommand word"),
    (
        &["plugin", "list"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["extension", "list"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["component", "list"],
        bitty_com::Mode::WaitCli,
        "deprecated alias spelling",
    ),
    (&["version"], bitty_com::Mode::WaitCli, "subcommand word"),
    (
        &["completion", "bash"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (&["comp", "zsh"], bitty_com::Mode::WaitCli, "alias spelling"),
    (
        &["cmd", "acme.tool", "run"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["x", "acme.tool", "run"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (
        &["shell-init", "zsh"],
        bitty_com::Mode::WaitCli,
        "subcommand word",
    ),
    (&["help"], bitty_com::Mode::WaitCli, "subcommand word"),
    (
        &["help", "plugin"],
        bitty_com::Mode::WaitCli,
        "subcommand word with a target",
    ),
    (&["+list-themes"], bitty_com::Mode::WaitCli, "+action alias"),
    (&["+show-config"], bitty_com::Mode::WaitCli, "+action alias"),
    (&["+show-font"], bitty_com::Mode::WaitCli, "read-only probe"),
    (
        &["+list-profiles"],
        bitty_com::Mode::WaitCli,
        "hidden completion helper",
    ),
    (
        &["+list-plugins", "--installed"],
        bitty_com::Mode::WaitCli,
        "hidden completion helper",
    ),
    (&["-h"], bitty_com::Mode::WaitCli, "help flag"),
    (&["--help"], bitty_com::Mode::WaitCli, "help flag"),
    (&["-V"], bitty_com::Mode::WaitCli, "version flag"),
    (&["--version"], bitty_com::Mode::WaitCli, "version flag"),
    (&["--headless"], bitty_com::Mode::WaitCli, "headless smoke"),
    (&["--test-mode"], bitty_com::Mode::WaitCli, "E2E servo loop"),
    (
        &["--headless", "--split", "v"],
        bitty_com::Mode::WaitCli,
        "headless wins over layout flags",
    ),
    (
        &["--safe", "doctor"],
        bitty_com::Mode::WaitCli,
        "global flag before the word",
    ),
    (
        &["--split", "v", "init"],
        bitty_com::Mode::WaitCli,
        "a word after layout flags still decides",
    ),
    (
        &["htop", "--headless"],
        bitty_com::Mode::WaitCli,
        "recognised flags stay global after the program",
    ),
    (
        &["htop", "--help"],
        bitty_com::Mode::WaitCli,
        "help wins after the program",
    ),
    // --- Usage errors the shell must see ------------------------------------
    (&["--bogus"], bitty_com::Mode::WaitCli, "unknown flag"),
    (&["--bogus=1"], bitty_com::Mode::WaitCli, "unknown flag"),
    (
        &["--help=x"],
        bitty_com::Mode::WaitCli,
        "a boolean flag owns no = form",
    ),
    (&["--json"], bitty_com::Mode::WaitCli, "config-only flag"),
    (&["--fix"], bitty_com::Mode::WaitCli, "doctor-only flag"),
    (
        &["--backup"],
        bitty_com::Mode::WaitCli,
        "config-migrate-only flag",
    ),
    (
        &["--no-backup"],
        bitty_com::Mode::WaitCli,
        "config-migrate-only flag",
    ),
    (
        &["--from", "x"],
        bitty_com::Mode::WaitCli,
        "profile-only flag",
    ),
    (
        &["--preset", "x"],
        bitty_com::Mode::WaitCli,
        "profile-only flag",
    ),
    (
        &["--log-level"],
        bitty_com::Mode::WaitCli,
        "a missing level is a usage error",
    ),
    (
        &["--log-level=bogus"],
        bitty_com::Mode::WaitCli,
        "an unknown level is a usage error",
    ),
    (
        &["--split-ratio"],
        bitty_com::Mode::WaitCli,
        "a missing ratio is a usage error",
    ),
    (
        &["--split-ratio", "--split"],
        bitty_com::Mode::WaitCli,
        "a flag cannot be a ratio",
    ),
    (
        &["--split-ratio=abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric ratio is a usage error",
    ),
    (
        &["--split=sideways"],
        bitty_com::Mode::WaitCli,
        "an unknown axis is a usage error",
    ),
    (
        &["--split", "v:abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric ratio is a usage error",
    ),
    (
        &["--split", ":0.4"],
        bitty_com::Mode::WaitCli,
        "a bare ratio is a usage error",
    ),
    (
        &["--layout"],
        bitty_com::Mode::WaitCli,
        "a missing layout is a usage error",
    ),
    (
        &["--focus"],
        bitty_com::Mode::WaitCli,
        "a missing focus is a usage error",
    ),
    // Values the parser stores raw and the startup checks reject before any
    // window exists (bitty#3224 review): the proxy must wait so the child's own
    // exit 2 reaches the caller instead of vanishing into a detached launch.
    (
        &["--layout", "garbage"],
        bitty_com::Mode::WaitCli,
        "an unknown layout spec fails the layout build",
    ),
    (
        &["--layout="],
        bitty_com::Mode::WaitCli,
        "an empty layout spec fails the layout build",
    ),
    (
        &["--layout", "stack:abc"],
        bitty_com::Mode::WaitCli,
        "an unparsable stack count fails the layout build",
    ),
    (
        &["--focus", "sideways"],
        bitty_com::Mode::WaitCli,
        "an unknown focus direction fails the focus check",
    ),
    (
        &["--focus=sideways"],
        bitty_com::Mode::WaitCli,
        "an unknown focus direction fails the focus check",
    ),
    (
        &["--font-size", "abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric size fails the config merge",
    ),
    (
        &["--font-size=abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric size fails the config merge",
    ),
    (
        &["--font-size", "-3"],
        bitty_com::Mode::WaitCli,
        "a non-positive size fails the config merge",
    ),
    (
        &["--font-size", "129"],
        bitty_com::Mode::WaitCli,
        "a size above 128 fails the config merge",
    ),
    (
        &["--opacity", "abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric opacity fails the config merge",
    ),
    (
        &["--opacity=abc"],
        bitty_com::Mode::WaitCli,
        "a non-numeric opacity fails the config merge",
    ),
    (
        &["--opacity", "1.5"],
        bitty_com::Mode::WaitCli,
        "an opacity above 1.0 fails the config merge",
    ),
];

#[cfg(test)]
#[test]
fn com_classifier_agrees_with_the_real_parser() {
    for (tail, expected, why) in CORPUS {
        let (parsed, classified) = classify_tail(tail);
        assert_eq!(
            parsed, *expected,
            "{tail:?}: the real parser disagrees with the documented intent ({why})"
        );
        assert_eq!(
            classified, *expected,
            "{tail:?}: bitty.com classifies differently than the documented intent ({why})"
        );
        assert_eq!(
            classified, parsed,
            "{tail:?}: bitty.com and the real parser disagree ({why})"
        );
    }
}

#[cfg(test)]
#[test]
fn every_subcommand_word_is_a_console_command() {
    for (name, aliases) in SUBCOMMANDS {
        for word in std::iter::once(name).chain(aliases.iter()) {
            let (parsed, classified) = classify_tail(&[word]);
            assert_eq!(
                parsed,
                bitty_com::Mode::WaitCli,
                "`bitty {word}` must parse as a console command"
            );
            assert_eq!(
                classified,
                bitty_com::Mode::WaitCli,
                "bitty.com must classify `bitty {word}` as a console command"
            );
        }
    }
    // The hidden deprecated alias is parser-only (never listed), but it is a
    // subcommand word like its canonical spelling.
    let (parsed, classified) = classify_tail(&["component"]);
    assert_eq!(parsed, bitty_com::Mode::WaitCli);
    assert_eq!(classified, bitty_com::Mode::WaitCli);
}

#[cfg(test)]
#[test]
fn subcommand_word_lists_do_not_drift() {
    let mut parser_side: Vec<&str> = Vec::new();
    for (name, aliases) in SUBCOMMANDS {
        parser_side.push(name);
        parser_side.extend(aliases.iter().copied());
    }
    parser_side.push("component");
    parser_side.sort_unstable();
    let mut proxy_side = bitty_com::grammar::SUBCOMMAND_WORDS.to_vec();
    proxy_side.sort_unstable();
    assert_eq!(
        proxy_side, parser_side,
        "bitty.com's SUBCOMMAND_WORDS must list exactly the parser's subcommand words"
    );
    for word in bitty_com::grammar::SUBCOMMAND_WORDS {
        assert!(
            help_canonical_cmd(word).is_some(),
            "`bitty {word}` is not a valid `bitty help` command"
        );
    }
}

#[cfg(test)]
#[test]
fn plus_action_lists_do_not_drift() {
    let mut parser_side: Vec<&str> = PlusAction::ALL.iter().map(|(name, _)| *name).collect();
    // `+show-font` is a read-only probe and the `+list-*` candidates run before
    // the parser, so both are console commands without being `PlusAction`s.
    parser_side.extend(["+show-font", "+list-profiles"]);
    parser_side.sort_unstable();
    let mut proxy_side = bitty_com::grammar::PLUS_ACTIONS.to_vec();
    proxy_side.sort_unstable();
    assert_eq!(
        proxy_side, parser_side,
        "bitty.com's PLUS_ACTIONS must list every +alias that dispatches a command"
    );
    for action in bitty_com::grammar::PLUS_ACTIONS {
        let (parsed, classified) = classify_tail(&[action]);
        assert_eq!(
            parsed,
            bitty_com::Mode::WaitCli,
            "`bitty {action}` must parse as a console command"
        );
        assert_eq!(classified, parsed, "`bitty {action}` disagrees");
    }
}
