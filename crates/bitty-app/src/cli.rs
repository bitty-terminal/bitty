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
    /// When true (`--safe`) never create a third-party plugin VM and never
    /// read the third-party store tree (recovery startup, RFC A.4 rule 6).
    pub(crate) safe: bool,
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
            safe: false,
            help: false,
            version: false,
            program: None,
            program_args: Vec::new(),
            unknown_flag: None,
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
            verbose: false,
            log_level: None,
        }
    }
}

/// Parses `raw` (including `argv[0]` at index 0) into [`Args`].
///
/// Recognised flags:
/// - `-h` / `--help` → help
/// - `-V` / `--version` → version
/// - `--headless` → headless smoke (also triggered by `BITTY_HEADLESS=1`)
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
/// - `run [OPTIONS] -- COMMAND...` → explicit child launch (CTX-0170);
///   a program literally named `run` needs `bitty run -- run ...` or
///   `bitty -- run ...`. Tokens after `run` are kept verbatim for
///   `run::parse_run_request`, which requires `--` before COMMAND.
/// - `config <path|check|edit>` → config subcommand (DEC-0007); a program
///   literally named `config` needs `bitty -- config ...`
/// - `init [--yes] [--force]` → opt-in setup wizard (#243, CTX-0149);
///   a program literally named `init` needs `bitty -- init ...`
/// - `doctor [--format table|json|jsonl] [--no-color]` → installation and
///   compatibility diagnosis (CTX-0175, local class, safe mode); a program
///   literally named `doctor` needs `bitty -- doctor ...`
/// - `list <themes|plugins|instances>` → resource enumeration (CTX-0172);
///   a program literally named `list`/`ls` needs `bitty -- list ...`
/// - `inspect <target> <value>` → state and ownership (CTX-0173, local
///   class, safe mode); a program literally named `inspect` needs
///   `bitty -- inspect ...`
/// - `--format SHAPE` → doctor/ctl/list/inspect output shape (parsed
///   globally, consumed by each subcommand dispatch; ignored by startup)
/// - `--no-color` → disable ANSI coloring in doctor/list table output
///   (accepted by inspect for parity; its tables are plain text)
/// - `--yes` / `--force` are init-only flags (parsed globally, consumed by
///   the init dispatch; ignored by normal startup)
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
        if token.starts_with("--split-ratio=") {
            let val = token.trim_start_matches("--split-ratio=");
            if let Ok(f) = val.parse::<f32>() {
                out.split_ratio = Some(f);
            } else {
                eprintln!("warning: invalid --split-ratio value {val:?} — ignoring");
            }
            i += 1;
            continue;
        }
        if token.starts_with("--split=") {
            let val = token.trim_start_matches("--split=");
            // val may be "h:0.3" or "horizontal" etc.
            let (axis, ratio) = parse_split_token(val);
            if let Some(ax) = axis {
                out.split_axis = Some(ax);
            } else if !val.is_empty() {
                eprintln!("warning: unknown --split axis {val:?} — defaulting to horizontal");
                out.split_axis = Some(SplitAxis::Horizontal);
            } else {
                out.split_axis = Some(SplitAxis::Horizontal);
            }
            if let Some(r) = ratio {
                out.split_ratio = Some(r);
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
                None => eprintln!(
                    "warning: unknown --log-level {val:?} (want error|warn|info|debug|trace) — ignoring"
                ),
            }
            i += 1;
            continue;
        }
        match token.as_str() {
            "--" => {
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
            "--safe" => {
                out.safe = true;
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
            "--format" => {
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
                        if let Some(ax) = axis {
                            out.split_axis = Some(ax);
                        } else {
                            // axis parse failed but ratio present? Keep default axis
                            out.split_axis = Some(SplitAxis::Horizontal);
                        }
                        if let Some(r) = ratio {
                            out.split_ratio = Some(r);
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
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let next = &raw[i + 1];
                    if let Ok(f) = next.parse::<f32>() {
                        out.split_ratio = Some(f);
                    } else {
                        eprintln!("warning: invalid --split-ratio value {next:?} — ignoring");
                    }
                    i += 2;
                } else {
                    eprintln!("warning: --split-ratio needs a numeric value — ignoring");
                    i += 1;
                }
            }
            "--layout" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.layout = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --layout needs a value (single|split:h[:ratio]|stack[:n]|overlay[:x,y,w,h]) — ignoring"
                    );
                    i += 1;
                }
            }
            "--focus" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.focus = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --focus needs a value (next|prev|up|down|left|right|<id>) — ignoring"
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
                        None => eprintln!(
                            "warning: unknown --log-level {val:?} (want error|warn|info|debug|trace) — ignoring"
                        ),
                    }
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --log-level needs a value (error|warn|info|debug|trace) — ignoring"
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
                if !program_set
                    && !out.config_word
                    && !out.doctor_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "config"
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
                             (default warn; tick stats need debug|trace;\n  \
                             also BITTY_LOG/RUST_LOG)\n  \
               --headless   Run a single headless tick smoke and exit (CI)\n  \
               --safe       Do not load third-party plugins (no plugin VM)\n  \
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
                 --format SHAPE  Doctor/ctl/list/inspect/plugin output shape:\n  \
                              table|json|jsonl (default table; parsed globally,\n  \
                              consumed by `bitty doctor`, `bitty ctl`,\n  \
                              `bitty list`, `bitty inspect`, and\n  \
                              `bitty plugin list|info`; ignored by startup).\n  \
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
            config path      Print the resolved config file path\n  \
           config check     Load + validate; print per-key sources\n  \
                            (cli/file/default), exit non-zero on invalid files\n  \
            config edit      Open the file in $VISUAL/$EDITOR (vi fallback);\n  \
                             creates parents + starter when missing, never\n  \
                             overwrites existing content\n  \
             init [--yes] [--force]  Opt-in interactive setup wizard (never\n  \
                              auto-runs): mascot greeting, shell/theme/font-size\n  \
                              picks, vim keybinding preset, writes init.lua.\n  \
                              --yes skips prompts (sane defaults); without\n  \
                              --force an existing file is never overwritten\n  \
                              (--force backs it up to init.lua.bak first).\n  \
                              Honors --config PATH / BITTY_CONFIG as the target.\n  \
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
            dev <verb>       Developer tracing, captures, dumps, overlays\n  \
                             (trace|capture|dump|overlay; local only, no\n  \
                             instance; `bitty dev --help` for detail)\n  \
           plugin <verb>     CLI-first plugin management (local, no VM):\n  \
                             list|install|remove|enable|disable|info over the\n  \
                             managed manifest (bitty-plugins.toml); install\n  \
                             requires capability consent; remove requires\n  \
                             --force; `bitty plugin --help` for detail\n  \
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
    env!("CARGO_PKG_VERSION").to_string()
}
