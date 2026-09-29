//! `bitty version`: version and build metadata (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty version`,
//! local class, stable `bitty version` / `bitty version --format json`).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty version [--format table|json|jsonl]`. `-V` / `--version`
//!   is an alias for the table form.
//! - Table (default) prints `bitty <semver> (<channel> <commit>)` on stdout.
//! - `--format json` / `--format jsonl` emit the versioned envelope (`v: 1`,
//!   `command: "version"`, `ok: true`, `result` with `version`/`channel`/
//!   `commit`) on stdout; diagnostics go to stderr so JSON is never corrupted.
//! - `result.version` is the bare semver and matches the `BITTY_VERSION`
//!   child-process indicator set by `bitty run` (`TERM`/`BITTY`/
//!   `BITTY_VERSION` are the three RFC-stable indicators).
//! - Class: local (no instance, no config load, no plugin VM, safe-mode
//!   clean). `--socket`/`--instance` combined with `version` fail closed
//!   (exit 2): they select a runtime target this command never uses.
//! - Extra positionals, bad `--format`, and stray `--` fail closed (exit 2,
//!   stderr only, no stdout envelope).
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success.
//! - `2` usage error (missing is impossible; extra positional, unknown flag,
//!   bad `--format`, stray `--`, `--socket`/`--instance` with `version`).

#![forbid(unsafe_code)]

use crate::cli::Args;
use crate::list::json_escape;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum bytes for a `--format` value.
pub const MAX_VERSION_FORMAT_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Build metadata
// ---------------------------------------------------------------------------

/// Bare semver (`CARGO_PKG_VERSION`); also the `BITTY_VERSION` child value.
#[must_use]
pub fn version_semver() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Release channel (`BITTY_CHANNEL` at build time, `dev`/`stable` default).
#[must_use]
pub fn version_channel() -> &'static str {
    option_env!("BITTY_CHANNEL").unwrap_or("unknown")
}

/// Short commit hash (`BITTY_COMMIT` at build time, `unknown` fallback).
#[must_use]
pub fn version_commit() -> &'static str {
    option_env!("BITTY_COMMIT").unwrap_or("unknown")
}

