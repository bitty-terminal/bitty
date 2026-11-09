//! `bitty ctl` outcome rendering and the client entry point (split from `ctl.rs`,
//! CTX-0307).

use super::client::extract_string_from;
use super::{
    CtlFormat, CtlIpcOutcome, CtlRequest, CtlTargeting, EXIT_COMPAT, EXIT_CONFIG, EXIT_CONFLICT,
    EXIT_GENERIC, EXIT_OK, EXIT_PERM, EXIT_RUNTIME, ResolvedTarget, ctl_roundtrip,
    list_live_instances, resolve_ctl_target,
};

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
    let target = match resolve_target(targeting) {
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
    // Issue #1520: a consent grant is never issuable from flags,
    // environment, configuration, or child-process inheritance. The
    // confirmation phrase is prompted on the local terminal here and
    // spliced into the params; `wire_params` emits an empty phrase that
    // the server always denies, so only this path can grant.
    let params = match request {
        CtlRequest::ConsentGrant {
            scope,
            terminal_id,
            family,
        } => {
            let what = match (terminal_id.as_deref(), family.as_deref()) {
                (Some(terminal), Some(family)) => format!("{family} {terminal}"),
                _ => scope.clone(),
            };
            match crate::consent::ExplicitConsent::confirm_interactive(&what) {
                Ok(_) => Some(crate::consent::consent_grant_params(
                    scope,
                    terminal_id.as_deref(),
                    family.as_deref(),
                    &crate::consent::expected_confirm_phrase(&what),
                )),
                Err(err) => {
                    if emit_json {
                        println!(
                            "{}",
                            format_failure(registry, "Denied", err.code(), err.message())
                        );
                    } else {
                        eprintln!("bitty ctl: consent not granted ({})", err.message());
                    }
                    return EXIT_PERM;
                }
            }
        }
        _ => request.wire_params(),
    };
    match ctl_roundtrip(
        &target.socket_path,
        method,
        params.as_deref(),
        current_uid(),
    ) {
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

/// Resolve the live target shared by `ctl` and `reload`.
///
/// Impure (process env + filesystem discovery); the exit-6 text is the RFC's
/// "no live instance / ambiguous instance" contract.
fn resolve_target(targeting: &CtlTargeting) -> Result<ResolvedTarget, String> {
    let env_socket = std::env::var("BITTY_SOCKET").ok();
    let env_instance = std::env::var("BITTY_INSTANCE_ID").ok();
    let env_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    resolve_ctl_target(
        targeting,
        env_socket.as_deref(),
        env_instance.as_deref(),
        env_xdg.as_deref(),
        current_uid(),
    )
}

/// Portable uid: `/proc/self` metadata is Linux-only (absent on macOS, where
/// it fell back to 0 and failed endpoint verification against a user-owned
/// socket dir). `current_unix_uid` is per-platform (rustix on Linux, nix
/// elsewhere).
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        bitty_ipc::current_unix_uid().unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        0u32
    }
}

/// Execute `bitty reload` (bitty#2927): one `core.config.reload` round-trip
/// with the reload-specific result contract.
///
/// The alias sends the same registry verb as `bitty ctl config reload`, so
/// scope, bounds, and the apply path are unchanged. Differences are output
/// only: the reply's `changed` / `rejected` / `restart_required` /
/// `plan_version` fields drive a reload-shaped report, and a rejected or
/// failed plan exits 3 with the dotted field path instead of reading as a
/// successful control call.
pub fn execute_reload(targeting: &CtlTargeting, dry_run: bool) -> i32 {
    /// Registry id for the envelope (`core.config.reload`).
    const REGISTRY: &str = "core.config.reload";
    let emit_json = matches!(targeting.format, CtlFormat::Json | CtlFormat::Jsonl);

    let target = match resolve_target(targeting) {
        Ok(target) => target,
        Err(message) => {
            let message = message.replacen("bitty ctl: ", "bitty reload: ", 1);
            if emit_json {
                println!(
                    "{}",
                    format_failure(REGISTRY, "Unavailable", "NoInstance", &message)
                );
            } else {
                eprintln!("{message}");
                eprintln!(
                    "bitty reload: no live instance to reload — start bitty first, \
                     or pass --socket PATH / --instance ID"
                );
            }
            return EXIT_RUNTIME;
        }
    };

    let params = dry_run.then(|| String::from("{\"dry_run\":true}"));
    let outcome = match ctl_roundtrip(
        &target.socket_path,
        bitty_ipc::ctl::METHOD_RELOAD_CONFIG,
        params.as_deref(),
        current_uid(),
    ) {
        Ok(outcome) => outcome,
        Err(message) => {
            if emit_json {
                println!(
                    "{}",
                    format_failure(REGISTRY, "Unavailable", "Transport", &message)
                );
            } else {
                eprintln!("{message}");
                eprintln!(
                    "bitty reload: no live instance to reload — start bitty first, \
                     or pass --socket PATH / --instance ID"
                );
            }
            return EXIT_RUNTIME;
        }
    };

    if !outcome.ok {
        let class = class_for_server_error(&outcome.category, &outcome.code);
        let exit = exit_for_server_error(&outcome.category, &outcome.code);
        if emit_json {
            println!(
                "{}",
                format_failure(REGISTRY, class, &outcome.code, &outcome.message)
            );
        } else {
            eprintln!(
                "bitty reload: {} ({}/{})",
                outcome.message, class, outcome.code
            );
        }
        return exit;
    }

    render_reload_outcome(&outcome.result_json, targeting, dry_run, &target)
}

