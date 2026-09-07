//! `bitty ctl` runtime control over `BITTY_SOCKET` (CTX-0171).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`ctl` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty ctl`, runtime class) and
//! `docs/specifications/ipc-agent-rfc.md` (instance selection, scopes).
//!
//! # Contract (implemented)
//!
//! Shape: `bitty ctl [--socket <path>] [--instance <id>] [--format <shape>]`
//! `<resource> <verb> [args]` where `--format` is `table|json|jsonl`
//! (default `table`). `--help` never requires an instance.
//!
//! ```sh
//! bitty ctl instance list
//! bitty ctl window list
//! bitty ctl view list --format json
//! bitty ctl terminal list --format json
//! bitty ctl terminal spawn [--cwd <path>]
//! bitty ctl terminal close t:3
//! bitty ctl terminal send t:1 "cargo test"
//! bitty ctl terminal text t:1 --format json
//! bitty ctl view split --right
//! bitty ctl view focus v:3
//! bitty ctl config reload
//! ```
//!
//! Every verb maps one-to-one to a registry executable (e.g.
//! `bitty ctl view split` maps to `core.view.split`). Arguments are validated
//! against bounds before any IPC frame is sent. Instance selection precedence
//! is exactly the RFC order: explicit `--socket`, then `--instance`, then
//! inherited `BITTY_SOCKET` / `BITTY_INSTANCE_ID`, then the exactly-one-live
//! shortcut, otherwise an ambiguity error (exit 6, never a silent pick).
//!
//! # Scope discipline (never ambient authority)
//!
//! Scopes are evaluated server-side on every request from the authenticated
//! identity; the client never asserts scopes in the envelope (the devtools
//! envelope rejects `auth`/`scope`/`role` outright). Each verb requires:
//!
//! - `terminal list|text`: `terminal.inspect`
//! - `terminal send`: `terminal.input`
//! - `terminal spawn|close`: `terminal.manage` (explicit elevation)
//! - `view list`: `view.inspect`; `view split|focus`: `view.manage`
//! - `window list`: `view.inspect`
//! - `config reload`: `config.modify` (explicit elevation)
//! - `instance list`: local discovery, no IPC, no scope (same-UID only)
//!
//! The live servo grants the CLI default (`terminal.inspect/input`,
//! `view.inspect/manage`, `config.inspect`, `plugin.inspect`) to the
//! authenticated same-UID peer. `terminal.manage` and `config.modify`
//! require explicit elevation via the pre-granted allowlist
//! `BITTY_CTL_ELEVATE` (comma-separated scopes, e.g.
//! `BITTY_CTL_ELEVATE=terminal.manage,config.modify`). Without elevation
//! those verbs fail closed with `Denied/ScopeViolation` (exit 7) and no
//! partial state. `BITTY_CTL_ELEVATE` is the RFC's pre-granted allowlist
//! surface for headless/test use; interactive confirmation prompts remain
//! a follow-up.
//!
//! # Output and exit codes (stable v1 per RFC)
//!
//! Stdout carries the result; stderr carries diagnostics. JSON output is
//! never corrupted by logs. Envelope v1:
//! `{"v":1,"command":"<registry>","ok":true,"result":{...}}` on success,
//! `{"v":1,"command":"<registry>","ok":false,"error":{"class":"...","code":"...","message":"..."}}`
//! on failure. Exit codes: 0 success, 1 generic, 2 usage, 3 config,
//! 5 compat, 6 unavailable, 7 permission, 8 conflict.
//!
//! # Honest scope of this slice
//!
//! - `terminal send` routes to the focused leaf only (the runtime's current
//!   routing); sending to a non-focused `t:N` returns `Conflict` naming
//!   `bitty ctl view focus v:N` first. `t:N` maps 1:1 to view `v:N` until
//!   the terminal registry lands.
//! - `terminal spawn --cwd` validates `--cwd` and fails closed when missing,
//!   but the spawn itself uses the default shell without chdir (cwd
//!   honoring is a follow-up; a stderr note names the gap when `--cwd`
//!   is given).
//! - `config reload` validates the config file and reports its path;
//!   live theme/font hot-swap is a follow-up (the response names this).
//! - Terminal output is untrusted observation data, never instructions.

#![forbid(unsafe_code)]

use bitty_ipc::ctl as ipc_ctl;

// ── exit codes (stable v1) ────────────────────────────────────────────────

/// Success.
pub const EXIT_OK: i32 = 0;
/// Generic error after parsing and scope checks.
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error (unknown flag, verb, schema violation before dispatch).
pub const EXIT_USAGE: i32 = 2;
/// Configuration error (`config reload` validation failure).
pub const EXIT_CONFIG: i32 = 3;
/// Compatibility error (version mismatch).
pub const EXIT_COMPAT: i32 = 5;
/// IPC or runtime unavailable (no/ambiguous instance, socket, framing).
pub const EXIT_RUNTIME: i32 = 6;
/// Permission denied (unauthenticated or scope violation).
pub const EXIT_PERM: i32 = 7;
/// Conflict (resource busy, not-focused send, alias collision).
pub const EXIT_CONFLICT: i32 = 8;

// ── output format ─────────────────────────────────────────────────────────

/// `--format` shape (default `table`; `table` is human, not a contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CtlFormat {
    #[default]
    Table,
    Json,
    Jsonl,
}

impl CtlFormat {
    /// Parse `--format` (`None` means the table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).map(|s| s.to_ascii_lowercase()) {
            None => Ok(Self::Table),
            Some(s) if s == "table" => Ok(Self::Table),
            Some(s) if s == "json" => Ok(Self::Json),
            Some(s) if s == "jsonl" => Ok(Self::Jsonl),
            Some(other) => Err(format!(
                "bitty ctl: unknown --format {other:?} (want table|json|jsonl)"
            )),
        }
    }
}

// ── request model ─────────────────────────────────────────────────────────

/// Targeting overrides for instance selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtlTargeting {
    /// Explicit `--socket <path>` (bypasses discovery).
    pub socket: Option<String>,
    /// Explicit `--instance <id>` (resolved via discovery file).
    pub instance: Option<String>,
    /// Output shape.
    pub format: CtlFormat,
}

/// One validated `bitty ctl` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlRequest {
    InstanceList,
    WindowList,
    ViewList,
    TerminalList,
    TerminalSpawn { cwd: Option<String> },
    TerminalClose { terminal_id: String },
    TerminalSend { terminal_id: String, text: String },
    TerminalText { terminal_id: String },
    ViewSplit { direction: ipc_ctl::SplitDirection },
    ViewFocus { view_id: String },
    ConfigReload,
}

impl CtlRequest {
    /// Registry executable id dispatched (for the output envelope).
    #[must_use]
    pub fn registry_id(&self) -> &'static str {
        match self {
            Self::InstanceList => "core.instance.list",
            Self::WindowList => "core.window.list",
            Self::ViewList => "core.view.list",
            Self::TerminalList => "core.terminal.list",
            Self::TerminalSpawn { .. } => "core.terminal.spawn",
            Self::TerminalClose { .. } => "core.terminal.close",
            Self::TerminalSend { .. } => "core.terminal.send",
            Self::TerminalText { .. } => "core.terminal.text",
            Self::ViewSplit { .. } => "core.view.split",
            Self::ViewFocus { .. } => "core.view.focus",
            Self::ConfigReload => "core.config.reload",
        }
    }

    /// Wire method for IPC verbs (`None` for local `instance list`).
    #[must_use]
    pub fn wire_method(&self) -> Option<&'static str> {
        match self {
            Self::InstanceList => None,
            Self::WindowList => Some(ipc_ctl::METHOD_LIST_WINDOWS),
            Self::ViewList => Some(ipc_ctl::METHOD_LIST_VIEWS),
            Self::TerminalList => Some(ipc_ctl::METHOD_LIST_TERMINALS),
            Self::TerminalSpawn { .. } => Some(ipc_ctl::METHOD_SPAWN_TERMINAL),
            Self::TerminalClose { .. } => Some(ipc_ctl::METHOD_CLOSE_TERMINAL),
            Self::TerminalSend { .. } => Some(ipc_ctl::METHOD_SEND_INPUT),
            Self::TerminalText { .. } => Some(ipc_ctl::METHOD_GET_TERMINAL_TEXT),
            Self::ViewSplit { .. } => Some(ipc_ctl::METHOD_SPLIT_VIEW),
            Self::ViewFocus { .. } => Some(ipc_ctl::METHOD_FOCUS_VIEW),
            Self::ConfigReload => Some(ipc_ctl::METHOD_RELOAD_CONFIG),
        }
    }

    /// Params JSON for the wire method (`None` means `{}`/absent).
    #[must_use]
    pub fn wire_params(&self) -> Option<String> {
        match self {
            Self::InstanceList
            | Self::WindowList
            | Self::ViewList
            | Self::TerminalList
            | Self::ConfigReload => None,
            Self::TerminalSpawn { cwd } => Some(ipc_ctl::params_spawn(cwd.as_deref())),
            Self::TerminalClose { terminal_id } => Some(ipc_ctl::params_terminal_id(terminal_id)),
            Self::TerminalSend { terminal_id, text } => {
                Some(ipc_ctl::params_send_input(terminal_id, text))
            }
            Self::TerminalText { terminal_id } => Some(ipc_ctl::params_terminal_id(terminal_id)),
            Self::ViewSplit { direction } => Some(ipc_ctl::params_split(*direction)),
            Self::ViewFocus { view_id } => Some(ipc_ctl::params_focus(view_id)),
        }
    }
}

/// `bitty ctl` parse failure. Every variant except `Help` maps to stderr
/// plus exit 2 (`UsageError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlParseError {
    /// `-h` / `--help` (print help to stdout, exit 0).
    Help,
    /// Usage violation with a one-line diagnostic.
    Usage { message: String },
}

impl CtlParseError {
    /// One-line stderr diagnostic (without the usage trailer).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Help => String::from("bitty ctl: help requested"),
            Self::Usage { message } => message.clone(),
        }
    }
}

// ── usage / help ──────────────────────────────────────────────────────────

/// Short usage for stderr (fail-closed exit 2 trailer).
#[must_use]
pub fn ctl_usage() -> String {
    String::from(
        "usage: bitty ctl [--socket PATH] [--instance ID] [--format table|json|jsonl] <resource> <verb> [args]\n\
         \n\
         resources:\n\
         \x20 instance list\n\
         \x20 window list\n\
         \x20 view list | view split [--left|--right|--up|--down] | view focus v:N\n\
         \x20 terminal list | terminal spawn [--cwd PATH] | terminal close t:N\n\
         \x20 terminal send t:N TEXT | terminal text t:N\n\
         \x20 config reload\n\
         examples:\n\
         \x20 bitty ctl instance list\n\
         \x20 bitty ctl terminal send t:1 \"cargo test\"\n\
         \x20 bitty ctl view split --right\n\
         \x20 bitty ctl config reload",
    )
}

/// Full help for `bitty ctl --help` (stdout, exit 0).
#[must_use]
pub fn ctl_help_text() -> String {
    format!(
        "bitty ctl — control a running instance (runtime, needs one live instance)\n\
         \n\
         Usage: bitty ctl [--socket PATH] [--instance ID] [--format SHAPE] <resource> <verb> [args]\n\
         \n\
         Targeting (precedence: --socket, --instance, BITTY_SOCKET/BITTY_INSTANCE_ID,\n\
         \x20 exactly-one-live fallback, else ambiguity error exit 6):\n  \
           --socket PATH    Explicit IPC socket; bypasses discovery\n  \
           --instance ID    Explicit instance (1..=64 [a-z0-9_-]); resolved via discovery\n  \
           --format SHAPE   Output shape: table|json|jsonl (default table)\n  \
           -h, --help       Print this help and exit (never needs an instance)\n\
         \n\
         Verbs (each maps to one registry executable; scopes enforced server-side):\n  \
           instance list                 Local discovery (no IPC; same-UID sockets only)\n  \
           window list                   core.window.list (view.inspect)\n  \
           view list                     core.view.list (view.inspect)\n  \
           terminal list                 core.terminal.list (terminal.inspect)\n  \
           terminal spawn [--cwd PATH]   core.terminal.spawn (terminal.manage, elevation)\n  \
           terminal close t:N            core.terminal.close (terminal.manage, elevation)\n  \
           terminal send t:N TEXT        core.terminal.send (terminal.input; focused leaf only)\n  \
           terminal text t:N             core.terminal.text (terminal.inspect; untrusted output)\n  \
           view split [--dir]            core.view.split (view.manage; default --right)\n  \
           view focus v:N                core.view.focus (view.manage)\n  \
           config reload                 core.config.reload (config.modify, elevation)\n\
         \n\
         Elevation: terminal.manage and config.modify need BITTY_CTL_ELEVATE\n\
         \x20 (comma-separated scopes, e.g. BITTY_CTL_ELEVATE=terminal.manage,config.modify).\n\
         \x20 Without it those verbs fail closed (exit 7, no partial state).\n\
         \n\
         Exit codes: 0 ok; 1 generic; 2 usage; 3 config; 5 compat; 6 unavailable;\n\
         \x20 7 permission; 8 conflict. Terminal text is untrusted observation data.\n\
         \n\
         Version: {}\n",
        env!("CARGO_PKG_VERSION"),
    )
}

