//! The argv classifier: console command or desktop launch?
//!
//! This module duplicates a *small* part of the real CLI grammar that lives in
//! `crates/bitty-terminal/src/cli.rs`. The duplication is deliberate (the proxy
//! must decide before it starts the GUI binary, and it links nothing), and it is
//! guarded: `bitty-terminal`'s `com_parity_tests` push one argv corpus through
//! both implementations and fail on any disagreement.
//!
//! # Rule
//!
//! A token proves console intent when the real parser would end the process
//! without showing a window:
//!
//! - a first-positional subcommand word ([`SUBCOMMAND_WORDS`]);
//! - a `+<action>` alias ([`PLUS_ACTIONS`]);
//! - help/version/open-flags ([`HELP_VERSION_FLAGS`], [`HEADLESS_FLAGS`]);
//! - a flag the parser rejects as an unknown flag (usage error, exit 2);
//! - a flag whose value the parser validates and that fails (usage error).
//!
//! Everything else is a desktop launch: the invocation either is bare, or it
//! configures the window (`--split`, `--theme`, `--stack`, ...), or it names a
//! program to run inside the terminal (`bitty -- htop`, `bitty htop`).
//!
//! Tokens after `--` are the program's own argv and are never inspected.
//! A recognised flag keeps being interpreted after the program word, exactly
//! like the real parser, so `bitty htop --headless` still classifies as console.

/// How the proxy must start the sibling `bitty.exe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Console invocation: inherit stdio, wait for the child, and return its
    /// exit code.
    WaitCli,
    /// Desktop invocation: start the child detached with no console and return
    /// 0 immediately.
    DetachGui,
}

/// Every first-positional word `bitty` dispatches as a subcommand.
///
/// Spelled exactly as the parser accepts it, including the stable aliases
/// (`cfg`, `ls`, `comp`) and the deprecated `component` spelling of `extension`.
pub const SUBCOMMAND_WORDS: &[&str] = &[
    "init",
    "config",
    "cfg",
    "ctl",
    "run",
    "list",
    "ls",
    "inspect",
    "dev",
    "doctor",
    "upgrade",
    "reload",
    "plugin",
    "extension",
    "component",
    "version",
    "completion",
    "comp",
    "cmd",
    "x",
    "shell-init",
    "help",
];

/// Every `+<action>` alias that dispatches a command instead of spawning a
/// program, plus the two hidden completion helpers that run before parsing.
///
/// `+show-font` is a read-only probe rather than a [`crate::grammar`]
/// `PlusAction`, but it is just as certainly a console command.
pub const PLUS_ACTIONS: &[&str] = &[
    "+list-themes",
    "+list-colors",
    "+list-fonts",
    "+list-keybinds",
    "+list-actions",
    "+list-plugins",
    "+list-extensions",
    "+show-config",
    "+validate-config",
    "+edit-config",
    "+show-font",
    "+list-profiles",
];

/// Flags that print to stdout and exit: help and version.
pub const HELP_VERSION_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Flags that run one bounded non-graphical pass and exit.
pub const HEADLESS_FLAGS: &[&str] = &["--headless", "--test-mode"];

/// Flags the parser rejects outside the subcommand that owns them, so a bare
/// use is a usage error printed to stderr (exit 2).
pub const CLI_ONLY_FLAGS: &[&str] = &["--json", "--fix"];

/// Flags a desktop launch accepts and that carry no console intent.
pub const GUI_FLAGS: &[&str] = &[
    "-v",
    "--verbose",
    "--safe",
    "--fail-loud",
    "--stack",
    "--overlay",
    "--yes",
    "--check",
    "--rollback",
    "--force",
    "--migrate",
    "--no-plugins",
    "--with-components",
    "--no-with-components",
    "--no-components",
    "--without-components",
    "--print-shell-init",
    "--no-print-shell-init",
    "--no-shell-init",
    "--no-color",
    "--dry-run",
];