/// Render the `core.config.reload` result with reload semantics.
///
/// A reply without a reload context (`probed: true`, no `reloaded`) is the
/// instance's honest "I cannot apply a reload" answer (`--safe`); it exits 6
/// rather than reporting success. Rejected, load-error, and apply-error
/// kinds exit 3 and name the dotted field path when the server supplied one.
fn render_reload_outcome(
    result_json: &str,
    targeting: &CtlTargeting,
    dry_run: bool,
    target: &ResolvedTarget,
) -> i32 {
    /// Registry id for the envelope (`core.config.reload`).
    const REGISTRY: &str = "core.config.reload";
    let emit_json = matches!(targeting.format, CtlFormat::Json | CtlFormat::Jsonl);

    if extract_bool(result_json, "reloaded").is_none()
        && extract_bool(result_json, "probed") == Some(true)
    {
        let message = String::from(
            "the running instance does not serve live reload (it was started with --safe); \
             restart it without --safe and retry",
        );
        if emit_json {
            println!(
                "{}",
                format_failure(REGISTRY, "Unavailable", "ReloadUnavailable", &message)
            );
        } else {
            eprintln!("bitty reload: {message}");
        }
        return EXIT_RUNTIME;
    }

    let Some(kind) = extract_string_from(result_json, "kind") else {
        let message = String::from("the instance returned an unexpected reload reply");
        if emit_json {
            println!(
                "{}",
                format_failure(REGISTRY, "Unavailable", "ReloadUnavailable", &message)
            );
        } else {
            eprintln!("bitty reload: {message}");
        }
        return EXIT_RUNTIME;
    };
    let changed = extract_string_array(result_json, "changed").unwrap_or_default();
    let restart = extract_string_array(result_json, "restart_required").unwrap_or_default();
    let plugins = extract_string_array(result_json, "plugins_restart_required").unwrap_or_default();
    let rejected = extract_string_array(result_json, "rejected").unwrap_or_default();
    let server_message = extract_string_from(result_json, "message").unwrap_or_default();
    let path = extract_string_from(result_json, "path").unwrap_or_default();

    if matches!(kind.as_str(), "rejected" | "load-error" | "apply-error") {
        let detail = match (rejected.is_empty(), server_message.is_empty()) {
            (false, false) => format!("rejected at {}: {server_message}", rejected.join(", ")),
            (false, true) => format!("rejected at {}", rejected.join(", ")),
            (true, false) => server_message,
            (true, true) => format!("config reload failed ({path})"),
        };
        let code = match kind.as_str() {
            "rejected" => "ReloadRejected",
            "apply-error" => "ConfigApplyError",
            _ => "ConfigLoadError",
        };
        if emit_json {
            println!("{}", format_failure(REGISTRY, "ConfigError", code, &detail));
        } else {
            eprintln!("bitty reload: {detail}");
        }
        return EXIT_CONFIG;
    }

    if emit_json {
        println!("{}", format_success(REGISTRY, result_json));
        return reload_exit(&plugins, dry_run);
    }
    print!(
        "{}",
        render_reload_table(
            &kind,
            dry_run,
            extract_number(result_json, "plan_version"),
            ReloadTableChanges {
                changed: &changed,
                restart: &restart,
                plugins: &plugins,
            },
            &path,
            &target.instance,
        )
    );
    reload_exit(&plugins, dry_run)
}

