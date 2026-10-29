//! `bittyd` command-line skeleton.
//!
//! This is the whole binary surface:
//!
//! - `bittyd start` runs the service loop: it binds the per-user endpoint,
//!   mints the operator capability, accepts connections, and exits when
//!   `daemon.stop` or a signal asks it to (bitty#2607);
//! - `bittyd --version` prints the crate version and the contract service
//!   version, then exits 0;
//! - `bittyd --help` prints usage and exits 0;
//! - `bittyd` with no arguments prints the [`SeamStatus`] and exits 0; it never
//!   binds an endpoint, creates a socket, or writes a runtime file;
//! - anything else is a usage error and exits 2.
//!
//! Exit codes: `0` success (including "already running", where the desired
//! state already holds), `1` internal failure before a listener accepted,
//! `2` usage, `6` endpoint tamper. Codes `3`, `4`, and `5` are the contract's
//! `daemon.stop` outcome codes; a running worker is terminated inside its
//! containment identity before the exit, so a clean stop is `0`.
//!
//! Parsing and rendering are pure so the behavior is testable without spawning
//! the process; [`run`] writes the rendered strings, runs the service for
//! `start`, and maps the outcome to an [`ExitCode`].

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

use crate::CRATE_VERSION;
use crate::contract::{BINARY_NAME, SERVICE_API_VERSION};
use crate::seam::SeamStatus;
use crate::service::server::{self, ServerConfig, ServerError, ServerOutcome};

/// Successful invocation.
pub const EXIT_OK: u8 = 0;

/// Usage error; the same code the other Bitty command-line surfaces use.
pub const EXIT_USAGE: u8 = 2;

/// Internal failure before any listener accepted a connection.
pub const EXIT_INTERNAL: u8 = 1;

/// Endpoint tamper (contract exit code `6`).
pub const EXIT_TAMPER: u8 = 6;

/// One parsed `bittyd` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Print crate and service versions.
    Version,
    /// Print usage.
    Help,
    /// Print the session-host status without binding anything.
    Status,
    /// Run the service loop.
    Start,
}

/// A rejected argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    message: String,
}

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UsageError {}

/// The rendered result of one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Process exit code.
    pub exit_code: u8,
    /// Bytes for standard output.
    pub stdout: String,
    /// Bytes for standard error.
    pub stderr: String,
}

/// Parse `bittyd` arguments (without the program name).
///
/// The seam accepts at most one argument. Multiple arguments are rejected
/// instead of silently ignoring all but the first, so a future flag cannot be
/// mistaken for a supported one.
///
/// # Errors
///
/// Returns [`UsageError`] for an unknown argument, a non-UTF-8 argument, or
/// more than one argument.
pub fn parse_args<I, S>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Invocation::Status);
    };
    if let Some(extra) = args.next() {
        return Err(UsageError::new(format!(
            "unexpected extra argument '{}'",
            extra.as_ref().to_string_lossy()
        )));
    }
    match first.as_ref().to_str() {
        Some("--version" | "-V") => Ok(Invocation::Version),
        Some("--help" | "-h") => Ok(Invocation::Help),
        Some("start") => Ok(Invocation::Start),
        Some(argument) => Err(UsageError::new(format!("unknown argument '{argument}'"))),
        None => Err(UsageError::new(format!(
            "argument is not valid UTF-8: '{}'",
            first.as_ref().to_string_lossy()
        ))),
    }
}

/// Usage text shared by `--help` and usage errors.
#[must_use]
pub fn usage() -> String {
    format!(
        "usage: {BINARY_NAME} [start | --version | --help]\n\
         \n\
         With no arguments, {BINARY_NAME} prints the session-host status and exits\n\
         0 without binding an endpoint. `{BINARY_NAME} start` binds the per-user\n\
         endpoint and serves the bitty-ipc v1 service until daemon.stop or a\n\
         SIGTERM/SIGINT drain.\n\
         \n\
         exit codes: 0 ok or already running, 1 startup failure, 2 usage error,\n\
         6 endpoint tamper\n"
    )
}

/// Render one invocation without performing any I/O.
#[must_use]
pub fn render<I, S>(args: I) -> Rendered
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match parse_args(args) {
        Ok(Invocation::Version) => Rendered {
            exit_code: EXIT_OK,
            stdout: format!("{BINARY_NAME} {CRATE_VERSION} (service API {SERVICE_API_VERSION})\n"),
            stderr: String::new(),
        },
        Ok(Invocation::Help) => Rendered {
            exit_code: EXIT_OK,
            stdout: usage(),
            stderr: String::new(),
        },
        Ok(Invocation::Status) => Rendered {
            exit_code: EXIT_OK,
            stdout: SeamStatus::current().render(),
            stderr: String::new(),
        },
        Ok(Invocation::Start) => Rendered {
            // `run` intercepts `Start`; this branch exists so the pure renderer
            // stays total and the parser tests can assert the variant.
            exit_code: EXIT_OK,
            stdout: String::new(),
            stderr: String::new(),
        },
        Err(error) => Rendered {
            exit_code: EXIT_USAGE,
            stdout: String::new(),
            stderr: format!("{BINARY_NAME}: {error}\n\n{}", usage()),
        },
    }
}