/// How a value flag consumes the token that follows it, and what it validates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueRule {
    /// Swallows a non-flag token; a missing value is not a usage error.
    Plain,
    /// Swallows a non-flag token or a negative number and needs a finite
    /// number (`--split-ratio`).
    Finite,
    /// Swallows a non-flag token and needs a known level (`--log-level`).
    Level,
    /// Swallows a non-flag token or a negative number and needs a point size
    /// the config merge accepts, i.e. finite and within `(0, 128]`
    /// (`--font-size`).
    FontSize,
    /// Swallows a non-flag token or a negative number and needs an opacity the
    /// config merge accepts, i.e. finite and within `[0.0, 1.0]`
    /// (`--opacity`).
    WindowOpacity,
    /// Requires a value token and needs a spec the startup layout builder
    /// accepts (`--layout`: `single|split[:axis[:ratio]]|stack[:n]|overlay[:x,y,w,h]`).
    Layout,
    /// Requires a value token and needs a spec the startup focus check accepts
    /// (`--focus`: `next|prev|up|down|left|right|<id>`).
    Focus,
    /// Optional `axis[:ratio]` value; a consumed bare ratio is a usage error
    /// (`--split`).
    Split,
}

impl ValueRule {
    /// The rule for an exact `--flag` token, if it takes a value at all.
    fn for_token(token: &str) -> Option<Self> {
        match token {
            "--format" | "--socket" | "--instance" | "--scrollback" | "--close-confirm"
            | "--gaps-in" | "--gaps-out" | "--border" | "--radius" | "--plugins" | "--theme"
            | "--font-family" | "--config" | "--profile" => Some(Self::Plain),
            "--font-size" => Some(Self::FontSize),
            "--opacity" => Some(Self::WindowOpacity),
            "--split-ratio" => Some(Self::Finite),
            "--log-level" => Some(Self::Level),
            "--layout" => Some(Self::Layout),
            "--focus" => Some(Self::Focus),
            "--split" => Some(Self::Split),
            _ => None,
        }
    }

    /// The rule for the `--flag=VALUE` form of a value flag.
    ///
    /// The parser absorbs a `=` form as one token for exactly these flags;
    /// every other `--name=value` spelling falls through to the unknown-flag
    /// arm, which is why this is a separate list-free lookup rather than
    /// "any known flag before the `=`".
    fn for_eq_form(name: &str) -> Option<Self> {
        match name {
            "--format" | "--socket" | "--instance" | "--scrollback" | "--close-confirm"
            | "--gaps-in" | "--gaps-out" | "--border" | "--radius" | "--plugins" | "--theme"
            | "--font-family" | "--config" | "--profile" => Some(Self::Plain),
            "--font-size" => Some(Self::FontSize),
            "--opacity" => Some(Self::WindowOpacity),
            "--split-ratio" => Some(Self::Finite),
            "--log-level" => Some(Self::Level),
            "--split" => Some(Self::Split),
            "--layout" => Some(Self::Layout),
            "--focus" => Some(Self::Focus),
            _ => None,
        }
    }

    /// True when the token after the flag is swallowed as the flag's value.
    fn swallows(self, next: Option<&str>) -> bool {
        let Some(next) = next else {
            return false;
        };
        match self {
            Self::Plain | Self::Layout | Self::Focus | Self::Level => !next.starts_with('-'),
            Self::Finite | Self::FontSize | Self::WindowOpacity => {
                !next.starts_with('-') || looks_like_negative_number(next)
            }
            Self::Split => split_value_is_axis(next),
        }
    }

    /// True when a *missing* value is a usage error rather than a default.
    fn requires_value(self) -> bool {
        match self {
            Self::Plain | Self::Split | Self::FontSize | Self::WindowOpacity => false,
            Self::Finite | Self::Level | Self::Layout | Self::Focus => true,
        }
    }