/// Exit code for a reload reply (bitty#2969).
///
/// A pending plugin-set change is the one real reload outcome that cannot
/// apply without a restart, so it is reported with the distinct restart code
/// even though the config plan itself is valid (or already applied). A
/// `--dry-run` only reports and keeps the documented dry-run exit 0.
fn reload_exit(plugins_restart_required: &[String], dry_run: bool) -> i32 {
    if dry_run || plugins_restart_required.is_empty() {
        EXIT_OK
    } else {
        crate::reload::EXIT_RESTART
    }
}

/// The three change lists a human reload table renders.
struct ReloadTableChanges<'a> {
    /// Dotted config paths that changed (or would change).
    changed: &'a [String],
    /// Dotted config paths that stay pending until restart.
    restart: &'a [String],
    /// Plugin ids whose installed enabled state changed since launch.
    plugins: &'a [String],
}

/// Human table for a successful reload (not a machine contract).
fn render_reload_table(
    kind: &str,
    dry_run: bool,
    plan_version: Option<u64>,
    changes: ReloadTableChanges<'_>,
    path: &str,
    instance: &str,
) -> String {
    let ReloadTableChanges {
        changed,
        restart,
        plugins,
    } = changes;
    let mut out = String::new();
    let headline = if dry_run { "dry run" } else { kind };
    match plan_version {
        Some(version) => out.push_str(&format!(
            "reload {headline} on {instance} — plan v{version}\n"
        )),
        None => out.push_str(&format!("reload {headline} on {instance}\n")),
    }
    if !path.is_empty() {
        out.push_str(&format!("  config: {path}\n"));
    }
    if changed.is_empty() {
        if kind == "unchanged" {
            out.push_str("  no settings changed\n");
        }
    } else {
        let label = if dry_run {
            "would change"
        } else if kind == "restart-required" {
            "differs"
        } else {
            "changed"
        };
        out.push_str(&format!("  {label}: {}\n", changed.join(", ")));
    }
    if !restart.is_empty() {
        out.push_str(&format!("  restart required: {}\n", restart.join(", ")));
    }
    if !plugins.is_empty() {
        out.push_str(&format!(
            "  restart required (plugin set): {}\n  restart bitty to apply an \
             installed-package change (a bundled id is ledger-only)\n",
            plugins.join(", ")
        ));
    }
    out
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
    // Portable uid: `/proc/self` metadata is Linux-only (absent on macOS,
    // where it fell back to 0 and failed endpoint verification against a
    // user-owned socket dir). `current_unix_uid` is per-platform
    // (rustix on Linux, nix elsewhere).
    let uid = bitty_ipc::current_unix_uid().unwrap_or(0);
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
        CtlRequest::PanelList => {
            // CTX-1117: human table with the focused (current) marker; JSON
            // stays the machine contract (`--format json`). Fail-soft to raw
            // on an unexpected shape (never corrupt stdout).
            match format_panels_table(result_json) {
                Some(table) => out.push_str(&table),
                None => {
                    out.push_str(result_json);
                    out.push('\n');
                }
            }
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
        CtlRequest::WorkspaceList => {
            // CTX-1117 (issue #1900): human table marks the active row from
            // the server `active`/`active_id` (no new `current` verb; the
            // marker is the affordance). JSON stays the contract.
            match format_workspaces_table(result_json) {
                Some(table) => out.push_str(&table),
                None => {
                    out.push_str(result_json);
                    out.push('\n');
                }
            }
        }
        CtlRequest::WorkspaceNew => {
            out.push_str(&format!(
                "new workspace on {} — result: {result_json}\n",
                target.instance
            ));
        }
        CtlRequest::WorkspaceClose { workspace_id } => {
            out.push_str(&format!("closed {workspace_id} on {}\n", target.instance));
        }
        CtlRequest::WorkspaceFocus { workspace_id } => {
            out.push_str(&format!("focused {workspace_id} on {}\n", target.instance));
        }
        CtlRequest::WorkspaceMove { workspace_id } => {
            out.push_str(&format!(
                "moved focused window to {workspace_id} on {}\n",
                target.instance
            ));
        }
        CtlRequest::WorkspaceRename { workspace_id, name } => {
            out.push_str(&format!(
                "renamed {workspace_id} to {name} on {}\n",
                target.instance
            ));
        }
        CtlRequest::WorkspaceMovePanel { position } => {
            out.push_str(&format!(
                "moved focused panel to position {position} on {}\n",
                target.instance
            ));
        }
        CtlRequest::ConfigReload => {
            // CTX-0814/#1397 + bitty#2927: an instance with a live-reload
            // context applies the change; without one (headless tests,
            // `--safe`) the reply stays probe-only. Never present a probe as
            // an applied reload.
            if extract_bool(result_json, "reloaded") == Some(true) {
                let kind = extract_string_from(result_json, "kind").unwrap_or_default();
                out.push_str(&format!("config {kind} on {}", target.instance));
                if let Some(version) = extract_number(result_json, "plan_version") {
                    out.push_str(&format!(" — plan v{version}"));
                }
                out.push('\n');
                let changed = extract_string_array(result_json, "changed").unwrap_or_default();
                if !changed.is_empty() {
                    out.push_str(&format!("  changed: {}\n", changed.join(", ")));
                }
                let restart =
                    extract_string_array(result_json, "restart_required").unwrap_or_default();
                if !restart.is_empty() {
                    out.push_str(&format!("  restart required: {}\n", restart.join(", ")));
                }
                // bitty#2969: the plugin set is a separate resource; a
                // pending enable/disable is named here too, so the registry
                // verb is never a silent no-op either.
                let plugins = extract_string_array(result_json, "plugins_restart_required")
                    .unwrap_or_default();
                if !plugins.is_empty() {
                    out.push_str(&format!(
                        "  restart required (plugin set): {}\n",
                        plugins.join(", ")
                    ));
                }
                let rejected = extract_string_array(result_json, "rejected").unwrap_or_default();
                if !rejected.is_empty() {
                    out.push_str(&format!("  rejected: {}\n", rejected.join(", ")));
                }
            } else {
                out.push_str(&format!(
                    "config probe on {} — result: {result_json}\n",
                    target.instance
                ));
                out.push_str(
                    "(probe-only: this instance has no live-reload context, e.g. --safe; \
                     the file was resolved, not applied)\n",
                );
            }
        }
        CtlRequest::InstanceList => {
            out.push_str(result_json);
            out.push('\n');
        }
        // Consent receipts are the audit trail (issue #1520): print the
        // server receipt verbatim. A granted bearer appears here exactly
        // once for the consenting caller and is never logged elsewhere.
        CtlRequest::ConsentGrant { .. }
        | CtlRequest::ConsentRevoke { .. }
        | CtlRequest::ConsentRevokeSession => {
            out.push_str(result_json);
            out.push('\n');
        }
    }
    out
}

