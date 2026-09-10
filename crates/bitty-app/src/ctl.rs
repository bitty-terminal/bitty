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
