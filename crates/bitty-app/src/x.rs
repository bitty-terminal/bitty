//! `bitty x`: qualified plugin namespace (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty x`,
//! extension class, collision-free qualified route) as refined by
//! `docs/interfaces/cli.md` (candidate extension grammar).
//!
//! # Contract (implemented slice)
//!
//! - Shape: `bitty x <publisher>.<name> <command> [args] [--format ...]`.
//!   The fully qualified route is always available, even when a short alias
//!   exists. `--help` variants never require an instance and never load a
//!   plugin VM:
//!   - `bitty x --help` enumerates every installed plugin (qualified id,
//!     short-alias status, provided commands) from static manifests.
//!   - `bitty x <id> --help` shows that plugin's command help from its
//!     static manifest.
//! - Short alias `bitty x <name>`: wired only when exactly one installed
//!   plugin claims that short name; two claimants produce a collision
//!   diagnostic (exit 8) and both aliases stay disabled until the user
//!   disambiguates with the qualified route. The qualified route is
//!   documented in every alias path.
//! - Alias discovery uses the static package manifest only; no plugin VM is
//!   ever loaded on this path (safe-mode clean).
//! - `--format table` (default) is human output; `--format json` /
//!   `--format jsonl` emit the versioned envelope (`v: 1`, `command: "x"`)
//!   on stdout with diagnostics on stderr.
//! - Missing id, malformed id, stray `--`, and `--socket`/`--instance`
//!   combined with `x` fail closed (exit 2). Dash-prefixed tokens after the
//!   plugin id are passed through as plugin args (bounded); they are never
//!   interpreted as Bitty flags.
//!
//! # Honest scope of this slice
//!
//! Extension execution is a follow-up: plugin command execution needs the
//! plugin runtime, which `bitty plugin` deliberately does not load either.
//! A known plugin plus a command therefore fails closed with a plugin error
//! (exit 4) naming the qualified route; no plugin code runs. An unknown
//! plugin id fails closed the same way (exit 4) with the installed set
//! listed. The optional top-level alias (`bitty <name>`) is deferred per the
//! RFC: withdrawing or adding it later is one revision because the qualified
//! route preserves every operation.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (help only in this slice).
//! - `2` usage error (missing/malformed id, stray `--`, `--socket`/
//!   `--instance` with `x`, bad `--format`, over-bound args).
//! - `4` plugin error (unknown plugin, execution follow-up, collision naming
//!   stays 8 below).
//! - `8` conflict (short-alias collision between installed plugins).

#![forbid(unsafe_code)]

use std::fmt::Write as _;

use crate::cli::Args;
use crate::cmd::{MAX_CMD_ARGS_BYTES, validate_qualified_id};
use crate::list::json_escape;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;
/// Plugin error (unknown plugin, execution follow-up).
pub const EXIT_PLUGIN: i32 = 4;
/// Conflict (short-alias collision).
pub const EXIT_CONFLICT: i32 = 8;

// ---------------------------------------------------------------------------
// Bounds (T-01 parity, fail closed with exit 2 before any lookup)
// ---------------------------------------------------------------------------

/// Maximum bytes for a plugin id token.
pub const MAX_X_ID_LEN: usize = 128;
/// Maximum bytes for a plugin command token.
pub const MAX_X_COMMAND_LEN: usize = 128;
/// Maximum plugin args after the command (bounded passthrough).
pub const MAX_X_ARGS: usize = 64;
/// Maximum bytes for a `--format` value.
pub const MAX_X_FORMAT_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Static catalog access (no VM, safe-mode clean)
// ---------------------------------------------------------------------------

/// One installed plugin row for help and alias resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPluginRow {
    /// Fully qualified id (`publisher.name`).
    pub id: String,
    /// Short name (after the dot).
    pub short: String,
    /// Manifest version.
    pub version: String,
    /// Provided command names (from static lazy triggers).
    pub commands: Vec<String>,
}

/// Read the installed plugin set from static manifests only.
///
/// Today the installed set is the bundled catalog; user-installed entries
/// join this function when the package manager lands. Never loads a VM.
#[must_use]
pub fn installed_plugins() -> Vec<XPluginRow> {
    rows_from_manifests(&bitty_plugin_host::bundled::all_bundled_manifests())
}