// ── human tables with current markers (CTX-1117, issue #1900) ─────────────
//
// Tables are human, never a contract (`--format json` stays the machine
// surface). All parsers are total and fail-soft to `None` (the caller falls
// back to raw JSON) so an unexpected shape never corrupts stdout. No new
// `current` verbs: the active/focused row *is* the affordance (documented in
// the PR body to keep scope tight).

/// Render `workspace list` server JSON as a table marking the active row.
///
/// Server shape: `{"workspaces":[...],"names":[...],"active":N,
/// `"active_id":"ws:M","count":C}`. Marks via `active_id` (stable `ws:N`
/// identity); falls back to the 1-based `active` index when `active_id` is
/// absent. Returns `None` on an unexpected shape.
pub(crate) fn format_workspaces_table(result_json: &str) -> Option<String> {
    let workspaces = extract_string_array(result_json, "workspaces")?;
    let names = extract_string_array(result_json, "names").unwrap_or_default();
    let active_id = extract_string_from(result_json, "active_id");
    let active_index = extract_number(result_json, "active");
    let mut out = String::from("WORKSPACE NAME ACTIVE\n");
    if workspaces.is_empty() {
        out.push_str("(no workspaces)\n");
        return Some(out);
    }
    for (idx, ws) in workspaces.iter().enumerate() {
        let name = names.get(idx).map(String::as_str).unwrap_or("-");
        let is_active = match active_id.as_deref() {
            Some(id) => ws == id,
            None => match active_index {
                Some(n) => (idx as u64) + 1 == n,
                None => false,
            },
        };
        out.push_str(&format!(
            "{} {} {}\n",
            ws,
            display_name(name),
            if is_active { "yes" } else { "no" }
        ));
    }
    Some(out)
}