// ── parsing ───────────────────────────────────────────────────────────────

/// Parse `tokens` (argv words after the `ctl` word) into a request plus targeting.
///
/// Pure and total for unit testing: no env/fs/net. Global `--socket`,
/// `--instance`, `--format` may appear before or after the resource word;
/// resource/verb matching is case-sensitive lowercase. Every bound violation
/// becomes [`CtlParseError::Usage`]; `-h`/`--help` anywhere becomes `Help`.
pub fn parse_ctl_request(tokens: &[String]) -> Result<(CtlRequest, CtlTargeting), CtlParseError> {
    let mut socket: Option<String> = None;
    let mut instance: Option<String> = None;
    let mut format_raw: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut split_dir: Option<ipc_ctl::SplitDirection> = None;
    let mut spawn_cwd: Option<String> = None;

    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(CtlParseError::Help);
        }
        if token == "--socket" {
            let value = tokens.get(i + 1).ok_or_else(|| CtlParseError::Usage {
                message: String::from("bitty ctl: --socket needs a path (see `bitty ctl --help`)"),
            })?;
            if value == "--" || value.starts_with('-') && value.len() > 1 && !value.contains('/') {
                // Allow absolute paths starting with `-`? No: `--socket` needs a real path.
                // A value starting with `-` is almost certainly a misplaced flag.
                if !value.starts_with('/') && !value.starts_with('.') {
                    return Err(CtlParseError::Usage {
                        message: format!(
                            "bitty ctl: --socket needs a path, got {value:?} (see `bitty ctl --help`)"
                        ),
                    });
                }
            }
            validate_socket_override(value)?;
            socket = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--socket=") {
            if value.is_empty() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: --socket needs a path (see `bitty ctl --help`)",
                    ),
                });
            }
            validate_socket_override(value)?;
            socket = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--instance" {
            let value = tokens.get(i + 1).ok_or_else(|| CtlParseError::Usage {
                message: String::from("bitty ctl: --instance needs an id (see `bitty ctl --help`)"),
            })?;
            validate_instance_override(value)?;
            instance = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--instance=") {
            if value.is_empty() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: --instance needs an id (see `bitty ctl --help`)",
                    ),
                });
            }
            validate_instance_override(value)?;
            instance = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--format" {
            let value = tokens.get(i + 1).ok_or_else(|| CtlParseError::Usage {
                message: String::from("bitty ctl: --format needs a value (table|json|jsonl)"),
            })?;
            // Validate eagerly so `ctl view list --format bogus` is exit 2.
            CtlFormat::parse(Some(value)).map_err(|message| CtlParseError::Usage { message })?;
            format_raw = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--format=") {
            CtlFormat::parse(Some(value)).map_err(|message| CtlParseError::Usage { message })?;
            format_raw = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--cwd" {
            let value = tokens.get(i + 1).ok_or_else(|| CtlParseError::Usage {
                message: String::from("bitty ctl: --cwd needs a path (terminal spawn only)"),
            })?;
            ipc_ctl::validate_ctl_cwd(value).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid --cwd: {err}"),
            })?;
            spawn_cwd = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--cwd=") {
            if value.is_empty() {
                return Err(CtlParseError::Usage {
                    message: String::from("bitty ctl: --cwd needs a path (terminal spawn only)"),
                });
            }
            ipc_ctl::validate_ctl_cwd(value).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid --cwd: {err}"),
            })?;
            spawn_cwd = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--left" || token == "--right" || token == "--up" || token == "--down" {
            if split_dir.is_some() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: view split takes exactly one direction (see `bitty ctl --help`)",
                    ),
                });
            }
            let dir = token.trim_start_matches("--");
            split_dir =
                Some(
                    ipc_ctl::SplitDirection::parse(dir).map_err(|_| CtlParseError::Usage {
                        message: format!("bitty ctl: unknown split direction {token:?}"),
                    })?,
                );
            i += 1;
            continue;
        }
        if token == "--" {
            return Err(CtlParseError::Usage {
                message: String::from("bitty ctl: unexpected `--` (see `bitty ctl --help`)"),
            });
        }
        if token.starts_with('-') {
            return Err(CtlParseError::Usage {
                message: format!("bitty ctl: unknown flag {token:?} (see `bitty ctl --help`)"),
            });
        }
        positionals.push(token.clone());
        i += 1;
    }

    let format = CtlFormat::parse(format_raw.as_deref())
        .map_err(|message| CtlParseError::Usage { message })?;
    let targeting = CtlTargeting {
        socket,
        instance,
        format,
    };

    // Resource/verb dispatch (case-sensitive, lowercase canonical).
    let resource = positionals.first().map(String::as_str);
    let verb = positionals.get(1).map(String::as_str);
    let rest = if positionals.len() > 2 {
        &positionals[2..]
    } else {
        &[][..]
    };
    match (resource, verb) {
        (None, _) => Err(CtlParseError::Usage {
            message: String::from("bitty ctl: missing <resource> <verb> (see `bitty ctl --help`)"),
        }),
        (Some(_), None) => Err(CtlParseError::Usage {
            message: String::from("bitty ctl: missing <verb> (see `bitty ctl --help`)"),
        }),
        (Some("instance"), Some("list")) => {
            reject_extra(rest, "instance list")?;
            reject_ctl_options_for("instance list", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::InstanceList, targeting))
        }
        (Some("window"), Some("list")) => {
            reject_extra(rest, "window list")?;
            reject_ctl_options_for("window list", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::WindowList, targeting))
        }
        (Some("view"), Some("list")) => {
            reject_extra(rest, "view list")?;
            reject_ctl_options_for("view list", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::ViewList, targeting))
        }
        (Some("terminal"), Some("list")) => {
            reject_extra(rest, "terminal list")?;
            reject_ctl_options_for("terminal list", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::TerminalList, targeting))
        }
        (Some("terminal"), Some("spawn")) => {
            reject_extra(rest, "terminal spawn")?;
            if split_dir.is_some() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: --left/--right/--up/--down belong to `view split`, not `terminal spawn`",
                    ),
                });
            }
            Ok((CtlRequest::TerminalSpawn { cwd: spawn_cwd }, targeting))
        }
        (Some("terminal"), Some("close")) => {
            reject_ctl_options_for("terminal close", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "terminal close", "t:N (e.g. t:3)")?;
            ipc_ctl::parse_terminal_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid terminal id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::TerminalClose {
                    terminal_id: id.to_string(),
                },
                targeting,
            ))
        }
        (Some("terminal"), Some("send")) => {
            reject_ctl_options_for("terminal send", split_dir.is_some(), spawn_cwd.is_some())?;
            if rest.is_empty() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: terminal send needs t:N and TEXT (e.g. terminal send t:1 \"cargo test\")",
                    ),
                });
            }
            let id = &rest[0];
            ipc_ctl::parse_terminal_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid terminal id {id:?}: {err}"),
            })?;
            if rest.len() < 2 {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: terminal send needs TEXT after t:N (e.g. terminal send t:1 \"cargo test\")",
                    ),
                });
            }
            // Join tail with single spaces so both quoted and bare forms work;
            // the bound applies to the joined text.
            let text = rest[1..].join(" ");
            ipc_ctl::validate_send_text(&text).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid send TEXT: {err}"),
            })?;
            Ok((
                CtlRequest::TerminalSend {
                    terminal_id: id.to_string(),
                    text,
                },
                targeting,
            ))
        }
        (Some("terminal"), Some("text")) => {
            reject_ctl_options_for("terminal text", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "terminal text", "t:N (e.g. t:1)")?;
            ipc_ctl::parse_terminal_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid terminal id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::TerminalText {
                    terminal_id: id.to_string(),
                },
                targeting,
            ))
        }
        (Some("view"), Some("split")) => {
            reject_extra(rest, "view split")?;
            if spawn_cwd.is_some() {
                return Err(CtlParseError::Usage {
                    message: String::from(
                        "bitty ctl: --cwd belongs to `terminal spawn`, not `view split`",
                    ),
                });
            }
            Ok((
                CtlRequest::ViewSplit {
                    direction: split_dir.unwrap_or(ipc_ctl::SplitDirection::Right),
                },
                targeting,
            ))
        }
        (Some("view"), Some("focus")) => {
            reject_ctl_options_for("view focus", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "view focus", "v:N (e.g. v:3)")?;
            ipc_ctl::parse_view_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid view id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::ViewFocus {
                    view_id: id.to_string(),
                },
                targeting,
            ))
        }
        (Some("config"), Some("reload")) => {
            reject_extra(rest, "config reload")?;
            reject_ctl_options_for("config reload", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::ConfigReload, targeting))
        }
        (Some(r), Some(v)) => Err(CtlParseError::Usage {
            message: format!("bitty ctl: unknown {r} {v} (see `bitty ctl --help`)"),
        }),
    }
}

fn single_arg<'a>(rest: &'a [String], what: &str, want: &str) -> Result<&'a str, CtlParseError> {
    match rest {
        [one] => Ok(one.as_str()),
        [] => Err(CtlParseError::Usage {
            message: format!("bitty ctl: {what} needs {want}"),
        }),
        _ => Err(CtlParseError::Usage {
            message: format!("bitty ctl: {what} takes exactly one argument ({want})"),
        }),
    }
}

fn reject_extra(rest: &[String], what: &str) -> Result<(), CtlParseError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(CtlParseError::Usage {
            message: format!("bitty ctl: {what} takes no arguments (got {:?})", rest[0]),
        })
    }
}

fn reject_ctl_options_for(what: &str, has_split: bool, has_cwd: bool) -> Result<(), CtlParseError> {
    if has_split {
        return Err(CtlParseError::Usage {
            message: format!(
                "bitty ctl: --left/--right/--up/--down belong to `view split`, not `{what}`"
            ),
        });
    }
    if has_cwd {
        return Err(CtlParseError::Usage {
            message: format!("bitty ctl: --cwd belongs to `terminal spawn`, not `{what}`"),
        });
    }
    Ok(())
}

fn validate_socket_override(path: &str) -> Result<(), CtlParseError> {
    if path.is_empty() {
        return Err(CtlParseError::Usage {
            message: String::from("bitty ctl: --socket needs a path (see `bitty ctl --help`)"),
        });
    }
    if path.contains('\0') {
        return Err(CtlParseError::Usage {
            message: String::from("bitty ctl: --socket must not contain NUL"),
        });
    }
    if path.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
        return Err(CtlParseError::Usage {
            message: format!(
                "bitty ctl: --socket path too long for AF_UNIX ({} > {} payload bytes)",
                path.len(),
                bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES
            ),
        });
    }
    Ok(())
}

fn validate_instance_override(id: &str) -> Result<(), CtlParseError> {
    if id.is_empty() || id.len() > bitty_ipc::devtools::MAX_INSTANCE_ID_LEN {
        return Err(CtlParseError::Usage {
            message: format!(
                "bitty ctl: --instance must be 1..={} (got {} bytes)",
                bitty_ipc::devtools::MAX_INSTANCE_ID_LEN,
                id.len()
            ),
        });
    }
    let ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(CtlParseError::Usage {
            message: String::from("bitty ctl: --instance must match ^[a-z0-9_-]+$"),
        });
    }
    Ok(())
}

// ── output envelope ───────────────────────────────────────────────────────

/// Escape a string for embedding in JSON output.
#[must_use]
pub fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Render a success envelope (`v: 1`) for `--format json`/`jsonl`.
#[must_use]
pub fn format_success(registry: &str, result_json: &str) -> String {
    format!("{{\"v\":1,\"command\":\"{registry}\",\"ok\":true,\"result\":{result_json}}}")
}

/// Render a failure envelope (`v: 1`) for `--format json`/`jsonl`.
#[must_use]
pub fn format_failure(registry: &str, class: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"{registry}\",\"ok\":false,\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}}}}",
        json_escape(class),
        json_escape(code),
        json_escape(message)
    )
}

