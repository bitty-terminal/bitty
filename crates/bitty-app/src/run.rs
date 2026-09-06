//! `bitty run -- COMMAND...`: explicit child launch (CTX-0170).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`run` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty run` local class).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty run [OPTIONS] -- COMMAND...` where `OPTIONS` is
//!   `--cwd <path>`, `--env KEY=VALUE` (repeatable), `--title <string>`.
//! - `--` is **required** before `COMMAND`. A bare token before `--`
//!   (e.g. `bitty run echo hi`) is a `UsageError` (exit 2), never an implicit
//!   child. This keeps `bitty htop` from silently meaning
//!   `bitty run -- htop` and leaves the top-level namespace stable for
//!   `ctl` / `doctor` / `config` / `init`.
//! - `COMMAND` is non-empty after `--` and is executed **directly** as
//!   `argv[0]` plus tail args: no shell, no interpolation, no splitting.
//!   After `--` every token is verbatim, including tokens that look like
//!   flags (`bitty run -- --help` runs a program named `--help`).
//! - Exit-code passthrough: the child is waited on; its numeric exit code
//!   becomes `bitty`'s exit code. Spawn failures (not found, bad `--cwd`)
//!   are generic errors (exit 1). Parse failures are usage errors (exit 2).
//! - Bare-`PROGRAM` conflict: the word `run` as the first positional is
//!   always this subcommand, never a program named `run`. To run a program
//!   literally named `run` (or `config`, `init`, `doctor`, ...), use the
//!   unambiguous spelling `bitty run -- run ...` (or the legacy escape
//!   `bitty -- run ...` for the pre-`run` path). The same holds for any
//!   colliding name: `bitty run -- config` runs `config`, it does not enter
//!   `bitty config`.
//! - Local class: no running instance, no IPC, no plugin VM. `--help` never
//!   requires an instance.
//!
//! # What `run` does not do (honest scope)
//!
//! - Bare `bitty PROGRAM` (e.g. `bitty /bin/bash`) still opens the terminal
//!   window with that program for backward compatibility (CTX-0136). It is
//!   legacy; prefer `bitty run -- PROGRAM` for colliding names. A future RFC
//!   may withdraw the bare spelling once the top level stabilizes; this
//!   slice does not withdraw it so existing flows keep working.
//! - `--title` is accepted and bounded but currently only observable as the
//!   `BITTY_TITLE` environment value in the child (future GUI window title).
//!   It never changes dispatch or exit codes.
//! - The child inherits the parent environment plus the stable indicators
//!   `TERM=bitty`, `BITTY=1`, `BITTY_VERSION=<semver>`; explicit `--env`
//!   entries win over those defaults. No credential is ever injected.
//!
//! # Bounds (T-01 parity, fail closed with exit 2)
//!
//! - `COMMAND` tokens: 1 to [`MAX_RUN_ARGS`], each 1 to [`MAX_RUN_ARG_LEN`]
//!   bytes, no NUL.
//! - `--env` entries: at most [`MAX_RUN_ENVS`]; key matches
//!   `^[A-Za-z_][A-Za-z0-9_]*$`, 1 to [`MAX_RUN_ENV_KEY_LEN`] bytes; value at
//!   most [`MAX_RUN_ENV_VALUE_LEN`] bytes; no NUL in either; `KEY=VALUE`
//!   shape required (bare `FOO` rejected).
//! - `--cwd`: 1 to [`MAX_RUN_CWD_LEN`] bytes, no NUL. Existence is checked
//!   at spawn time (missing dir is exit 1, not exit 2).
//! - `--title`: at most [`MAX_RUN_TITLE_LEN`] bytes, no NUL.

#![forbid(unsafe_code)]