    /// True when this rule turns the *value* into a usage error (exit 2).
    ///
    /// Every arm mirrors the check the real binary performs before any window
    /// exists: `--split-ratio`/`--log-level`/`--split` at parse time,
    /// `--font-size`/`--opacity` at the config merge, `--layout` when the
    /// startup layout is built and `--focus` when the startup focus spec is
    /// validated. The diagnostic itself belongs to the child.
    fn rejects_value(self, value: &str) -> bool {
        match self {
            Self::Plain => false,
            Self::Finite => !is_finite_number(value),
            Self::Level => !is_known_level(value),
            Self::Split => !split_value_has_axis(value),
            Self::FontSize => !is_valid_font_size(value),
            Self::WindowOpacity => !is_valid_opacity(value),
            Self::Layout => !is_valid_layout_spec(value),
            Self::Focus => !is_valid_focus_spec(value),
        }
    }
}

/// Classify one invocation.
///
/// `args` is the argument vector **without** `argv[0]` (the proxy's own name
/// carries no meaning for the grammar). The environment is not consulted; use
/// [`classify_with_env`] for the shipped behavior.
#[must_use]
pub fn classify(args: &[String]) -> Mode {
    classify_with_env(args, false)
}

/// [`classify`] with the `BITTY_HEADLESS` fallback folded in.
///
/// A runner that exports `BITTY_HEADLESS=1` instead of passing `--headless`
/// makes the real binary run the headless smoke and exit, so the proxy must wait
/// for it just like the flag form.
#[must_use]
pub fn classify_with_env(args: &[String], headless_env: bool) -> Mode {
    if headless_env {
        return Mode::WaitCli;
    }
    classify_argv(args)
}

/// True when `BITTY_HEADLESS` asks for the headless smoke, spelled exactly as
/// `bitty-terminal`'s parser reads it (`1` or `true`, case-insensitive).
#[must_use]
pub fn headless_requested_by_env() -> bool {
    std::env::var("BITTY_HEADLESS")
        .is_ok_and(|value| value == "1" || value.to_lowercase() == "true")
}

/// The argv-only classifier behind [`classify`] and [`classify_with_env`].
fn classify_argv(args: &[String]) -> Mode {
    let mut program_set = false;
    let mut index = 0usize;
    while index < args.len() {
        let token = args[index].as_str();
        index += 1;

        // Everything after `--` is the spawned program's argv.
        if token == "--" {
            return Mode::DetachGui;
        }

        // `--flag=value`: absorbed as one token for the flags that own that
        // form, an unknown flag for everything else (`--help=x` included).
        if token.starts_with("--") {
            if let Some((name, value)) = token.split_once('=') {
                match ValueRule::for_eq_form(name) {
                    Some(rule) => {
                        if rule.rejects_value(value) {
                            return Mode::WaitCli;
                        }
                        continue;
                    }
                    None => {
                        if !program_set {
                            return Mode::WaitCli;
                        }
                        continue;
                    }
                }
            }
        }

        if let Some(rule) = ValueRule::for_token(token) {
            let next = args.get(index).map(String::as_str);
            if rule.swallows(next) {
                index += 1;
                if let Some(value) = next {
                    if rule.rejects_value(value) {
                        return Mode::WaitCli;
                    }
                }
            } else if rule.requires_value() {
                // A missing value for these is a usage error, not a default.
                return Mode::WaitCli;
            }
            continue;
        }

        if HELP_VERSION_FLAGS.contains(&token)
            || HEADLESS_FLAGS.contains(&token)
            || CLI_ONLY_FLAGS.contains(&token)
        {
            return Mode::WaitCli;
        }
        if GUI_FLAGS.contains(&token) {
            continue;
        }
        if token.starts_with('-') {
            // A dash token before the program is an unknown flag (usage error);
            // after it, the token belongs to that program.
            if !program_set {
                return Mode::WaitCli;
            }
            continue;
        }

        // A bare token: subcommand word, `+action`, or the program to spawn.
        if program_set {
            continue;
        }
        if SUBCOMMAND_WORDS.contains(&token) || PLUS_ACTIONS.contains(&token) {
            return Mode::WaitCli;
        }
        program_set = true;
    }
    Mode::DetachGui
}

