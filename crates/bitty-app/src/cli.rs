//! CLI argument parsing, help/version text, and the owned argument bag.

use bitty_runtime::SplitAxis;

use crate::logging::LogLevel;
use crate::spawn::{looks_like_negative_number, parse_split_token};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Owned argument bag for the composition root.
///
/// `program` is the optional `argv[0]` to spawn inside the PTY. When `None`
/// the spawn layer resolves to the default shell (`$SHELL` or `/bin/sh` via
/// [`resolve_default_shell`); parsing itself stays `None` to keep arg parsing
/// pure (CTX-0136).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Args {
    /// When true the binary runs a single headless tick smoke and exits.
    pub(crate) headless: bool,
    /// When true (`--test-mode`) run the deterministic headless E2E servo
    /// loop instead of the graphical event loop (CTX-0506, research 043):
    /// the real Runtime plus the `BITTY_SOCKET` IPC surface, no display, no
    /// GPU, no VM, until the elevated `bitty.debug/testExit` verb applies.
    /// Grants no new authority; takes precedence over `--headless`.
    pub(crate) test_mode: bool,
    /// When true (`--safe`) never create a third-party plugin VM, never
    /// read the third-party store tree (recovery startup, RFC A.4 rule 6),
    /// and select the built-in safe effective config
    /// ([`bitty_config::safe_merged`]): every external config layer
    /// (`--config`/`BITTY_CONFIG`, profile, CLI appearance overrides) is
    /// ignored and decoration is forced to the safe `0/0/1/0/0` geometry
    /// with the opaque outline pair (CTX-0346, R-009/P0-AC-019).
    pub(crate) safe: bool,
    /// When true (`--fail-loud`) a requested startup step that fails is
    /// fatal instead of fail-soft: a failed primary shell spawn, a failed
    /// startup pane shell, or an attempted-but-unavailable IPC servo aborts
    /// with a non-zero exit code (CTX-0481, issue #762). The default keeps
    /// the documented fail-soft path where headless smoke still ticks.
    pub(crate) fail_loud: bool,
    /// When true (`--mascot`) print the Bittie mascot art to stdout and
    /// exit 0 (issue #1318, CTX-0729). Local class: no config, no
    /// instance, no plugin VM, no network, no stdin read; also records
    /// the first-run splash marker best-effort so a later normal launch
    /// does not repeat the greeting.
    pub(crate) mascot: bool,
    /// When true (`--no-splash`) suppress the first-run mascot splash for
    /// one normal launch without touching the marker file.
    pub(crate) no_splash: bool,
    /// When true print help and exit 0.
    pub(crate) help: bool,
    /// When true print version and exit 0.
    pub(crate) version: bool,
    /// Optional explicit program to spawn via `Runtime::spawn_shell`.
    /// When `None`, the spawn layer falls back to the default shell chain
    /// ([`crate::spawn::resolve_default_shell`]: configured `terminal.shell` > `$SHELL` >
    /// `/bin/sh`); explicit values are used verbatim.
    pub(crate) program: Option<String>,
    /// Extra argv tail for the program (reserved; not yet forwarded to
    /// `PtyBuilder::arg` because `Runtime::spawn_shell` currently takes a
    /// single `&str` — documented as a follow-up).
    pub(crate) program_args: Vec<String>,
    /// First unknown pre-`--` dash-flag (CR-APP-01 fail-closed, exit 2).
    /// A `-`-prefixed token before any positional program is never a
    /// program to spawn (a typo must not execute a binary); after a
    /// program is set, dash-tokens are that program's argv tail instead.
    pub(crate) unknown_flag: Option<String>,
    /// First invalid `--split-ratio` / `--split` / `--log-level` /
    /// `--layout` / `--focus` value (CTX-0480 fail-closed, exit 2).
    /// Warn-ignored values previously exited 0 and silently ran the
    /// default; now the startup dispatch prints usage and exits 2.
    /// Missing values for the same flags are recorded here as well.
    pub(crate) cli_value_error: Option<String>,
    /// Optional split axis (from `--split`).
    pub(crate) split_axis: Option<SplitAxis>,
    /// Optional split ratio (from `--split-ratio` or `--split` colon form).
    pub(crate) split_ratio: Option<f32>,
    /// When true, request a stack layout (from `--stack`).
    pub(crate) stack: bool,
    /// When true, request an overlay layout (from `--overlay`).
    pub(crate) overlay: bool,
    /// Raw layout spec (from `--layout`), e.g. "single", "split:h:0.5", "stack", "overlay:5,5,20,10".
    pub(crate) layout: Option<String>,
    /// Raw focus spec (from `--focus`), e.g. "next", "prev", "up", "1".
    pub(crate) focus: Option<String>,
    /// Explicit config file path (from `--config`). When `None` the
    /// `BITTY_CONFIG` env override is honored next, else the default XDG
    /// path is probed (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
    /// `~/.config/bitty/init.lua`, then `config.lua`); see `bitty-config::file`.
    pub(crate) config_path: Option<String>,
    /// Named profile (from `--profile`, else `BITTY_PROFILE` env).
    /// Loads `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` as the
    /// [`LayerKind::Profile`](bitty_config::LayerKind::Profile) base UNDER
    /// the user file (`init.lua` still wins; `--theme` wins over both).
    /// Missing/invalid profiles fail closed (exit 2, no fallback).
    /// CTX-0180: keep this + `config_path`/`theme`/appearance resolution in
    /// [`crate::config_cli::load_merged_config`] (one `Cli` plan for every present CLI field).
    pub(crate) profile: Option<String>,
    /// CLI theme override (from `--theme`). Wins over the config file and the
    /// named profile, which win over defaults
    /// (`CLI > file > profile > defaults` via `bitty-config` merge;
    /// see [`crate::config_cli::load_merged_config`] for the CTX-0180 extension point).
    pub(crate) theme: Option<String>,
    /// CLI font-family override (from `--font-family`, CTX-0180). Raw family
    /// name; blank means no override. Wins over the file for one launch;
    /// sibling fields (size, spacing, padding) keep file values.
    pub(crate) font_family: Option<String>,
    /// CLI font-size override in points (from `--font-size`, CTX-0180). Raw
    /// text on purpose: invalid values fail closed at merge time (exit 2
    /// with usage), never warn-ignored. Wins over the file for one launch.
    pub(crate) font_size: Option<String>,
    /// CLI window-opacity override (from `--opacity`, CTX-0180). Raw text on
    /// purpose: invalid values fail closed at merge time (exit 2 with
    /// usage), never warn-ignored. Wins over the file for one launch.
    pub(crate) opacity: Option<String>,
    /// `bitty config <verb>` subcommand (CLI-first management per DEC-0007).
    /// `None` means normal terminal startup. A program literally named
    /// `config` must be invoked as `bitty -- config ...`.
    pub(crate) config_cmd: Option<ConfigCommand>,
    /// True once the first positional `config` word is seen (subcommand
    /// mode); unknown/missing verbs fail closed via usage instead of
    /// spawning a program named `config`.
    pub(crate) config_word: bool,
    /// Unexpected extra positionals in subcommand mode (dispatch errors).
    pub(crate) config_args: Vec<String>,

    /// `bitty init` opt-in setup wizard (#243, CTX-0149). True once the
    /// first positional `init` word is seen; extra positionals land in
    /// `init_args` and fail closed via usage. A program literally named
    /// `init` must be invoked as `bitty -- init ...`.
    pub(crate) init_word: bool,
    /// `--yes`: wizard skips prompts and writes sane defaults.
    pub(crate) init_yes: bool,
    /// `--force`: wizard overwrites an existing config (with `.bak` backup).
    pub(crate) init_force: bool,
    /// `--scrollback LINES`: init-only explicit answer (raw text, validated
    /// fail-closed by the init dispatch; missing value fails closed there).
    pub(crate) init_scrollback: Option<String>,
    /// `--close-confirm MODE`: init-only explicit answer
    /// (always|when_busy|never; raw, validated fail-closed at dispatch).
    pub(crate) init_close_confirm: Option<String>,
    /// `--gaps-in PX`: init-only explicit answer (raw, validated at dispatch).
    pub(crate) init_gaps_in: Option<String>,
    /// `--gaps-out PX`: init-only explicit answer (raw, validated at dispatch).
    pub(crate) init_gaps_out: Option<String>,
    /// `--border PX`: init-only explicit answer (raw, validated at dispatch).
    pub(crate) init_border: Option<String>,
    /// `--radius PX`: init-only explicit answer (raw, validated at dispatch).
    pub(crate) init_radius: Option<String>,
    /// Unexpected extra positionals in init mode (dispatch errors).
    pub(crate) init_args: Vec<String>,
    /// `bitty doctor` installation and compatibility diagnosis (CTX-0175).
    /// True once the first positional `doctor` word is seen; extra bare
    /// positionals land in `doctor_args` and fail closed via usage. A
    /// program literally named `doctor` must be invoked as
    /// `bitty -- doctor ...`.
    pub(crate) doctor_word: bool,
    /// Raw `--format` value for `doctor` (`table|json|jsonl`; default table).
    /// Parsed globally so it composes before or after the `doctor` word;
    /// consumed only by the doctor dispatch, ignored by normal startup.
    pub(crate) doctor_format: Option<String>,
    /// `--no-color`: disable ANSI coloring in doctor table output.
    pub(crate) doctor_no_color: bool,
    /// Unexpected extra positionals in doctor mode (dispatch errors).
    pub(crate) doctor_args: Vec<String>,
    /// `bitty run -- COMMAND...` explicit child launch (CTX-0170).
    /// True once the first positional `run` word is seen (subcommand mode);
    /// a program literally named `run` needs `bitty run -- run ...` or the
    /// legacy `bitty -- run ...`. Tokens after the word land verbatim in
    /// `run_raw` for [`crate::run::parse_run_request`]; `--` is required there.
    pub(crate) run_word: bool,
    /// Raw tokens after the `run` word (options, `--`, COMMAND) for
    /// [`crate::run::parse_run_request`]. Empty until `run_word` is set.
    pub(crate) run_raw: Vec<String>,
    /// `bitty ctl` runtime control (CTX-0171, runtime class).
    /// True once the first positional `ctl` word is seen; a program
    /// literally named `ctl` needs `bitty run -- ctl ...` or the legacy
    /// `bitty -- ctl ...`. Tokens after the word land verbatim in
    /// `ctl_raw` for [`crate::ctl::parse_ctl_request`].
    pub(crate) ctl_word: bool,
    /// Raw tokens after the `ctl` word for [`crate::ctl::parse_ctl_request`].
    /// Empty until `ctl_word` is set.
    pub(crate) ctl_raw: Vec<String>,
    /// Global `--socket` before the `ctl` word (merged at dispatch;
    /// post-`ctl` `--socket` in `ctl_raw` wins when both agree, conflicts
    /// are usage errors).
    pub(crate) ctl_socket_pre: Option<String>,
    /// Global `--instance` before the `ctl` word (merged at dispatch).
    pub(crate) ctl_instance_pre: Option<String>,

    /// `bitty list <kind>` enumeration (CTX-0172). True once the first
    /// positional `list`/`ls` word is seen; a program literally named
    /// `list`/`ls` must be invoked as `bitty -- list ...`.
    pub(crate) list_word: bool,
    /// Invoked spelling (`list` or `ls`) for envelope `command`.
    pub(crate) list_spelling: String,
    /// Raw kind token after `list` (validated at dispatch).
    pub(crate) list_kind: Option<String>,
    /// Raw `--format` value for `list` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `list` word;
    /// consumed only by the list dispatch, ignored by normal startup.
    pub(crate) list_format: Option<String>,
    /// Explicit `--socket` for `list instances` (advisory, OS-authenticated).
    pub(crate) list_socket: Option<String>,
    /// Explicit `--instance` for `list instances`.
    pub(crate) list_instance: Option<String>,
    /// `--no-color` for list table output (also honours `NO_COLOR`).
    pub(crate) list_no_color: bool,
    /// Unexpected extra positionals in list mode (dispatch errors).
    pub(crate) list_args: Vec<String>,
    /// `bitty inspect <target> <value>` state and ownership (CTX-0173).
    /// True once the first positional `inspect` word is seen; a program
    /// literally named `inspect` must be invoked as `bitty -- inspect ...`.
    pub(crate) inspect_word: bool,
    /// Raw target token after `inspect` (validated at dispatch).
    pub(crate) inspect_target: Option<String>,
    /// Raw value token after the target (validated at dispatch).
    pub(crate) inspect_value: Option<String>,
    /// Raw `--format` value for `inspect` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `inspect` word;
    /// consumed only by the inspect dispatch, ignored by normal startup.
    pub(crate) inspect_format: Option<String>,
    /// `--no-color` for inspect table output (accepted for script parity;
    /// tables are plain text).
    pub(crate) inspect_no_color: bool,
    /// Unexpected extra positionals in inspect mode (dispatch errors).
    pub(crate) inspect_args: Vec<String>,
    /// `bitty dev` tracing, captures, dumps, and overlays (CTX-0174).
    /// True once the first positional `dev` word is seen; a program
    /// literally named `dev` must be invoked as `bitty -- dev ...`.
    pub(crate) dev_word: bool,
    /// Raw tokens after the `dev` word for [`crate::dev::parse_dev_request`].
    /// Empty until `dev_word` is set.
    pub(crate) dev_raw: Vec<String>,
    /// Raw `--format` value for `dev` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `dev` word;
    /// consumed only by the dev dispatch, ignored by normal startup.
    pub(crate) dev_format: Option<String>,
    /// `--no-color` for dev output (accepted for parity; tables are plain).
    pub(crate) dev_no_color: bool,
    /// Global `--socket` before the `dev` word (rejected at dispatch:
    /// dev is local-only).
    pub(crate) dev_socket_pre: Option<String>,
    /// Global `--instance` before the `dev` word (rejected at dispatch).
    pub(crate) dev_instance_pre: Option<String>,
    /// `bitty plugin` CLI-first management (CTX-0150, DEC-0007). True once
    /// the first positional `plugin` word is seen; a program literally named
    /// `plugin` needs `bitty -- plugin ...` (or `bitty run -- plugin ...`).
    /// Tokens after the word land verbatim in `plugin_raw` for
    /// [`crate::plugin::parse_plugin_request`].
    pub(crate) plugin_word: bool,
    /// Raw tokens after the `plugin` word (verb, id, flags) for
    /// [`crate::plugin::parse_plugin_request`]. Empty until `plugin_word` is set.
    pub(crate) plugin_raw: Vec<String>,
    /// Global `--format` before the `plugin` word (fallback merged at
    /// dispatch; a post-word `--format` wins).
    pub(crate) plugin_format: Option<String>,
    /// `--no-color` for plugin table output (global or post-word).
    pub(crate) plugin_no_color: bool,
    /// `bitty version` version and build metadata (#1375, CTX-0763).
    /// True once the first positional `version` word is seen; a program
    /// literally named `version` needs `bitty run -- version ...` or the
    /// legacy `bitty -- version ...`. Tokens after the word land verbatim
    /// in `version_raw` for [`crate::version::parse_version_request`].
    /// `-V` / `--version` is an alias for the table form.
    pub(crate) version_word: bool,
    /// Raw tokens after the `version` word for
    /// [`crate::version::parse_version_request`]. Empty until `version_word`
    /// is set.
    pub(crate) version_raw: Vec<String>,
    /// `bitty completion <shell>` shell completion (#1375, CTX-0763).
    /// True once the first positional `completion`/`comp` word is seen; a
    /// program literally named `completion`/`comp` needs
    /// `bitty run -- completion ...` or the legacy `bitty -- completion ...`.
    /// Tokens after the word land verbatim in `completion_raw` for
    /// [`crate::completion::parse_completion_request`].
    pub(crate) completion_word: bool,
    /// Invoked spelling (`completion` or `comp`) for usage/help.
    pub(crate) completion_spelling: String,
    /// Raw tokens after the `completion` word (shell, flags) for
    /// [`crate::completion::parse_completion_request`]. Empty until
    /// `completion_word` is set.
    pub(crate) completion_raw: Vec<String>,
    /// `bitty cmd` direct qualified executable invocation (#1375, CTX-0763).
    /// True once the first positional `cmd` word is seen; a program
    /// literally named `cmd` needs `bitty run -- cmd ...` or the legacy
    /// `bitty -- cmd ...`. Tokens after the word land verbatim in `cmd_raw`
    /// for [`crate::cmd::parse_cmd_request`].
    pub(crate) cmd_word: bool,
    /// Raw tokens after the `cmd` word for [`crate::cmd::parse_cmd_request`].
    /// Empty until `cmd_word` is set.
    pub(crate) cmd_raw: Vec<String>,
    /// `bitty x` qualified plugin namespace (#1375, CTX-0763). True once
    /// the first positional `x` word is seen; a program literally named `x`
    /// needs `bitty run -- x ...` or the legacy `bitty -- x ...`. Tokens
    /// after the word land verbatim in `x_raw` for
    /// [`crate::x::parse_x_request`].
    pub(crate) x_word: bool,
    /// Raw tokens after the `x` word for [`crate::x::parse_x_request`].
    /// Empty until `x_word` is set.
    pub(crate) x_raw: Vec<String>,
    /// When true emit per-frame `bitty tick` stats (CTX-0190).
    /// `-v` / `--verbose` (also `BITTY_VERBOSE=1`); shorthand for
    /// `--log-level debug`. Default (unset) is quiet: no tick lines.
    pub(crate) verbose: bool,
    /// Explicit stderr log level from `--log-level LEVEL` (CTX-0190).
    /// `None` means derive from `--verbose`/env/default in
    /// [`crate::logging::effective_log_level`]. Tick stats require `Debug`/`Trace`.
    pub(crate) log_level: Option<LogLevel>,
}