/// Maximum `COMMAND` tokens after `--` (program plus tail args).
pub const MAX_RUN_ARGS: usize = 128;
/// Maximum bytes per `COMMAND` token.
pub const MAX_RUN_ARG_LEN: usize = 32_768;
/// Maximum `--env KEY=VALUE` entries.
pub const MAX_RUN_ENVS: usize = 128;
/// Maximum bytes for an `--env` key.
pub const MAX_RUN_ENV_KEY_LEN: usize = 256;
/// Maximum bytes for an `--env` value.
pub const MAX_RUN_ENV_VALUE_LEN: usize = 32_768;
/// Maximum bytes for `--cwd`.
pub const MAX_RUN_CWD_LEN: usize = 4096;
/// Maximum bytes for `--title`.
pub const MAX_RUN_TITLE_LEN: usize = 256;

/// Owned `bitty run` request: validated options plus verbatim `COMMAND`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// `--cwd <path>`; applied as the child working directory.
    pub cwd: Option<String>,
    /// `--env KEY=VALUE` entries in CLI order; last duplicate wins at spawn.
    pub env: Vec<(String, String)>,
    /// `--title <string>`; exposed as `BITTY_TITLE` in the child.
    pub title: Option<String>,
    /// `COMMAND...` after `--`: `command[0]` is the program, rest are args.
    pub command: Vec<String>,
}

/// `bitty run` parse failure. The caller maps every variant except
/// [`RunParseError::Help`] to stderr plus exit 2 (`UsageError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunParseError {
    /// `--help` / `-h` appeared before `--`: print help, exit 0.
    Help,
    /// No `--` separator before `COMMAND` (includes bare `bitty run echo`).
    MissingSeparator { hint: String },
    /// `--` present but no `COMMAND` after it.
    MissingCommand,
    /// Unknown flag before `--` (e.g. `--headless`, `--split`).
    UnknownFlag { flag: String },
    /// Option needs a value but none (or `--`) followed.
    MissingValue { option: String },
    /// `--env` value is not `KEY=VALUE` or violates key/value bounds.
    InvalidEnv { value: String, reason: String },
    /// `--cwd` violates length / NUL bounds.
    InvalidCwd { reason: String },
    /// `--title` violates length / NUL bounds.
    InvalidTitle { reason: String },
    /// `COMMAND` violates count / length / NUL bounds.
    InvalidCommand { reason: String },
}

impl RunParseError {
    /// One-line stderr diagnostic (without usage trailer).
    pub fn message(&self) -> String {
        match self {
            Self::Help => String::from("bitty run: help requested"),
            Self::MissingSeparator { hint } => {
                if hint.is_empty() {
                    String::from(
                        "bitty run: missing `--` before COMMAND (usage: bitty run [OPTIONS] -- COMMAND...)",
                    )
                } else {
                    format!(
                        "bitty run: missing `--` before COMMAND (saw {hint:?}; usage: bitty run [OPTIONS] -- COMMAND...)"
                    )
                }
            }
            Self::MissingCommand => String::from(
                "bitty run: missing COMMAND after `--` (usage: bitty run [OPTIONS] -- COMMAND...)",
            ),
            Self::UnknownFlag { flag } => {
                format!("bitty run: unknown flag {flag:?} (see `bitty run --help`)")
            }
            Self::MissingValue { option } => {
                format!("bitty run: {option} needs a value (see `bitty run --help`)")
            }
            Self::InvalidEnv { value, reason } => {
                format!("bitty run: invalid --env {value:?}: {reason}")
            }
            Self::InvalidCwd { reason } => {
                format!("bitty run: invalid --cwd: {reason}")
            }
            Self::InvalidTitle { reason } => {
                format!("bitty run: invalid --title: {reason}")
            }
            Self::InvalidCommand { reason } => {
                format!("bitty run: invalid COMMAND: {reason}")
            }
        }
    }
}