/// Mirrors `bitty_terminal`'s `spawn::looks_like_negative_number`: `-0.5` is a
/// value, `--verbose` is a flag.
fn looks_like_negative_number(token: &str) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    matches!(chars.next(), Some(c) if c.is_ascii_digit() || c == '.')
}

/// Mirrors `spawn::parse_split_axis`: the axis spellings `--split` accepts.
fn is_split_axis(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "horizontal" | "h" | "horiz" | "hor" | "vertical" | "v" | "vert" | "ver"
    )
}

/// True when `--split <token>` consumes `token` (it names an axis, a ratio, or
/// both), mirroring `spawn::parse_split_token`.
fn split_value_is_axis(token: &str) -> bool {
    match token.split_once(':') {
        Some((axis, ratio)) => is_split_axis(axis.trim()) || ratio.trim().parse::<f32>().is_ok(),
        None => is_split_axis(token.trim()),
    }
}

/// True when `--split=<value>` names an axis, the only non-error form.
fn split_value_has_axis(value: &str) -> bool {
    if value.trim().is_empty() {
        // `--split=` falls back to a horizontal split without an error.
        return true;
    }
    match value.split_once(':') {
        Some((axis, ratio)) => is_split_axis(axis.trim()) && ratio.trim().parse::<f32>().is_ok(),
        None => is_split_axis(value.trim()),
    }
}

/// Mirrors `bitty_terminal`'s `validate_split_ratio_value`: finite `f32` only.
fn is_finite_number(value: &str) -> bool {
    match value.trim().parse::<f32>() {
        Ok(number) => number.is_finite(),
        Err(_) => false,
    }
}

/// Mirrors `bitty_terminal`'s `logging::LogLevel::parse`.
fn is_known_level(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "error" | "warn" | "warning" | "info" | "debug" | "trace" | "verbose"
    )
}

/// Mirrors the `--font-size` half of `bitty_config`'s
/// `CliOverrides::validate_appearance_overrides`: the merge parses the raw as a
/// finite `f32` and validates it as `font.size`, so the accepted range is
/// `(0, 128]`. A blank raw is not an override at all (the parser warns and
/// ignores it), so it stays a desktop launch.
fn is_valid_font_size(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    match trimmed.parse::<f32>() {
        Ok(size) => size.is_finite() && size > 0.0 && size <= 128.0,
        Err(_) => false,
    }
}

/// Mirrors the `--opacity` half of `CliOverrides::validate_appearance_overrides`:
/// the merge parses the raw as a finite `f32` and validates it as
/// `window.opacity`, i.e. `[0.0, 1.0]`. A blank raw is not an override.
fn is_valid_opacity(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    match trimmed.parse::<f32>() {
        Ok(opacity) => opacity.is_finite() && (0.0..=1.0).contains(&opacity),
        Err(_) => false,
    }
}