/// One parsed `panel list` row for table rendering.
struct PanelRow {
    id: String,
    view: String,
    bounds: String,
    role: String,
    focused: bool,
}

/// Render `panel list` server JSON as a table marking the focused (current)
/// row.
///
/// Server shape: `{"panels":[{"id":"p:N","view":"v:N",
/// `"bounds":{"x":X,"y":Y,"width":W,"height":H},"role":"tiled",
/// `"focused":bool}]}`. `focused` is the current-panel marker (mirrors
/// `view list`; no new `current` verb). Returns `None` on an unexpected
/// shape.
pub(crate) fn format_panels_table(result_json: &str) -> Option<String> {
    let rows = parse_panels(result_json)?;
    let mut out = String::from("PANEL VIEW BOUNDS ROLE FOCUSED\n");
    if rows.is_empty() {
        out.push_str("(no panels)\n");
        return Some(out);
    }
    for row in &rows {
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            row.id,
            row.view,
            row.bounds,
            row.role,
            if row.focused { "yes" } else { "no" }
        ));
    }
    Some(out)
}

/// Display-safe workspace name for table output (plain, no quoting).
///
/// Names are display state only (renames are bounded server-side); blank
/// names render as `-` so the column never collapses.
fn display_name(raw: &str) -> &str {
    if raw.trim().is_empty() { "-" } else { raw }
}

/// Extract a top-level `"key": [...]` array of JSON strings as Rust strings.
///
/// Quote-aware, handles `\"`/`\\`/`\n`/`\r`/`\t`/`\uXXXX` via the same
/// unescape as [`extract_string_from`]; returns `None` when the key or a
/// well-formed array is absent.
fn extract_string_array(text: &str, key: &str) -> Option<Vec<String>> {
    let array_body = extract_array_body(text, key)?;
    let mut items = Vec::new();
    let bytes = array_body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Skip whitespace/commas.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'"' {
            return None;
        }
        let (literal, next) = scan_string_literal(&array_body, i)?;
        let unescaped = unescape_literal(&literal)?;
        items.push(unescaped);
        i = next;
    }
    Some(items)
}

/// Locate the raw value slice immediately after `"key":` at the top level of a
/// JSON object (either a `{...}` object or an already-unwrapped object body).
///
/// Quote- and depth-aware: keys inside string values or nested `{}`/`[]`
/// containers are ignored.
///
/// Duplicate top-level `key` entries are refused (`None`): the two scanners in
/// this workspace (here and `plugin_ipc::top_key`) both fail closed rather
/// than pick a winner, because a first-wins reader facing a last-wins parser
/// is exactly the ambiguity a hostile payload needs.
fn find_top_level_key_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(trimmed);
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut expect_key = true;
    let mut found: Option<&'a str> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let (literal, next) = scan_string_literal(body, i)?;
                if depth == 0 && expect_key {
                    let after = body[next..].trim_start();
                    if let Some(rest) = after.strip_prefix(':') {
                        if unescape_literal(&literal).as_deref() == Some(key) {
                            if found.is_some() {
                                return None;
                            }
                            found = Some(rest.trim_start());
                        }
                        expect_key = false;
                        i = body.len() - rest.len();
                        continue;
                    }
                }
                i = next;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' if depth == 0 => {
                expect_key = true;
                i += 1;
            }
            b':' if depth == 0 => {
                expect_key = false;
                i += 1;
            }
            _ => {
                let ch = body[i..].chars().next()?;
                i += ch.len_utf8();
            }
        }
    }
    found
}

/// Whether `rest` begins at a top-level JSON value delimiter (end, whitespace,
/// `,`, or `}`), so an identifier or float prefix cannot be read as the value.
fn at_json_delimiter(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}')
}

/// Extract a top-level `"key": <number>` as `u64` (for `active`, bounds).
///
/// Non-digit suffixes are refused, so `1.5`, `1e2`, and `1abc` are absent
/// rather than silently truncated to their integer prefix.
fn extract_number(text: &str, key: &str) -> Option<u64> {
    let rest = find_top_level_key_value(text, key)?;
    if rest.starts_with('"') || rest.starts_with('{') || rest.starts_with('[') {
        return None;
    }
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 || digits_len > 20 || !at_json_delimiter(&rest[digits_len..]) {
        return None;
    }
    rest[..digits_len].parse::<u64>().ok()
}