/// Short usage for stderr (fail-closed exit 2 trailer).
pub fn run_usage() -> String {
    String::from(
        "usage: bitty run [--cwd PATH] [--env KEY=VALUE ...] [--title STRING] -- COMMAND...\n\
         \n\
         runs COMMAND directly (no shell); `--` is required; exit code is the child's.\n\
         examples:\n\
         \x20 bitty run -- htop\n\
         \x20 bitty run --cwd /tmp -- echo hi\n\
         \x20 bitty run --env FOO=bar -- printenv FOO",
    )
}

/// Full help for `bitty run --help` (stdout, exit 0).
pub fn run_help_text() -> String {
    format!(
        "bitty run — start a child program (local, no instance needed)\n\
         \n\
         Usage: bitty run [OPTIONS] -- COMMAND...\n\
         \n\
         Options:\n  \
           --cwd PATH         Run COMMAND in PATH (must exist at spawn; else exit 1)\n  \
           --env KEY=VALUE    Add/override one child env entry (repeatable, at most {MAX_RUN_ENVS})\n  \
                              KEY must match ^[A-Za-z_][A-Za-z0-9_]*$ (1..={MAX_RUN_ENV_KEY_LEN} bytes)\n  \
           --title STRING     Window-title hint (at most {MAX_RUN_TITLE_LEN} bytes; exposed as BITTY_TITLE)\n  \
           -h, --help         Print this help and exit\n\
         \n\
         Separator:\n  \
           `--` is required before COMMAND. `bitty run echo hi` (no `--`) is a\n  \
           usage error (exit 2). After `--` every token is verbatim COMMAND,\n  \
           even flags: `bitty run -- --help` runs a program named `--help`.\n\
         \n\
         Child environment:\n  \
           Inherits the parent plus TERM=bitty, BITTY=1, BITTY_VERSION={version}\n  \
           (plus BITTY_TITLE when --title is given). Explicit --env wins over\n  \
           those defaults. No credential is ever injected.\n\
         \n\
         Exit codes:\n  \
           0  child exited 0\n  \
           N  child exited N (passthrough, 1..=255)\n  \
           1  spawn failure (program not found, bad --cwd, killed by signal)\n  \
           2  usage error (missing `--`, missing COMMAND, unknown flag, bad --env/--cwd/--title)\n\
         \n\
         Bare-PROGRAM note:\n  \
           `bitty htop` never means `bitty run -- htop`. The word `run` as the\n  \
           first positional is always this subcommand; to run a program whose\n  \
           name collides with a subcommand (run, config, init, doctor, ...),\n  \
           use the unambiguous spelling: `bitty run -- <name> ...`.\n  \
           Bare `bitty PROGRAM` still opens the terminal for compatibility;\n  \
           prefer `bitty run -- PROGRAM` for new scripts.\n\
         \n\
         Examples:\n  \
           bitty run -- htop\n  \
           bitty run -- /bin/echo hello\n  \
           bitty run --cwd /tmp --env FOO=bar -- printenv FOO\n  \
           bitty run -- sh -c 'echo hi'\n",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// True when `key` is a valid `--env` key: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    true
}

/// Parses `tokens` (argv words after the `run` word) into a [`RunRequest`].
///
/// Pure and total for unit testing: no env/fs/process access. Every bound
/// violation becomes [`RunParseError`]; the caller prints
/// [`RunParseError::message`] plus [`run_usage`] to stderr and exits 2,
/// except [`RunParseError::Help`] which prints [`run_help_text`] to stdout
/// and exits 0.
pub fn parse_run_request(tokens: &[String]) -> Result<RunRequest, RunParseError> {
    let mut cwd: Option<String> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut title: Option<String> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            let command: Vec<String> = tokens[i + 1..].to_vec();
            if command.is_empty() {
                return Err(RunParseError::MissingCommand);
            }
            validate_command(&command)?;
            return Ok(RunRequest {
                cwd,
                env,
                title,
                command,
            });
        }
        if token == "--help" || token == "-h" {
            return Err(RunParseError::Help);
        }
        if token == "--cwd" {
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| RunParseError::MissingValue {
                    option: String::from("--cwd"),
                })?;
            if value == "--" {
                return Err(RunParseError::MissingValue {
                    option: String::from("--cwd"),
                });
            }
            validate_cwd(value)?;
            cwd = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--cwd=") {
            if value.is_empty() || value == "--" {
                return Err(RunParseError::MissingValue {
                    option: String::from("--cwd"),
                });
            }
            validate_cwd(value)?;
            cwd = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--env" {
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| RunParseError::MissingValue {
                    option: String::from("--env"),
                })?;
            if value == "--" {
                return Err(RunParseError::MissingValue {
                    option: String::from("--env"),
                });
            }
            let (k, v) = parse_env_entry(value)?;
            if env.len() >= MAX_RUN_ENVS {
                return Err(RunParseError::InvalidEnv {
                    value: value.clone(),
                    reason: format!("too many --env entries (at most {MAX_RUN_ENVS})"),
                });
            }
            env.push((k, v));
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--env=") {
            if value.is_empty() || value == "--" {
                return Err(RunParseError::MissingValue {
                    option: String::from("--env"),
                });
            }
            let (k, v) = parse_env_entry(value)?;
            if env.len() >= MAX_RUN_ENVS {
                return Err(RunParseError::InvalidEnv {
                    value: value.to_string(),
                    reason: format!("too many --env entries (at most {MAX_RUN_ENVS})"),
                });
            }
            env.push((k, v));
            i += 1;
            continue;
        }
        if token == "--title" {
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| RunParseError::MissingValue {
                    option: String::from("--title"),
                })?;
            if value == "--" {
                return Err(RunParseError::MissingValue {
                    option: String::from("--title"),
                });
            }
            validate_title(value)?;
            title = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--title=") {
            // `--title=` with an empty value means an empty title (allowed:
            // clears the hint); `--title=--` is a literal title, not a
            // separator, because `=` form is unambiguous.
            validate_title(value)?;
            title = Some(value.to_string());
            i += 1;
            continue;
        }
        if token.starts_with('-') {
            return Err(RunParseError::UnknownFlag {
                flag: token.clone(),
            });
        }
        // Bare token before `--`: the separator is mandatory, so fail closed
        // rather than guessing. Names the token so `bitty run htop` tells
        // the user to write `bitty run -- htop`.
        return Err(RunParseError::MissingSeparator {
            hint: token.clone(),
        });
    }
    // No `--` seen at all (includes empty `bitty run`).
    Err(RunParseError::MissingSeparator {
        hint: String::new(),
    })
}