/// Run `bittyd` with the arguments after the program name.
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match parse_args(args) {
        Ok(Invocation::Start) => run_start(),
        Ok(invocation) => write_rendered(rendered_invocation(invocation)),
        Err(error) => write_rendered(usage_error(error)),
    }
}

/// Run the service loop and map its outcome to an exit code.
fn run_start() -> ExitCode {
    let config = match ServerConfig::from_process() {
        Ok(config) => config,
        Err(error) => return report(error),
    };
    match server::run(config) {
        Ok(ServerOutcome::Stopped { mode }) => {
            report_text(&format!("{BINARY_NAME}: stopped ({mode})\n"));
            ExitCode::from(EXIT_OK)
        }
        Ok(ServerOutcome::Signalled) => {
            report_text(&format!("{BINARY_NAME}: stopped (signal drain)\n"));
            ExitCode::from(EXIT_OK)
        }
        Ok(ServerOutcome::Tampered) => {
            report_error(&format!(
                "{BINARY_NAME}: endpoint tamper; no weaker transport is attempted\n"
            ));
            ExitCode::from(EXIT_TAMPER)
        }
        Err(ServerError::AlreadyRunning { socket }) => {
            report_error(&format!(
                "{BINARY_NAME}: already running (socket {})\n",
                socket.display()
            ));
            ExitCode::from(EXIT_OK)
        }
        Err(error) => report(error),
    }
}

fn report(error: impl fmt::Display) -> ExitCode {
    report_error(&format!("{BINARY_NAME}: {error}\n"));
    ExitCode::from(EXIT_INTERNAL)
}

fn report_text(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
}

fn report_error(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
}

fn write_rendered(rendered: Rendered) -> ExitCode {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let _ = stdout.write_all(rendered.stdout.as_bytes());
    let _ = stderr.write_all(rendered.stderr.as_bytes());
    ExitCode::from(rendered.exit_code)
}

fn rendered_invocation(invocation: Invocation) -> Rendered {
    match invocation {
        Invocation::Version => Rendered {
            exit_code: EXIT_OK,
            stdout: format!("{BINARY_NAME} {CRATE_VERSION} (service API {SERVICE_API_VERSION})\n"),
            stderr: String::new(),
        },
        Invocation::Help => Rendered {
            exit_code: EXIT_OK,
            stdout: usage(),
            stderr: String::new(),
        },
        Invocation::Status => Rendered {
            exit_code: EXIT_OK,
            stdout: SeamStatus::current().render(),
            stderr: String::new(),
        },
        Invocation::Start => Rendered {
            exit_code: EXIT_OK,
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

fn usage_error(error: UsageError) -> Rendered {
    Rendered {
        exit_code: EXIT_USAGE,
        stdout: String::new(),
        stderr: format!("{BINARY_NAME}: {error}\n\n{}", usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(args: &[&str]) -> Rendered {
        render(args)
    }

    #[test]
    fn no_arguments_reports_the_seam_status() {
        let result = rendered(&[]);
        assert_eq!(result.exit_code, EXIT_OK);
        assert!(result.stderr.is_empty());
        assert!(result.stdout.contains("session-host seam"));
        assert!(result.stdout.contains("endpoint: none bound"));
        assert!(result.stdout.contains("ipc service: wired"));
        assert!(!result.stdout.contains("usage:"));
    }

    #[test]
    fn version_prints_the_crate_and_service_versions() {
        let result = rendered(&["--version"]);
        assert_eq!(result.exit_code, EXIT_OK);
        assert!(result.stderr.is_empty());
        assert!(result.stdout.contains(CRATE_VERSION));
        assert!(result.stdout.contains("service API 1.2"));
    }

    #[test]
    fn help_prints_usage() {
        let result = rendered(&["--help"]);
        assert_eq!(result.exit_code, EXIT_OK);
        assert!(result.stderr.is_empty());
        assert!(result.stdout.contains("usage: bittyd"));
        assert!(
            result
                .stdout
                .contains("exit codes: 0 ok or already running, 1 startup failure, 2 usage error,")
        );
    }

    #[test]
    fn unknown_argument_is_a_usage_error() {
        let result = rendered(&["--serve"]);
        assert_eq!(result.exit_code, EXIT_USAGE);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.contains("unknown argument '--serve'"));
        assert!(result.stderr.contains("usage: bittyd"));
    }

    #[test]
    fn extra_arguments_are_a_usage_error() {
        let result = rendered(&["--version", "--help"]);
        assert_eq!(result.exit_code, EXIT_USAGE);
        assert!(result.stderr.contains("unexpected extra argument '--help'"));
    }

    #[test]
    fn start_is_the_only_serving_invocation() {
        assert_eq!(parse_args(["start"]), Ok(Invocation::Start));
        assert_eq!(
            parse_args(["start", "--quiet"]),
            Err(UsageError::new("unexpected extra argument '--quiet'"))
        );
    }

    #[test]
    fn invocation_variants_are_distinct() {
        assert_eq!(parse_args(["--version"]), Ok(Invocation::Version));
        assert_eq!(parse_args(["-V"]), Ok(Invocation::Version));
        assert_eq!(parse_args(["--help"]), Ok(Invocation::Help));
        assert_eq!(parse_args(["-h"]), Ok(Invocation::Help));
        assert_eq!(parse_args(Vec::<&str>::new()), Ok(Invocation::Status));
    }
}
