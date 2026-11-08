//! `bittyd` command-line surface.
//!
//! This is the whole binary surface:
//!
//! - `bittyd start` runs the service loop: it binds the per-user endpoint,
//!   mints the operator capability, accepts connections, and exits when
//!   `daemon.stop` or a signal asks it to (bitty#2607);
//! - `bittyd stop [--force | --drain]` speaks the authenticated endpoint as
//!   the operator and issues `daemon.stop` (contract `D-05`);
//! - `bittyd --version` prints the crate version and the contract service
//!   version, then exits 0;
//! - `bittyd --help` prints usage and exits 0;
//! - `bittyd` with no arguments prints the [`SeamStatus`] and exits 0; it never
//!   binds an endpoint, creates a socket, or writes a runtime file;
//! - anything else is a usage error and exits 2.
//!
//! Exit codes (`D-05`): `0` success (including "already running" and a stop
//! with no active sessions), `1` internal failure, `2` usage, `3` active
//! sessions on a default stop, `4` a drain deadline forced the remainder,
//! `5` a worker could not be verified gone, `6` endpoint tamper, a failed
//! authentication invariant, or an endpoint this account may not use. That is
//! the closed set the binary can return; there is no `7`. The permission class
//! a system-owned channel produces is exit `6`, because the peer cannot be
//! authenticated through it.
//!
//! Parsing and rendering are pure so the behavior is testable without spawning
//! the process; [`run`] writes the rendered strings, runs the service for
//! `start`, issues the operator stop for `stop`, and maps the outcome to an
//! [`ExitCode`].

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

use crate::CRATE_VERSION;
use crate::client::{self, StopError, StopReply};
use crate::contract::{BINARY_NAME, SERVICE_API_VERSION};
use crate::seam::SeamStatus;
use crate::service::dispatch::StopMode;
use crate::service::server::{self, ServerConfig, ServerError, ServerOutcome};

/// Successful invocation.
pub const EXIT_OK: u8 = 0;

/// Usage error; the same code the other Bitty command-line surfaces use.
pub const EXIT_USAGE: u8 = 2;

/// Internal failure before any listener accepted a connection.
pub const EXIT_INTERNAL: u8 = 1;

/// A default stop refused because sessions are active (`D-05`).
pub const EXIT_ACTIVE_SESSIONS: u8 = 3;

/// A drain deadline forced the remainder (`D-05`).
pub const EXIT_DRAIN_FORCED: u8 = 4;

/// A worker could not be verified gone (`D-05`).
pub const EXIT_UNVERIFIED_WORKER: u8 = 5;

/// Endpoint tamper, a failed authentication invariant, or an endpoint this
/// account may not use (contract exit `6`).
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
    /// Issue an authenticated operator stop over the served endpoint.
    Stop(StopMode),
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
/// `stop` takes at most one mode flag: the default is `refuse`, `--force` is
/// the immediate `force` mode, and `--drain` is the grace-period `drain` mode.
/// A second flag, an unknown flag, or a flag on another subcommand is a usage
/// error. Multiple bare arguments are rejected instead of silently ignoring
/// all but the first, so a future flag cannot be mistaken for a supported one.
///
/// # Errors
///
/// Returns [`UsageError`] for an unknown argument or flag, a non-UTF-8
/// argument, or more arguments than the subcommand accepts.
pub fn parse_args<I, S>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Invocation::Status);
    };
    let first = first.as_ref();
    let Some(first) = first.to_str() else {
        return Err(UsageError::new(format!(
            "argument is not valid UTF-8: '{}'",
            first.to_string_lossy()
        )));
    };
    if first != "stop" {
        if let Some(extra) = args.next() {
            return Err(UsageError::new(format!(
                "unexpected extra argument '{}'",
                extra.as_ref().to_string_lossy()
            )));
        }
        return match first {
            "--version" | "-V" => Ok(Invocation::Version),
            "--help" | "-h" => Ok(Invocation::Help),
            "start" => Ok(Invocation::Start),
            argument => Err(UsageError::new(format!("unknown argument '{argument}'"))),
        };
    }

    let Some(flag) = args.next() else {
        return Ok(Invocation::Stop(StopMode::Refuse));
    };
    if let Some(extra) = args.next() {
        return Err(UsageError::new(format!(
            "unexpected extra argument '{}'",
            extra.as_ref().to_string_lossy()
        )));
    }
    let flag = flag.as_ref().to_str().ok_or_else(|| {
        UsageError::new(format!(
            "stop flag is not valid UTF-8: '{}'",
            flag.as_ref().to_string_lossy()
        ))
    })?;
    match flag {
        "--force" => Ok(Invocation::Stop(StopMode::Force)),
        "--drain" => Ok(Invocation::Stop(StopMode::Drain)),
        argument => Err(UsageError::new(format!("unknown stop flag '{argument}'"))),
    }
}