/// Validates one `--env KEY=VALUE` entry, splitting on the first `=`.
fn parse_env_entry(raw: &str) -> Result<(String, String), RunParseError> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: String::from("want KEY=VALUE (missing `=`)"),
        })?;
    if key.is_empty() {
        return Err(RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: String::from("empty KEY (want KEY=VALUE)"),
        });
    }
    if key.len() > MAX_RUN_ENV_KEY_LEN {
        return Err(RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: format!("KEY must be <= {MAX_RUN_ENV_KEY_LEN} bytes"),
        });
    }
    if !is_valid_env_key(key) {
        return Err(RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: String::from("KEY must match ^[A-Za-z_][A-Za-z0-9_]*$"),
        });
    }
    if value.len() > MAX_RUN_ENV_VALUE_LEN {
        return Err(RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: format!("VALUE must be <= {MAX_RUN_ENV_VALUE_LEN} bytes"),
        });
    }
    if key.contains('\0') || value.contains('\0') {
        return Err(RunParseError::InvalidEnv {
            value: raw.to_string(),
            reason: String::from("KEY and VALUE must not contain NUL"),
        });
    }
    Ok((key.to_string(), value.to_string()))
}

/// Validates `--cwd` shape (existence is checked at spawn, exit 1).
fn validate_cwd(value: &str) -> Result<(), RunParseError> {
    if value.is_empty() {
        return Err(RunParseError::InvalidCwd {
            reason: String::from("must not be empty"),
        });
    }
    if value.len() > MAX_RUN_CWD_LEN {
        return Err(RunParseError::InvalidCwd {
            reason: format!("must be <= {MAX_RUN_CWD_LEN} bytes"),
        });
    }
    if value.contains('\0') {
        return Err(RunParseError::InvalidCwd {
            reason: String::from("must not contain NUL"),
        });
    }
    Ok(())
}

