//! `bitty cmd`: direct qualified executable invocation (CTX-0763, #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty cmd`, mixed
//! class, escape hatch for generators that emit registry ids verbatim).
//!
//! # Contract (implemented slice)
//!
//! - Shape: `bitty cmd <qualified-id> [--format table|json|jsonl] [-- <args-json>]`.
//! - The qualified id validates against the registry rule (`<= 128` bytes,
//!   dot/`:`-separated segments matching `^[a-z][a-z0-9_]*$`, at least two
//!   segments, no whitespace/control bytes). Both the dot form
//!   (`core.terminal.text`) and the colon form (`example.markdown:render`)
//!   are accepted, mirroring `bitty inspect command`.
//! - `--` before a raw JSON argument blob is the only legal separator use;
//!   exactly one blob is accepted (bounded below). A second `--` or a second
//!   blob is a usage error.
//! - `--format table` (default) is human output; `--format json` /
//!   `--format jsonl` emit the versioned envelope (`v: 1`, `command: "cmd"`)
//!   on stdout with diagnostics on stderr.
//! - `--help` never requires an instance and never loads a plugin VM.
//! - Missing id, malformed id, unknown flags, bad `--format`, stray positionals,
//!   and `--socket`/`--instance` combined with `cmd` fail closed (exit 2).
//!
//! # Honest scope of this slice
//!
//! Live dispatch against the terminal registry is a follow-up: the shared
//! executable registry has not landed as a module yet (see `ctl`, whose verbs
//! map one-to-one to registry executables once it does). A well-formed id
//! therefore fails closed with `Unavailable` (exit 6) naming the sugar form
//! (`bitty ctl ...`) where one exists. No IPC frame is sent, no instance is
//! touched, and no plugin code is loaded on this path.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (help only in this slice).
//! - `2` usage error (missing/malformed id, unknown flag, bad `--format`,
//!   extra positional, malformed args blob, `--socket`/`--instance`).
//! - `6` runtime unavailable (well-formed id with no live dispatch target).

#![forbid(unsafe_code)]

use std::fmt::Write as _;

use crate::cli::Args;
use crate::list::json_escape;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;
/// IPC or runtime unavailable (no live dispatch target in this slice).
pub const EXIT_RUNTIME: i32 = 6;

// ---------------------------------------------------------------------------
// Bounds (T-01 parity, fail closed with exit 2 before any dispatch)
// ---------------------------------------------------------------------------

/// Maximum bytes for a qualified id (registry rule: 3..=128).
pub const MAX_CMD_ID_LEN: usize = 128;
/// Maximum bytes for a `--format` value.
pub const MAX_CMD_FORMAT_LEN: usize = 16;
/// Maximum bytes for the raw JSON argument blob (bounded parser input).
pub const MAX_CMD_ARGS_BYTES: usize = 65_536;
/// Maximum nesting depth accepted in the args blob (wire rule parity: 32).
pub const MAX_CMD_ARGS_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// Qualified-id validation (shared with `bitty x`)
// ---------------------------------------------------------------------------

/// Validate one dot/`:`-separated segment (non-empty, starts lowercase,
/// rest `[a-z0-9_-]` to match the manifest id grammar).
#[must_use]
pub(crate) fn valid_id_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > MAX_CMD_ID_LEN {
        return false;
    }
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Validate a qualified executable/plugin id.
///
/// Accepts the dot form (`core.terminal.text`, `bitty-terminal.workspace`)
/// and the colon form (`example.markdown:render`): both separators split
/// segments, and at least two segments are required (a bare `foo` never names
/// an executable). Dashes are accepted to match the installed manifest set
/// (e.g. `bitty-terminal.workspace`); other punctuation is rejected.
pub(crate) fn validate_qualified_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err(String::from("qualified id must not be empty"));
    }
    if id.len() > MAX_CMD_ID_LEN {
        return Err(format!(
            "qualified id too long ({} bytes, max {MAX_CMD_ID_LEN})",
            id.len()
        ));
    }
    if id
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(String::from(
            "qualified id must not contain whitespace or control bytes",
        ));
    }
    // Split on both separators; empty segments (leading/trailing/doubled
    // separators) fail via `valid_id_segment`.
    let segments: Vec<&str> = id.split(['.', ':']).collect();
    if segments.len() < 2 {
        return Err(format!(
            "qualified id {id:?} needs at least two segments (e.g. core.terminal.text)"
        ));
    }
    for segment in &segments {
        if !valid_id_segment(segment) {
            return Err(format!(
                "qualified id {id:?}: invalid segment {segment:?} (want ^[a-z][a-z0-9_-]*$)"
            ));
        }
    }
    Ok(())
}