/// Map a server error `(category, code)` to the stable CLI exit code.
///
/// - `usage/*` after IPC is a generic argument failure (exit 1); usage
///   *before* IPC never reaches here (exit 2 at parse).
/// - `auth/*` scope denials and `Denied` are permission (exit 7).
/// - Transport/framing/rate-limit/timeout/unavailable are exit 6.
/// - `Conflict` is exit 8; `ConfigError` is exit 3; `VersionMismatch` is 5.
#[must_use]
pub fn exit_for_server_error(category: &str, code: &str) -> i32 {
    match (category, code) {
        (_, "ScopeDenied" | "ScopeViolation" | "ForbiddenField") => EXIT_PERM,
        ("auth", _) => EXIT_PERM,
        (_, "Unauthenticated") => EXIT_PERM,
        (_, "Denied") => EXIT_PERM,
        (_, "Conflict") => EXIT_CONFLICT,
        (_, "ConfigError" | "InvalidConfig") => EXIT_CONFIG,
        (_, "UnsupportedVersion" | "VersionMismatch") => EXIT_COMPAT,
        ("transport", _) | (_, "FrameTooLarge" | "PayloadTooLarge" | "PayloadCap") => EXIT_RUNTIME,
        ("budget", _) | (_, "RateLimited") => EXIT_RUNTIME,
        (_, "Timeout" | "Unavailable" | "Transport") => EXIT_RUNTIME,
        ("usage", "NotFound" | "UnknownMethod") => EXIT_GENERIC,
        ("usage", _) => EXIT_GENERIC,
        _ => EXIT_GENERIC,
    }
}

/// Map a server error `(category, code)` to the envelope error class.
#[must_use]
pub fn class_for_server_error(category: &str, code: &str) -> &'static str {
    match (category, code) {
        (_, "ScopeDenied" | "ScopeViolation" | "ForbiddenField" | "Denied" | "Unauthenticated") => {
            "Denied"
        }
        (_, "Conflict") => "Conflict",
        (_, "ConfigError" | "InvalidConfig") => "ConfigError",
        (_, "UnsupportedVersion" | "VersionMismatch") => "VersionMismatch",
        ("transport", _)
        | ("budget", _)
        | (
            _,
            "FrameTooLarge" | "PayloadTooLarge" | "PayloadCap" | "RateLimited" | "Timeout"
            | "Unavailable" | "Transport",
        ) => "Unavailable",
        ("usage", "NotFound" | "UnknownMethod") => "NotFound",
        _ => "Error",
    }
}

// ── socket resolution (client) ────────────────────────────────────────────

/// Resolved IPC target: socket path plus a human label for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Socket path to connect.
    pub socket_path: String,
    /// Instance label (for table output and errors).
    pub instance: String,
}

/// Resolve the socket path per the RFC precedence: explicit `--socket`,
/// then `--instance`, then inherited `BITTY_SOCKET` / `BITTY_INSTANCE_ID`,
/// then the exactly-one-live shortcut, else ambiguity (exit 6).
///
/// Pure over injected env (no process-env access) so tests stay hermetic.
/// `uid` seeds the last-resort base `/run/user/<uid>`; filesystem discovery
/// (listing live sockets) runs only when no explicit target selects one.
pub fn resolve_ctl_target(
    targeting: &CtlTargeting,
    env_socket: Option<&str>,
    env_instance: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    uid: u32,
) -> Result<ResolvedTarget, String> {
    // 1. Explicit --socket bypasses all discovery (fails closed on shape;
    //    authentication happens at connect via socket modes + UID).
    if let Some(sock) = targeting.socket.as_deref() {
        if sock.is_empty() || sock.contains('\0') {
            return Err(String::from("bitty ctl: --socket is not a usable path"));
        }
        if sock.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "bitty ctl: --socket path too long for AF_UNIX ({} > {} payload bytes)",
                sock.len(),
                bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES
            ));
        }
        let instance = targeting
            .instance
            .clone()
            .or_else(|| env_instance.filter(|s| !s.is_empty()).map(String::from))
            .unwrap_or_else(|| String::from("default"));
        return Ok(ResolvedTarget {
            socket_path: sock.to_string(),
            instance,
        });
    }
    // 2. Explicit --instance resolves via the discovery file layout.
    if let Some(id) = targeting.instance.as_deref() {
        let path = bitty_ipc::devtools::resolve_socket_path(uid, xdg_runtime_dir, None, Some(id))
            .map_err(|err| format!("bitty ctl: cannot resolve --instance {id:?}: {err}"))?;
        return Ok(ResolvedTarget {
            socket_path: path,
            instance: id.to_string(),
        });
    }
    // 3. Inherited advisory context (still authenticated at connect).
    let has_env_socket = env_socket.is_some_and(|s| !s.is_empty());
    let has_env_base = xdg_runtime_dir.is_some_and(|s| !s.is_empty());
    if has_env_socket || env_instance.is_some_and(|s| !s.is_empty()) || has_env_base {
        let path = bitty_ipc::devtools::resolve_socket_path(
            uid,
            xdg_runtime_dir,
            env_socket.filter(|s| !s.is_empty()),
            env_instance.filter(|s| !s.is_empty()),
        )
        .map_err(|err| format!("bitty ctl: cannot resolve inherited target: {err}"))?;
        let instance = env_instance
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string();
        return Ok(ResolvedTarget {
            socket_path: path,
            instance,
        });
    }
    // 4. Exactly-one-live shortcut: enumerate candidate sockets and require
    //    exactly one live peer. Zero or many is an ambiguity error (exit 6),
    //    never a silent pick.
    let candidates = discover_live_sockets(xdg_runtime_dir, uid);
    match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(String::from(
            "bitty ctl: no live instance (set --socket or --instance, or run bitty first)",
        )),
        many => {
            let mut names: Vec<String> = many.iter().map(|c| c.instance.clone()).collect();
            names.sort();
            Err(format!(
                "bitty ctl: ambiguous instance ({} live: {}); pass --socket or --instance (see `bitty ctl instance list`)",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

/// Base directory for socket discovery (`XDG_RUNTIME_DIR` or `/run/user/<uid>`).
#[cfg(unix)]
fn discovery_base(xdg_runtime_dir: Option<&str>, uid: u32) -> Option<String> {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => Some(dir.to_string()),
        _ => Some(format!("/run/user/{uid}")),
    }
}

/// Enumerate live sockets under `<base>/bitty/*.sock`.
///
/// A candidate is live when `connect` succeeds; stale files (refused) are
/// skipped, never removed here (the servo reclaims on bind). Best-effort:
/// unreadable directories yield no candidates (ambiguity error downstream).
#[cfg(unix)]
fn discover_live_sockets(xdg_runtime_dir: Option<&str>, uid: u32) -> Vec<ResolvedTarget> {
    use std::os::unix::net::UnixStream;

    let Some(base) = discovery_base(xdg_runtime_dir, uid) else {
        return Vec::new();
    };
    let leaf = std::path::Path::new(&base).join(bitty_ipc::devtools::SOCKET_LEAF_DIR);
    let entries = std::fs::read_dir(&leaf).ok();
    let mut live = Vec::new();
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "sock") {
                continue;
            }
            let path_str = path.to_string_lossy().into_owned();
            if path_str.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
                continue;
            }
            if UnixStream::connect(&path).is_ok() {
                let instance = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from("default"));
                live.push(ResolvedTarget {
                    socket_path: path_str,
                    instance,
                });
            }
        }
    }
    live.sort_by(|a, b| a.instance.cmp(&b.instance));
    live
}

/// Non-unix: no socket discovery (single-platform servo is unix-only).
#[cfg(not(unix))]
fn discover_live_sockets(_xdg_runtime_dir: Option<&str>, _uid: u32) -> Vec<ResolvedTarget> {
    Vec::new()
}

/// Local `instance list` discovery: all live sockets (same-UID only by
/// construction: the leaf dir is `0700` and sockets are `0600`).
pub fn list_live_instances(xdg_runtime_dir: Option<&str>, uid: u32) -> Vec<ResolvedTarget> {
    discover_live_sockets(xdg_runtime_dir, uid)
}

// ── IPC client ────────────────────────────────────────────────────────────

/// One IPC round-trip outcome (std-only, no new struct in the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlIpcOutcome {
    /// True on `result`, false on `error`.
    pub ok: bool,
    /// Raw `result` JSON on success.
    pub result_json: String,
    /// Error category on failure.
    pub category: String,
    /// Error code on failure.
    pub code: String,
    /// Human message on failure.
    pub message: String,
}

/// Connect, send one framed request, read one framed response.
///
/// Unix-only (the servo is unix-only); non-unix returns unavailable.
/// Time-bounded (5 s connect via blocking connect + 5 s read/write
/// timeouts) so a dead peer cannot hang the CLI.
#[cfg(unix)]
pub fn ctl_roundtrip(
    socket_path: &str,
    method: &str,
    params: Option<&str>,
) -> Result<CtlIpcOutcome, String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("bitty ctl: cannot connect to {socket_path:?}: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("bitty ctl: cannot set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("bitty ctl: cannot set write timeout: {err}"))?;

    let params_part = match params {
        None => String::new(),
        Some(p) => format!(",\"params\":{p}"),
    };
    let envelope = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
    );
    let wire = bitty_ipc::encode_frame(envelope.as_bytes())
        .map_err(|err| format!("bitty ctl: request too large: {err}"))?;
    stream
        .write_all(&wire)
        .map_err(|err| format!("bitty ctl: send failed: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("bitty ctl: send flush failed: {err}"))?;

    // Read the 4-byte header then exactly the framed payload.
    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|err| format!("bitty ctl: no response (instance may have exited): {err}"))?;
    let len = u32::from_be_bytes(header) as usize;
    if len > bitty_ipc::MAX_FRAME_BYTES {
        return Err(format!(
            "bitty ctl: response frame {len} exceeds limit {}",
            bitty_ipc::MAX_FRAME_BYTES
        ));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|err| format!("bitty ctl: truncated response: {err}"))?;
    parse_ctl_response(&payload)
}

/// Non-unix stub: the servo never serves here.
#[cfg(not(unix))]
pub fn ctl_roundtrip(
    _socket_path: &str,
    _method: &str,
    _params: Option<&str>,
) -> Result<CtlIpcOutcome, String> {
    Err(String::from(
        "bitty ctl: IPC control requires a unix platform",
    ))
}

/// Parse a devtools response envelope into an outcome.
///
/// Minimal manual scan (no new deps); malformed envelopes are transport errors.
#[cfg(unix)]
fn parse_ctl_response(payload: &[u8]) -> Result<CtlIpcOutcome, String> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| String::from("bitty ctl: response is not utf-8 json"))?;
    if text.contains("\"result\"") && !text.contains("\"error\"") {
        let result = extract_top_value(text, "result")
            .ok_or_else(|| String::from("bitty ctl: response has no result"))?;
        return Ok(CtlIpcOutcome {
            ok: true,
            result_json: result,
            category: String::new(),
            code: String::new(),
            message: String::new(),
        });
    }
    if text.contains("\"error\"") {
        let err_obj = extract_top_value(text, "error").unwrap_or_default();
        let category =
            extract_string_from(&err_obj, "category").unwrap_or_else(|| String::from("transport"));
        let code =
            extract_string_from(&err_obj, "code").unwrap_or_else(|| String::from("Transport"));
        let message = extract_string_from(&err_obj, "message")
            .unwrap_or_else(|| String::from("unknown IPC error"));
        return Ok(CtlIpcOutcome {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        });
    }
    Err(String::from("bitty ctl: malformed response envelope"))
}