/// Usage text shared by `--help` and usage errors.
#[must_use]
pub fn usage() -> String {
    format!(
        "usage: {BINARY_NAME} [start | stop [--force | --drain] | --version | --help]\n\
         \n\
         With no arguments, {BINARY_NAME} prints the session-host status and exits\n\
         0 without binding an endpoint. `{BINARY_NAME} start` binds the per-user\n\
         endpoint and serves the bitty-ipc v1 service until daemon.stop or a\n\
         SIGTERM/SIGINT drain. `{BINARY_NAME} stop` asks the running daemon to\n\
         stop when no session is active; `--drain` requests the contract drain\n\
         mode, and `--force` terminates every session immediately. The stop path\n\
         uses the authenticated per-user endpoint; there is no TCP fallback.\n\
         \n\
         exit codes: 0 ok, already running, or no daemon; 1 startup or transport\n\
         failure; 2 usage error; 3 active sessions on a default stop; 4 drain\n\
         deadline forced the remainder; 5 a worker could not be verified gone;\n\
         6 endpoint tamper, a failed authentication invariant, or an endpoint\n\
         this account may not use. `--drain` exits 4 when it has to terminate a\n\
         live session; it is the only drain outcome this build serves.\n"
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
        Ok(Invocation::Stop(_)) => Rendered {
            // `run` intercepts `Stop`: the exchange needs the endpoint, so the
            // pure renderer never performs it.
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
        Ok(Invocation::Stop(mode)) => run_stop(mode),
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
        Ok(ServerOutcome::Stopped { mode, unverified }) => {
            let code = stopped_exit_code(mode, unverified);
            report_text(&format!("{BINARY_NAME}: stopped ({mode})\n"));
            ExitCode::from(code)
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

/// Issue one authenticated `daemon.stop` and map its result to an exit code.
fn run_stop(mode: StopMode) -> ExitCode {
    match client::stop(mode) {
        Ok(reply) => {
            let code = stop_reply_exit_code(&reply);
            report_text(&format!(
                "{BINARY_NAME}: stopped ({}) with {} active session(s)\n",
                reply.mode, reply.active_sessions
            ));
            if reply.unverified > 0 {
                report_error(&format!(
                    "{BINARY_NAME}: {} worker(s) could not be verified gone\n",
                    reply.unverified
                ));
            }
            ExitCode::from(code)
        }
        Err(StopError::Absent) => {
            report_text(&format!("{BINARY_NAME}: no running daemon\n"));
            ExitCode::from(EXIT_OK)
        }
        Err(error) => {
            let code = stop_error_exit_code(&error);
            report_error(&format!("{BINARY_NAME}: stop: {error}\n"));
            ExitCode::from(code)
        }
    }
}

/// The `D-05` exit code for the `bittyd start` process after an accepted stop.
///
/// A force stop that left a worker unverified is exit `5`; every other
/// accepted stop is `0`. A signal drain is mapped separately and stays `0` in
/// this slice.
#[must_use]
pub fn stopped_exit_code(mode: StopMode, unverified: bool) -> u8 {
    if mode == StopMode::Force && unverified {
        EXIT_UNVERIFIED_WORKER
    } else {
        EXIT_OK
    }
}

/// The `D-05` exit code for an accepted `daemon.stop`.
///
/// `force` is `0`, or `5` when a worker could not be verified gone. `drain` is
/// `0`, or `4` when the served loop reports that it forced the remainder.
/// `refuse` is `0`.
#[must_use]
pub fn stop_reply_exit_code(reply: &StopReply) -> u8 {
    match reply.mode {
        StopMode::Refuse => EXIT_OK,
        StopMode::Drain if reply.drain_forced => EXIT_DRAIN_FORCED,
        StopMode::Drain => EXIT_OK,
        StopMode::Force if reply.unverified > 0 => EXIT_UNVERIFIED_WORKER,
        StopMode::Force => EXIT_OK,
    }
}

/// The `D-05` exit code for a refused or failed stop exchange.
///
/// A channel this account may not use is the same authentication-invariant
/// class as an unsafe capability file, so both are exit `6`. There is no
/// separate permission code: `D-05` closes the set at `0`-`6`.
#[must_use]
pub fn stop_error_exit_code(error: &StopError) -> u8 {
    match error {
        StopError::Absent => EXIT_OK,
        StopError::Authentication | StopError::Permission => EXIT_TAMPER,
        StopError::Refused { code, .. } if code == "ActiveSessions" => EXIT_ACTIVE_SESSIONS,
        StopError::Refused { code, .. } if code == "DaemonStopping" => EXIT_OK,
        StopError::UnsupportedPlatform
        | StopError::InvalidEndpoint
        | StopError::Refused { .. }
        | StopError::Transport
        | StopError::Protocol => EXIT_INTERNAL,
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
        Invocation::Stop(_) => Rendered {
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
        assert!(result.stdout.contains("service API 1.5"));
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
                .contains("3 active sessions on a default stop; 4 drain")
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
    fn stop_parses_the_three_contract_modes() {
        assert_eq!(parse_args(["stop"]), Ok(Invocation::Stop(StopMode::Refuse)));
        assert_eq!(
            parse_args(["stop", "--force"]),
            Ok(Invocation::Stop(StopMode::Force))
        );
        assert_eq!(
            parse_args(["stop", "--drain"]),
            Ok(Invocation::Stop(StopMode::Drain))
        );
        assert_eq!(
            parse_args(["stop", "--force", "--drain"]),
            Err(UsageError::new("unexpected extra argument '--drain'"))
        );
        assert_eq!(
            parse_args(["stop", "--all"]),
            Err(UsageError::new("unknown stop flag '--all'"))
        );
        assert_eq!(
            parse_args(["--force"]),
            Err(UsageError::new("unknown argument '--force'"))
        );
    }

    fn stop_reply(mode: StopMode, unverified: u64, drain_forced: bool) -> StopReply {
        StopReply {
            mode,
            active_sessions: 0,
            terminated: 0,
            unverified,
            drain_forced,
        }
    }

    #[test]
    fn stop_exit_codes_match_d05() {
        assert_eq!(
            stop_reply_exit_code(&stop_reply(StopMode::Refuse, 0, false)),
            EXIT_OK
        );
        assert_eq!(
            stop_reply_exit_code(&stop_reply(StopMode::Force, 0, false)),
            EXIT_OK
        );
        assert_eq!(
            stop_reply_exit_code(&stop_reply(StopMode::Force, 1, false)),
            EXIT_UNVERIFIED_WORKER
        );
        assert_eq!(
            stop_reply_exit_code(&stop_reply(StopMode::Drain, 0, false)),
            EXIT_OK
        );
        assert_eq!(
            stop_reply_exit_code(&stop_reply(StopMode::Drain, 0, true)),
            EXIT_DRAIN_FORCED
        );
    }

    #[test]
    fn stop_refusal_exit_codes_match_d05() {
        let active = StopError::Refused {
            class: "Denied".to_owned(),
            code: "ActiveSessions".to_owned(),
            counts: Some((2, 16)),
        };
        assert_eq!(stop_error_exit_code(&active), EXIT_ACTIVE_SESSIONS);
        assert_eq!(
            stop_error_exit_code(&StopError::Refused {
                class: "Denied".to_owned(),
                code: "DaemonStopping".to_owned(),
                counts: None,
            }),
            EXIT_OK
        );
        assert_eq!(
            stop_error_exit_code(&StopError::Authentication),
            EXIT_TAMPER
        );
        assert_eq!(stop_error_exit_code(&StopError::Permission), EXIT_TAMPER);
        assert_eq!(stop_error_exit_code(&StopError::Absent), EXIT_OK);
        assert_eq!(stop_error_exit_code(&StopError::Transport), EXIT_INTERNAL);
    }

    #[test]
    fn the_help_text_names_the_closed_exit_set() {
        let text = usage();
        for code in [
            "0 ok",
            "1 startup",
            "2 usage",
            "3 active",
            "4 drain",
            "5 a worker",
            "6 endpoint",
        ] {
            assert!(text.contains(code), "help must pin {code:?}: {text}");
        }
        assert!(
            !text.contains("7 "),
            "the closed set has no 7 in help: {text}"
        );
    }

    #[test]
    fn a_forced_stop_with_an_unverified_worker_exits_five() {
        assert_eq!(stopped_exit_code(StopMode::Force, false), EXIT_OK);
        assert_eq!(
            stopped_exit_code(StopMode::Force, true),
            EXIT_UNVERIFIED_WORKER
        );
        assert_eq!(stopped_exit_code(StopMode::Refuse, false), EXIT_OK);
        assert_eq!(stopped_exit_code(StopMode::Drain, false), EXIT_OK);
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