/// `bitty config` subcommand verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigCommand {
    /// Print the resolved config file path.
    Path,
    /// Load + validate and print per-key sources (the testing hook).
    Check,
    /// Open the file in `$VISUAL`/`$EDITOR` (never overwrites existing).
    Edit,
}

impl ConfigCommand {
    /// Parse a verb token.
    pub(crate) fn parse(token: &str) -> Option<Self> {
        match token {
            "path" => Some(Self::Path),
            "check" => Some(Self::Check),
            "edit" => Some(Self::Edit),
            _ => None,
        }
    }

    /// Verb name for usage/errors.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Check => "check",
            Self::Edit => "edit",
        }
    }
}

impl Args {
    pub(crate) fn new() -> Self {
        Self {
            headless: false,
            test_mode: false,
            safe: false,
            fail_loud: false,
            mascot: false,
            no_splash: false,
            help: false,
            version: false,
            program: None,
            program_args: Vec::new(),
            unknown_flag: None,
            cli_value_error: None,
            split_axis: None,
            split_ratio: None,
            stack: false,
            overlay: false,
            layout: None,
            focus: None,
            config_path: None,
            profile: None,
            theme: None,
            font_family: None,
            font_size: None,
            opacity: None,
            config_cmd: None,
            config_word: false,
            config_args: Vec::new(),

            init_word: false,
            init_yes: false,
            init_force: false,
            init_scrollback: None,
            init_close_confirm: None,
            init_gaps_in: None,
            init_gaps_out: None,
            init_border: None,
            init_radius: None,
            init_args: Vec::new(),
            doctor_word: false,
            doctor_format: None,
            doctor_no_color: false,
            doctor_args: Vec::new(),
            run_word: false,
            run_raw: Vec::new(),
            ctl_word: false,
            ctl_raw: Vec::new(),
            ctl_socket_pre: None,
            ctl_instance_pre: None,

            list_word: false,
            list_spelling: String::from("list"),
            list_kind: None,
            list_format: None,
            list_socket: None,
            list_instance: None,
            list_no_color: false,
            list_args: Vec::new(),
            inspect_word: false,
            inspect_target: None,
            inspect_value: None,
            inspect_format: None,
            inspect_no_color: false,
            inspect_args: Vec::new(),
            dev_word: false,
            dev_raw: Vec::new(),
            dev_format: None,
            dev_no_color: false,
            dev_socket_pre: None,
            dev_instance_pre: None,
            plugin_word: false,
            plugin_raw: Vec::new(),
            plugin_format: None,
            plugin_no_color: false,
            version_word: false,
            version_raw: Vec::new(),
            completion_word: false,
            completion_spelling: String::from("completion"),
            completion_raw: Vec::new(),
            cmd_word: false,
            cmd_raw: Vec::new(),
            x_word: false,
            x_raw: Vec::new(),
            verbose: false,
            log_level: None,
        }
    }
}