/// Test hook: build rows from an explicit manifest slice (covers collisions
/// and empty registries without touching the global catalog).
#[must_use]
pub fn rows_from_manifests(manifests: &[bitty_plugin_host::PluginManifest]) -> Vec<XPluginRow> {
    let mut rows: Vec<XPluginRow> = manifests
        .iter()
        .map(|m| {
            let id = m.identity.id.to_string();
            let short = id.rsplit('.').next().unwrap_or(&id).to_string();
            let mut commands: Vec<String> =
                m.lazy.commands.iter().map(|c| c.id.to_string()).collect();
            commands.sort();
            commands.dedup();
            XPluginRow {
                id,
                short,
                version: m.identity.version.clone(),
                commands,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Resolve a plugin token to its row.
///
/// Qualified ids (`publisher.name`) match exactly. Short names match only
/// when exactly one installed plugin claims them; two claimants are a
/// collision (both disabled, qualified route required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XResolve {
    /// Exactly one plugin owns the token.
    Found(XPluginRow),
    /// No installed plugin owns the token.
    Unknown,
    /// The short name is claimed by several plugins (all named).
    Collision(Vec<String>),
}

/// Resolve `token` against `rows` (pure; see [`installed_plugins`]).
#[must_use]
pub fn resolve_plugin(token: &str, rows: &[XPluginRow]) -> XResolve {
    if token.contains('.') {
        match rows.iter().find(|r| r.id == token) {
            Some(row) => XResolve::Found(row.clone()),
            None => XResolve::Unknown,
        }
    } else {
        let mut claimants: Vec<&XPluginRow> = rows.iter().filter(|r| r.short == token).collect();
        claimants.sort_by(|a, b| a.id.cmp(&b.id));
        match claimants.len() {
            0 => XResolve::Unknown,
            1 => XResolve::Found(claimants[0].clone()),
            _ => XResolve::Collision(claimants.into_iter().map(|r| r.id.clone()).collect()),
        }
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Output shape for `bitty x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XFormat {
    /// Human output (default).
    Table,
    /// Versioned envelope, one JSON value.
    Json,
    /// Versioned envelope, one JSON value per line.
    Jsonl,
}

impl XFormat {
    /// Parse a `--format` value (case-insensitive, trimmed).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(value) => {
                let shape = value.trim().to_lowercase();
                match shape.as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    _ => Err(format!(
                        "bitty x: unknown --format {value:?} (want table|json|jsonl)\n{}",
                        x_usage()
                    )),
                }
            }
        }
    }
}

/// Validated `bitty x` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XRequest {
    /// Plugin token as invoked (qualified id or short alias).
    pub plugin: String,
    /// Plugin command plus passthrough args (empty for `--help` paths).
    pub command_args: Vec<String>,
    /// Output shape.
    pub format: XFormat,
    /// True when any `--help` token requests plugin help instead of dispatch.
    pub help: bool,
}

/// Parse failure: help vs usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XParseError {
    /// `bitty x --help`: print [`x_help_text`] to stdout, exit 0.
    Help,
    /// Usage error: print the message (it already ends with usage) to stderr,
    /// exit 2.
    Usage(String),
}

impl XParseError {
    /// Render the stderr message for [`XParseError::Usage`].
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Help => String::from("bitty x: help requested"),
            Self::Usage(message) => message,
        }
    }
}

/// Usage line for `bitty x`.
#[must_use]
pub fn x_usage() -> String {
    String::from("Usage: bitty x <publisher>.<name> <command> [args] [--format table|json|jsonl]")
}

/// Help text enumerating the installed plugin set (`--help` path).
#[must_use]
pub fn x_help_text() -> String {
    let mut out = String::from(
        "bitty x — qualified plugin namespace (extension, no VM load)\n\
         \n\
         Usage: bitty x <publisher>.<name> <command> [args]\n\
         \n\
         Every plugin command is addressable without alias or help\n\
         regeneration. A short alias `bitty x <name>` works only when a single\n\
         installed plugin claims that name; collisions disable both aliases\n\
         until the qualified route disambiguates. `bitty x <id> --help` shows\n\
         one plugin's commands.\n\
         \n\
         Installed plugins:\n",
    );
    let rows = installed_plugins();
    if rows.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for row in &rows {
            let _ = write!(out, "  {} ({})", row.id, row.version);
            if row.commands.is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(out, " — {}", row.commands.join(", "));
            }
        }
    }
    out.push_str(
        "\nExtension execution is a follow-up: this build resolves the route\n\
         and fails closed without running plugin code.\n",
    );
    out
}

/// Help text for one plugin's commands (`bitty x <id> --help` path).
#[must_use]
pub fn x_plugin_help_text(row: &XPluginRow) -> String {
    let mut out = format!(
        "bitty x {} — plugin commands (static manifest, no VM load)\n\
         \n\
         Version {}. Invoke as `bitty x {} <command> [args]`",
        row.id, row.version, row.id
    );
    if row.short != row.id {
        let _ = write!(out, " (short alias `bitty x {}`)", row.short);
    }
    out.push_str(".\n\nCommands:\n");
    if row.commands.is_empty() {
        out.push_str("  (none declared)\n");
    } else {
        for command in &row.commands {
            let _ = writeln!(out, "  {command}");
        }
    }
    out
}

/// Validate post-`x` tokens plus the optional pre-word `--format`.
///
/// `pre_format` is the global `--format` seen before the `x` word; an
/// explicit post-word `--format` wins.
pub fn parse_x_request(
    tokens: &[String],
    pre_format: Option<&str>,
) -> Result<XRequest, XParseError> {
    let mut plugin: Option<String> = None;
    let mut command_args: Vec<String> = Vec::new();
    let mut format: Option<String> = None;
    let mut help = false;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            if plugin.is_none() {
                return Err(XParseError::Help);
            }
            help = true;
            i += 1;
            continue;
        }
        if token == "--no-color" {
            // Accepted for parity; x tables are plain text.
            i += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--format=") {
            if value.len() > MAX_X_FORMAT_LEN {
                return Err(usage_too_long_format());
            }
            format = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--format" {
            if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                if tokens[i + 1].len() > MAX_X_FORMAT_LEN {
                    return Err(usage_too_long_format());
                }
                format = Some(tokens[i + 1].clone());
                i += 2;
            } else {
                return Err(XParseError::Usage(format!(
                    "bitty x: --format needs a value (table|json|jsonl)\n{}",
                    x_usage()
                )));
            }
            continue;
        }
        if token == "--" {
            return Err(XParseError::Usage(format!(
                "bitty x: unexpected `--` (plugin args pass through without a separator)\n{}",
                x_usage()
            )));
        }
        if plugin.is_none() {
            if token.len() > MAX_X_ID_LEN {
                return Err(XParseError::Usage(format!(
                    "bitty x: plugin id too long (max {MAX_X_ID_LEN} bytes)\n{}",
                    x_usage()
                )));
            }
            // Short aliases skip segment validation here: resolution reports
            // unknown/collision diagnostics, and only qualified ids validate
            // as registry ids.
            if token.contains('.') {
                if let Err(reason) = validate_qualified_id(token) {
                    return Err(XParseError::Usage(format!(
                        "bitty x: invalid plugin id: {reason}\n{}",
                        x_usage()
                    )));
                }
            } else if token.is_empty()
                || token
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            {
                return Err(XParseError::Usage(format!(
                    "bitty x: invalid plugin id {token:?}\n{}",
                    x_usage()
                )));
            }
            plugin = Some(token.clone());
            i += 1;
            continue;
        }
        if command_args.len() >= MAX_X_ARGS {
            return Err(XParseError::Usage(format!(
                "bitty x: too many arguments (max {MAX_X_ARGS})\n{}",
                x_usage()
            )));
        }
        if token.len() > MAX_CMD_ARGS_BYTES {
            return Err(XParseError::Usage(format!(
                "bitty x: argument too long (max {MAX_CMD_ARGS_BYTES} bytes)\n{}",
                x_usage()
            )));
        }
        command_args.push(token.clone());
        i += 1;
    }
    let Some(plugin) = plugin else {
        return Err(XParseError::Usage(format!(
            "bitty x: missing <publisher>.<name>\n{}",
            x_usage()
        )));
    };
    if !help && command_args.is_empty() {
        return Err(XParseError::Usage(format!(
            "bitty x: missing <command> for plugin {plugin:?} (see `bitty x {plugin} --help`)\n{}",
            x_usage()
        )));
    }
    // Command tokens validate like loose ids (bounded, no control bytes);
    // deeper schema checks belong to the executing runtime (follow-up).
    if !help {
        let command = &command_args[0];
        if command.len() > MAX_X_COMMAND_LEN
            || command
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(XParseError::Usage(format!(
                "bitty x: invalid command {command:?}\n{}",
                x_usage()
            )));
        }
    }
    let shape = format.as_deref().or(pre_format);
    match XFormat::parse(shape) {
        Ok(format) => Ok(XRequest {
            plugin,
            command_args,
            format,
            help,
        }),
        Err(message) => Err(XParseError::Usage(message)),
    }
}

fn usage_too_long_format() -> XParseError {
    XParseError::Usage(format!(
        "bitty x: --format value too long (max {MAX_X_FORMAT_LEN} bytes)\n{}",
        x_usage()
    ))
}

/// Render an ok:false envelope for plugin-error and collision paths.
#[must_use]
pub fn format_x_error_envelope(kind: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"x\",\"ok\":false,\"error\":{{\"class\":\"{kind}\",\"code\":\"{code}\",\"message\":\"{}\"}}}}",
        json_escape(message)
    )
}

/// Execute a validated request; returns the process exit code.
///
/// Help paths print to stdout (exit 0). Unknown plugins and execution
/// attempts fail closed (exit 4); short-alias collisions fail closed
/// (exit 8). Table diagnostics go to stderr; json/jsonl emit the ok:false
/// envelope on stdout plus the diagnostic on stderr.
pub fn run_x(request: &XRequest) -> i32 {
    let rows = installed_plugins();
    match resolve_plugin(&request.plugin, &rows) {
        XResolve::Collision(claimants) => {
            let message = format!(
                "bitty x: short alias {:?} is claimed by several plugins ({}) — \
                 both aliases disabled until the qualified route disambiguates \
                 (e.g. `bitty x {} --help`)",
                request.plugin,
                claimants.join(", "),
                claimants[0]
            );
            emit_x_error(request.format, "Conflict", "AliasCollision", &message);
            EXIT_CONFLICT
        }
        XResolve::Unknown => {
            let known: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
            let message = if known.is_empty() {
                format!(
                    "bitty x: unknown plugin {:?} (no plugins installed)",
                    request.plugin
                )
            } else {
                format!(
                    "bitty x: unknown plugin {:?} (installed: {})",
                    request.plugin,
                    known.join(", ")
                )
            };
            emit_x_error(request.format, "NotFound", "UnknownPlugin", &message);
            EXIT_PLUGIN
        }
        XResolve::Found(row) => {
            if request.help {
                // `bitty x <id> --help`: static command help, exit 0.
                println!("{}", x_plugin_help_text(&row));
                return EXIT_OK;
            }
            let command = request.command_args[0].clone();
            let known_command = row
                .commands
                .iter()
                .any(|c| c == &command || c.rsplit(':').next().is_some_and(|tail| tail == command));
            let message = if known_command {
                format!(
                    "bitty x: extension execution is a follow-up — plugin {:?} command {:?} \
                     resolves but no plugin code runs in this build (qualified route reserved)",
                    row.id, command
                )
            } else {
                let available = if row.commands.is_empty() {
                    String::from("(none declared)")
                } else {
                    row.commands.join(", ")
                };
                format!(
                    "bitty x: plugin {:?} has no command {:?} (available: {available})",
                    row.id, command
                )
            };
            emit_x_error(request.format, "NotFound", "ExtensionUnavailable", &message);
            EXIT_PLUGIN
        }
    }
}

fn emit_x_error(format: XFormat, kind: &str, code: &str, message: &str) {
    match format {
        XFormat::Table => {
            eprintln!("{message}");
        }
        XFormat::Json | XFormat::Jsonl => {
            println!("{}", format_x_error_envelope(kind, code, message));
            eprintln!("{message}");
        }
    }
}

/// Runs `bitty x`; returns the process exit code.
///
/// - `bitty x --help` prints the installed set to stdout, exit 0, and never
///   needs an instance or a plugin VM.
/// - The global pre-word `--format` applies when the post-word tokens set
///   none (post-word wins).
/// - `--socket`/`--instance` combined with `x` are usage errors.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.ctl_socket_pre.is_some()
        || args.list_socket.is_some()
        || args.dev_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_instance.is_some()
        || args.dev_instance_pre.is_some()
    {
        eprintln!(
            "bitty x: --socket/--instance need a runtime command (x carries no target selection)\n{}",
            x_usage()
        );
        return EXIT_USAGE;
    }
    let post_has_format = args
        .x_raw
        .iter()
        .any(|t| t == "--format" || t.starts_with("--format="));
    let pre = if post_has_format {
        None
    } else {
        args.doctor_format.as_deref()
    };
    match parse_x_request(&args.x_raw, pre) {
        Err(XParseError::Help) => {
            print!("{}", x_help_text());
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{}", err.message());
            EXIT_USAGE
        }
        Ok(request) => run_x(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn fixture_rows() -> Vec<XPluginRow> {
        vec![
            XPluginRow {
                id: String::from("example.markdown"),
                short: String::from("markdown"),
                version: String::from("1.0.0"),
                commands: vec![String::from("example.markdown:render")],
            },
            XPluginRow {
                id: String::from("other.markdown"),
                short: String::from("markdown"),
                version: String::from("2.0.0"),
                commands: vec![],
            },
            XPluginRow {
                id: String::from("solo.sessions"),
                short: String::from("sessions"),
                version: String::from("0.1.0"),
                commands: vec![String::from("solo.sessions:open")],
            },
        ]
    }

    #[test]
    fn resolution_prefers_qualified_short_needs_unique() {
        let rows = fixture_rows();
        assert!(matches!(
            resolve_plugin("example.markdown", &rows),
            XResolve::Found(_)
        ));
        assert!(matches!(
            resolve_plugin("missing-thing", &rows),
            XResolve::Unknown
        ));
        assert!(matches!(
            resolve_plugin("nope.unknown", &rows),
            XResolve::Unknown
        ));
        assert!(matches!(
            resolve_plugin("sessions", &rows),
            XResolve::Found(_)
        ));
        match resolve_plugin("markdown", &rows) {
            XResolve::Collision(ids) => {
                assert_eq!(ids, vec!["example.markdown", "other.markdown"]);
            }
            other => panic!("expected collision, got {other:?}"),
        }
    }

    #[test]
    fn installed_set_reads_static_manifests() {
        let rows = installed_plugins();
        assert!(!rows.is_empty(), "bundled catalog is the installed set");
        assert!(rows.iter().all(|r| r.id.contains('.')));
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), rows.len(), "no duplicate rows");
    }

    #[test]
    fn missing_command_separator_and_bad_format_fail() {
        assert!(matches!(
            parse_x_request(&[], None),
            Err(XParseError::Usage(_))
        ));
        assert!(matches!(
            parse_x_request(&tokens(&["example.markdown"]), None),
            Err(XParseError::Usage(_))
        ));
        assert!(matches!(
            parse_x_request(&tokens(&["example.markdown", "render", "--"]), None),
            Err(XParseError::Usage(_))
        ));
        assert!(matches!(
            parse_x_request(&tokens(&["--help", "extra"]), None),
            Err(XParseError::Help)
        ));
        let req = parse_x_request(&tokens(&["example.markdown", "--help"]), None).unwrap();
        assert!(req.help);
        let req = parse_x_request(
            &tokens(&[
                "example.markdown",
                "render",
                "README.md",
                "--format",
                "json",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(req.format, XFormat::Json);
        assert_eq!(req.command_args, vec!["render", "README.md"]);
    }

    #[test]
    fn dash_args_pass_through_never_flags() {
        let req = parse_x_request(
            &tokens(&["example.markdown", "render", "--file", "README.md"]),
            None,
        )
        .unwrap();
        assert_eq!(req.command_args, vec!["render", "--file", "README.md"]);
    }

    #[test]
    fn error_envelope_is_versioned() {
        let envelope = format_x_error_envelope("NotFound", "UnknownPlugin", "nope");
        assert!(envelope.contains("\"v\":1"), "envelope: {envelope}");
        assert!(
            envelope.contains("\"command\":\"x\""),
            "envelope: {envelope}"
        );
        assert!(envelope.contains("\"ok\":false"), "envelope: {envelope}");
    }
}