/// True when `--layout <spec>` names a layout the startup builder accepts.
///
/// Mirrors `bitty_terminal`'s `layout_cmd::parse_layout_spec`, whose `None`
/// makes `try_build_layout` fail closed with exit 2 before the window exists:
/// `single|leaf|1`, `split[:axis[:ratio]]`, `stack[:n]` and
/// `overlay[:x,y,w,h]`, each with the real clamping (a finite ratio and any
/// numeric stack count are accepted; the builder clamps loudly afterwards).
fn is_valid_layout_spec(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    let trimmed = lower.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == "single" || trimmed == "leaf" || trimmed == "1" {
        return true;
    }
    if trimmed.starts_with("split") {
        let rest = trimmed.trim_start_matches("split").trim_start_matches(':');
        if rest.is_empty() {
            return true;
        }
        let mut parts = rest.split(':');
        let axis = parts.next().unwrap_or("").trim();
        let ratio = parts.next().map(str::trim);
        // Trailing junk (`split:h:0.5:extra`) is malformed.
        if parts.next().is_some() {
            return false;
        }
        if !is_split_axis(axis) {
            return false;
        }
        return match ratio {
            Some(value) => is_finite_number(value),
            None => true,
        };
    }
    if trimmed.starts_with("stack") {
        let rest = trimmed.trim_start_matches("stack").trim_start_matches(':');
        return rest.is_empty() || rest.parse::<usize>().is_ok();
    }
    if trimmed.starts_with("overlay") {
        let rest = trimmed
            .trim_start_matches("overlay")
            .trim_start_matches(':');
        if rest.is_empty() {
            return true;
        }
        let mut count = 0usize;
        let mut valid = true;
        for part in rest.split(',') {
            count += 1;
            if part.trim().parse::<u16>().is_err() {
                valid = false;
            }
        }
        return valid && count == 4;
    }
    false
}