/// Table form: `bitty <semver> (<channel> <commit>)` (RFC-stable).
#[must_use]
pub fn version_text() -> String {
    format!(
        "bitty {} ({} {})",
        version_semver(),
        version_channel(),
        version_commit()
    )
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Output shape for `bitty version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionFormat {
    /// Human line (default; also what `-V` / `--version` print).
    Table,
    /// Versioned envelope, one JSON value.
    Json,
    /// Versioned envelope, one JSON value per line (single line here).
    Jsonl,
}

impl VersionFormat {
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
                        "bitty version: unknown --format {value:?} (want table|json|jsonl)\n{}",
                        version_usage()
                    )),
                }
            }
        }
    }
}

/// Validated `bitty version` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRequest {
    /// Output shape.
    pub format: VersionFormat,
}

/// Parse failure: help vs usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionParseError {
    /// `--help` requested: print [`version_help_text`] to stdout, exit 0.
    Help,
    /// Usage error: print the message (it already ends with usage) to stderr,
    /// exit 2.
    Usage(String),
}

impl VersionParseError {
    /// Render the stderr message for [`VersionParseError::Usage`].
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Help => String::from("bitty version: help requested"),
            Self::Usage(message) => message,
        }
    }
}

/// Usage line for `bitty version`.
#[must_use]
pub fn version_usage() -> String {
    String::from("Usage: bitty version [--format table|json|jsonl]")
}

/// Help text for `bitty version` (`--help` never needs an instance or VM).
#[must_use]
pub fn version_help_text() -> String {
    format!(
        "bitty version — version and build metadata (local, no instance)\n\
         \n\
         Usage: bitty version [--format table|json|jsonl]\n\
         \n\
         Table (default) prints `{}` on stdout. `--format json` /\n\
         `--format jsonl` emit the versioned envelope (v: 1, command:\n\
         \"version\") with the same version/channel/commit fields.\n\
         `-V` / `--version` is an alias for the table form.\n",
        version_text()
    )
}

/// Validate post-`version` tokens plus the optional pre-word `--format`.
///
/// `pre_format` is the global `--format` seen before the `version` word (the
/// shared slot also feeding doctor/list/inspect/dev); an explicit post-word
/// `--format` wins, matching the RFC scoping rule.
pub fn parse_version_request(
    tokens: &[String],
    pre_format: Option<&str>,
) -> Result<VersionRequest, VersionParseError> {
    let mut format: Option<String> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(VersionParseError::Help);
        }
        if token == "--no-color" {
            // Accepted for global-flag parity; version output is never
            // colorized.
            i += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--format=") {
            if value.len() > MAX_VERSION_FORMAT_LEN {
                return Err(VersionParseError::Usage(format!(
                    "bitty version: --format value too long (max {MAX_VERSION_FORMAT_LEN} bytes)\n{}",
                    version_usage()
                )));
            }
            format = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--format" {
            if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                if tokens[i + 1].len() > MAX_VERSION_FORMAT_LEN {
                    return Err(VersionParseError::Usage(format!(
                        "bitty version: --format value too long (max {MAX_VERSION_FORMAT_LEN} bytes)\n{}",
                        version_usage()
                    )));
                }
                format = Some(tokens[i + 1].clone());
                i += 2;
            } else {
                return Err(VersionParseError::Usage(format!(
                    "bitty version: --format needs a value (table|json|jsonl)\n{}",
                    version_usage()
                )));
            }
            continue;
        }
        return Err(VersionParseError::Usage(format!(
            "bitty version: unexpected argument {token:?}\n{}",
            version_usage()
        )));
    }
    let shape = format.as_deref().or(pre_format);
    match VersionFormat::parse(shape) {
        Ok(format) => Ok(VersionRequest { format }),
        Err(message) => Err(VersionParseError::Usage(message)),
    }
}

/// Render the success envelope for `--format json` / `--format jsonl`.
#[must_use]
pub fn format_version_envelope() -> String {
    format!(
        "{{\"v\":1,\"command\":\"version\",\"ok\":true,\"result\":{{\"version\":\"{}\",\"channel\":\"{}\",\"commit\":\"{}\"}}}}",
        json_escape(version_semver()),
        json_escape(version_channel()),
        json_escape(version_commit())
    )
}

/// Execute a validated request; returns the process exit code.
pub fn run_version(request: &VersionRequest) -> i32 {
    match request.format {
        VersionFormat::Table => {
            println!("{}", version_text());
        }
        VersionFormat::Json | VersionFormat::Jsonl => {
            println!("{}", format_version_envelope());
        }
    }
    EXIT_OK
}

/// Runs `bitty version`; returns the process exit code.
///
/// - `--help` (as the `-V`/`--version` flag companion or inside the post-word
///   tokens) prints help to stdout, exit 0, and never needs an instance.
/// - The global pre-word `--format` applies when the post-word tokens set
///   none; conflicting values cannot occur (single slot each side).
/// - `--socket`/`--instance` combined with `version` are usage errors: they
///   select a runtime target this local command never uses.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.ctl_socket_pre.is_some()
        || args.list_socket.is_some()
        || args.dev_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_instance.is_some()
        || args.dev_instance_pre.is_some()
    {
        eprintln!(
            "bitty version: --socket/--instance need a runtime command (version is local)\n{}",
            version_usage()
        );
        return EXIT_USAGE;
    }
    // Detect an explicit post-word `--format` so the global pre-word value
    // only fills the gap (RFC scoping: post-word wins on conflict, and here
    // the parser already enforces single-value).
    let post_has_format = args
        .version_raw
        .iter()
        .any(|t| t == "--format" || t.starts_with("--format="));
    let pre = if post_has_format {
        None
    } else {
        args.doctor_format.as_deref()
    };
    match parse_version_request(&args.version_raw, pre) {
        Err(VersionParseError::Help) => {
            print!("{}", version_help_text());
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{}", err.message());
            EXIT_USAGE
        }
        Ok(request) => run_version(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_form_carries_semver_channel_commit() {
        let text = version_text();
        assert!(text.starts_with("bitty "), "table form: {text}");
        assert!(text.contains(version_semver()), "semver: {text}");
        assert!(text.contains(version_channel()), "channel: {text}");
        assert!(text.contains(version_commit()), "commit: {text}");
        assert!(!version_semver().contains(' '), "bare semver");
    }

    #[test]
    fn format_defaults_table_rejects_unknown() {
        assert_eq!(VersionFormat::parse(None).unwrap(), VersionFormat::Table);
        assert_eq!(
            VersionFormat::parse(Some("JSON")).unwrap(),
            VersionFormat::Json
        );
        assert_eq!(
            VersionFormat::parse(Some("jsonl")).unwrap(),
            VersionFormat::Jsonl
        );
        assert!(VersionFormat::parse(Some("yaml")).is_err());
    }

    #[test]
    fn bare_word_parses_with_pre_format_merge() {
        let req = parse_version_request(&[], None).unwrap();
        assert_eq!(req.format, VersionFormat::Table);
        let req = parse_version_request(&[], Some("json")).unwrap();
        assert_eq!(req.format, VersionFormat::Json);
    }

    #[test]
    fn post_word_format_wins_and_help_short_circuits() {
        let tokens = vec!["--format".to_string(), "jsonl".to_string()];
        let req = parse_version_request(&tokens, Some("table")).unwrap();
        assert_eq!(req.format, VersionFormat::Jsonl);
        let tokens = vec!["--help".to_string()];
        assert_eq!(
            parse_version_request(&tokens, None),
            Err(VersionParseError::Help)
        );
    }

    #[test]
    fn stray_positional_dash_separator_and_missing_value_fail() {
        for tokens in [
            vec!["extra".to_string()],
            vec!["--".to_string()],
            vec!["--format".to_string()],
            vec!["--bogus".to_string()],
        ] {
            assert!(
                matches!(
                    parse_version_request(&tokens, None),
                    Err(VersionParseError::Usage(_))
                ),
                "tokens: {tokens:?}"
            );
        }
    }

    #[test]
    fn envelope_is_versioned_with_bare_semver() {
        let envelope = format_version_envelope();
        assert!(envelope.contains("\"v\":1"), "envelope: {envelope}");
        assert!(
            envelope.contains("\"command\":\"version\""),
            "envelope: {envelope}"
        );
        assert!(
            envelope.contains(&format!("\"version\":\"{}\"", version_semver())),
            "envelope: {envelope}"
        );
        let mut out = String::new();
        out.push_str(&envelope);
        assert!(!out.is_empty());
    }
}