/// Validates `--title` shape (currently exposed as `BITTY_TITLE`).
fn validate_title(value: &str) -> Result<(), RunParseError> {
    if value.len() > MAX_RUN_TITLE_LEN {
        return Err(RunParseError::InvalidTitle {
            reason: format!("must be <= {MAX_RUN_TITLE_LEN} bytes"),
        });
    }
    if value.contains('\0') {
        return Err(RunParseError::InvalidTitle {
            reason: String::from("must not contain NUL"),
        });
    }
    Ok(())
}

/// Validates `COMMAND` count / length / NUL bounds.
fn validate_command(command: &[String]) -> Result<(), RunParseError> {
    if command.is_empty() {
        return Err(RunParseError::MissingCommand);
    }
    if command.len() > MAX_RUN_ARGS {
        return Err(RunParseError::InvalidCommand {
            reason: format!("too many COMMAND tokens (at most {MAX_RUN_ARGS})"),
        });
    }
    for token in command {
        if token.is_empty() {
            return Err(RunParseError::InvalidCommand {
                reason: String::from("COMMAND token must not be empty"),
            });
        }
        if token.len() > MAX_RUN_ARG_LEN {
            return Err(RunParseError::InvalidCommand {
                reason: format!("COMMAND token must be <= {MAX_RUN_ARG_LEN} bytes"),
            });
        }
        if token.contains('\0') {
            return Err(RunParseError::InvalidCommand {
                reason: String::from("COMMAND token must not contain NUL"),
            });
        }
    }
    Ok(())
}