/// Extract a top-level `"key": true|false` as `bool`.
fn extract_bool(text: &str, key: &str) -> Option<bool> {
    let rest = find_top_level_key_value(text, key)?;
    if let Some(after) = rest.strip_prefix("true") {
        if after.is_empty()
            || after.starts_with(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}')
        {
            return Some(true);
        }
    }
    if let Some(after) = rest.strip_prefix("false") {
        if after.is_empty()
            || after.starts_with(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}')
        {
            return Some(false);
        }
    }
    None
}

/// Extract the raw inside of a top-level `"key": [...]` array (without the
/// outer brackets), balanced and quote-aware. Returns `None` when absent.
fn extract_array_body(text: &str, key: &str) -> Option<String> {
    let rest = find_top_level_key_value(text, key)?;
    if !rest.starts_with('[') {
        return None;
    }
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
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[1..idx].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Scan one JSON string literal starting at `start` (which must be `"`).
/// Returns the literal including quotes plus the next byte index.
fn scan_string_literal(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                return Some((text[start..i + 1].to_string(), i + 1));
            }
            b'\\' => {
                let escaped = text.get(i + 1..)?.chars().next()?;
                i += 1 + escaped.len_utf8();
            }
            _ => {
                let ch = text[i..].chars().next()?;
                i += ch.len_utf8();
            }
        }
    }
    None
}