/// Record the first invalid `--split-ratio` / `--split` / `--log-level` /
/// `--layout` / `--focus` value so startup dispatch can fail closed
/// (CTX-0480, exit 2). Parsing stays total; `main` prints the recorded
/// message plus usage and exits 2.
fn record_value_error(out: &mut Args, msg: String) {
    if out.cli_value_error.is_none() {
        out.cli_value_error = Some(msg);
    }
}

/// Validate a `--split-ratio` value: numeric finite `f32`. Range is *not*
/// rejected here — out-of-range finite values clamp loudly at layout build
/// ([`crate::layout_cmd::clamp_ratio_loudly`]); non-numeric or non-finite
/// values are usage errors.
fn validate_split_ratio_value(val: &str) -> Result<f32, String> {
    match val.trim().parse::<f32>() {
        Ok(f) if f.is_finite() => Ok(f),
        _ => Err(format!(
            "invalid --split-ratio value {val:?} (want a finite number, e.g. 0.5)"
        )),
    }
}

/// Validate a `--split` value (`AXIS`, `AXIS:RATIO`, or `:RATIO`) for the
/// fail-closed dispatch path (CTX-0480). A colon whose ratio side is empty
/// or unparsable is an error instead of a silent 0.5 default; non-finite
/// ratios are errors. Finite out-of-range ratios stay raw and clamp loudly
/// at layout build.
fn validate_split_value(val: &str) -> Result<(Option<SplitAxis>, Option<f32>), String> {
    let (axis, ratio) = parse_split_token(val);
    if let Some((_, ratio_part)) = val.split_once(':') {
        if ratio.is_none() && ratio_part.trim().parse::<f32>().is_err() {
            return Err(format!(
                "invalid --split ratio {val:?} (want a finite number)"
            ));
        }
    }
    if let Some(r) = ratio {
        if !r.is_finite() {
            return Err(format!(
                "invalid --split ratio {val:?} (want a finite number)"
            ));
        }
    }
    Ok((axis, ratio))
}

/// Route one token verbatim into the active `version`/`completion`/`cmd`/`x`
/// post-word buffer (CTX-0763, #1375).
///
/// These four words consume the argv tail verbatim (like `run`/`ctl`/`dev`/
/// `plugin`): their dedicated parsers own `--format`/`--help`/separator
/// validation there. Space-form value flags (`--format`/`--socket`/
/// `--instance`) take their value along when the next token is not a flag.
/// Returns the next index when a new word owned the token, `None` when no new
/// word is active (the caller falls through to the existing arms).
fn push_verbatim_new_word(out: &mut Args, raw: &[String], i: usize) -> Option<usize> {
    let token = raw.get(i)?;
    let buf = if out.version_word {
        &mut out.version_raw
    } else if out.completion_word {
        &mut out.completion_raw
    } else if out.cmd_word {
        &mut out.cmd_raw
    } else if out.x_word {
        &mut out.x_raw
    } else {
        return None;
    };
    buf.push(token.clone());
    if (token == "--format" || token == "--socket" || token == "--instance")
        && raw.get(i + 1).is_some_and(|next| !next.starts_with('-'))
    {
        buf.push(raw[i + 1].clone());
        Some(i + 2)
    } else {
        Some(i + 1)
    }
}

