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
//! bitty ctl workspace list --format json
//! bitty ctl workspace new
//! bitty ctl workspace close ws:2
//! bitty ctl workspace focus ws:1
//! bitty ctl workspace move ws:2
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
//! - `workspace list`: `view.inspect`; `workspace new|focus|move`: `view.manage`
//! - `workspace close`: `terminal.manage` (explicit elevation: kills live sessions)
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

// ── submodules ───────────────────────────────────────────

mod apply;
mod client;
mod render;
mod request;

// ---------------------------------------------------------------------------
// `bitty ctl` CLI entry point (relocated from `main.rs`, CTX-0305)
// ---------------------------------------------------------------------------

use crate::cli::Args;

/// Runs `bitty ctl`; returns the process exit code.
///
/// - `--help` (anywhere in `ctl_raw`) prints help to stdout, exit 0, and
///   never requires an instance.
/// - Global `--socket`/`--instance`/`--format` before the `ctl` word merge
///   with per-`ctl` flags (per-`ctl` wins when only one side sets a value;
///   conflicting values are usage errors, exit 2).
/// - Other parse failures print the diagnostic plus usage to stderr (exit 2).
/// - Runtime verbs resolve targeting and speak IPC; exit codes follow the
///   stable v1 mapping (0 ok, 6 unavailable, 7 permission, 8 conflict).
pub(crate) fn run_cli(args: &Args) -> i32 {
    match parse_ctl_request(&args.ctl_raw) {
        Err(CtlParseError::Help) => {
            print!("{}", ctl_help_text());
            0
        }
        Err(err) => {
            eprintln!("{}\n{}", err.message(), ctl_usage());
            EXIT_USAGE
        }
        Ok((request, mut targeting)) => {
            // Merge global pre-`ctl` targeting: per-`ctl` flags win when
            // only one side sets a value; differing values are conflicts.
            if let Some(pre) = args.ctl_socket_pre.as_deref() {
                match targeting.socket.as_deref() {
                    None => targeting.socket = Some(pre.to_string()),
                    Some(post) if post == pre => {}
                    Some(post) => {
                        eprintln!(
                            "bitty ctl: conflicting --socket {pre:?} vs {post:?} (pass once; see `bitty ctl --help`)\n{}",
                            ctl_usage()
                        );
                        return EXIT_USAGE;
                    }
                }
            }
            if let Some(pre) = args.ctl_instance_pre.as_deref() {
                match targeting.instance.as_deref() {
                    None => targeting.instance = Some(pre.to_string()),
                    Some(post) if post == pre => {}
                    Some(post) => {
                        eprintln!(
                            "bitty ctl: conflicting --instance {pre:?} vs {post:?} (pass once; see `bitty ctl --help`)\n{}",
                            ctl_usage()
                        );
                        return EXIT_USAGE;
                    }
                }
            }
            // Global --format before `ctl` applies when `ctl` set none.
            // `parse_ctl_request` defaults to table, so detect an explicit
            // post-`ctl` format by re-scanning `ctl_raw` for the flag.
            let post_has_format = args
                .ctl_raw
                .iter()
                .any(|t| t == "--format" || t.starts_with("--format="));
            if !post_has_format {
                if let Some(global) = args.doctor_format.as_deref() {
                    match CtlFormat::parse(Some(global)) {
                        Ok(fmt) => targeting.format = fmt,
                        Err(message) => {
                            eprintln!("{message}\n{}", ctl_usage());
                            return EXIT_USAGE;
                        }
                    }
                }
            }
            execute_ctl(&request, &targeting)
        }
    }
}

#[cfg(test)]
mod tests;

pub use apply::{drain_global_control_queue, granted_scopes_for_servo};
pub use client::{
    CtlIpcOutcome, ResolvedTarget, ctl_roundtrip, list_live_instances, resolve_ctl_target,
};
pub use render::execute_ctl;
pub use request::{
    CtlFormat, CtlParseError, CtlRequest, CtlTargeting, ctl_help_text, ctl_usage, parse_ctl_request,
};