/// Unescape one JSON string literal (including surrounding quotes).
fn unescape_literal(literal: &str) -> Option<String> {
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
            '/' => out.push('/'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000c}'),
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

/// Parse the `panels` array from `panel list` server JSON into rows.
fn parse_panels(result_json: &str) -> Option<Vec<PanelRow>> {
    let body = extract_array_body(result_json, "panels")?;
    let objects = split_objects(&body)?;
    let mut rows = Vec::with_capacity(objects.len());
    for obj in objects {
        let id = extract_string_from(&obj, "id")?;
        let view = extract_string_from(&obj, "view").unwrap_or_else(|| String::from("-"));
        let role = extract_string_from(&obj, "role").unwrap_or_else(|| String::from("tiled"));
        let focused = extract_bool(&obj, "focused").unwrap_or(false);
        let bounds = match extract_object_body(&obj, "bounds") {
            Some(bounds_obj) => {
                let x = extract_number(&bounds_obj, "x").unwrap_or(0);
                let y = extract_number(&bounds_obj, "y").unwrap_or(0);
                let width = extract_number(&bounds_obj, "width").unwrap_or(0);
                let height = extract_number(&bounds_obj, "height").unwrap_or(0);
                format!("{width}x{height}+{x}+{y}")
            }
            None => String::from("-"),
        };
        rows.push(PanelRow {
            id,
            view,
            bounds,
            role,
            focused,
        });
    }
    Some(rows)
}

/// Split an array body into its top-level `{...}` objects (quote-aware).
fn split_objects(body: &str) -> Option<Vec<String>> {
    let mut objects = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut start: Option<usize> = None;
    for (idx, ch) in body.char_indices() {
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
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    let s = start?;
                    objects.push(body[s..idx + ch.len_utf8()].to_string());
                    start = None;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    Some(objects)
}

/// Extract the raw inside of `"key": {...}` (without outer braces).
fn extract_object_body(text: &str, key: &str) -> Option<String> {
    let rest = find_top_level_key_value(text, key)?;
    if !rest.starts_with('{') {
        return None;
    }
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
                    return Some(rest[1..idx].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{reload_exit, render_reload_table};

    #[test]
    fn pending_plugin_set_renders_and_exits_restart() {
        // bitty#2969: the plugin-set change is named in the human table and
        // never reads as a successful reload (exit 4, not 0).
        let table = render_reload_table(
            "restart-required",
            false,
            Some(1),
            super::ReloadTableChanges {
                changed: &[],
                restart: &[],
                plugins: &[String::from("xuepoo.fixture")],
            },
            "(defaults; no file)",
            "dev",
        );
        assert!(
            table.contains("restart required (plugin set): xuepoo.fixture"),
            "{table}"
        );
        assert!(table.contains("restart bitty to apply"), "{table}");
        assert!(
            table.contains("a bundled id is ledger-only"),
            "the table must not promise a restart applies a bundled id: {table}"
        );
        assert_eq!(
            reload_exit(&[String::from("xuepoo.fixture")], false),
            4,
            "a pending plugin set exits the literal restart code"
        );
        assert_eq!(
            reload_exit(&[String::from("xuepoo.fixture")], true),
            super::EXIT_OK,
            "a dry run only reports and keeps exit 0"
        );
        assert_eq!(reload_exit(&[], false), super::EXIT_OK);
        assert_eq!(crate::reload::EXIT_RESTART, 4);
    }

    #[test]
    fn json_extractors_handle_escaped_multibyte_utf8_and_quoted_keys_3075() {
        // A backslash followed by a multi-byte UTF-8 scalar must not leave the
        // scanner on a non-char byte boundary and panic.
        assert_eq!(
            super::scan_string_literal("\"\\é\"", 0),
            Some(("\"\\é\"".to_string(), 5))
        );
        assert_eq!(super::scan_string_literal("\"\\", 0), None);
        assert_eq!(
            super::unescape_literal("\"a\\/b\\bc\\fd\""),
            Some("a/b\u{0008}c\u{000c}d".to_string())
        );

        // Keys embedded inside earlier string values must not shadow real
        // top-level keys.
        let ws_json = r#"{"names":["\"active\":2,\"workspaces\":[\"ws:99\"]"],"workspaces":["ws:1","ws:2"],"active":1,"active_id":"ws:1"}"#;
        let table = super::format_workspaces_table(ws_json).expect("workspace table");
        assert!(
            table.contains("ws:1 \"active\":2,\"workspaces\":[\"ws:99\"] yes"),
            "{table}"
        );
        assert!(table.contains("ws:2 - no"), "{table}");
        assert!(!table.contains("\nws:99 "), "{table}");

        let reload_json = r#"{"message":"\"reloaded\":true,\"plan_version\":99","reloaded":false,"plan_version":3}"#;
        assert_eq!(super::extract_bool(reload_json, "reloaded"), Some(false));
        assert_eq!(super::extract_number(reload_json, "plan_version"), Some(3));
    }

    #[test]
    fn json_extractors_ignore_nested_same_name_keys_3075() {
        // The input a raw `text.find("\"active\":")` reader gets wrong: a
        // nested object carries the same key names, with different values,
        // *before* the real top-level entries.
        let json = r#"{"outer":{"active":99,"workspaces":["ws:99"]},"names":["main","logs"],"workspaces":["ws:1","ws:2"],"active":1,"active_id":"ws:1"}"#;
        assert_eq!(super::extract_number(json, "active"), Some(1));
        let table = super::format_workspaces_table(json).expect("workspace table");
        assert!(table.contains("\nws:1 main yes"), "{table}");
        assert!(table.contains("\nws:2 logs no"), "{table}");
        assert!(
            !table.contains("ws:99"),
            "a nested array must not be read as the top-level one: {table}"
        );

        // Same shape for the string/bool scanners.
        let reload_json =
            r#"{"meta":{"reloaded":true,"plan_version":99},"reloaded":false,"plan_version":3}"#;
        assert_eq!(super::extract_bool(reload_json, "reloaded"), Some(false));
        assert_eq!(super::extract_number(reload_json, "plan_version"), Some(3));

        // Duplicate top-level keys are ambiguous across layers, so both
        // scanners refuse them instead of picking a winner.
        assert!(
            super::format_workspaces_table(r#"{"workspaces":["ws:1"],"workspaces":["ws:9"]}"#)
                .is_none(),
            "a duplicated top-level array must fail closed"
        );
        assert_eq!(
            super::extract_number(r#"{"active":1,"active":2}"#, "active"),
            None
        );
        assert_eq!(
            super::extract_bool(r#"{"reloaded":true,"reloaded":false}"#, "reloaded"),
            None
        );

        // Numeric-looking prefixes are refused, not truncated to the digits.
        assert_eq!(super::extract_number(r#"{"active":1.5}"#, "active"), None);
        assert_eq!(super::extract_number(r#"{"active":1e2}"#, "active"), None);
        assert_eq!(super::extract_number(r#"{"active":1abc}"#, "active"), None);
        assert_eq!(super::extract_number(r#"{"active":true}"#, "active"), None);
        assert_eq!(super::extract_number(r#"{"active":"1"}"#, "active"), None);
    }
}