/// Parses `raw` (including `argv[0]` at index 0) into [`Args`].
///
/// Recognised flags:
/// - `-h` / `--help` → help
/// - `-V` / `--version` → version
/// - `--headless` → headless smoke (also triggered by `BITTY_HEADLESS=1`)
/// - `--safe` → safe recovery mode (no third-party plugin VM, built-in safe
///   effective config)
/// - `--fail-loud` → a failed requested startup step (shell spawn, pane
///   shells, IPC servo) aborts with a non-zero exit code instead of the
///   default fail-soft warning path (CTX-0481; also `BITTY_FAIL_LOUD=1`)
/// - `--mascot` → print the Bittie mascot art and exit 0 (#1318)
/// - `--no-splash` → suppress the first-run mascot splash for one launch
/// - `--split [AXIS]` → split layout (AXIS = horizontal|h / vertical|v, default horizontal)
/// - `--split=AXIS[:RATIO]` → split with optional ratio
/// - `--split-ratio RATIO` → ratio for split
/// - `--stack` → stack layout (2 panes)
/// - `--overlay` → overlay layout
/// - `--layout SPEC` → explicit layout spec (single, split:h[:ratio], stack[:n], overlay[:x,y,w,h])
/// - `--focus SPEC` → focus (next|prev|up|down|left|right|<id>)
/// - `--config PATH` → explicit user config file (`init.lua`); when omitted
///   `BITTY_CONFIG` env is honored next, else the XDG default is probed
///   (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
///   `~/.config/bitty/init.lua`, then `config.lua` alias)
/// - `--profile NAME` → named profile
///   (`$XDG_CONFIG_HOME/bitty/profiles/<name>.lua`, else `BITTY_PROFILE`
///   env); layered UNDER the user file (`init.lua` still wins), `--theme`
///   wins over both. Missing profiles fail closed (exit 2).
/// - `--theme NAME` → CLI theme override (wins over config file, which wins
///   over defaults; see `bitty-config::file::resolve_effective`)
/// - `--font-family NAME` → CLI font-family override for one launch
///   (CTX-0180; blank means no override; wins over the file, siblings keep
///   file values)
/// - `--font-size PTS` → CLI font-size override for one launch (CTX-0180;
///   raw text, validated at merge time: invalid values fail closed with
///   usage instead of launching)
/// - `--opacity FLOAT` → CLI window-opacity override for one launch
///   (CTX-0180; raw text, validated at merge time like `--font-size`)
/// - `-v` / `--verbose` → emit per-frame `bitty tick` stats (CTX-0190;
///   also `BITTY_VERBOSE=1`). Shorthand for `--log-level debug`; default is
///   quiet (no tick lines).
/// - `--log-level LEVEL` → stderr level `error|warn|info|debug|trace`
///   (CTX-0190; also `BITTY_LOG`/`RUST_LOG`). Tick stats require
///   `debug`/`trace`; default is `warn` (quiet).
/// - `--yes` → init-only: skip prompts, write sane defaults (CTX-0149).
/// - `--force` → init-only: overwrite an existing config file, backing it
///   up to `<file>.bak` first (CTX-0149).
/// - init-only value flags (parsed globally, validated fail-closed by the
///   init dispatch; CTX-0345): `--scrollback LINES`,
///   `--close-confirm always|when_busy|never`, `--gaps-in PX`,
///   `--gaps-out PX`, `--border PX`, `--radius PX`; the existing
///   `--theme`/`--font-family`/`--font-size` flags also answer their init
///   step. A flag answered step is skipped in the interactive wizard and
///   wins over the `--yes` defaults.
/// - `run [OPTIONS] -- COMMAND...` → explicit child launch (CTX-0170);
///   a program literally named `run` needs `bitty run -- run ...` or
///   `bitty -- run ...`. Tokens after `run` are kept verbatim for
///   `run::parse_run_request`, which requires `--` before COMMAND.
/// - `config <path|check|edit>` → config subcommand (DEC-0007); a program
///   literally named `config` needs `bitty -- config ...`. `cfg` is the
///   stable v1 alias (same executable).
/// - `version [--format table|json|jsonl]` → version and build metadata
///   (#1375, CTX-0763); a program literally named `version` needs
///   `bitty run -- version ...` or `bitty -- version ...`. Tokens after the
///   word are kept verbatim for `version::parse_version_request`.
/// - `completion <shell>` → shell completion (#1375, CTX-0763); `comp` is
///   the stable v1 alias. A program literally named `completion`/`comp`
///   needs `bitty run -- completion ...` or `bitty -- completion ...`.
///   Tokens after the word are kept verbatim for
///   `completion::parse_completion_request`.
/// - `cmd <qualified-id> [--format SHAPE] [-- <args-json>]` → direct
///   qualified executable invocation (#1375, CTX-0763); a program literally
///   named `cmd` needs `bitty run -- cmd ...` or `bitty -- cmd ...`.
///   Tokens after the word are kept verbatim for `cmd::parse_cmd_request`.
/// - `x <publisher>.<name> <command> [args]` → qualified plugin namespace
///   (#1375, CTX-0763); a program literally named `x` needs
///   `bitty run -- x ...` or `bitty -- x ...`. Tokens after the word are
///   kept verbatim for `x::parse_x_request`.
/// - `init [--yes] [--force] [value flags]` → opt-in setup wizard (#243,
///   CTX-0149; guided config surface CTX-0345); a program literally named
///   `init` needs `bitty -- init ...`
/// - `doctor [--format table|json|jsonl] [--no-color]` → installation and
///   compatibility diagnosis (CTX-0175, local class, safe mode); a program
///   literally named `doctor` needs `bitty -- doctor ...`
/// - `list <themes|plugins|instances>` → resource enumeration (CTX-0172);
///   a program literally named `list`/`ls` needs `bitty -- list ...`
/// - `inspect <target> <value>` → state and ownership (CTX-0173, local
///   class, safe mode); a program literally named `inspect` needs
///   `bitty -- inspect ...`
/// - `--format SHAPE` → doctor/ctl/list/inspect/dev/plugin/version/cmd/x
///   output shape (parsed globally, consumed by each subcommand dispatch;
///   ignored by startup)
/// - `--no-color` → disable ANSI coloring in doctor/list table output
///   (accepted by inspect for parity; its tables are plain text)
/// - `--yes` / `--force` / init value flags are init-only (parsed globally,
///   consumed by the init dispatch; ignored by normal startup)
/// - `--` → treat the rest as program argv verbatim
///
/// The first non-flag token becomes `program`; additional non-flag tokens
/// after it become `program_args`. Unknown long flags are reported to stderr
/// but do not abort parsing — the binary stays total and keeps the invalid
/// token as a program name so callers see the error on `spawn_shell`.
pub(crate) fn parse_args(raw: &[String]) -> Args {
    let mut out = Args::new();
    // Env fallback for CI runners that set BITTY_HEADLESS without editing argv.
    if std::env::var("BITTY_HEADLESS").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        out.headless = true;
    }
    // CTX-0190: honour BITTY_VERBOSE without editing argv (mirrors BITTY_HEADLESS).
    if std::env::var("BITTY_VERBOSE").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        out.verbose = true;
    }
    // CTX-0481: honour BITTY_FAIL_LOUD for CI runners (mirrors BITTY_HEADLESS).
    if std::env::var("BITTY_FAIL_LOUD").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        out.fail_loud = true;
    }
    if raw.len() <= 1 {
        return out;
    }
    let mut after_double_dash = false;
    let mut program_set = false;
    let mut i = 1usize;
    while i < raw.len() {
        let token = &raw[i];
        if after_double_dash {
            if !program_set {
                out.program = Some(token.clone());
                program_set = true;
            } else {
                out.program_args.push(token.clone());
            }
            i += 1;
            continue;
        }
        // Handle flags with `=` first
        if let Some(val) = token.strip_prefix("--format=") {
            // CTX-0763: post-word tokens stay verbatim for the
            // `version`/`completion`/`cmd`/`x` parsers (own `--format` there).
            if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                i = next;
                continue;
            }
            // Raw on purpose: validated at doctor/ctl/list/inspect/dev dispatch
            // (fail-closed exit 2 on unknown shapes, never warn-ignored).
            // Merged CTX-0171 + CTX-0172 + CTX-0173 + CTX-0174: same token feeds
            // every dispatch; the inactive dispatches ignore their field.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request`, which owns `--format` validation
            // there; pre-word values feed `dev_format`.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            out.doctor_format = Some(val.to_string());
            out.list_format = Some(val.to_string());
            out.inspect_format = Some(val.to_string());
            out.dev_format = Some(val.to_string());
            out.plugin_format = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--socket=") {
            // CTX-0763: post-word tokens stay verbatim for the
            // `version`/`completion`/`cmd`/`x` parsers (rejected there: those
            // commands carry no target selection).
            if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                i = next;
                continue;
            }
            // `bitty ctl --socket PATH` global form (before the `ctl` word);
            // raw on purpose, validated at ctl/list dispatch (exit 2 on shape).
            // After the `ctl` word tokens go verbatim to `ctl_raw` instead.
            // Merged CTX-0171 + CTX-0172: pre-word form feeds both dispatches.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request` (rejected local-only there); pre-word
            // values are stashed for the dev dispatch to reject explicitly.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            if !out.ctl_word {
                out.ctl_socket_pre = Some(val.to_string());
                out.list_socket = Some(val.to_string());
                out.dev_socket_pre = Some(val.to_string());
            } else {
                out.ctl_raw.push(token.clone());
            }
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--instance=") {
            // CTX-0763: post-word tokens stay verbatim for the
            // `version`/`completion`/`cmd`/`x` parsers (rejected there).
            if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                i = next;
                continue;
            }
            // Merged CTX-0171 + CTX-0172: pre-word form feeds both dispatches.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request` (rejected local-only there); pre-word
            // values are stashed for the dev dispatch to reject explicitly.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            if !out.ctl_word {
                out.ctl_instance_pre = Some(val.to_string());
                out.list_instance = Some(val.to_string());
                out.dev_instance_pre = Some(val.to_string());
            } else {
                out.ctl_raw.push(token.clone());
            }
            i += 1;
            continue;
        }
        // CTX-0345: `bitty init` value flags accept both `--flag VALUE` and
        // `--flag=VALUE`; raw text is validated fail-closed by the init
        // dispatch (an empty `=` value fails closed there, never a default).
        if let Some((flag, value)) = token
            .strip_prefix("--")
            .and_then(|rest| rest.split_once('='))
        {
            let slot = match flag {
                "scrollback" => Some(&mut out.init_scrollback),
                "close-confirm" => Some(&mut out.init_close_confirm),
                "gaps-in" => Some(&mut out.init_gaps_in),
                "gaps-out" => Some(&mut out.init_gaps_out),
                "border" => Some(&mut out.init_border),
                "radius" => Some(&mut out.init_radius),
                _ => None,
            };
            if let Some(slot) = slot {
                *slot = Some(value.to_string());
                i += 1;
                continue;
            }
        }
        if token.starts_with("--split-ratio=") {
            let val = token.trim_start_matches("--split-ratio=");
            match validate_split_ratio_value(val) {
                Ok(f) => out.split_ratio = Some(f),
                Err(msg) => record_value_error(&mut out, msg),
            }
            i += 1;
            continue;
        }
        if token.starts_with("--split=") {
            let val = token.trim_start_matches("--split=");
            // val may be "h:0.3" or "horizontal" etc.
            match validate_split_value(val) {
                Ok((axis, ratio)) => {
                    if let Some(ax) = axis {
                        out.split_axis = Some(ax);
                    } else {
                        if !val.trim().is_empty() {
                            // CTX-0480: unknown axis fails closed instead of
                            // silently defaulting to horizontal.
                            record_value_error(
                                &mut out,
                                format!(
                                    "unknown --split axis {val:?} (want h|horizontal|v|vertical[:ratio])"
                                ),
                            );
                        }
                        out.split_axis = Some(SplitAxis::Horizontal);
                    }
                    if let Some(r) = ratio {
                        out.split_ratio = Some(r);
                    }
                }
                Err(msg) => record_value_error(&mut out, msg),
            }
            i += 1;
            continue;
        }
        if token.starts_with("--layout=") {
            let val = token.trim_start_matches("--layout=");
            out.layout = Some(val.to_string());
            i += 1;
            continue;
        }
        if token.starts_with("--focus=") {
            let val = token.trim_start_matches("--focus=");
            out.focus = Some(val.to_string());
            i += 1;
            continue;
        }
        if token.starts_with("--config=") {
            let val = token.trim_start_matches("--config=");
            if val.trim().is_empty() {
                eprintln!("warning: --config needs a file path — ignoring");
            } else {
                out.config_path = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--profile=") {
            let val = token.trim_start_matches("--profile=");
            if val.trim().is_empty() {
                eprintln!("warning: --profile needs a profile name — ignoring");
            } else {
                out.profile = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--theme=") {
            let val = token.trim_start_matches("--theme=");
            out.theme = Some(val.to_string());
            i += 1;
            continue;
        }

        if token.starts_with("--font-family=") {
            let val = token.trim_start_matches("--font-family=");
            if val.trim().is_empty() {
                eprintln!("warning: --font-family needs a family name — ignoring");
            } else {
                out.font_family = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--font-size=") {
            let val = token.trim_start_matches("--font-size=");
            if val.trim().is_empty() {
                eprintln!("warning: --font-size needs a point size — ignoring");
            } else {
                // Raw on purpose: validated at merge time (fail-closed).
                out.font_size = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--opacity=") {
            let val = token.trim_start_matches("--opacity=");
            if val.trim().is_empty() {
                eprintln!("warning: --opacity needs a value — ignoring");
            } else {
                // Raw on purpose: validated at merge time (fail-closed).
                out.opacity = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--log-level=") {
            let val = token.trim_start_matches("--log-level=");
            match LogLevel::parse(val) {
                Some(level) => out.log_level = Some(level),
                None => record_value_error(
                    &mut out,
                    format!("unknown --log-level {val:?} (want error|warn|info|debug|trace)"),
                ),
            }
            i += 1;
            continue;
        }
        match token.as_str() {
            "--" => {
                // CTX-0763: post-word `--` stays verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers (`cmd` owns the
                // separator; the other three reject it as a usage error).
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                // In `list`/`inspect`/`dev` mode `--` is a stray separator
                // (UsageError at dispatch); elsewhere it ends flags for
                // PROGRAM argv.
                if out.list_word {
                    out.list_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    out.inspect_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    i += 1;
                    continue;
                }
                after_double_dash = true;
                i += 1;
            }
            "-h" | "--help" => {
                out.help = true;
                i += 1;
            }
            "-V" | "--version" => {
                out.version = true;
                i += 1;
            }
            "-v" | "--verbose" => {
                out.verbose = true;
                i += 1;
            }
            "--headless" => {
                out.headless = true;
                i += 1;
            }
            "--test-mode" => {
                out.test_mode = true;
                i += 1;
            }
            "--safe" => {
                out.safe = true;
                i += 1;
            }
            "--fail-loud" => {
                out.fail_loud = true;
                i += 1;
            }
            "--mascot" => {
                out.mascot = true;
                i += 1;
            }
            "--no-splash" => {
                out.no_splash = true;
                i += 1;
            }
            "--stack" => {
                out.stack = true;
                i += 1;
            }
            "--overlay" => {
                out.overlay = true;
                i += 1;
            }
            "--yes" => {
                // `bitty init --yes`: non-interactive defaults (ignored by
                // normal startup and the `config` subcommand).
                out.init_yes = true;
                i += 1;
            }
            "--force" => {
                // `bitty init --force`: overwrite an existing config file
                // (with a `.bak` backup). Ignored elsewhere.
                out.init_force = true;
                i += 1;
            }
            "--scrollback" => {
                // `bitty init --scrollback LINES`: init-only explicit answer
                // (raw; validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_scrollback = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_scrollback = Some(String::new());
                    i += 1;
                }
            }
            "--close-confirm" => {
                // `bitty init --close-confirm MODE`: init-only explicit answer
                // (raw; validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_close_confirm = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_close_confirm = Some(String::new());
                    i += 1;
                }
            }
            "--gaps-in" => {
                // `bitty init --gaps-in PX`: init-only explicit answer (raw;
                // validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_gaps_in = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_gaps_in = Some(String::new());
                    i += 1;
                }
            }
            "--gaps-out" => {
                // `bitty init --gaps-out PX`: init-only explicit answer (raw;
                // validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_gaps_out = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_gaps_out = Some(String::new());
                    i += 1;
                }
            }
            "--border" => {
                // `bitty init --border PX`: init-only explicit answer (raw;
                // validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_border = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_border = Some(String::new());
                    i += 1;
                }
            }
            "--radius" => {
                // `bitty init --radius PX`: init-only explicit answer (raw;
                // validated fail-closed at the init dispatch).
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.init_radius = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    out.init_radius = Some(String::new());
                    i += 1;
                }
            }
            "--format" => {
                // CTX-0763: post-word pairs stay verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers.
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                // `bitty doctor/ctl/list/inspect/dev --format SHAPE`: raw on
                // purpose, validated at dispatch (fail-closed exit 2). Parsed
                // globally so it composes before or after the subcommand word.
                // Merged CTX-0171 + CTX-0172 + CTX-0173 + CTX-0174: same token
                // feeds every dispatch; missing warns (doctor/ctl) and
                // fail-closes list/inspect/dev via empty.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request`; pre-word values feed `dev_format`.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        out.dev_raw.push(String::new());
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.doctor_format = Some(raw[i + 1].clone());
                    out.list_format = Some(raw[i + 1].clone());
                    out.inspect_format = Some(raw[i + 1].clone());
                    out.dev_format = Some(raw[i + 1].clone());
                    out.plugin_format = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --format needs a value (table|json|jsonl) — ignoring");
                    out.list_format = Some(String::new());
                    out.inspect_format = Some(String::new());
                    out.dev_format = Some(String::new());
                    out.plugin_format = Some(String::new());
                    i += 1;
                }
            }
            "--socket" => {
                // CTX-0763: post-word pairs stay verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers (rejected there).
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                // `bitty ctl/list --socket PATH` global form (before the word).
                // Raw on purpose, validated at dispatch. After the `ctl` word
                // tokens go verbatim to `ctl_raw` (see `ctl` arm below).
                // Merged: feeds both; missing warns and fail-closes list.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request` (rejected local-only there);
                // pre-word values are stashed for the dev dispatch to reject.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.ctl_socket_pre = Some(raw[i + 1].clone());
                    out.list_socket = Some(raw[i + 1].clone());
                    out.dev_socket_pre = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --socket needs a path — ignoring");
                    out.list_socket = Some(String::new());
                    out.dev_socket_pre = Some(String::new());
                    i += 1;
                }
            }
            "--instance" => {
                // CTX-0763: post-word pairs stay verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers (rejected there).
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                // `bitty ctl/list --instance ID` global form (before the word).
                // Merged: feeds both; missing warns and fail-closes list.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request` (rejected local-only there);
                // pre-word values are stashed for the dev dispatch to reject.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.ctl_instance_pre = Some(raw[i + 1].clone());
                    out.list_instance = Some(raw[i + 1].clone());
                    out.dev_instance_pre = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --instance needs an id — ignoring");
                    out.list_instance = Some(String::new());
                    out.dev_instance_pre = Some(String::new());
                    i += 1;
                }
            }
            "--no-color" => {
                // CTX-0763: post-word `--no-color` stays verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers (accepted there for
                // parity; their output is never colorized).
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                out.doctor_no_color = true;
                out.list_no_color = true;
                out.inspect_no_color = true;
                out.dev_no_color = true;
                out.plugin_no_color = true;
                i += 1;
            }
            "--split" => {
                // Check next token for axis (if not a flag)
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let next = raw[i + 1].clone();
                    let (axis, ratio) = parse_split_token(&next);
                    if axis.is_some() || ratio.is_some() {
                        match validate_split_value(&next) {
                            Ok((ax, r)) => {
                                if let Some(ax) = ax {
                                    out.split_axis = Some(ax);
                                } else {
                                    // CTX-0480: bare `:ratio` without an axis is a
                                    // usage error (previously silently horizontal).
                                    record_value_error(
                                        &mut out,
                                        format!(
                                            "unknown --split axis {next:?} (want h|horizontal|v|vertical[:ratio])"
                                        ),
                                    );
                                    out.split_axis = Some(SplitAxis::Horizontal);
                                }
                                if let Some(r) = r {
                                    out.split_ratio = Some(r);
                                }
                            }
                            Err(msg) => record_value_error(&mut out, msg),
                        }
                        i += 2;
                    } else {
                        // Next token is not an axis/ratio (e.g. "/bin/bash"), treat --split as horizontal without consuming
                        out.split_axis = Some(SplitAxis::Horizontal);
                        i += 1;
                    }
                } else {
                    out.split_axis = Some(SplitAxis::Horizontal);
                    i += 1;
                }
            }
            "--split-ratio" => {
                // A negative token is a value, not a flag: out-of-range
                // finite ratios clamp loudly at layout build (CTX-0480).
                if i + 1 < raw.len()
                    && (!raw[i + 1].starts_with('-') || looks_like_negative_number(&raw[i + 1]))
                {
                    let next = raw[i + 1].clone();
                    match validate_split_ratio_value(&next) {
                        Ok(f) => out.split_ratio = Some(f),
                        Err(msg) => record_value_error(&mut out, msg),
                    }
                    i += 2;
                } else {
                    record_value_error(
                        &mut out,
                        "--split-ratio needs a numeric value (e.g. 0.5)".to_string(),
                    );
                    i += 1;
                }
            }
            "--layout" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.layout = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    record_value_error(
                        &mut out,
                        "--layout needs a value (single|split:h[:ratio]|stack[:n]|overlay[:x,y,w,h])"
                            .to_string(),
                    );
                    i += 1;
                }
            }
            "--focus" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.focus = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    record_value_error(
                        &mut out,
                        "--focus needs a value (next|prev|up|down|left|right|<id>)".to_string(),
                    );
                    i += 1;
                }
            }
            "--config" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.config_path = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --config needs a file path — ignoring");
                    i += 1;
                }
            }
            "--profile" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.profile = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --profile needs a profile name — ignoring");
                    i += 1;
                }
            }
            "--theme" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.theme = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --theme needs a theme name — ignoring");
                    i += 1;
                }
            }

            "--font-family" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.font_family = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --font-family needs a family name — ignoring");
                    i += 1;
                }
            }
            "--font-size" => {
                if i + 1 < raw.len()
                    && (!raw[i + 1].starts_with('-') || looks_like_negative_number(&raw[i + 1]))
                {
                    // Raw on purpose: validated at merge time (fail-closed).
                    out.font_size = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --font-size needs a point size — ignoring");
                    i += 1;
                }
            }
            "--opacity" => {
                if i + 1 < raw.len()
                    && (!raw[i + 1].starts_with('-') || looks_like_negative_number(&raw[i + 1]))
                {
                    // Raw on purpose: validated at merge time (fail-closed).
                    out.opacity = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --opacity needs a value — ignoring");
                    i += 1;
                }
            }
            "--log-level" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let val = raw[i + 1].clone();
                    match LogLevel::parse(&val) {
                        Some(level) => out.log_level = Some(level),
                        None => record_value_error(
                            &mut out,
                            format!(
                                "unknown --log-level {val:?} (want error|warn|info|debug|trace)"
                            ),
                        ),
                    }
                    i += 2;
                } else {
                    record_value_error(
                        &mut out,
                        "--log-level needs a value (error|warn|info|debug|trace)".to_string(),
                    );
                    i += 1;
                }
            }
            s if s.starts_with('-') => {
                // In `list`/`inspect`/`dev` mode unknown flags fail closed at
                // dispatch (exit 2); elsewhere a `-`-prefixed token before
                // any positional program is a usage error (CR-APP-01, exit
                // 2), never a program to spawn. Once a program is set,
                // dash-tokens are that program's argv tail (e.g. `cat -A`).
                // (`dev` normally breaks verbatim at its word, so this arm
                // only fires for flags before the word — kept for parity.)
                // CTX-0763: post-word flags stay verbatim for the
                // `version`/`completion`/`cmd`/`x` parsers.
                if let Some(next) = push_verbatim_new_word(&mut out, raw, i) {
                    i = next;
                    continue;
                }
                if out.list_word {
                    out.list_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    out.inspect_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    i += 1;
                    continue;
                }
                if program_set {
                    out.program_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.unknown_flag.is_none() {
                    out.unknown_flag = Some(token.clone());
                }
                i += 1;
            }
            _ => {
                // `bitty run -- COMMAND...` explicit child launch (CTX-0170,
                // first positional only; `--` escape bypasses via
                // after_double_dash). The word `run` is always this
                // subcommand, never a program named `run`: use
                // `bitty run -- run ...` (or legacy `bitty -- run ...`) for
                // that program. Tokens after the word are kept verbatim for
                // `run::parse_run_request`, which enforces the required `--`.
                if !program_set
                    && !out.config_word
                    && !out.init_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "run"
                {
                    out.run_word = true;
                    out.run_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty ctl` runtime control (CTX-0171, first positional
                // only; `--` escape bypasses via after_double_dash). The word
                // `ctl` is always this subcommand, never a program named
                // `ctl`: use `bitty run -- ctl ...` (or legacy
                // `bitty -- ctl ...`) for that program. Tokens after the word
                // are kept verbatim for `ctl::parse_ctl_request`.
                if !program_set
                    && !out.config_word
                    && !out.init_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.doctor_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "ctl"
                {
                    out.ctl_word = true;
                    out.ctl_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty config <verb>` subcommand (first positional only;
                // `--` escape hatch bypasses this via after_double_dash).
                // A program literally named `config` needs `bitty -- config`.
                // `cfg` is the stable v1 alias (same executable).
                if !program_set
                    && !out.config_word
                    && !out.doctor_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && (token == "config" || token == "cfg")
                {
                    out.config_word = true;
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        let verb = raw[i + 1].clone();
                        match ConfigCommand::parse(&verb) {
                            Some(cmd) => {
                                out.config_cmd = Some(cmd);
                            }
                            None => {
                                // Unknown verb: record for fail-closed usage.
                                out.config_args.push(verb);
                            }
                        }
                        i += 2;
                    } else {
                        // Bare `bitty config` (or `config` + flag): dispatch
                        // prints usage + exit 2.
                        i += 1;
                    }
                    continue;
                }
                if out.config_word {
                    // Verb may follow flags (`config --config X check`): take
                    // the first bare token as the verb when none is set yet.
                    if out.config_cmd.is_none() && out.config_args.is_empty() {
                        if let Some(cmd) = ConfigCommand::parse(token) {
                            out.config_cmd = Some(cmd);
                            i += 1;
                            continue;
                        }
                    }
                    // Extra positionals in subcommand mode fail closed.
                    out.config_args.push(token.clone());
                    i += 1;
                    continue;
                }

                // `bitty init` opt-in setup wizard (first positional only;
                // `--` escape hatch bypasses this via after_double_dash).
                // A program literally named `init` needs `bitty -- init`.
                // Flags (`--yes`, `--force`, `--config`) compose in any
                // order around the word.
                if !program_set
                    && !out.init_word
                    && !out.doctor_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "init"
                {
                    out.init_word = true;
                    i += 1;
                    continue;
                }
                if out.init_word {
                    // Extra positionals in init mode fail closed at dispatch.
                    out.init_args.push(token.clone());
                    i += 1;
                    continue;
                }
                // `bitty doctor` diagnosis (first positional only; CTX-0175).
                // `--` escape hatch bypasses this via after_double_dash.
                // A program literally named `doctor` needs `bitty -- doctor`.
                // Flags (`--format`, `--no-color`, `--config`, `--profile`)
                // compose in any order around the word. Mutual exclusion
                // with `config`/`init` words falls out naturally: once one
                // word is seen the other word lands in that mode's extra
                // args and fails closed at dispatch.
                if !program_set
                    && !out.doctor_word
                    && !out.config_word
                    && !out.init_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "doctor"
                {
                    out.doctor_word = true;
                    i += 1;
                    continue;
                }
                if out.doctor_word {
                    // Doctor takes no positionals: fail closed at dispatch.
                    out.doctor_args.push(token.clone());
                    i += 1;
                    continue;
                }

                // `bitty list <kind>` enumeration (first positional only;
                // CTX-0172). `ls` is the stable alias for `list` per
                // cli-contract-rfc. A program literally named `list`/`ls`
                // needs `bitty -- list ...` (or `bitty run -- list ...`).
                if !program_set
                    && !out.config_word
                    && !out.list_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.inspect_word
                    && !out.dev_word
                    && (token == "list" || token == "ls")
                {
                    out.list_word = true;
                    out.list_spelling = token.clone();
                    i += 1;
                    continue;
                }
                if out.list_word {
                    // Kind is the first bare token after `list`; the rest
                    // fail closed at dispatch (including stray `--`, which
                    // was recorded above as a list arg).
                    if out.list_kind.is_none() {
                        // Allow `-h/--help` after `list` to flow to the
                        // global help path (main shows list help when both
                        // are set); any other flag-looking token was already
                        // captured as a list arg above.
                        out.list_kind = Some(token.clone());
                    } else {
                        out.list_args.push(token.clone());
                    }
                    i += 1;
                    continue;
                }
                // `bitty inspect <target> <value>` state and ownership (first
                // positional only; CTX-0173). A program literally named
                // `inspect` needs `bitty -- inspect ...` (or
                // `bitty run -- inspect ...`).
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && token == "inspect"
                {
                    out.inspect_word = true;
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    // Target is the first bare token after `inspect`, value
                    // the second; the rest fail closed at dispatch (including
                    // stray `--`, which was recorded above as an inspect arg).
                    if out.inspect_target.is_none() {
                        // Allow `-h/--help` after `inspect` to flow to the
                        // global help path (main shows inspect help when both
                        // are set); any other flag-looking token was already
                        // captured as an inspect arg above.
                        out.inspect_target = Some(token.clone());
                    } else if out.inspect_value.is_none() {
                        out.inspect_value = Some(token.clone());
                    } else {
                        out.inspect_args.push(token.clone());
                    }
                    i += 1;
                    continue;
                }
                // `bitty dev` tracing, captures, dumps, overlays (first
                // positional only; CTX-0174). The word `dev` is always this
                // subcommand, never a program named `dev`: use
                // `bitty run -- dev ...` (or legacy `bitty -- dev ...`) for
                // that program. Tokens after the word are kept verbatim for
                // `dev::parse_dev_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && token == "dev"
                {
                    out.dev_word = true;
                    out.dev_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty plugin` CLI-first management (CTX-0150, DEC-0007).
                // The word `plugin` is always this subcommand, never a
                // program named `plugin`: use `bitty run -- plugin ...` (or
                // legacy `bitty -- plugin ...`) for that program. Tokens
                // after the word are kept verbatim for
                // `plugin::parse_plugin_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && token == "plugin"
                {
                    out.plugin_word = true;
                    out.plugin_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty version` version and build metadata (first positional
                // only; CTX-0763, #1375). The word `version` is always this
                // subcommand, never a program named `version`: use
                // `bitty run -- version ...` (or legacy `bitty -- version ...`)
                // for that program. Tokens after the word are kept verbatim
                // for `version::parse_version_request`. `-V` / `--version`
                // is an alias for the table form.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && !out.version_word
                    && !out.completion_word
                    && !out.cmd_word
                    && !out.x_word
                    && token == "version"
                {
                    out.version_word = true;
                    out.version_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty completion <shell>` shell completion (first positional
                // only; CTX-0763, #1375). `comp` is the stable v1 alias. The
                // words are always this subcommand, never a program: use
                // `bitty run -- completion ...` (or legacy
                // `bitty -- completion ...`) for that program. Tokens after
                // the word are kept verbatim for
                // `completion::parse_completion_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && !out.version_word
                    && !out.completion_word
                    && !out.cmd_word
                    && !out.x_word
                    && (token == "completion" || token == "comp")
                {
                    out.completion_word = true;
                    out.completion_spelling = token.clone();
                    out.completion_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty cmd` direct qualified executable invocation (first
                // positional only; CTX-0763, #1375). The word `cmd` is always
                // this subcommand, never a program named `cmd`: use
                // `bitty run -- cmd ...` (or legacy `bitty -- cmd ...`) for
                // that program. Tokens after the word are kept verbatim for
                // `cmd::parse_cmd_request`, which owns the `--` separator.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && !out.version_word
                    && !out.completion_word
                    && !out.cmd_word
                    && !out.x_word
                    && token == "cmd"
                {
                    out.cmd_word = true;
                    out.cmd_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty x` qualified plugin namespace (first positional only;
                // CTX-0763, #1375). The word `x` is always this subcommand,
                // never a program named `x`: use `bitty run -- x ...` (or
                // legacy `bitty -- x ...`) for that program. Tokens after the
                // word are kept verbatim for `x::parse_x_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && !out.version_word
                    && !out.completion_word
                    && !out.cmd_word
                    && !out.x_word
                    && token == "x"
                {
                    out.x_word = true;
                    out.x_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                if !program_set {
                    out.program = Some(token.clone());
                    program_set = true;
                } else {
                    out.program_args.push(token.clone());
                }
                i += 1;
            }
        }
    }
    out
}

pub(crate) fn help_text() -> String {
    format!(
        "bitty {} — Correct Terminal (thin composition root)\n\
         \n\
         Usage: bitty [OPTIONS] [--] [PROGRAM [ARGS...]]\n\
         \n\
         Options:\n  \
           -h, --help       Print this help and exit\n  \
            -V, --version    Print version and exit\n  \
            -v, --verbose    Emit per-frame `bitty tick` stats on stderr\n  \
                             (shorthand for --log-level debug; default quiet)\n  \
                --log-level LEVEL  Stderr level: error|warn|info|debug|trace\n  \
                             (default warn: startup info lines need info,\n  \
                             tick stats need debug|trace; also BITTY_LOG/RUST_LOG)\n  \
               --headless   Run a single headless tick smoke and exit (CI)\n  \
               --test-mode  Run the deterministic headless E2E servo loop\n  \
                            (no display/GPU/VM): real runtime + BITTY_SOCKET IPC\n  \
                            until `bitty.debug/testExit` (debug.control\n  \
                            elevation); grants no new authority\n  \
               --safe       Safe mode: do not load third-party plugins (no\n  \
                            plugin VM), and use the built-in safe config\n  \
                            (decoration 0/0/1/0/0, opaque outline pair);\n  \
                            ignores --config/BITTY_CONFIG/profiles/CLI overrides\n  \
               --fail-loud  Fail-loud startup: a failed shell/pane spawn or\n  \
                            IPC servo aborts with a non-zero exit code\n  \
                            instead of the default fail-soft warning path\n  \
               --mascot      Print the Bittie mascot art and exit\n  \
               --no-splash   Suppress the first-run mascot splash once\n  \
               --split [AXIS]  Split layout: AXIS = horizontal|h / vertical|v (default h, ratio 0.5)\n  \
               --split=AXIS[:RATIO]  Split with optional ratio (e.g. --split=h:0.3)\n  \
               --split-ratio RATIO  Ratio for --split (0.10..0.90, default 0.5)\n  \
               --stack      Stack layout (2 panes, full bounds, last on top)\n  \
               --overlay    Overlay layout (base + 20×10 floating at 5,5)\n  \
               --layout SPEC  Explicit layout: single | split:h[:ratio] | split:v[:ratio]\n  \
           \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20stack[:N] | overlay[:X,Y,W,H]  (overrides --split/--stack/--overlay)\n  \
               --focus SPEC Focus: next|prev|up|down|left|right|<id> (e.g. --focus next, --focus 2)\n  \
               --config PATH  Explicit user config file (init.lua). When omitted\n  \
                            BITTY_CONFIG is honored next, else the XDG default\n  \
                            is probed ($XDG_CONFIG_HOME/bitty/init.lua,\n  \
                            fallback ~/.config/bitty/init.lua, then config.lua alias).\n  \
                            Invalid files fail closed (clear stderr, exit non-zero,\n  \
                            no panic).\n  \
               --profile NAME Named profile ($XDG_CONFIG_HOME/bitty/profiles/<name>.lua,\n  \
                            else BITTY_PROFILE). Layered UNDER the user file\n  \
                            (init.lua still wins; --theme wins over both).\n  \
                            Missing profiles fail closed (exit 2, no fallback).\n  \
                            With --config + --profile together both layers load\n  \
                            (explicit file wins) with a stderr warning.\n  \
                            Env: BITTY_CONFIG (path), BITTY_PROFILE (name).\n  \
                            Precedence: CLI flags > BITTY_* env > file+profile > defaults.\n  \
               --theme NAME   CLI theme override (e.g. --theme dark). Wins over the\n  \
                             config file, which wins over defaults.\n  \
                --font-family NAME  CLI font family override for one launch\n  \
                             (e.g. --font-family \"JetBrainsMono Nerd Font\").\n  \
                             Wins over the file; blank means no override.\n  \
                --font-size PTS  CLI font size override in points for one launch\n  \
                             (e.g. --font-size 14; range (0, 128]). Invalid\n  \
                             values fail closed (exit 2 with this usage).\n  \
                 --opacity FLOAT  CLI window opacity override for one launch\n  \
                              (e.g. --opacity 0.95; range [0.0, 1.0]). Invalid\n  \
                              values fail closed (exit 2 with this usage).\n  \
                              Precedence: CLI flags > file > profile > defaults;\n  \
                              each flag overrides only its own field (siblings\n  \
                              keep file values).\n  \
                 --format SHAPE  Doctor/ctl/list/inspect/plugin/version/cmd/x\n  \
                              output shape:\n  \
                              table|json|jsonl (default table; parsed globally,\n  \
                              consumed by `bitty doctor`, `bitty ctl`,\n  \
                              `bitty list`, `bitty inspect`,\n  \
                              `bitty plugin list|info`, `bitty version`,\n  \
                              `bitty cmd`, and `bitty x` (`bitty completion`\n  \
                              emits a script and ignores it; ignored by startup).\n\
               --socket PATH   Ctl target socket (global `bitty --socket P ctl ...`\n  \
                              or `bitty ctl --socket P ...`; bypasses discovery).\n  \
               --instance ID   Ctl target instance (global or per-`ctl` flag).\n  \
                 --no-color     Disable ANSI coloring in doctor/list table output\n  \
                               (accepted by inspect for parity; its tables are plain)\n  \
               --           End of flags; remaining tokens are PROGRAM argv\n\
         \n\
          Subcommands (CLI-first management, DEC-0007):\n  \
            run [--cwd PATH] [--env K=V ...] [--title S] -- COMMAND...  Explicit child launch (local)\n  \
                             Runs COMMAND directly (no shell); `--` is required;\n  \
                             exit code is the child's. `bitty htop` never means\n  \
                             `bitty run -- htop`; colliding names need `run --`.\n  \
            ctl [--socket P] [--instance ID] [--format SHAPE] <resource> <verb>  Control a running instance (runtime)\n  \
                             instance|window|view|terminal list; terminal spawn|close|send|text;\n  \
                             view split|focus; config reload. `ctl --help` never needs an instance.\n  \
            config path      Print the resolved config file path (alias `cfg`)\n\
           config check     Load + validate; print per-key sources\n  \
                            (cli/file/default), exit non-zero on invalid files\n  \
            config edit      Open the file in $VISUAL/$EDITOR (vi fallback);\n  \
                             creates parents + starter when missing, never\n  \
                             overwrites existing content\n  \
             init [--yes] [--force] [VALUE FLAGS]  Opt-in guided setup wizard\n  \
                              (never auto-runs): mascot greeting, shell/theme/\n  \
                              font family+size/decoration gaps+border+radius/\n  \
                              scrollback/close_confirm picks, vim keybinding\n  \
                              preset, writes init.lua with shipped keys only.\n  \
                              --yes skips prompts (sane defaults); without\n  \
                              --force an existing file is never overwritten\n  \
                              (--force backs it up to init.lua.bak first).\n  \
                              Without a TTY on stdin, --yes is required\n  \
                              (otherwise exit 2; never a hang).\n  \
                              Honors --config PATH / BITTY_CONFIG as the target.\n  \
                              Value flags answer one step and skip its prompt:\n  \
                              --theme NAME, --font-family NAME, --font-size PTS,\n  \
                              --scrollback LINES, --close-confirm MODE,\n  \
                              --gaps-in PX, --gaps-out PX, --border PX,\n  \
                              --radius PX (each validated fail-closed).\n  \
            doctor [--format SHAPE] [--no-color]  Diagnose installation and\n  \
                              compatibility (local class, safe mode): binary,\n  \
                              config, keymaps, fonts, display, GPU, clipboard,\n  \
                              PTY, terminfo, shell, images, plugins.\n  \
                              Exit 0 when all pass (warns allowed), 1 on\n  \
                              recoverable failure, else the strongest\n  \
                              category code (3 config, 5 compat, 8 conflict).\n  \
             list <kind>      Enumerate resources: themes|plugins|instances\n  \
                              (--format table|json|jsonl, --socket/--instance for\n  \
                              instances; alias `ls`; `bitty list --help` for detail)\n  \
             inspect <target> <value>  Explain state and ownership (local, safe mode)\n  \
                              command|key|plugin|config|protocol\n  \
                              (--format table|json|jsonl; `bitty inspect --help`)\n  \
            dev <verb>       Developer tracing, captures, synthesis, dumps,\n  \
                             overlays\n  \
                             (trace|capture|synthesize|dump|overlay; local only, no\n  \
                             instance; `bitty dev --help` for detail)\n  \
           plugin <verb>     CLI-first plugin management (local, no VM):\n  \
                             list|install|remove|enable|disable|info over the\n  \
                             managed manifest (bitty-plugins.toml); install\n  \
                             requires capability consent; remove requires\n  \
                             --force; `bitty plugin --help` for detail\n  \
            cmd <qualified-id> [--format SHAPE] [-- <args-json>]  Direct qualified\n  \
                             executable invocation for automation/diagnostics\n  \
                             (e.g. `bitty cmd core.terminal.text --format json\n  \
                             -- '{{\"terminal_id\": \"t:4\"}}'`; `bitty cmd --help`)\n\
            x <publisher>.<name> <command> [args]  Qualified plugin namespace\n  \
                             (extension, no VM load; `bitty x --help` lists\n  \
                             installed plugins, `bitty x <id> --help` its\n  \
                             commands)\n  \
            completion <shell>  Emit shell completion script to stdout\n  \
                             (bash|zsh|fish|powershell|nushell; alias `comp`;\n  \
                             `bitty completion --help` for detail)\n  \
            version [--format SHAPE]  Version and build metadata:\n  \
                             `bitty <semver> (<channel> <commit>)` on stdout\n  \
                             (same fields in `--format json`; `-V`/`--version`\n  \
                             alias; local, no instance)\n\
         \n\
         Arguments:\n  \
           PROGRAM          Program to spawn inside the PTY (direct argv[0],\n  \
                            no shell interpolation). When omitted defaults to\n  \
                            $SHELL or /bin/sh; --headless still ticks.\n\
         \n\
         Layout:\n  \
           The app constructs a LayoutNode via bitty-ui and calls Runtime::set_layout.\n  \
           Precedence: --layout > --stack > --overlay > --split > single (default).\n  \
           Examples: --split, --split v, --split=h:0.3 --stack, --overlay,\n  \
                     --layout single, --layout split:h:0.5, --layout stack:3,\n  \
                     --layout overlay:5,5,20,10\n\
         \n\
         Focus:\n  \
           --focus moves focus after layout install (Direction via FocusDirection\n  \
           or numeric ViewId). Keyboard in real mode is keymap-driven (CTX-0153\n  \
           single-owner rule): a bound chord is consumed by its chrome action\n  \
           and never reaches the PTY; unbound keys (Tab, arrows, plain\n  \
           letters) always go to the shell. Defaults (Alt is the Mod):\n  \
           Alt+h/j/k/l and Ctrl+Alt+arrows move focus, Alt+1..9 jumps to\n  \
           view N, Alt+u/Alt+i pages up/down, Shift+Alt+h/j/k/l splits,\n  \
           Shift+Ctrl+h/j/k/l resizes, Alt+w closes, Alt+z/m/f zooms,\n  \
           Ctrl+Tab cycles, Ctrl+Shift+C/V copy/paste;\n  \
           see `bitty config check` for the active table.\n\
         \n\
         Modes:\n  \
           headless         Surface::headless software present, no display/GPU.\n  \
                            Triggered by --headless, BITTY_HEADLESS=1, or\n  \
                            App::run -> DisplayUnavailable fallback. Proves\n  \
                            split/stack/overlay composition deterministically\n  \
                            (separate runtimes same bytes/layout → identical RGBA;\n  \
                            different layouts → distinct RGBA). No window/GPU.\n  \
           real             App::run event loop with Window creation and\n  \
                            PlatformEvent -> Runtime::handle_platform_event\n  \
                            plus layout-aware tick -> present. GPU attach\n  \
                            (GpuContext + SurfaceTarget) is an honest env-gated\n  \
                            gap: runtime still presents via the headless seam\n  \
                            until the attach_gpu slice lands. See module docs.\n\
         \n\
          Config file (Lua, wezterm-style init.lua):\n  \
            return {{ theme = \"dark\", font = {{ family = \"JetBrainsMono Nerd Font\", size = 12 }} }}\n  \
                            Evaluated in the bitty-lua sandbox (same budgets as\n  \
                            plugins; no io/os). Unknown keys fail closed.\n  \
         \n\
         Examples:\n  \
           bitty --help\n  \
           bitty --version\n  \
           bitty run --help\n  \
           bitty run -- htop\n  \
           bitty run --cwd /tmp --env FOO=bar -- printenv FOO\n  \
           bitty --headless\n  \
           bitty --headless --split v --focus next\n  \
           bitty --headless --layout stack:2 --focus 2\n  \
           bitty --headless --layout overlay:5,5,20,10\n  \
            bitty --headless -- /bin/bash\n  \
            bitty /bin/bash\n  \
            bitty -- /bin/cat -A\n  \
           bitty doctor\n  \
           bitty doctor --format json\n  \
           bitty plugin list\n  \
           bitty plugin install bitty-terminal.tabs --yes\n",
        version_text()
    )
}

pub(crate) fn version_text() -> String {
    crate::version::version_text()
}