/// Validate the raw JSON argument blob structurally (no JSON parser in the
/// workspace: balanced `{}`/`[]` with string awareness, single root value of
/// object/array shape, depth capped at [`MAX_CMD_ARGS_DEPTH`]).
pub(crate) fn validate_args_blob(blob: &str) -> Result<(), String> {
    if blob.len() > MAX_CMD_ARGS_BYTES {
        return Err(format!(
            "args blob too large ({} bytes, max {MAX_CMD_ARGS_BYTES})",
            blob.len()
        ));
    }
    let trimmed = blob.trim();
    if trimmed.is_empty() {
        return Err(String::from("args blob must not be empty"));
    }
    let opener = trimmed.as_bytes()[0];
    if opener != b'{' && opener != b'[' {
        return Err(String::from(
            "args blob must be a JSON object or array (e.g. -- '{\"terminal_id\": \"t:4\"}')",
        ));
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut stack: Vec<u8> = Vec::new();
    for &b in trimmed.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                stack.push(b'}');
                depth += 1;
            }
            b'[' => {
                stack.push(b']');
                depth += 1;
            }
            b'}' | b']' => {
                if stack.pop() != Some(b) {
                    return Err(String::from("args blob has unbalanced brackets"));
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        if depth > MAX_CMD_ARGS_DEPTH {
            return Err(format!(
                "args blob nests too deep (max {MAX_CMD_ARGS_DEPTH})"
            ));
        }
    }
    if in_string {
        return Err(String::from("args blob has an unterminated string"));
    }
    if !stack.is_empty() {
        return Err(String::from("args blob has unbalanced brackets"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Output shape for `bitty cmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdFormat {
    /// Human diagnostic (default).
    Table,
    /// Versioned envelope, one JSON value.
    Json,
    /// Versioned envelope, one JSON value per line (single line here).
    Jsonl,
}

impl CmdFormat {
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
                        "bitty cmd: unknown --format {value:?} (want table|json|jsonl)\n{}",
                        cmd_usage()
                    )),
                }
            }
        }
    }
}

/// Validated `bitty cmd` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdRequest {
    /// Qualified executable id (dot or colon form, as invoked).
    pub id: String,
    /// Output shape.
    pub format: CmdFormat,
    /// Raw JSON argument blob after `--` (validated structurally).
    pub args_json: Option<String>,
}

/// Parse failure: help vs usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdParseError {
    /// `--help` requested: print [`cmd_help_text`] to stdout, exit 0.
    Help,
    /// Usage error: print the message (it already ends with usage) to stderr,
    /// exit 2.
    Usage(String),
}

impl CmdParseError {
    /// Render the stderr message for [`CmdParseError::Usage`].
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Help => String::from("bitty cmd: help requested"),
            Self::Usage(message) => message,
        }
    }
}

/// Usage line for `bitty cmd`.
#[must_use]
pub fn cmd_usage() -> String {
    String::from("Usage: bitty cmd <qualified-id> [--format table|json|jsonl] [-- <args-json>]")
}

/// Help text for `bitty cmd` (`--help` never needs an instance or VM).
#[must_use]
pub fn cmd_help_text() -> String {
    String::from(
        "bitty cmd — invoke a qualified executable directly (escape hatch)\n\
         \n\
         Usage: bitty cmd <qualified-id> [--format table|json|jsonl] [-- <args-json>]\n\
         \n\
         Invokes any registry executable by id for generators and diagnostics\n\
         that name the executable explicitly, e.g.\n\
         `bitty cmd core.terminal.text --format json -- '{\"terminal_id\": \"t:4\"}'`.\n\
         Both the dot form (`core.terminal.text`) and the colon form\n\
         (`example.markdown:render`) validate; scopes are checked server-side\n\
         and the output envelope and exit code match the sugar form.\n\
         \n\
         Live dispatch against the terminal registry is a follow-up: this\n\
         build validates the id and fails closed (exit 6) without sending IPC.\n",
    )
}

/// Validate post-`cmd` tokens plus the optional pre-word `--format`.
///
/// `pre_format` is the global `--format` seen before the `cmd` word; an
/// explicit post-word `--format` wins.
pub fn parse_cmd_request(
    tokens: &[String],
    pre_format: Option<&str>,
) -> Result<CmdRequest, CmdParseError> {
    let mut id: Option<String> = None;
    let mut format: Option<String> = None;
    let mut args_json: Option<String> = None;
    let mut seen_separator = false;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(CmdParseError::Help);
        }
        if token == "--no-color" {
            // Accepted for parity; cmd diagnostics are plain text.
            i += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--format=") {
            if value.len() > MAX_CMD_FORMAT_LEN {
                return Err(usage_too_long_format());
            }
            format = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--format" {
            if seen_separator {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: --format belongs before `--` (the blob after `--` is raw JSON)\n{}",
                    cmd_usage()
                )));
            }
            if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                if tokens[i + 1].len() > MAX_CMD_FORMAT_LEN {
                    return Err(usage_too_long_format());
                }
                format = Some(tokens[i + 1].clone());
                i += 2;
            } else {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: --format needs a value (table|json|jsonl)\n{}",
                    cmd_usage()
                )));
            }
            continue;
        }
        if token == "--" {
            if seen_separator {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: duplicate `--` (a single JSON blob follows one `--`)\n{}",
                    cmd_usage()
                )));
            }
            if id.is_none() {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: `--` needs a qualified id first\n{}",
                    cmd_usage()
                )));
            }
            seen_separator = true;
            i += 1;
            continue;
        }
        if seen_separator {
            if args_json.is_some() {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: unexpected argument {token:?} (one JSON blob follows `--`)\n{}",
                    cmd_usage()
                )));
            }
            if let Err(reason) = validate_args_blob(token) {
                return Err(CmdParseError::Usage(format!(
                    "bitty cmd: invalid args blob: {reason}\n{}",
                    cmd_usage()
                )));
            }
            args_json = Some(token.clone());
            i += 1;
            continue;
        }
        if token.starts_with('-') {
            return Err(CmdParseError::Usage(format!(
                "bitty cmd: unknown flag {token:?}\n{}",
                cmd_usage()
            )));
        }
        if id.is_some() {
            return Err(CmdParseError::Usage(format!(
                "bitty cmd: unexpected argument {token:?}\n{}",
                cmd_usage()
            )));
        }
        if let Err(reason) = validate_qualified_id(token) {
            return Err(CmdParseError::Usage(format!(
                "bitty cmd: invalid qualified id: {reason}\n{}",
                cmd_usage()
            )));
        }
        id = Some(token.clone());
        i += 1;
    }
    let Some(id) = id else {
        return Err(CmdParseError::Usage(format!(
            "bitty cmd: missing <qualified-id>\n{}",
            cmd_usage()
        )));
    };
    let shape = format.as_deref().or(pre_format);
    match CmdFormat::parse(shape) {
        Ok(format) => Ok(CmdRequest {
            id,
            format,
            args_json,
        }),
        Err(message) => Err(CmdParseError::Usage(message)),
    }
}

fn usage_too_long_format() -> CmdParseError {
    CmdParseError::Usage(format!(
        "bitty cmd: --format value too long (max {MAX_CMD_FORMAT_LEN} bytes)\n{}",
        cmd_usage()
    ))
}

/// Render the `Unavailable` envelope for `--format json` / `--format jsonl`.
#[must_use]
pub fn format_cmd_unavailable_envelope(id: &str, message: &str) -> String {
    let mut out = String::with_capacity(256);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"cmd\",\"ok\":false,\"error\":{{\"class\":\"Unavailable\",\"code\":\"RegistryDispatch\",\"message\":\"{}: {}\"}}}}",
        json_escape(id),
        json_escape(message)
    );
    out
}

/// Unavailable diagnostic shared by the table and envelope paths.
#[must_use]
pub fn cmd_unavailable_message(id: &str) -> String {
    format!(
        "bitty cmd: direct invocation of {id:?} is unavailable in this build \
         (terminal-registry dispatch is a follow-up; no IPC was sent — \
         use the `bitty ctl` sugar form where one exists)"
    )
}

/// Execute a validated request; returns the process exit code.
///
/// This slice has no live dispatch target: every well-formed id fails closed
/// with `Unavailable` (exit 6). Table diagnostics go to stderr; json/jsonl
/// emit the ok:false envelope on stdout plus the diagnostic on stderr.
pub fn run_cmd(request: &CmdRequest) -> i32 {
    let message = cmd_unavailable_message(&request.id);
    match request.format {
        CmdFormat::Table => {
            eprintln!("{message}");
        }
        CmdFormat::Json | CmdFormat::Jsonl => {
            println!("{}", format_cmd_unavailable_envelope(&request.id, &message));
            eprintln!("{message}");
        }
    }
    EXIT_RUNTIME
}

/// Runs `bitty cmd`; returns the process exit code.
///
/// - `--help` prints help to stdout, exit 0, and never needs an instance.
/// - The global pre-word `--format` applies when the post-word tokens set
///   none (post-word wins).
/// - `--socket`/`--instance` combined with `cmd` are usage errors.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.ctl_socket_pre.is_some()
        || args.list_socket.is_some()
        || args.dev_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_instance.is_some()
        || args.dev_instance_pre.is_some()
    {
        eprintln!(
            "bitty cmd: --socket/--instance need a runtime sugar command (cmd carries no target selection)\n{}",
            cmd_usage()
        );
        return EXIT_USAGE;
    }
    let post_has_format = args
        .cmd_raw
        .iter()
        .any(|t| t == "--format" || t.starts_with("--format="));
    let pre = if post_has_format {
        None
    } else {
        args.doctor_format.as_deref()
    };
    match parse_cmd_request(&args.cmd_raw, pre) {
        Err(CmdParseError::Help) => {
            print!("{}", cmd_help_text());
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{}", err.message());
            EXIT_USAGE
        }
        Ok(request) => run_cmd(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn qualified_ids_validate_dot_and_colon_forms() {
        assert!(validate_qualified_id("core.terminal.text").is_ok());
        assert!(validate_qualified_id("example.markdown:render").is_ok());
        assert!(validate_qualified_id("bitty-terminal.workspace").is_ok());
        assert!(validate_qualified_id("a-b.c-d").is_ok());
        for bad in [
            "", "foo", ".a", "a.", "a..b", "A.b", "-a.b", "a b.c", "a.\u{0}b",
        ] {
            assert!(validate_qualified_id(bad).is_err(), "id: {bad:?}");
        }
        let long = format!("a.{}", "b".repeat(MAX_CMD_ID_LEN));
        assert!(validate_qualified_id(&long).is_err());
    }

    #[test]
    fn args_blobs_validate_structurally() {
        assert!(validate_args_blob("{\"terminal_id\": \"t:4\"}").is_ok());
        assert!(validate_args_blob("[1, {\"a\": \"}{\"}]").is_ok());
        assert!(validate_args_blob("{\"a\": 1").is_err());
        assert!(validate_args_blob("{\"a\": \"unterminated}").is_err());
        assert!(validate_args_blob("printenv FOO").is_err());
        assert!(validate_args_blob("").is_err());
        let deep = format!("{{\"a\": {}}}", "[1, ".repeat(MAX_CMD_ARGS_DEPTH + 1));
        assert!(validate_args_blob(&deep).is_err());
    }

    #[test]
    fn id_format_separator_blob_parse() {
        let req = parse_cmd_request(&tokens(&["core.view.split", "--right"]), None);
        assert!(matches!(req, Err(CmdParseError::Usage(_))));
        let req = parse_cmd_request(
            &tokens(&[
                "core.terminal.text",
                "--format",
                "json",
                "--",
                "{\"terminal_id\":\"t:4\"}",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(req.id, "core.terminal.text");
        assert_eq!(req.format, CmdFormat::Json);
        assert_eq!(
            req.args_json,
            Some(String::from("{\"terminal_id\":\"t:4\"}"))
        );
        // Pre-word format merges; post-word wins.
        let req = parse_cmd_request(&tokens(&["core.view.split"]), Some("jsonl")).unwrap();
        assert_eq!(req.format, CmdFormat::Jsonl);
        let req = parse_cmd_request(
            &tokens(&["core.view.split", "--format=json"]),
            Some("jsonl"),
        )
        .unwrap();
        assert_eq!(req.format, CmdFormat::Json);
    }

    #[test]
    fn missing_id_double_separator_and_help() {
        assert!(matches!(
            parse_cmd_request(&[], None),
            Err(CmdParseError::Usage(_))
        ));
        assert!(matches!(
            parse_cmd_request(&tokens(&["--", "{}"]), None),
            Err(CmdParseError::Usage(_))
        ));
        assert!(matches!(
            parse_cmd_request(&tokens(&["a.b", "--", "{}", "{}"]), None),
            Err(CmdParseError::Usage(_))
        ));
        assert_eq!(
            parse_cmd_request(&tokens(&["--help"]), None),
            Err(CmdParseError::Help)
        );
    }

    #[test]
    fn unavailable_envelope_is_versioned() {
        let envelope = format_cmd_unavailable_envelope("core.view.split", "nope");
        assert!(envelope.contains("\"v\":1"), "envelope: {envelope}");
        assert!(
            envelope.contains("\"command\":\"cmd\""),
            "envelope: {envelope}"
        );
        assert!(envelope.contains("\"ok\":false"), "envelope: {envelope}");
        assert!(envelope.contains("Unavailable"), "envelope: {envelope}");
    }
}
