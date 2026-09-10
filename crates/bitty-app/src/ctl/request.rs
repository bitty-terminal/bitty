//! `bitty ctl` request model and parsing (split from `ctl.rs`, CTX-0307).

use bitty_ipc::ctl as ipc_ctl;

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
    WorkspaceList,
    WorkspaceNew,
    WorkspaceClose { workspace_id: String },
    WorkspaceFocus { workspace_id: String },
    WorkspaceMove { workspace_id: String },
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
            Self::WorkspaceList => "core.workspace.list",
            Self::WorkspaceNew => "core.workspace.new",
            Self::WorkspaceClose { .. } => "core.workspace.close",
            Self::WorkspaceFocus { .. } => "core.workspace.focus",
            Self::WorkspaceMove { .. } => "core.workspace.move",
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
            Self::WorkspaceList => Some(ipc_ctl::METHOD_LIST_WORKSPACES),
            Self::WorkspaceNew => Some(ipc_ctl::METHOD_NEW_WORKSPACE),
            Self::WorkspaceClose { .. } => Some(ipc_ctl::METHOD_CLOSE_WORKSPACE),
            Self::WorkspaceFocus { .. } => Some(ipc_ctl::METHOD_FOCUS_WORKSPACE),
            Self::WorkspaceMove { .. } => Some(ipc_ctl::METHOD_MOVE_WORKSPACE),
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
            | Self::WorkspaceList
            | Self::WorkspaceNew
            | Self::ConfigReload => None,
            Self::TerminalSpawn { cwd } => Some(ipc_ctl::params_spawn(cwd.as_deref())),
            Self::TerminalClose { terminal_id } => Some(ipc_ctl::params_terminal_id(terminal_id)),
            Self::TerminalSend { terminal_id, text } => {
                Some(ipc_ctl::params_send_input(terminal_id, text))
            }
            Self::TerminalText { terminal_id } => Some(ipc_ctl::params_terminal_id(terminal_id)),
            Self::ViewSplit { direction } => Some(ipc_ctl::params_split(*direction)),
            Self::ViewFocus { view_id } => Some(ipc_ctl::params_focus(view_id)),
            Self::WorkspaceClose { workspace_id }
            | Self::WorkspaceFocus { workspace_id }
            | Self::WorkspaceMove { workspace_id } => Some(ipc_ctl::params_workspace(workspace_id)),
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
         \x20 workspace list | workspace new | workspace close ws:N | workspace focus ws:N | workspace move ws:N\n\
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
            workspace list                core.workspace.list (view.inspect)\n  \
            workspace new                 core.workspace.new (view.manage)\n  \
            workspace close ws:N          core.workspace.close (terminal.manage, elevation; kills live sessions)\n  \
            workspace focus ws:N          core.workspace.focus (view.manage)\n  \
            workspace move ws:N           core.workspace.move (view.manage; moves focused window)\n  \
            config reload                 core.config.reload (config.modify, elevation)\n\
         \n\
         Elevation: only terminal spawn, terminal close (terminal.manage),\n  \
            workspace close (terminal.manage), and\n\
         \x20 config reload (config.modify) need BITTY_CTL_ELEVATE\n\
         \x20 (comma-separated scopes, e.g. BITTY_CTL_ELEVATE=terminal.manage,config.modify).\n\
         \x20 Without it those four verbs fail closed (exit 7, no partial state).\n\
         \x20 All other verbs — including view split / view focus (view.manage)\n\
         \x20 and workspace list / new / focus / move, and every list, terminal send,\n\
         \x20 and terminal text verb — need no elevation.\n\
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
        (Some("workspace"), Some("list")) => {
            reject_extra(rest, "workspace list")?;
            reject_ctl_options_for("workspace list", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::WorkspaceList, targeting))
        }
        (Some("workspace"), Some("new")) => {
            reject_extra(rest, "workspace new")?;
            reject_ctl_options_for("workspace new", split_dir.is_some(), spawn_cwd.is_some())?;
            Ok((CtlRequest::WorkspaceNew, targeting))
        }
        (Some("workspace"), Some("close")) => {
            reject_ctl_options_for("workspace close", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "workspace close", "ws:N (e.g. ws:2)")?;
            ipc_ctl::parse_workspace_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid workspace id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::WorkspaceClose {
                    workspace_id: id.to_string(),
                },
                targeting,
            ))
        }
        (Some("workspace"), Some("focus")) => {
            reject_ctl_options_for("workspace focus", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "workspace focus", "ws:N (e.g. ws:1)")?;
            ipc_ctl::parse_workspace_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid workspace id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::WorkspaceFocus {
                    workspace_id: id.to_string(),
                },
                targeting,
            ))
        }
        (Some("workspace"), Some("move")) => {
            reject_ctl_options_for("workspace move", split_dir.is_some(), spawn_cwd.is_some())?;
            let id = single_arg(rest, "workspace move", "ws:N (e.g. ws:2)")?;
            ipc_ctl::parse_workspace_id(id).map_err(|err| CtlParseError::Usage {
                message: format!("bitty ctl: invalid workspace id {id:?}: {err}"),
            })?;
            Ok((
                CtlRequest::WorkspaceMove {
                    workspace_id: id.to_string(),
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