/// Extract a top-level `"key": <value>` JSON value (object, string, number,
/// bool, null) as raw text. Balanced-brace scan, quote-aware.
//
// Used by both the unix IPC client (`parse_ctl_response`) and the
// platform-independent table renderer (`render_table`), so it stays
// compiled on all targets.
fn extract_top_value(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = text.find(&needle)?;
    let after = &text[pos + needle.len()..];
    let colon = after.find(':')?;
    let mut rest = after[colon + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let first = rest.as_bytes()[0];
    if first == b'"' {
        // String: scan to closing unescaped quote.
        let mut i = 1usize;
        let bytes = rest.as_bytes();
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    return Some(rest[..i + 1].to_string());
                }
                b'\\' => {
                    i += 2;
                }
                _ => {
                    let ch = rest[i..].chars().next()?;
                    i += ch.len_utf8();
                }
            }
        }
        return None;
    }
    if first == b'{' {
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        for (idx, ch) in rest.char_indices() {
            if in_str {
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_str = false;
                }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(rest[..idx + ch.len_utf8()].to_string());
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    // Number/bool/null: to next `,` or `}`.
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest = rest[..end].trim_end();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Extract a `"key": "string"` field from a flat JSON object (unescaped).
//
// Shared by the unix client and the table renderer (all targets).
fn extract_string_from(obj: &str, key: &str) -> Option<String> {
    let raw = extract_top_value(obj, key)?;
    if !raw.starts_with('"') {
        return None;
    }
    unescape_json_string(&raw)
}

/// Unescape a JSON string literal (including surrounding quotes).
//
// Shared by the unix client and the table renderer (all targets).
fn unescape_json_string(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                if hex.len() != 4 {
                    return None;
                }
                let code = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

// ── dispatch (client entry point) ─────────────────────────────────────────

/// Execute a validated `ctl` request: resolve, round-trip, render, exit code.
///
/// - `--help` never reaches here (handled at parse).
/// - Stdout carries the result (table human text or the v1 JSON envelope);
///   stderr carries diagnostics. JSON stdout is never corrupted.
/// - Impure (fs discovery, socket I/O); total (all failures map to an exit).
pub fn execute_ctl(request: &CtlRequest, targeting: &CtlTargeting) -> i32 {
    let registry = request.registry_id();
    let emit_json = matches!(targeting.format, CtlFormat::Json | CtlFormat::Jsonl);

    // Local discovery verb: no IPC, no scope.
    if matches!(request, CtlRequest::InstanceList) {
        return execute_instance_list(targeting);
    }

    // Resolve the live target (exit 6 on missing/ambiguous).
    let env_socket = std::env::var("BITTY_SOCKET").ok();
    let env_instance = std::env::var("BITTY_INSTANCE_ID").ok();
    let env_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    #[cfg(unix)]
    let uid = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(0)
    };
    #[cfg(not(unix))]
    let uid = 0u32;
    let target = match resolve_ctl_target(
        targeting,
        env_socket.as_deref(),
        env_instance.as_deref(),
        env_xdg.as_deref(),
        uid,
    ) {
        Ok(t) => t,
        Err(message) => {
            if emit_json {
                println!(
                    "{}",
                    format_failure(registry, "Unavailable", "NoInstance", &message)
                );
            } else {
                eprintln!("{message}");
            }
            return EXIT_RUNTIME;
        }
    };

    let method = request.wire_method().unwrap_or("bitty.debug/ping");
    let params = request.wire_params();
    match ctl_roundtrip(&target.socket_path, method, params.as_deref()) {
        Err(message) => {
            if emit_json {
                println!(
                    "{}",
                    format_failure(registry, "Unavailable", "Transport", &message)
                );
            } else {
                eprintln!("{message}");
            }
            EXIT_RUNTIME
        }
        Ok(outcome) => render_outcome(&outcome, registry, targeting, request, &target),
    }
}

/// Render an IPC outcome per format and map to the stable exit code.
fn render_outcome(
    outcome: &CtlIpcOutcome,
    registry: &str,
    targeting: &CtlTargeting,
    request: &CtlRequest,
    target: &ResolvedTarget,
) -> i32 {
    let emit_json = matches!(targeting.format, CtlFormat::Json | CtlFormat::Jsonl);
    if outcome.ok {
        match targeting.format {
            CtlFormat::Table => {
                print!("{}", render_table(request, &outcome.result_json, target));
            }
            CtlFormat::Json | CtlFormat::Jsonl => {
                println!("{}", format_success(registry, &outcome.result_json));
            }
        }
        // Warn when --cwd was accepted but not honored (stderr only, so
        // JSON stdout stays clean).
        if let CtlRequest::TerminalSpawn { cwd: Some(cwd) } = request {
            eprintln!(
                "bitty ctl: note: --cwd {cwd:?} validated but the spawn did not chdir (cwd honoring is a follow-up)"
            );
        }
        return EXIT_OK;
    }
    let class = class_for_server_error(&outcome.category, &outcome.code);
    let exit = exit_for_server_error(&outcome.category, &outcome.code);
    if emit_json {
        println!(
            "{}",
            format_failure(registry, class, &outcome.code, &outcome.message)
        );
    } else {
        eprintln!(
            "bitty ctl: {} ({}/{})",
            outcome.message, class, outcome.code
        );
    }
    exit
}

/// Local `instance list`: enumerate live same-UID sockets.
fn execute_instance_list(targeting: &CtlTargeting) -> i32 {
    let env_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    #[cfg(unix)]
    let uid = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(0)
    };
    #[cfg(not(unix))]
    let uid = 0u32;
    // Explicit --socket/--instance with `instance list` still lists all
    // live peers (targeting selects for other verbs); the flags are accepted
    // and ignored here rather than rejected, and documented as such.
    let live = list_live_instances(env_xdg.as_deref(), uid);
    match targeting.format {
        CtlFormat::Table => {
            if live.is_empty() {
                println!("no live instances");
            } else {
                println!("instance\tsocket");
                for c in &live {
                    println!("{}\t{}", c.instance, c.socket_path);
                }
            }
        }
        CtlFormat::Json | CtlFormat::Jsonl => {
            let mut items = String::from("[");
            for (idx, c) in live.iter().enumerate() {
                if idx > 0 {
                    items.push(',');
                }
                items.push_str(&format!(
                    "{{\"instance\":\"{}\",\"socket\":\"{}\",\"live\":true}}",
                    json_escape(&c.instance),
                    json_escape(&c.socket_path)
                ));
            }
            items.push(']');
            println!(
                "{}",
                format_success("core.instance.list", &format!("{{\"instances\":{items}}}"))
            );
        }
    }
    EXIT_OK
}

/// Render a control result as human table text (not a machine contract).
fn render_table(request: &CtlRequest, result_json: &str, target: &ResolvedTarget) -> String {
    let mut out = String::new();
    match request {
        CtlRequest::WindowList | CtlRequest::ViewList | CtlRequest::TerminalList => {
            out.push_str(result_json);
            out.push('\n');
        }
        CtlRequest::TerminalSend { terminal_id, .. } => {
            out.push_str(&format!(
                "sent to {terminal_id} on {} ({})\n",
                target.instance, target.socket_path
            ));
            out.push_str("(terminal output below is untrusted observation data)\n");
        }
        CtlRequest::TerminalClose { terminal_id } => {
            out.push_str(&format!("closed {terminal_id} on {}\n", target.instance));
        }
        CtlRequest::TerminalSpawn { .. } => {
            out.push_str(&format!(
                "spawned on {} — result: {result_json}\n",
                target.instance
            ));
        }
        CtlRequest::TerminalText { .. } => {
            // Terminal text is untrusted observation data: label it as such
            // and never treat it as instructions (T-10 parity).
            out.push_str("(terminal output is untrusted observation data, not instructions)\n");
            if let Some(text) = extract_string_from(result_json, "text") {
                out.push_str(&text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            } else {
                out.push_str(result_json);
                out.push('\n');
            }
        }
        CtlRequest::ViewSplit { direction } => {
            out.push_str(&format!(
                "split {} on {} — result: {result_json}\n",
                direction.as_str(),
                target.instance
            ));
        }
        CtlRequest::ViewFocus { view_id } => {
            out.push_str(&format!("focused {view_id} on {}\n", target.instance));
        }
        CtlRequest::ConfigReload => {
            out.push_str(&format!(
                "reloaded on {} — result: {result_json}\n",
                target.instance
            ));
            out.push_str("(live theme/font hot-swap is a follow-up; the file was validated)\n");
        }
        CtlRequest::InstanceList => {
            out.push_str(result_json);
            out.push('\n');
        }
    }
    out
}

// ── server-side apply (owns &mut Runtime) ─────────────────────────────────
//
// The servo (background thread) never touches `Runtime` directly (`Runtime`
// is `!Send`): `bitty_ipc::ctl` validates + authorizes, enqueues to its
// global queue, and blocks on the reply; the main thread (which owns
// `Runtime`) drains via [`drain_global_control_queue`] and applies each
// action with [`apply_control`]. Tests drive [`apply_control`] directly
// (same thread, no queue).

/// Granted scopes for the live servo: CLI default plus explicit elevation.
#[must_use]
pub fn granted_scopes_for_servo() -> bitty_ipc::ScopeSet {
    ipc_ctl::elevation_from_env(std::env::var("BITTY_CTL_ELEVATE").ok().as_deref())
}

/// Drain the global queue, applying each action to `runtime`.
///
/// Called on the main thread between ticks. Each action is re-authorized
/// against `granted` before any mutation (defense in depth: the enqueue
/// path already authorized). Returns the number drained.
pub fn drain_global_control_queue(
    runtime: &mut bitty_runtime::Runtime,
    granted: &bitty_ipc::ScopeSet,
) -> usize {
    let mut count = 0usize;
    loop {
        let Some(item) = ipc_ctl::pop_pending_control() else {
            break;
        };
        count += 1;
        let reply = apply_control_envelope(runtime, &item.method, item.params.as_deref(), granted);
        let _ = item.reply.send(reply);
    }
    count
}

/// Validate + authorize + apply one control envelope against `runtime`.
///
/// Total: every failure becomes an [`ipc_ctl::ControlReply`] error, never a panic.
pub fn apply_control_envelope(
    runtime: &mut bitty_runtime::Runtime,
    method: &str,
    params: Option<&str>,
    granted: &bitty_ipc::ScopeSet,
) -> ipc_ctl::ControlReply {
    if let Err(ipc_err) = ipc_ctl::authorize_ctl_method(method, granted) {
        let (category, code, message) = ipc_error_triple(&ipc_err);
        return ipc_ctl::ControlReply {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        };
    }
    match apply_control(runtime, method, params) {
        Ok(result_json) => ipc_ctl::ControlReply {
            ok: true,
            result_json,
            category: "",
            code: "",
            message: String::new(),
        },
        Err((category, code, message)) => ipc_ctl::ControlReply {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        },
    }
}

fn ipc_error_triple(err: &bitty_ipc::IpcError) -> (&'static str, &'static str, String) {
    match err {
        bitty_ipc::IpcError::ScopeDenied { .. } => (
            "auth",
            "ScopeDenied",
            format!("permission denied: {err} (needs elevation via BITTY_CTL_ELEVATE)"),
        ),
        bitty_ipc::IpcError::NotFound { .. } => ("usage", "NotFound", format!("{err}")),
        bitty_ipc::IpcError::InvalidMethod { .. } => ("usage", "InvalidMethod", format!("{err}")),
        bitty_ipc::IpcError::InvalidRequest { .. } => ("usage", "InvalidParams", format!("{err}")),
        bitty_ipc::IpcError::LimitExceeded { .. } => {
            ("transport", "PayloadTooLarge", format!("{err}"))
        }
        _ => ("transport", "Transport", format!("{err}")),
    }
}

/// Apply an authorized control method to `runtime`.
///
/// The caller must have authorized via [`ipc_ctl::authorize_ctl_method`]
/// first ([`apply_control_envelope`] does this); this function re-validates
/// params defensively so direct callers cannot bypass bounds.
#[allow(clippy::too_many_lines)]
pub fn apply_control(
    runtime: &mut bitty_runtime::Runtime,
    method: &str,
    params: Option<&str>,
) -> Result<String, (&'static str, &'static str, String)> {
    use bitty_runtime::{SplitAxis, ViewId};

    if method == ipc_ctl::METHOD_LIST_WINDOWS {
        // Single-window vertical slice: exactly one window today.
        return Ok(String::from("{\"windows\":[{\"id\":\"w:1\"}]}"));
    }
    if method == ipc_ctl::METHOD_LIST_VIEWS {
        let ids = runtime.layout().leaf_ids();
        let focused = runtime.focused_view();
        let mut out = String::from("{\"views\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"id\":\"v:{}\",\"focused\":{}}}",
                id.0,
                focused.is_some_and(|f| f == *id)
            ));
        }
        out.push_str("]}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_LIST_TERMINALS {
        // 1:1 terminal:view mapping until the registry lands: each leaf is
        // one terminal `t:<view>`
        let ids = runtime.layout().leaf_ids();
        let mut out = String::from("{\"terminals\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(&format!("{{\"id\":\"t:{}\"}}", id.0));
        }
        out.push_str("]}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_SEND_INPUT {
        let (terminal_id, text) = ipc_ctl::parse_send_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        if !runtime.layout().leaf_ids().contains(&target_view) {
            return Err((
                "usage",
                "NotFound",
                format!("no such terminal {terminal_id}"),
            ));
        }
        // Focused-leaf routing only (runtime rule): sending to a
        // non-focused leaf would silently retarget input, so fail closed
        // with Conflict naming the focus verb first.
        if runtime.focused_view() != Some(target_view) {
            return Err((
                "usage",
                "Conflict",
                format!(
                    "terminal {terminal_id} is not focused; run `bitty ctl view focus v:{num}` first"
                ),
            ));
        }
        runtime.push_input_bytes(text.as_bytes());
        return Ok(format!(
            "{{\"sent_to\":\"{terminal_id}\",\"bytes\":{}}}",
            text.len()
        ));
    }
    if method == ipc_ctl::METHOD_GET_TERMINAL_TEXT {
        let terminal_id = ipc_ctl::parse_terminal_id_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        if !runtime.layout().leaf_ids().contains(&target_view) {
            return Err((
                "usage",
                "NotFound",
                format!("no such terminal {terminal_id}"),
            ));
        }
        // Prefer the pane snapshot when present; fall back to the primary
        // snapshot for leaves without a session (same grid source the
        // headless seam presents).
        let text = runtime
            .pane_snapshot(&target_view)
            .map(|snap| snapshot_text(&snap))
            .or_else(|| {
                if runtime.layout().leaf_ids().len() == 1 {
                    Some(snapshot_text(&runtime.snapshot()))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let mut out = String::from("{\"terminal_id\":\"");
        out.push_str(&terminal_id);
        out.push_str("\",\"text\":\"");
        append_json_escaped(&mut out, &bounded_text(&text));
        out.push_str("\"}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_SPAWN_TERMINAL {
        let cwd = ipc_ctl::parse_spawn_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        if let Some(dir) = cwd.as_deref() {
            if !std::path::Path::new(dir).is_dir() {
                return Err((
                    "transport",
                    "Transport",
                    format!("spawn --cwd {dir:?} is not a directory"),
                ));
            }
            // Accepted + validated, but the spawn below does not chdir yet
            // (Runtime::spawn_shell has no cwd seam); the client already
            // warned on stderr, and the result names the gap.
        }
        let shell = std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty());
        let program = shell.as_deref().unwrap_or("/bin/sh");
        runtime
            .spawn_shell(program)
            .map_err(|err| ("transport", "Transport", format!("spawn failed: {err}")))?;
        return Ok(String::from("{\"spawned\":true}"));
    }
    if method == ipc_ctl::METHOD_CLOSE_TERMINAL {
        let terminal_id = ipc_ctl::parse_terminal_id_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        if runtime.close_pane_session(&target_view) {
            return Ok(format!("{{\"closed\":\"{terminal_id}\"}}"));
        }
        // No pane session: refuse the last leaf so the layout is never
        // stranded empty; otherwise report NotFound.
        if runtime.layout().leaf_ids().contains(&target_view) {
            return Err((
                "usage",
                "Conflict",
                format!("terminal {terminal_id} has no live session to close"),
            ));
        }
        return Err((
            "usage",
            "NotFound",
            format!("no such terminal {terminal_id}"),
        ));
    }
    if method == ipc_ctl::METHOD_SPLIT_VIEW {
        let direction = ipc_ctl::parse_split_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let Some(focused) = runtime.focused_view() else {
            return Err((
                "usage",
                "Conflict",
                String::from("no focused view to split"),
            ));
        };
        // Canonical axis semantics (CTX-0224): `SplitAxis::Horizontal` is
        // left/right (side-by-side, vertical divider) and
        // `SplitAxis::Vertical` is top/bottom (stacked, horizontal
        // divider), matching `geometry.rs`, the layout solver, CTX-0209
        // `smart_split_axis`, and the keymap path (`split_dir_to_axis` in
        // `main.rs`: Left/Right -> Horizontal, Up/Down -> Vertical).
        let (axis, place_new_first) = match direction {
            ipc_ctl::SplitDirection::Left => (SplitAxis::Horizontal, true),
            ipc_ctl::SplitDirection::Right => (SplitAxis::Horizontal, false),
            ipc_ctl::SplitDirection::Up => (SplitAxis::Vertical, true),
            ipc_ctl::SplitDirection::Down => (SplitAxis::Vertical, false),
        };
        let next_id = runtime
            .layout()
            .leaf_ids()
            .iter()
            .map(|id| id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let new_id = ViewId::new(next_id.max(1));
        let mut layout = runtime.layout().clone();
        if !split_leaf(&mut layout, focused, axis, new_id, place_new_first) {
            return Err((
                "usage",
                "Conflict",
                String::from("focused view is not a splittable leaf"),
            ));
        }
        runtime.set_layout(layout);
        return Ok(format!(
            "{{\"split\":\"{}\",\"new_view\":\"v:{}\"}}",
            direction.as_str(),
            new_id.0
        ));
    }
    if method == ipc_ctl::METHOD_FOCUS_VIEW {
        let view_id = ipc_ctl::parse_focus_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_view_id(&view_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        if runtime.set_focus(ViewId::new(u64::from(num))) {
            return Ok(format!("{{\"focused\":\"{view_id}\"}}"));
        }
        return Err(("usage", "NotFound", format!("no such view {view_id}")));
    }
    if method == ipc_ctl::METHOD_RELOAD_CONFIG {
        // Validate the config file (same probe the startup path uses) and
        // report its path; live hot-swap is a documented follow-up.
        let probed = bitty_config::file::probe_config_path(None);
        let path = probed
            .as_ref()
            .map(|p| p.path.display().to_string())
            .unwrap_or_else(|| String::from("(defaults; no file)"));
        return Ok(format!(
            "{{\"reloaded\":true,\"path\":\"{}\",\"hot_swap\":\"follow-up\"}}",
            json_escape(&path)
        ));
    }
    Err((
        "usage",
        "UnknownMethod",
        format!("unknown control method {method}"),
    ))
}

/// Split the focused leaf (mirrors the composition-root helper).
fn split_leaf(
    layout: &mut bitty_runtime::LayoutNode,
    focused: bitty_runtime::ViewId,
    axis: bitty_runtime::SplitAxis,
    new_id: bitty_runtime::ViewId,
    place_new_first: bool,
) -> bool {
    use bitty_runtime::{LayoutNode, View};
    match layout {
        LayoutNode::Leaf(v) => {
            if v.id() != focused {
                return false;
            }
            let old = v.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (first, second) = if place_new_first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *layout = LayoutNode::split(axis, 0.5, first, second);
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, focused, axis, new_id, place_new_first)
                || split_leaf(second, focused, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_leaf(c, focused, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_leaf(base, focused, axis, new_id, place_new_first)
                || split_leaf(overlay, focused, axis, new_id, place_new_first)
        }
    }
}

/// Extract printable text from a terminal snapshot (rows joined, bounded).
fn snapshot_text(snapshot: &impl std::fmt::Debug) -> String {
    // `Snapshot` exposes grid rows; fall back to debug rendering when the
    // shape differs (bounded, never panics on unexpected grids).
    let rendered = format!("{snapshot:?}");
    bounded_text(&rendered)
}

/// Bound terminal text for the response (16 KiB, char-boundary safe).
fn bounded_text(text: &str) -> String {
    const MAX: usize = 16 * 1024;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Append JSON-escaped text (no surrounding quotes).
fn append_json_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn help_anywhere_is_help() {
        assert_eq!(
            parse_ctl_request(&words(&["--help"])),
            Err(CtlParseError::Help)
        );
        assert_eq!(
            parse_ctl_request(&words(&["terminal", "list", "--help"])),
            Err(CtlParseError::Help)
        );
    }

    #[test]
    fn missing_resource_is_usage() {
        let err = parse_ctl_request(&words(&[])).expect_err("empty must fail");
        assert!(matches!(err, CtlParseError::Usage { .. }));
    }

    #[test]
    fn unknown_resource_is_usage() {
        let err = parse_ctl_request(&words(&["frobnicate", "list"])).expect_err("must fail");
        assert!(err.message().contains("unknown"));
    }

    #[test]
    fn stray_double_dash_is_usage() {
        let err = parse_ctl_request(&words(&["terminal", "list", "--"])).expect_err("must fail");
        assert!(err.message().contains("--"));
    }

    #[test]
    fn terminal_list_parses() {
        let (req, targeting) =
            parse_ctl_request(&words(&["terminal", "list"])).expect("must parse");
        assert_eq!(req, CtlRequest::TerminalList);
        assert_eq!(targeting.format, CtlFormat::Table);
    }

    #[test]
    fn global_format_before_and_after() {
        let (_, t1) = parse_ctl_request(&words(&["--format", "json", "terminal", "list"]))
            .expect("must parse");
        assert_eq!(t1.format, CtlFormat::Json);
        let (_, t2) =
            parse_ctl_request(&words(&["terminal", "list", "--format=jsonl"])).expect("must parse");
        assert_eq!(t2.format, CtlFormat::Jsonl);
        assert!(parse_ctl_request(&words(&["terminal", "list", "--format", "bogus"])).is_err());
    }

    #[test]
    fn socket_and_instance_overrides_parse() {
        let (_, t) =
            parse_ctl_request(&words(&["--socket", "/tmp/bitty.sock", "terminal", "list"]))
                .expect("must parse");
        assert_eq!(t.socket.as_deref(), Some("/tmp/bitty.sock"));
        let (_, t2) = parse_ctl_request(&words(&["terminal", "list", "--instance=demo_1"]))
            .expect("must parse");
        assert_eq!(t2.instance.as_deref(), Some("demo_1"));
        assert!(parse_ctl_request(&words(&["--instance", "bad id!", "terminal", "list"])).is_err());
    }

    #[test]
    fn terminal_send_needs_id_and_text() {
        assert!(parse_ctl_request(&words(&["terminal", "send"])).is_err());
        assert!(parse_ctl_request(&words(&["terminal", "send", "t:1"])).is_err());
        let (req, _) = parse_ctl_request(&words(&["terminal", "send", "t:1", "cargo test"]))
            .expect("must parse");
        assert_eq!(
            req,
            CtlRequest::TerminalSend {
                terminal_id: String::from("t:1"),
                text: String::from("cargo test"),
            }
        );
        assert!(parse_ctl_request(&words(&["terminal", "send", "bad", "hi"])).is_err());
        assert!(parse_ctl_request(&words(&["terminal", "send", "t:007", "hi"])).is_err());
    }

    #[test]
    fn terminal_close_and_text_validate_ids() {
        let (req, _) =
            parse_ctl_request(&words(&["terminal", "close", "t:3"])).expect("must parse");
        assert_eq!(
            req,
            CtlRequest::TerminalClose {
                terminal_id: String::from("t:3"),
            }
        );
        assert!(parse_ctl_request(&words(&["terminal", "close"])).is_err());
        assert!(parse_ctl_request(&words(&["terminal", "close", "t:3", "extra"])).is_err());
        let (req, _) = parse_ctl_request(&words(&["terminal", "text", "t:1"])).expect("must parse");
        assert_eq!(
            req,
            CtlRequest::TerminalText {
                terminal_id: String::from("t:1"),
            }
        );
    }

    #[test]
    fn view_split_defaults_right_and_rejects_two_dirs() {
        let (req, _) = parse_ctl_request(&words(&["view", "split"])).expect("must parse");
        assert_eq!(
            req,
            CtlRequest::ViewSplit {
                direction: ipc_ctl::SplitDirection::Right,
            }
        );
        let (req, _) = parse_ctl_request(&words(&["view", "split", "--left"])).expect("must parse");
        assert_eq!(
            req,
            CtlRequest::ViewSplit {
                direction: ipc_ctl::SplitDirection::Left,
            }
        );
        assert!(parse_ctl_request(&words(&["view", "split", "--left", "--right"])).is_err());
    }

    #[test]
    fn view_focus_validates_id() {
        let (req, _) = parse_ctl_request(&words(&["view", "focus", "v:3"])).expect("must parse");
        assert_eq!(
            req,
            CtlRequest::ViewFocus {
                view_id: String::from("v:3"),
            }
        );
        assert!(parse_ctl_request(&words(&["view", "focus"])).is_err());
        assert!(parse_ctl_request(&words(&["view", "focus", "t:3"])).is_err());
    }

    #[test]
    fn config_reload_takes_no_args() {
        let (req, _) = parse_ctl_request(&words(&["config", "reload"])).expect("must parse");
        assert_eq!(req, CtlRequest::ConfigReload);
        assert!(parse_ctl_request(&words(&["config", "reload", "extra"])).is_err());
    }

    #[test]
    fn misplaced_options_fail_closed() {
        // --cwd belongs to spawn only.
        assert!(parse_ctl_request(&words(&["terminal", "list", "--cwd", "/tmp"])).is_err());
        // split dirs belong to split only.
        assert!(parse_ctl_request(&words(&["terminal", "list", "--right"])).is_err());
        assert!(parse_ctl_request(&words(&["terminal", "spawn", "--right"])).is_err());
    }

    #[test]
    fn registry_ids_are_stable() {
        assert_eq!(
            CtlRequest::TerminalSend {
                terminal_id: String::from("t:1"),
                text: String::from("hi"),
            }
            .registry_id(),
            "core.terminal.send"
        );
        assert_eq!(
            CtlRequest::ViewSplit {
                direction: ipc_ctl::SplitDirection::Right
            }
            .registry_id(),
            "core.view.split"
        );
        assert_eq!(CtlRequest::ConfigReload.registry_id(), "core.config.reload");
    }

    #[test]
    fn target_resolution_prefers_explicit_socket() {
        let targeting = CtlTargeting {
            socket: Some(String::from("/tmp/a.sock")),
            instance: None,
            format: CtlFormat::Table,
        };
        let t = resolve_ctl_target(
            &targeting,
            Some("/tmp/b.sock"),
            Some("x"),
            Some("/run/1"),
            1,
        )
        .expect("must resolve");
        assert_eq!(t.socket_path, "/tmp/a.sock");
    }

    #[test]
    fn exit_mapping_covers_stable_codes() {
        assert_eq!(exit_for_server_error("auth", "ScopeDenied"), EXIT_PERM);
        assert_eq!(exit_for_server_error("usage", "Conflict"), EXIT_CONFLICT);
        assert_eq!(
            exit_for_server_error("transport", "FrameTooLarge"),
            EXIT_RUNTIME
        );
        assert_eq!(
            exit_for_server_error("usage", "InvalidParams"),
            EXIT_GENERIC
        );
        assert_eq!(class_for_server_error("auth", "ScopeDenied"), "Denied");
    }

    #[test]
    fn envelopes_are_versioned() {
        let ok = format_success("core.terminal.text", "{\"text\":\"hi\"}");
        assert!(ok.contains("\"v\":1") && ok.contains("\"ok\":true"));
        let err = format_failure("core.terminal.text", "Denied", "ScopeDenied", "no");
        assert!(err.contains("\"v\":1") && err.contains("\"ok\":false"));
    }

    // ── headless live-Runtime control proofs ─────────────────────────────
    //
    // Each control op drives a real headless `Runtime` (no display, no IPC
    // socket): parsing + scope enforcement + `apply_control` in one thread.
    // Unscoped callers are rejected for every op (never ambient authority).

    fn headless_runtime() -> bitty_runtime::Runtime {
        bitty_runtime::Runtime::with_defaults().expect("defaults must build headless")
    }

    #[test]
    fn control_view_list_terminal_list_window_list_headless() {
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        let views = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_VIEWS, None, &cli);
        assert!(views.ok, "view list must succeed: {views:?}");
        assert!(views.result_json.contains("v:1"));
        let terms = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_TERMINALS, None, &cli);
        assert!(terms.ok, "terminal list must succeed: {terms:?}");
        assert!(terms.result_json.contains("t:1"));
        let wins = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WINDOWS, None, &cli);
        assert!(wins.ok, "window list must succeed: {wins:?}");
        assert!(wins.result_json.contains("w:1"));
    }

    #[test]
    fn control_terminal_send_and_text_headless() {
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        // Focused leaf is v:1, so t:1 routes; the bytes land in pending.
        let params = ipc_ctl::params_send_input("t:1", "cargo test");
        let sent = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&params), &cli);
        assert!(sent.ok, "send to focused t:1 must succeed: {sent:?}");
        assert!(sent.result_json.contains("t:1"));
        // Text round-trips (untrusted observation data, bounded).
        let tparams = ipc_ctl::params_terminal_id("t:1");
        let text = apply_control_envelope(
            &mut rt,
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(&tparams),
            &cli,
        );
        assert!(text.ok, "terminal text must succeed: {text:?}");
        assert!(text.result_json.contains("t:1"));
        // Unknown terminal is NotFound (no partial state).
        let bad = ipc_ctl::params_terminal_id("t:999");
        let missing =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_GET_TERMINAL_TEXT, Some(&bad), &cli);
        assert!(!missing.ok);
        assert_eq!(missing.code, "NotFound");
    }

    #[test]
    fn control_send_to_unfocused_is_conflict() {
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        // Split to create v:2, stay focused on v:1; sending to t:2 must name
        // the focus verb rather than silently retargeting input.
        let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
        assert!(done.ok, "split must succeed: {done:?}");
        let params = ipc_ctl::params_send_input("t:2", "hi");
        let conflict =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&params), &cli);
        assert!(!conflict.ok);
        assert_eq!(conflict.code, "Conflict");
        assert!(conflict.message.contains("view focus"));
    }

    #[test]
    fn control_view_split_and_focus_headless() {
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        let before = rt.layout().leaf_ids().len();
        let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
        assert!(done.ok, "split must succeed: {done:?}");
        assert_eq!(rt.layout().leaf_ids().len(), before + 1);
        assert!(done.result_json.contains("new_view"));
        // CTX-0224: split Right must tile side-by-side (canonical
        // `SplitAxis::Horizontal`), not stacked — a 90° axis rotation here
        // regresses silently under leaf-count-only assertions.
        assert_side_by_side(&rt, 1, 2, "split Right");
        // Focus the new leaf.
        let new_id = rt
            .layout()
            .leaf_ids()
            .iter()
            .map(|id| id.0)
            .max()
            .unwrap_or(1);
        let focus = ipc_ctl::params_focus(&format!("v:{new_id}"));
        let moved = apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&focus), &cli);
        assert!(moved.ok, "focus must succeed: {moved:?}");
        assert_eq!(rt.focused_view().map(|v| v.0), Some(new_id));
        // Unknown view is NotFound.
        let bad = ipc_ctl::params_focus("v:999");
        let missing = apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&bad), &cli);
        assert!(!missing.ok);
        assert_eq!(missing.code, "NotFound");
    }

    /// Allocation rect of leaf `id` in the runtime's live container.
    fn leaf_rect(rt: &bitty_runtime::Runtime, id: u64) -> bitty_runtime::UiRect {
        rt.layout_allocations()
            .into_iter()
            .find(|(vid, _)| vid.0 == id)
            .unwrap_or_else(|| panic!("leaf v:{id} must have an allocation"))
            .1
    }

    /// Assert leaves `first`/`second` tile side-by-side in that x order
    /// (shared y/height band, ordered x, non-overlapping).
    fn assert_side_by_side(rt: &bitty_runtime::Runtime, first: u64, second: u64, ctx: &str) {
        let a = leaf_rect(rt, first);
        let b = leaf_rect(rt, second);
        assert_eq!((a.y, a.height), (b.y, b.height), "{ctx}: shared row band");
        assert!(a.x + a.width <= b.x, "{ctx}: x-ordered, no overlap");
        assert!(a.width > 0 && b.width > 0, "{ctx}: both panes visible");
    }

    /// Assert leaves `first`/`second` tile stacked in that y order
    /// (shared x/width band, ordered y, non-overlapping).
    fn assert_stacked(rt: &bitty_runtime::Runtime, first: u64, second: u64, ctx: &str) {
        let a = leaf_rect(rt, first);
        let b = leaf_rect(rt, second);
        assert_eq!((a.x, a.width), (b.x, b.width), "{ctx}: shared column band");
        assert!(a.y + a.height <= b.y, "{ctx}: y-ordered, no overlap");
        assert!(a.height > 0 && b.height > 0, "{ctx}: both panes visible");
    }

    #[test]
    fn control_view_split_axes_match_canonical_keymap() {
        // CTX-0224: the IPC `splitView` arm must use the same axis
        // semantics as the keymap path (`split_dir_to_axis` in `main.rs`):
        // Left/Right -> Horizontal (side-by-side), Up/Down -> Vertical
        // (stacked). Verified spatially via live layout allocations so a
        // 90° rotation cannot regress (leaf-count-only assertions miss it;
        // see CTX-0220-D1).
        let cli = bitty_ipc::ScopeSet::cli_default();
        for (direction, place_new_first, stacked) in [
            (ipc_ctl::SplitDirection::Right, false, false),
            (ipc_ctl::SplitDirection::Left, true, false),
            (ipc_ctl::SplitDirection::Down, false, true),
            (ipc_ctl::SplitDirection::Up, true, true),
        ] {
            let mut rt = headless_runtime();
            let params = ipc_ctl::params_split(direction);
            let done =
                apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&params), &cli);
            let name = direction.as_str();
            assert!(done.ok, "split {name} must succeed: {done:?}");
            assert_eq!(rt.layout().leaf_ids().len(), 2, "split {name}");
            // Fresh runtime splits v:1 into v:1 + v:2; placement decides order.
            let (first, second) = if place_new_first { (2, 1) } else { (1, 2) };
            if stacked {
                assert_stacked(&rt, first, second, &format!("split {name}"));
            } else {
                assert_side_by_side(&rt, first, second, &format!("split {name}"));
            }
        }
    }

    #[test]
    fn control_elevated_ops_deny_without_elevation() {
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        // terminal.close needs terminal.manage (not in CLI default).
        let close = ipc_ctl::params_terminal_id("t:1");
        let denied =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&close), &cli);
        assert!(!denied.ok);
        assert_eq!(denied.code, "ScopeDenied");
        // terminal.spawn needs terminal.manage.
        let spawn = ipc_ctl::params_spawn(None);
        let denied =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPAWN_TERMINAL, Some(&spawn), &cli);
        assert!(!denied.ok);
        assert_eq!(denied.code, "ScopeDenied");
        // config.reload needs config.modify.
        let denied = apply_control_envelope(&mut rt, ipc_ctl::METHOD_RELOAD_CONFIG, None, &cli);
        assert!(!denied.ok);
        assert_eq!(denied.code, "ScopeDenied");
    }

    #[test]
    fn control_unscoped_callers_rejected_for_every_op() {
        let mut rt = headless_runtime();
        let empty = bitty_ipc::ScopeSet::new();
        let cases: Vec<(&str, Option<String>)> = vec![
            (ipc_ctl::METHOD_LIST_WINDOWS, None),
            (ipc_ctl::METHOD_LIST_VIEWS, None),
            (ipc_ctl::METHOD_LIST_TERMINALS, None),
            (
                ipc_ctl::METHOD_SPAWN_TERMINAL,
                Some(ipc_ctl::params_spawn(None)),
            ),
            (
                ipc_ctl::METHOD_CLOSE_TERMINAL,
                Some(ipc_ctl::params_terminal_id("t:1")),
            ),
            (
                ipc_ctl::METHOD_SEND_INPUT,
                Some(ipc_ctl::params_send_input("t:1", "hi")),
            ),
            (
                ipc_ctl::METHOD_GET_TERMINAL_TEXT,
                Some(ipc_ctl::params_terminal_id("t:1")),
            ),
            (
                ipc_ctl::METHOD_SPLIT_VIEW,
                Some(ipc_ctl::params_split(ipc_ctl::SplitDirection::Right)),
            ),
            (
                ipc_ctl::METHOD_FOCUS_VIEW,
                Some(ipc_ctl::params_focus("v:1")),
            ),
            (ipc_ctl::METHOD_RELOAD_CONFIG, None),
        ];
        for (method, params) in cases {
            let reply = apply_control_envelope(&mut rt, method, params.as_deref(), &empty);
            assert!(!reply.ok, "{method} with empty scopes must fail");
            assert_eq!(
                reply.code, "ScopeDenied",
                "{method} must be ScopeDenied, got {reply:?}"
            );
        }
    }

    #[test]
    fn control_elevated_close_reports_not_found_not_denied() {
        // With elevation, auth passes and existence resolves: closing an
        // absent terminal is NotFound (proving the scope check passed).
        let mut rt = headless_runtime();
        let all = bitty_ipc::ScopeSet::all();
        let bad = ipc_ctl::params_terminal_id("t:999");
        let missing =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&bad), &all);
        assert!(!missing.ok);
        assert_eq!(missing.code, "NotFound");
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_RELOAD_CONFIG, None, &all);
        assert!(done.ok, "elevated reload must succeed: {done:?}");
    }

    #[test]
    #[cfg(unix)]
    fn control_socketpair_roundtrip_headless_live_instance() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        // Full stack over real framing: client bytes -> `serve_connection`
        // -> scope check -> cross-thread queue -> main-thread `Runtime`
        // apply -> reply -> client parse. `Runtime` never leaves this thread.
        // CTX-0220: hold the file-local serial guard — the control queue is
        // process-global and the `wm_*` socket tests drain it concurrently.
        let _wm_guard = hold_wm_lock();
        let mut rt = headless_runtime();
        // Ensure a clean queue (other tests may have left entries on timeout).
        while ipc_ctl::pop_pending_control().is_some() {}

        let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
        let server_info = bitty_ipc::devtools::ServerInfo::new(
            "ctl-proof".to_string(),
            "/tmp/bitty-ctl-proof.sock".to_string(),
            80,
            24,
        );
        let context = bitty_ipc::devtools::ServeContext::with_granted(
            &server_info,
            bitty_ipc::ScopeSet::cli_default(),
        );
        let (mut client, mut server_stream) = UnixStream::pair().expect("socketpair");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let peer = bitty_ipc::devtools::transport_attested_peer(0);
        let handle = std::thread::spawn(move || {
            let mut limiter = bitty_ipc::RateLimiter::rc9_default();
            let clock = || 0u64;
            bitty_ipc::devtools::serve_connection(
                &mut server_stream,
                peer,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
        });

        // Helper: send one devtools envelope, drain the control queue
        // against the live `Runtime`, then read one framed response.
        fn roundtrip(
            client: &mut UnixStream,
            rt: &mut bitty_runtime::Runtime,
            method: &str,
            params: Option<&str>,
        ) -> String {
            let params_part = match params {
                None => String::new(),
                Some(p) => format!(",\"params\":{p}"),
            };
            let envelope = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
            );
            let wire = bitty_ipc::encode_frame(envelope.as_bytes()).unwrap();
            client.write_all(&wire).unwrap();
            // The server thread has now enqueued; drain on this thread
            // (the sole `Runtime` owner) before reading the reply.
            //
            // Poll briefly: the enqueue races the send, so retry until the
            // queue is non-empty or a bound is hit (fail-closed, no sleep
            // loops in production — this spin is test-only).
            let granted = bitty_ipc::ScopeSet::cli_default();
            for _ in 0..100 {
                let drained = drain_global_control_queue(rt, &granted);
                if drained > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let mut header = [0u8; 4];
            client.read_exact(&mut header).unwrap();
            let len = u32::from_be_bytes(header) as usize;
            let mut body = vec![0u8; len];
            client.read_exact(&mut body).unwrap();
            String::from_utf8(body).unwrap()
        }

        // Allowed without elevation: view list reflects live layout.
        let body = roundtrip(&mut client, &mut rt, ipc_ctl::METHOD_LIST_VIEWS, None);
        assert!(
            body.contains("\"result\""),
            "view list must succeed: {body:?}"
        );
        assert!(body.contains("v:1"));

        // Allowed: send to the focused terminal.
        let params = ipc_ctl::params_send_input("t:1", "proof-bytes");
        let body = roundtrip(
            &mut client,
            &mut rt,
            ipc_ctl::METHOD_SEND_INPUT,
            Some(&params),
        );
        assert!(body.contains("\"result\""), "send must succeed: {body:?}");
        assert!(!rt.drain_pending_input().is_empty());

        // Denied without elevation: close needs terminal.manage.
        let params = ipc_ctl::params_terminal_id("t:1");
        let body = roundtrip(
            &mut client,
            &mut rt,
            ipc_ctl::METHOD_CLOSE_TERMINAL,
            Some(&params),
        );
        assert!(body.contains("\"error\""), "close must be denied: {body:?}");
        assert!(body.contains("ScopeDenied"));

        drop(client);
        let stats = handle.join().unwrap().unwrap();
        assert!(stats.requests >= 3, "server must see all requests");
        assert!(stats.denied >= 1, "denial must be counted");
    }

    // ── CTX-0220: headless WM-flow coverage over the devtools IPC surface ──
    //
    // Seat-contested: no GUI driving, no ydotool, no screenshots. A real Unix
    // socket is served by `serve_connection` on a server thread while the
    // test thread owns `Runtime` and drains the global control queue — the
    // same code path the live servo drives. Synchronization is
    // reply-correlation plus deadline-bounded `yield_now` polls (no sleeps);
    // the process-global control queue (and automation/introspection stores)
    // serialize on the file-local guard (CTX-0179 pattern).
    //
    // Where no IPC verb exists for an assertion (directional focus-move,
    // zoom, resize, layout-leaf removal), the Runtime seam is driven directly
    // and the missing method is recorded in
    // `recording/ctx-0220/missing-ipc-methods.md` for the CTX-0188 follow-up.
    // No new IPC methods are added here (out of scope).

    /// Serial guard for the process-global control queue + automation stores.
    #[cfg(unix)]
    fn wm_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    fn hold_wm_lock() -> std::sync::MutexGuard<'static, ()> {
        wm_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Short temp socket path (macOS SUN_LEN: payload < 100 bytes).
    #[cfg(unix)]
    fn wm_socket_path(tag: &str) -> String {
        let path = format!("/tmp/btw{}{tag}/s.sock", std::process::id());
        assert!(
            path.len() < 100,
            "socket path must fit macOS SUN_LEN: {path} ({} bytes)",
            path.len()
        );
        path
    }

    /// Connect with a deadline via `yield_now` retries (no sleeps): the
    /// server thread binds concurrently, so the first attempts may race it.
    #[cfg(unix)]
    fn wm_connect(path: &str) -> std::os::unix::net::UnixStream {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                        .unwrap();
                    return stream;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(err) => panic!("connect {path} within deadline: {err}"),
            }
        }
    }

    #[cfg(unix)]
    fn wm_owner_uid(path: &str) -> u32 {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
    }

    /// Serve one connection on a real socket with explicit granted scopes.
    /// Returns the server thread; it asserts request/response parity itself.
    #[cfg(unix)]
    fn spawn_wm_server(
        socket_path: String,
        granted: bitty_ipc::ScopeSet,
        session: &str,
        min_requests: u64,
    ) -> std::thread::JoinHandle<()> {
        let session = session.to_string();
        std::thread::spawn(move || {
            let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let verified = bitty_ipc::devtools::transport_attested_peer(wm_owner_uid(&socket_path));
            let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
            let server = bitty_ipc::devtools::ServerInfo::new(
                "wm-proof".to_string(),
                socket_path.clone(),
                80,
                24,
            );
            let context =
                bitty_ipc::devtools::ServeContext::with_granted_session(&server, granted, &session);
            let mut limiter = bitty_ipc::RateLimiter::rc9_default();
            let clock = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0)
            };
            let stats = bitty_ipc::devtools::serve_connection(
                &mut stream,
                verified,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
            .unwrap();
            assert!(
                stats.requests >= min_requests,
                "expected at least {min_requests} requests, saw {}",
                stats.requests
            );
            assert_eq!(stats.responses, stats.requests);
        })
    }

    /// Test-thread side of the WM harness: owns `Runtime` (it is `!Send`)
    /// and drains the global control queue so the server thread's
    /// `enqueue_control_and_wait` unblocks with a correlated reply.
    #[cfg(unix)]
    struct WmHarness {
        stream: std::os::unix::net::UnixStream,
        next_id: u64,
        rt: bitty_runtime::Runtime,
        granted: bitty_ipc::ScopeSet,
    }

    #[cfg(unix)]
    impl WmHarness {
        fn new(stream: std::os::unix::net::UnixStream, granted: bitty_ipc::ScopeSet) -> Self {
            // Clean slate: other tests may have left queue entries behind.
            while bitty_ipc::ctl::pop_pending_control().is_some() {}
            Self {
                stream,
                next_id: 1,
                rt: headless_runtime(),
                granted,
            }
        }

        fn send_envelope(&mut self, method: &str, params: Option<&str>) -> u64 {
            use std::io::Write;

            let id = self.next_id;
            self.next_id += 1;
            let params_part = match params {
                None => String::new(),
                Some(p) => format!(",\"params\":{p}"),
            };
            let envelope = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
            );
            let wire = bitty_ipc::encode_frame(envelope.as_bytes()).unwrap();
            self.stream.write_all(&wire).unwrap();
            self.stream.flush().unwrap();
            id
        }

        fn read_reply(&mut self, id: u64) -> String {
            use std::io::Read;

            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header).unwrap();
            let len = u32::from_be_bytes(header) as usize;
            let mut body = vec![0u8; len];
            self.stream.read_exact(&mut body).unwrap();
            let text = String::from_utf8(body).unwrap();
            assert!(
                text.contains(&format!("\"id\":{id}")),
                "response lost correlation id {id}: {text}"
            );
            text
        }

        /// Control verbs (`splitView`, `focusView`, …) enqueue and block the
        /// server thread: drain until the queue yields work (deadline-bound,
        /// well under the 5 s enqueue timeout), then read the reply the
        /// drain produced. The reply itself is the generation-wait — when it
        /// arrives, the mutation has been applied.
        fn ctl(&mut self, method: &str, params: Option<&str>) -> String {
            let id = self.send_envelope(method, params);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                if drain_global_control_queue(&mut self.rt, &self.granted) > 0 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "drain deadline hit for {method} (no live runtime draining?)"
                );
                std::thread::yield_now();
            }
            self.read_reply(id)
        }

        /// Direct verbs (`synthesizeInput`, `captureFrame`, `getInputRing`)
        /// answer without the control queue: plain request/response.
        fn direct(&mut self, method: &str, params: &str) -> String {
            let id = self.send_envelope(method, Some(params));
            self.read_reply(id)
        }

        fn leaf_ids(&self) -> Vec<u64> {
            self.rt.layout().leaf_ids().iter().map(|id| id.0).collect()
        }
    }

    /// Remove `target` from a split tree, collapsing its parent to the
    /// sibling — the same semantics as `TerminalApp::close_focused_leaf`
    /// (`main.rs`). There is no Runtime/IPC leaf-removal verb today (see the
    /// CTX-0188-followup note), so close→survivor asserts through this seam
    /// plus `Runtime::set_layout` (whose first-leaf refocus rule is product
    /// code under test).
    fn wm_prune_split_leaf(
        node: &mut bitty_runtime::LayoutNode,
        target: bitty_runtime::ViewId,
    ) -> bool {
        use bitty_runtime::LayoutNode;

        match node {
            LayoutNode::Leaf(_) => false,
            LayoutNode::Split { first, second, .. } => {
                let first_hit = matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == target);
                let second_hit = matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == target);
                if first_hit {
                    *node = (**second).clone();
                    true
                } else if second_hit {
                    *node = (**first).clone();
                    true
                } else {
                    wm_prune_split_leaf(first, target) || wm_prune_split_leaf(second, target)
                }
            }
            LayoutNode::Stack(children) => {
                if let Some(pos) = children
                    .iter()
                    .position(|c| matches!(c, LayoutNode::Leaf(v) if v.id() == target))
                {
                    if children.len() <= 1 {
                        return false;
                    }
                    children.remove(pos);
                    true
                } else {
                    children.iter_mut().any(|c| wm_prune_split_leaf(c, target))
                }
            }
            LayoutNode::Overlay { base, overlay, .. } => {
                wm_prune_split_leaf(base, target) || wm_prune_split_leaf(overlay, target)
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn wm_split_routing_close_survivor_over_socket() {
        let _guard = hold_wm_lock();
        let granted = bitty_ipc::ScopeSet::all();
        let socket_path = wm_socket_path("sr");
        bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
        // 8 control verbs below: list, split, list, send-denied, focus,
        // send-ok, text, close-denied = 8 requests.
        let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-flow", 8);
        let mut h = WmHarness::new(wm_connect(&socket_path), granted);

        // Single leaf, focused.
        let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
        assert!(body.contains("\"result\""), "view list: {body}");
        assert!(
            body.contains("\"id\":\"v:1\",\"focused\":true"),
            "one focused leaf: {body}"
        );

        // Split right over IPC: new leaf appears, focus stays on v:1.
        let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
        assert!(
            body.contains("\"new_view\":\"v:2\""),
            "split names v:2: {body}"
        );
        assert_eq!(h.leaf_ids(), vec![1, 2]);
        let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
        assert!(
            body.contains("\"id\":\"v:1\",\"focused\":true"),
            "focus stays: {body}"
        );
        assert!(
            body.contains("\"id\":\"v:2\",\"focused\":false"),
            "new leaf unfocused: {body}"
        );

        // Focused-only routing: sending to the unfocused leaf fails closed
        // and names the focus verb instead of retargeting input.
        let params = ipc_ctl::params_send_input("t:2", "hi");
        let body = h.ctl(ipc_ctl::METHOD_SEND_INPUT, Some(&params));
        assert!(
            body.contains("\"error\""),
            "unfocused send must fail: {body}"
        );
        assert!(body.contains("Conflict"), "must be Conflict: {body}");
        assert!(
            body.contains("view focus"),
            "must name the focus verb: {body}"
        );
        assert!(
            h.rt.drain_pending_input().is_empty(),
            "denied bytes must not queue"
        );

        // Focus the new leaf over IPC, then input routes.
        let params = ipc_ctl::params_focus("v:2");
        let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
        assert!(body.contains("\"focused\":\"v:2\""), "focus moves: {body}");
        let params = ipc_ctl::params_send_input("t:2", "wm-proof");
        let body = h.ctl(ipc_ctl::METHOD_SEND_INPUT, Some(&params));
        assert!(body.contains("\"sent_to\":\"t:2\""), "send routes: {body}");
        assert!(body.contains("\"bytes\":8"), "byte count: {body}");
        assert!(!h.rt.drain_pending_input().is_empty(), "bytes must queue");
        let params = ipc_ctl::params_terminal_id("t:2");
        let body = h.ctl(ipc_ctl::METHOD_GET_TERMINAL_TEXT, Some(&params));
        assert!(
            body.contains("\"terminal_id\":\"t:2\""),
            "text serves: {body}"
        );

        // Close over IPC tears down the pane *session*, not the leaf: with
        // no live PTY session headlessly this fails closed (Conflict) and
        // the layout is untouched — never a half-removed leaf.
        let params = ipc_ctl::params_terminal_id("t:2");
        let body = h.ctl(ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&params));
        assert!(
            body.contains("\"error\""),
            "session-less close must fail: {body}"
        );
        assert!(body.contains("Conflict"), "must be Conflict: {body}");
        assert!(
            body.contains("no live session"),
            "must name the gap: {body}"
        );
        assert_eq!(h.leaf_ids(), vec![1, 2], "layout untouched by failed close");

        drop(h);
        server.join().unwrap();
        std::fs::remove_file(&socket_path).ok();
    }

    #[test]
    fn wm_close_survivor_via_runtime_seam() {
        // Portable close→survivor half (the socket half above proves
        // `closeTerminal` fails closed without a session): split via the IPC
        // handler, prune the focused leaf via the Runtime seam (no
        // leaf-removal IPC verb exists — see the CTX-0188-followup note),
        // and let `Runtime::set_layout` refocus the survivor.
        let mut rt = headless_runtime();
        let elevated = bitty_ipc::ScopeSet::all();
        let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let done =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &elevated);
        assert!(done.ok, "split must succeed: {done:?}");
        assert_eq!(rt.layout().leaf_ids().len(), 2);
        let focus = ipc_ctl::params_focus("v:2");
        let moved =
            apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&focus), &elevated);
        assert!(moved.ok, "focus must succeed: {moved:?}");

        let mut layout = rt.layout().clone();
        assert!(wm_prune_split_leaf(
            &mut layout,
            bitty_runtime::ViewId::new(2)
        ));
        // Pruning a missing leaf or the last leaf refuses (no empty tree).
        assert!(!wm_prune_split_leaf(
            &mut layout.clone(),
            bitty_runtime::ViewId::new(9)
        ));
        rt.set_layout(layout);
        assert_eq!(rt.leaf_count(), 1);
        assert_eq!(
            rt.focused_view(),
            Some(bitty_runtime::ViewId::new(1)),
            "refocus to survivor"
        );
        let allocs = rt.layout_allocations();
        assert_eq!(allocs.len(), 1);
        assert_eq!(allocs[0].1, rt.container(), "survivor reflows full-bleed");
    }

    #[test]
    #[cfg(unix)]
    fn wm_focus_move_across_leaves_over_socket() {
        let _guard = hold_wm_lock();
        let granted = bitty_ipc::ScopeSet::all();
        let socket_path = wm_socket_path("fm");
        bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
        // split, focus, split, then one focus+list pair per leaf (3) = 9.
        let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-focus", 9);
        let mut h = WmHarness::new(wm_connect(&socket_path), granted);

        // Three leaves: v:1 left, v:2 top-right, v:3 bottom-right.
        let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
        assert!(body.contains("\"new_view\":\"v:2\""), "first split: {body}");
        let params = ipc_ctl::params_focus("v:2");
        let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
        assert!(body.contains("\"focused\":\"v:2\""), "focus v:2: {body}");
        let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Down);
        let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
        assert!(
            body.contains("\"new_view\":\"v:3\""),
            "second split: {body}"
        );
        assert_eq!(h.leaf_ids(), vec![1, 2, 3]);

        // `focusView` reaches every leaf; exactly one flag is set each time.
        for target in ["v:1", "v:2", "v:3"] {
            let params = ipc_ctl::params_focus(target);
            let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
            assert!(
                body.contains(&format!("\"focused\":\"{target}\"")),
                "focus {target}: {body}"
            );
            let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
            for leaf in ["v:1", "v:2", "v:3"] {
                let want = leaf == target;
                assert!(
                    body.contains(&format!("\"id\":\"{leaf}\",\"focused\":{want}")),
                    "flags after focusing {target}: {body}"
                );
            }
        }

        // Directional `move_focus` has no IPC verb (owed follow-up), so it
        // is proven via the Runtime seam on the IPC-built layout. Spatial
        // assertions stay orientation-agnostic on purpose: defect CTX-0220-D1
        // (filed, not fixed here) — the IPC split axis mapping is rotated
        // 90° vs the canonical keymap path (`split_dir_to_axis` in main.rs
        // maps Right→Horizontal/left-right, while `apply_control` maps
        // Right→Vertical/top-bottom), so exact spatial expectations would
        // enshrine the bug.
        use bitty_runtime::{FocusDirection, ViewId};

        let leaves = h.leaf_ids();
        // Depth-first cycling reaches the whole leaf set and wraps.
        assert!(h.rt.set_focus(ViewId::new(1)));
        assert_eq!(h.rt.move_focus(FocusDirection::Prev), Some(ViewId::new(3)));
        assert!(h.rt.set_focus(ViewId::new(3)));
        assert_eq!(h.rt.move_focus(FocusDirection::Next), Some(ViewId::new(1)));
        // Spatial moves from every leaf never leave the leaf set (edge
        // moves may return `None`, keeping focus) and are deterministic
        // (pure function of layout + container + focus).
        for id in 1..=3u64 {
            assert!(h.rt.set_focus(ViewId::new(id)));
            for dir in [
                FocusDirection::Up,
                FocusDirection::Down,
                FocusDirection::Left,
                FocusDirection::Right,
            ] {
                assert!(h.rt.set_focus(ViewId::new(id)));
                let first = h.rt.move_focus(dir);
                assert!(
                    first.is_none_or(|v| leaves.contains(&v.0)),
                    "spatial move {dir:?} from v:{id} must stay in-set, got {first:?}"
                );
                let focused = h.rt.focused_view().expect("focus must persist");
                assert!(leaves.contains(&focused.0), "focus must stay valid");
                assert!(h.rt.set_focus(ViewId::new(id)));
                let second = h.rt.move_focus(dir);
                assert_eq!(second, first, "spatial move must be deterministic");
            }
        }

        drop(h);
        server.join().unwrap();
        std::fs::remove_file(&socket_path).ok();
    }

    #[test]
    fn wm_zoom_on_off_reflow_via_runtime_seam() {
        // No zoom IPC verb exists (canonical zoom is
        // `TerminalApp::apply_chrome_action(ToggleZoom)` in main.rs, already
        // unit-covered there); the Runtime seam replicates its exact
        // stash→single-leaf→restore steps while `listViews` keeps the IPC
        // handler in the loop.
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
        assert!(done.ok, "split must succeed: {done:?}");
        assert_eq!(rt.leaf_count(), 2);

        let container = rt.container();
        let before = rt.layout_allocations();
        assert_eq!(before.len(), 2);
        let focused = rt.focused_view().expect("focus must exist");

        // Zoom on: stash the tree, present only the focused leaf.
        let backup = rt.layout().clone();
        let view = rt
            .layout()
            .find_leaf(focused)
            .cloned()
            .expect("focused leaf");
        rt.set_layout(bitty_runtime::LayoutNode::leaf(view));
        assert_eq!(rt.leaf_count(), 1);
        assert_eq!(rt.focused_view(), Some(focused));
        let zoomed = rt.layout_allocations();
        assert_eq!(zoomed.len(), 1);
        assert_eq!(zoomed[0].0, focused);
        // Zero-gap default tiles edge-to-edge: the zoomed leaf is full-bleed.
        assert_eq!(zoomed[0].1, container);
        let views = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_VIEWS, None, &cli);
        assert!(views.ok, "listViews while zoomed: {views:?}");
        assert!(views.result_json.contains("\"focused\":true"));

        // Zoom off: restore the tree bit-identically, focus preserved.
        rt.set_layout(backup);
        assert_eq!(rt.leaf_count(), 2);
        assert_eq!(rt.focused_view(), Some(focused));
        assert_eq!(
            rt.layout_allocations(),
            before,
            "restore must reflow identically"
        );
    }

    #[test]
    fn wm_resize_reflow_via_runtime_seam() {
        // No resize IPC verb exists; `set_container` + `reflow_layout` is the
        // documented headless seam (no physical surface required).
        let mut rt = headless_runtime();
        let cli = bitty_ipc::ScopeSet::cli_default();
        let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
        assert!(done.ok, "split must succeed: {done:?}");
        let focused = rt.focused_view().expect("focus must exist");

        let area = |rt: &bitty_runtime::Runtime| {
            rt.layout_allocations()
                .iter()
                .find(|(id, _)| *id == focused)
                .map(|(_, r)| u32::from(r.width) * u32::from(r.height))
                .unwrap_or(0)
        };
        let before = area(&rt);
        assert!(before > 0, "focused leaf must have area");

        // Grow: the focused leaf gains cells and stays inside the container.
        rt.set_container(bitty_runtime::UiRect::new(0, 0, 160, 48));
        let grown_allocs = rt.reflow_layout();
        assert_eq!(rt.container(), bitty_runtime::UiRect::new(0, 0, 160, 48));
        for (_, rect) in &grown_allocs {
            assert!(
                rect.x + rect.width <= 160 && rect.y + rect.height <= 48,
                "in bounds: {rect:?}"
            );
        }
        assert!(
            area(&rt) > before,
            "grow must add cells to the focused leaf"
        );
        let view = rt.layout().find_leaf(focused).expect("focused leaf");
        let alloc = grown_allocs
            .iter()
            .find(|(id, _)| *id == focused)
            .expect("alloc");
        assert_eq!(
            (view.cols(), view.rows()),
            (alloc.1.width, alloc.1.height),
            "view tracks alloc"
        );

        // Shrink: cells are taken back, leaf count and focus untouched.
        rt.set_container(bitty_runtime::UiRect::new(0, 0, 40, 12));
        rt.reflow_layout();
        assert!(area(&rt) <= before, "shrink must take cells back");
        assert_eq!(rt.leaf_count(), 2);
        assert_eq!(rt.focused_view(), Some(focused));
    }

    #[test]
    #[cfg(unix)]
    fn wm_automation_surface_tracks_split_layout() {
        // `synthesizeInput`/`captureFrame` (CTX-0188) against an IPC-split
        // layout: automation addressing follows the new leaf, and the frame
        // surface keeps serving (redacted) after WM mutations.
        use bitty_ipc::devtools::{
            AutomationFamily, clear_automation_for_tests, clear_introspection_for_tests,
            issue_automation_bearer, publish_grid_text,
        };

        let _guard = hold_wm_lock();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        publish_grid_text(
            vec!["$ echo wm".to_string(), "wm".to_string()],
            1,
            7,
            true,
            41,
            80,
            24,
        );

        let granted = bitty_ipc::ScopeSet::all();
        let socket_path = wm_socket_path("au");
        bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
        // split, synth, ring, capture = 4 requests.
        let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-auto", 4);
        let mut h = WmHarness::new(wm_connect(&socket_path), granted);

        let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
        assert!(body.contains("\"new_view\":\"v:2\""), "split first: {body}");

        // Bearers bind (session, terminal, family); the servo stamps uptime
        // near zero at spawn, so issue at zero like the CTX-0188 harness.
        let synth = issue_automation_bearer("wm-auto", "t:2", AutomationFamily::Synthesize, 0)
            .expect("synth bearer");
        let params = format!(
            "{{\"terminalId\":\"t:2\",\"bearer\":\"{synth}\",\"originLabel\":\"wm-harness\",\"events\":[{{\"type\":\"key\",\"key\":\"Enter\"}}]}}"
        );
        let receipt = h.direct("bitty.debug/synthesizeInput", &params);
        assert!(
            receipt.contains("\"accepted\":1"),
            "synth receipt: {receipt}"
        );
        assert!(
            receipt.contains("\"synthetic\":true"),
            "synthetic flag: {receipt}"
        );
        let ring = h.direct("bitty.debug/getInputRing", "{\"limit\":10}");
        assert!(
            ring.contains("[synthetic:wm-harness]"),
            "synthetic marker for the new leaf: {ring}"
        );

        let cap = issue_automation_bearer("wm-auto", "t:2", AutomationFamily::Capture, 0)
            .expect("capture bearer");
        let params =
            format!("{{\"terminalId\":\"t:2\",\"bearer\":\"{cap}\",\"format\":\"semantic\"}}");
        let frame = h.direct("bitty.debug/captureFrame", &params);
        assert!(
            frame.contains("\"snapshot\":\"frame\""),
            "frame serves post-split: {frame}"
        );
        assert!(
            frame.contains("\"trust\":\"untrusted-observation\""),
            "untrusted label: {frame}"
        );

        drop(h);
        server.join().unwrap();
        clear_automation_for_tests();
        clear_introspection_for_tests();
        std::fs::remove_file(&socket_path).ok();
    }
}