/// Executes a validated [`RunRequest`]: spawns `command[0]` directly with
/// tail args, inherits stdio, waits, and returns the exit code to `main`
/// for passthrough.
///
/// - `Ok` spawn plus numeric child status returns that code (0..=255).
/// - Spawn failure or signal termination (no numeric code) prints to stderr
///   and returns 1 (generic error, never 2: parsing already succeeded).
/// - Impure (process spawn, env, fs for `--cwd`); total (all failures map
///   to 1, never panics).
pub fn execute_run(request: &RunRequest) -> i32 {
    let program = &request.command[0];
    let args = &request.command[1..];
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(cwd) = &request.cwd {
        cmd.current_dir(cwd);
    }
    // Stable child indicators (RFC: TERM/BITTY/BITTY_VERSION stable for v1).
    // Explicit --env wins over these defaults; the parent environment is
    // otherwise inherited unchanged (including advisory BITTY_SOCKET when
    // launched inside Bitty — the server still authenticates the socket).
    cmd.env("TERM", "bitty");
    cmd.env("BITTY", "1");
    cmd.env("BITTY_VERSION", env!("CARGO_PKG_VERSION"));
    if let Some(title) = &request.title {
        cmd.env("BITTY_TITLE", title);
    }
    for (key, value) in &request.env {
        cmd.env(key, value);
    }
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());
    match cmd.status() {
        Ok(status) => match status.code() {
            Some(code) => code,
            None => {
                eprintln!("bitty run: child terminated by signal (no exit code)");
                1
            }
        },
        Err(err) => {
            eprintln!("bitty run: cannot run {program:?}: {err}");
            1
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
    fn empty_run_is_missing_separator() {
        let err = parse_run_request(&[]).expect_err("empty must fail");
        assert_eq!(
            err,
            RunParseError::MissingSeparator {
                hint: String::new()
            }
        );
    }

    #[test]
    fn bare_token_without_separator_fails_closed() {
        // `bitty run echo hi` must not run echo: `--` is required.
        let err = parse_run_request(&words(&["echo", "hi"])).expect_err("must require --");
        assert_eq!(
            err,
            RunParseError::MissingSeparator {
                hint: String::from("echo")
            }
        );
    }

    #[test]
    fn run_without_command_after_separator_fails() {
        let err = parse_run_request(&words(&["--"])).expect_err("must need COMMAND");
        assert_eq!(err, RunParseError::MissingCommand);
    }

    #[test]
    fn minimal_command_parses() {
        let req = parse_run_request(&words(&["--", "echo", "hi"])).expect("minimal must parse");
        assert_eq!(req.command, words(&["echo", "hi"]));
        assert_eq!(req.cwd, None);
        assert!(req.env.is_empty());
        assert_eq!(req.title, None);
    }

    #[test]
    fn separator_is_required_even_with_options() {
        // Options alone do not imply a separator.
        let err = parse_run_request(&words(&["--cwd", "/tmp"])).expect_err("options need -- too");
        assert_eq!(
            err,
            RunParseError::MissingSeparator {
                hint: String::new()
            }
        );
    }

    #[test]
    fn options_before_separator_parse() {
        let req = parse_run_request(&words(&[
            "--cwd", "/tmp", "--env", "FOO=bar", "--title", "demo", "--", "printenv", "FOO",
        ]))
        .expect("options must parse");
        assert_eq!(req.cwd.as_deref(), Some("/tmp"));
        assert_eq!(req.env, vec![(String::from("FOO"), String::from("bar"))]);
        assert_eq!(req.title.as_deref(), Some("demo"));
        assert_eq!(req.command, words(&["printenv", "FOO"]));
    }

    #[test]
    fn equals_forms_parse() {
        let req = parse_run_request(&words(&[
            "--cwd=/tmp",
            "--env=FOO=bar",
            "--title=demo",
            "--",
            "echo",
        ]))
        .expect("equals forms must parse");
        assert_eq!(req.cwd.as_deref(), Some("/tmp"));
        assert_eq!(req.env, vec![(String::from("FOO"), String::from("bar"))]);
        assert_eq!(req.title.as_deref(), Some("demo"));
    }

    #[test]
    fn unknown_flag_before_separator_is_usage_error() {
        let err = parse_run_request(&words(&["--headless", "--", "echo"]))
            .expect_err("unknown flag must fail");
        assert_eq!(
            err,
            RunParseError::UnknownFlag {
                flag: String::from("--headless")
            }
        );
    }

    #[test]
    fn flag_after_separator_is_command_not_flag() {
        // `run -- --help` runs a program named `--help`, it is not help.
        let req = parse_run_request(&words(&["--", "--help"])).expect("must be COMMAND");
        assert_eq!(req.command, words(&["--help"]));
    }

    #[test]
    fn help_before_separator_is_help() {
        assert_eq!(
            parse_run_request(&words(&["--help"])).expect_err("help"),
            RunParseError::Help
        );
        assert_eq!(
            parse_run_request(&words(&["-h"])).expect_err("help"),
            RunParseError::Help
        );
    }

    #[test]
    fn colliding_names_after_separator_are_commands() {
        // Conflict case: `run -- config` runs `config`, it does not enter
        // `bitty config`. Same for `run`, `init`, `doctor`.
        for name in ["config", "run", "init", "doctor", "ctl", "htop"] {
            let req = parse_run_request(&words(&["--", name])).expect("colliding name must parse");
            assert_eq!(req.command, words(&[name]));
        }
    }

    #[test]
    fn env_requires_equals() {
        let err = parse_run_request(&words(&["--env", "FOO", "--", "echo"]))
            .expect_err("bare FOO must fail");
        assert!(matches!(err, RunParseError::InvalidEnv { .. }), "{err:?}");
    }

    #[test]
    fn env_key_shape_is_enforced() {
        for bad in ["1FOO=bar", "FOO-BAR=baz", "=bar", "FOO BAR=baz"] {
            let err = parse_run_request(&words(&["--env", bad, "--", "echo"]))
                .expect_err("bad key must fail");
            assert!(
                matches!(err, RunParseError::InvalidEnv { .. }),
                "{bad:?} -> {err:?}"
            );
        }
        // Good keys parse.
        let req = parse_run_request(&words(&["--env", "_FOO1=bar", "--", "echo"]))
            .expect("good key must parse");
        assert_eq!(req.env, vec![(String::from("_FOO1"), String::from("bar"))]);
    }

    #[test]
    fn env_value_may_contain_equals() {
        let req = parse_run_request(&words(&["--env", "FOO=a=b=c", "--", "echo"]))
            .expect("value may contain =");
        assert_eq!(req.env, vec![(String::from("FOO"), String::from("a=b=c"))]);
    }

    #[test]
    fn too_many_envs_fail_closed() {
        let mut tokens = Vec::new();
        for n in 0..(MAX_RUN_ENVS + 1) {
            tokens.push(String::from("--env"));
            tokens.push(format!("K{n}=v"));
        }
        tokens.push(String::from("--"));
        tokens.push(String::from("echo"));
        let err = parse_run_request(&tokens).expect_err("too many envs must fail");
        assert!(matches!(err, RunParseError::InvalidEnv { .. }), "{err:?}");
    }

    #[test]
    fn cwd_empty_fails() {
        let err = parse_run_request(&words(&["--cwd", "", "--", "echo"]))
            .expect_err("empty cwd must fail");
        assert!(matches!(err, RunParseError::InvalidCwd { .. }), "{err:?}");
    }

    #[test]
    fn title_bound_is_enforced() {
        let long = "t".repeat(MAX_RUN_TITLE_LEN + 1);
        let err = parse_run_request(&words(&["--title", long.as_str(), "--", "echo"]))
            .expect_err("long title must fail");
        assert!(matches!(err, RunParseError::InvalidTitle { .. }), "{err:?}");
    }

    #[test]
    fn command_count_bound_is_enforced() {
        let mut tokens = vec![String::from("--"), String::from("echo")];
        for n in 0..MAX_RUN_ARGS {
            tokens.push(format!("arg{n}"));
        }
        // program + MAX_RUN_ARGS tail args = MAX_RUN_ARGS+1 tokens total.
        let err = parse_run_request(&tokens).expect_err("too many args must fail");
        assert!(
            matches!(err, RunParseError::InvalidCommand { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn nul_in_command_fails() {
        let err = parse_run_request(&words(&["--", "echo", "a\0b"])).expect_err("NUL must fail");
        assert!(
            matches!(err, RunParseError::InvalidCommand { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn execute_true_returns_zero_and_false_nonzero() {
        // Headless-safe: `true`/`false` exist on Linux CI runners.
        let ok = RunRequest {
            cwd: None,
            env: Vec::new(),
            title: None,
            command: words(&["true"]),
        };
        assert_eq!(execute_run(&ok), 0);
        let fail = RunRequest {
            cwd: None,
            env: Vec::new(),
            title: None,
            command: words(&["false"]),
        };
        assert_ne!(execute_run(&fail), 0);
    }

    #[test]
    fn execute_missing_program_is_generic_error_not_usage() {
        let missing = RunRequest {
            cwd: None,
            env: Vec::new(),
            title: None,
            command: words(&["bitty-run-definitely-missing-program-xyz"]),
        };
        assert_eq!(execute_run(&missing), 1);
    }

    #[test]
    fn execute_missing_cwd_is_generic_error() {
        let bad_cwd = RunRequest {
            cwd: Some(String::from("/bitty-run-definitely-missing-dir-xyz")),
            env: Vec::new(),
            title: None,
            command: words(&["true"]),
        };
        assert_eq!(execute_run(&bad_cwd), 1);
    }
}