/// True when `--focus <spec>` names a target the startup focus check accepts.
///
/// Mirrors `bitty_terminal`'s `layout_cmd::is_valid_focus_spec`: the direction
/// words and their short aliases, or a numeric view id.
fn is_valid_focus_spec(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    match lower.as_str() {
        "next" | "n" | "prev" | "previous" | "p" | "up" | "down" | "left" | "right" => true,
        _ => spec.trim().parse::<u64>().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{Mode, classify, classify_with_env, headless_requested_by_env};

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| String::from(*arg)).collect()
    }

    /// Table-driven: (argv tail, expected mode, why).
    const CORPUS: &[(&[&str], Mode, &str)] = &[
        // Bare and window-configured launches.
        (&[], Mode::DetachGui, "bare launch opens the window"),
        (
            &["--split", "v"],
            Mode::DetachGui,
            "--split consumes its axis",
        ),
        (&["--stack"], Mode::DetachGui, "--stack is a layout request"),
        (
            &["--theme", "dark"],
            Mode::DetachGui,
            "--theme takes a value",
        ),
        (
            &["--config", "init"],
            Mode::DetachGui,
            "a value is never a word",
        ),
        (&["htop"], Mode::DetachGui, "a positional is the program"),
        (&["--", "init"], Mode::DetachGui, "-- ends the grammar"),
        (
            &["+unknown"],
            Mode::DetachGui,
            "an unknown +token is a program",
        ),
        // Console subcommands.
        (&["init"], Mode::WaitCli, "subcommand word"),
        (
            &["init", "--yes"],
            Mode::WaitCli,
            "subcommand word with flags",
        ),
        (&["cfg", "show"], Mode::WaitCli, "alias spelling"),
        (&["ls", "themes"], Mode::WaitCli, "alias spelling"),
        (&["help", "plugin"], Mode::WaitCli, "help word"),
        (&["+list-themes"], Mode::WaitCli, "+action alias"),
        (&["+show-font"], Mode::WaitCli, "read-only probe"),
        (
            &["+list-profiles"],
            Mode::WaitCli,
            "hidden completion helper",
        ),
        // Synchronous one-shot flags.
        (&["--help"], Mode::WaitCli, "help flag"),
        (&["-h"], Mode::WaitCli, "short help flag"),
        (&["--version"], Mode::WaitCli, "version flag"),
        (&["-V"], Mode::WaitCli, "short version flag"),
        (&["--headless"], Mode::WaitCli, "headless smoke"),
        (&["--test-mode"], Mode::WaitCli, "E2E servo loop"),
        (&["htop", "--headless"], Mode::WaitCli, "flags stay global"),
        // Usage errors.
        (&["--bogus"], Mode::WaitCli, "unknown flag"),
        (&["--json"], Mode::WaitCli, "config-only flag"),
        (&["--fix"], Mode::WaitCli, "doctor-only flag"),
        (&["--help=x"], Mode::WaitCli, "no = form on a boolean"),
        (&["--log-level", "loud"], Mode::WaitCli, "invalid level"),
        (&["--log-level"], Mode::WaitCli, "missing level"),
        (
            &["--split-ratio", "abc"],
            Mode::WaitCli,
            "non-numeric ratio",
        ),
        (&["--split-ratio"], Mode::WaitCli, "missing ratio"),
        (&["--split=sideways"], Mode::WaitCli, "unknown axis"),
        (&["--layout"], Mode::WaitCli, "missing layout"),
        (&["--focus"], Mode::WaitCli, "missing focus"),
        // Value errors the real binary reaches before any window exists.
        (
            &["--layout", "garbage"],
            Mode::WaitCli,
            "unknown layout spec",
        ),
        (&["--layout=garbage"], Mode::WaitCli, "unknown layout spec"),
        (&["--layout="], Mode::WaitCli, "empty layout spec"),
        (
            &["--layout", "stack:abc"],
            Mode::WaitCli,
            "unparsable stack count",
        ),
        (
            &["--layout", "split:h:1.5:extra"],
            Mode::WaitCli,
            "trailing layout junk",
        ),
        (
            &["--layout", "single"],
            Mode::DetachGui,
            "valid leaf layout",
        ),
        (
            &["--layout=split:h:0.3"],
            Mode::DetachGui,
            "valid split layout",
        ),
        (
            &["--layout", "stack:2"],
            Mode::DetachGui,
            "valid stack layout",
        ),
        (
            &["--focus", "sideways"],
            Mode::WaitCli,
            "unknown focus direction",
        ),
        (
            &["--focus=sideways"],
            Mode::WaitCli,
            "unknown focus direction",
        ),
        (
            &["--focus", "next"],
            Mode::DetachGui,
            "valid focus direction",
        ),
        (&["--focus=1"], Mode::DetachGui, "valid numeric focus id"),
        (&["--font-size", "abc"], Mode::WaitCli, "non-numeric size"),
        (&["--font-size=abc"], Mode::WaitCli, "non-numeric size"),
        (
            &["--font-size", "-3"],
            Mode::WaitCli,
            "size below the range",
        ),
        (
            &["--font-size", "129"],
            Mode::WaitCli,
            "size above the range",
        ),
        (
            &["--font-size", "12.5"],
            Mode::DetachGui,
            "valid point size",
        ),
        (
            &["--font-size="],
            Mode::DetachGui,
            "a blank = value is not an override",
        ),
        (&["--opacity", "abc"], Mode::WaitCli, "non-numeric opacity"),
        (&["--opacity=abc"], Mode::WaitCli, "non-numeric opacity"),
        (
            &["--opacity", "1.5"],
            Mode::WaitCli,
            "opacity above the range",
        ),
        (&["--opacity", "0.9"], Mode::DetachGui, "valid opacity"),
        (
            &["--opacity="],
            Mode::DetachGui,
            "a blank = value is not an override",
        ),
        (
            &["htop", "--bogus"],
            Mode::DetachGui,
            "program argv is not ours",
        ),
    ];

    #[test]
    fn corpus_classifies_as_documented() {
        for (args, expected, why) in CORPUS {
            assert_eq!(classify(&argv(args)), *expected, "{args:?}: {why}");
        }
    }

    #[test]
    fn unknown_plus_token_spawns_a_program_like_the_real_parser() {
        // The real parser keeps the historical program-spawn behavior for a
        // `+` token it does not know, so the proxy must not treat it as a
        // console command.
        assert_eq!(classify(&argv(&["+not-an-action"])), Mode::DetachGui);
    }

    #[test]
    fn negative_split_ratio_is_a_value_not_a_flag() {
        assert_eq!(
            classify(&argv(&["--split-ratio", "-0.5", "--split", "h"])),
            Mode::DetachGui
        );
        // A negative size is swallowed as a value too, but the merge rejects it
        // as out of range, so the child must own the diagnostic.
        assert_eq!(
            classify(&argv(&["--font-size", "-3"])),
            Mode::WaitCli,
            "a non-positive point size is a merge error"
        );
        // A dash token that is not a number is not a value: it is an unknown
        // flag, which is a usage error on its own.
        assert_eq!(
            classify(&argv(&["--font-size", "-abc"])),
            Mode::WaitCli,
            "a non-numeric dash token is an unknown flag"
        );
    }

    #[test]
    fn layout_specs_are_validated_like_the_startup_builder() {
        for (spec, expected) in [
            ("garbage", Mode::WaitCli),
            ("", Mode::WaitCli),
            ("stack:abc", Mode::WaitCli),
            ("split:sideways", Mode::WaitCli),
            ("split:h:abc", Mode::WaitCli),
            ("split:h:0.5:extra", Mode::WaitCli),
            ("overlay:1,2,3", Mode::WaitCli),
            ("single", Mode::DetachGui),
            ("1", Mode::DetachGui),
            ("split", Mode::DetachGui),
            ("split:v:0.4", Mode::DetachGui),
            ("stack", Mode::DetachGui),
            ("stack:3", Mode::DetachGui),
            ("overlay", Mode::DetachGui),
            ("overlay:5,5,20,10", Mode::DetachGui),
            ("Single", Mode::DetachGui),
        ] {
            assert_eq!(
                classify(&argv(&["--layout", spec])),
                expected,
                "--layout {spec:?}"
            );
            let joined = format!("--layout={spec}");
            assert_eq!(
                classify(&argv(&[joined.as_str()])),
                expected,
                "--layout={spec:?}"
            );
        }
    }

    #[test]
    fn focus_specs_are_validated_like_the_startup_check() {
        for (spec, expected) in [
            ("sideways", Mode::WaitCli),
            ("", Mode::WaitCli),
            ("nextt", Mode::WaitCli),
            ("-1", Mode::WaitCli),
            ("next", Mode::DetachGui),
            ("n", Mode::DetachGui),
            ("previous", Mode::DetachGui),
            ("right", Mode::DetachGui),
            ("0", Mode::DetachGui),
            ("7", Mode::DetachGui),
        ] {
            assert_eq!(
                classify(&argv(&["--focus", spec])),
                expected,
                "--focus {spec:?}"
            );
            let joined = format!("--focus={spec}");
            assert_eq!(
                classify(&argv(&[joined.as_str()])),
                expected,
                "--focus={spec:?}"
            );
        }
    }

    #[test]
    fn appearance_values_are_validated_like_the_config_merge() {
        // `--font-size` is accepted only within `(0, 128]`; a blank value is
        // not an override at all.
        for (value, expected) in [
            ("abc", Mode::WaitCli),
            ("0", Mode::WaitCli),
            ("-3", Mode::WaitCli),
            ("nan", Mode::WaitCli),
            ("inf", Mode::WaitCli),
            ("128.5", Mode::WaitCli),
            ("12.5", Mode::DetachGui),
            ("1", Mode::DetachGui),
            ("128", Mode::DetachGui),
            ("", Mode::DetachGui),
        ] {
            assert_eq!(
                classify(&argv(&["--font-size", value])),
                expected,
                "--font-size {value:?}"
            );
            let joined = format!("--font-size={value}");
            assert_eq!(
                classify(&argv(&[joined.as_str()])),
                expected,
                "--font-size={value:?}"
            );
        }
        // `--opacity` is accepted only within `[0.0, 1.0]`.
        for (value, expected) in [
            ("abc", Mode::WaitCli),
            ("1.5", Mode::WaitCli),
            ("-0.1", Mode::WaitCli),
            ("nan", Mode::WaitCli),
            ("0", Mode::DetachGui),
            ("0.9", Mode::DetachGui),
            ("1", Mode::DetachGui),
            ("", Mode::DetachGui),
        ] {
            assert_eq!(
                classify(&argv(&["--opacity", value])),
                expected,
                "--opacity {value:?}"
            );
            let joined = format!("--opacity={value}");
            assert_eq!(
                classify(&argv(&[joined.as_str()])),
                expected,
                "--opacity={value:?}"
            );
        }
    }

    #[test]
    fn split_ratio_form_is_validated() {
        assert_eq!(classify(&argv(&["--split=h:0.3"])), Mode::DetachGui);
        assert_eq!(classify(&argv(&["--split="])), Mode::DetachGui);
        assert_eq!(
            classify(&argv(&["--split=v:abc"])),
            Mode::WaitCli,
            "a colon with a non-numeric ratio is a usage error"
        );
        assert_eq!(
            classify(&argv(&["--split", ":0.4"])),
            Mode::WaitCli,
            "a bare ratio is a usage error"
        );
    }

    #[test]
    fn first_console_token_wins_and_stops_the_scan() {
        assert_eq!(
            classify(&argv(&["--split", "v", "init", "--force"])),
            Mode::WaitCli
        );
        assert_eq!(
            classify(&argv(&["init", "--", "--help"])),
            Mode::WaitCli,
            "a subcommand word precedes the separator"
        );
    }

    #[test]
    fn empty_and_flag_only_launches_detach() {
        assert_eq!(classify(&[]), Mode::DetachGui);
        assert_eq!(classify(&argv(&["-v", "--safe"])), Mode::DetachGui);
        assert_eq!(classify(&argv(&["--dry-run"])), Mode::DetachGui);
    }

    /// Marker that turns the probe test below into the child assertion.
    const PROBE_ENV: &str = "BITTY_COM_HEADLESS_ENV_PROBE";
    /// The expected `headless_requested_by_env()` answer for the child.
    const PROBE_EXPECT: &str = "BITTY_COM_HEADLESS_ENV_EXPECT";
    /// Filter that selects the probe in the re-executed test binary.
    const PROBE_TEST: &str = "grammar::tests::headless_env_probe_child";

    /// The `BITTY_HEADLESS` fallback must be proven through the reader, not
    /// assumed: the variable is set in a child run of this test binary (edition
    /// 2024 makes `std::env::set_var` unsafe and this crate forbids `unsafe`)
    /// and the child asks [`headless_requested_by_env`] itself.
    #[test]
    fn headless_env_fallback_reads_the_environment() {
        for (value, expected) in [
            (Some("1"), true),
            (Some("true"), true),
            (Some("TRUE"), true),
            (Some("0"), false),
            (Some("yes"), false),
            (None, false),
        ] {
            let mut probe = Command::new(std::env::current_exe().expect("test binary path"));
            probe
                .args(["--exact", PROBE_TEST, "--nocapture", "--test-threads=1"])
                .env(PROBE_ENV, "1")
                .env(PROBE_EXPECT, if expected { "true" } else { "false" });
            match value {
                Some(value) => probe.env("BITTY_HEADLESS", value),
                None => probe.env_remove("BITTY_HEADLESS"),
            };
            let output = probe.output().expect("re-exec the probe");
            assert!(
                output.status.success(),
                "child probe failed for BITTY_HEADLESS={value:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Child half of [`headless_env_fallback_reads_the_environment`]: it reads
    /// the real environment variable and must agree with the expected answer.
    #[test]
    fn headless_env_probe_child() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        let expected = std::env::var(PROBE_EXPECT).as_deref() == Ok("true");
        assert_eq!(headless_requested_by_env(), expected, "reader answer");
        let want = if expected {
            Mode::WaitCli
        } else {
            Mode::DetachGui
        };
        assert_eq!(
            classify_with_env(&[], headless_requested_by_env()),
            want,
            "a bare launch must follow the env fallback"
        );
    }
}
