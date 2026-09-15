//! Stderr verbosity gating (`--verbose` / `--log-level` / `BITTY_LOG` / `RUST_LOG`).

use std::sync::atomic::{AtomicU8, Ordering};

use crate::cli::Args;

/// Diagnostic verbosity for stderr logs (CTX-0190).
///
/// Ordering is `Error < Warn < Info < Debug < Trace`. The default is [`LogLevel::Warn`]
/// (quiet): warnings/errors plus user-facing key info (paste confirm/cancel,
/// startup summary) are always emitted; per-frame `bitty tick` stats sit at
/// `Debug`/`Trace` and require `--verbose` / `--log-level debug|trace`
/// (or `BITTY_LOG`/`RUST_LOG`). The devtools trace path (`Runtime::tick`
/// return + inspect snapshots) keeps full fidelity regardless of this gate —
/// only the stderr rendering is filtered, with the format guarded so the
/// disabled hot path pays just one comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors (quiet default).
    Warn,
    /// Key user-facing info plus warnings/errors.
    Info,
    /// Per-frame tick stats and other diagnostics.
    Debug,
    /// Full per-frame fidelity (same tick line as `Debug` today).
    Trace,
}

impl LogLevel {
    /// Quiet default: warnings/errors (plus unconditional key info).
    pub(crate) fn default_level() -> Self {
        Self::Warn
    }

    /// Parses `error|warn|warning|info|debug|trace|verbose` (case-insensitive,
    /// surrounding whitespace ignored). `verbose` maps to [`LogLevel::Debug`]
    /// so `--log-level verbose` behaves like `--verbose`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            "verbose" => Some(Self::Debug),
            _ => None,
        }
    }

    /// True when per-frame `bitty tick` stderr lines are emitted.
    ///
    /// Tick stats are `Debug`/`Trace` diagnostics: visible with `--verbose`
    /// (which resolves to [`LogLevel::Debug`]) or `--log-level debug|trace`.
    /// Pure for unit testing; the hot path calls this before formatting.
    pub(crate) fn tick_enabled(self) -> bool {
        self >= Self::Debug
    }
}

/// Derives a [`LogLevel`] from a `BITTY_LOG`/`RUST_LOG`-style value.
///
/// Accepts bare levels (`debug`, `trace`, ...) and `RUST_LOG`-style
/// comma-separated directives (`bitty=debug`, `info,bitty-app=trace`,
/// `warn`). Each directive is `[target=]level`: a level only counts when it
/// parses exactly, so a target name that merely contains a level word
/// (`mydebug=warn`, CTX-0482) never flips the gate. Bare levels set the
/// global candidate; directives for this app's own targets (`bitty*`) set
/// the more specific candidate and win over the global one. Directives for
/// other targets are ignored (this gate renders only bitty diagnostics).
/// Returns `None` when no applicable level appears.
pub(crate) fn log_level_from_env_value(value: &str) -> Option<LogLevel> {
    let mut global: Option<LogLevel> = None;
    let mut app: Option<LogLevel> = None;
    for directive in value.split(',') {
        let directive = directive.trim();
        if directive.is_empty() {
            continue;
        }
        let (target, level_text) = match directive.split_once('=') {
            Some((target, level)) => (target.trim(), level.trim()),
            None => ("", directive),
        };
        let Some(level) = LogLevel::parse(level_text) else {
            continue;
        };
        if target.is_empty() {
            global = Some(level);
        } else if target.starts_with("bitty") {
            app = Some(level);
        }
    }
    app.or(global)
}

// ── process-wide stderr gate (CTX-0482) ───────────────────────────────────

/// Installed once by `main` from [`effective_log_level`]; diagnostics on
/// non-args paths (spawn, plugin activation, IPC servo) read it so startup
/// lines stop bypassing `--log-level` without threading a level through
/// every helper signature.
static STDERR_GATE: AtomicU8 = AtomicU8::new(LogLevel::Warn as u8);

/// Installs the process-wide stderr gate (CTX-0482).
pub(crate) fn install_stderr_gate(level: LogLevel) {
    STDERR_GATE.store(level as u8, Ordering::Relaxed);
}

/// Current process-wide stderr gate (default [`LogLevel::Warn`]).
fn stderr_gate() -> LogLevel {
    match STDERR_GATE.load(Ordering::Relaxed) {
        0 => LogLevel::Error,
        2 => LogLevel::Info,
        3 => LogLevel::Debug,
        4 => LogLevel::Trace,
        // 1 and any future/unknown code: quiet default (fail-soft logging).
        _ => LogLevel::Warn,
    }
}

/// Emits an info-class diagnostic when the gate allows it (CTX-0482).
///
/// The closure keeps the disabled path allocation-free (same discipline as
/// the tick-line gate).
pub(crate) fn info(message: impl FnOnce() -> String) {
    if stderr_gate() >= LogLevel::Info {
        eprintln!("{}", message());
    }
}

/// Emits a warning when the gate allows it (CTX-0482).
///
/// Warnings sit at [`LogLevel::Warn`]: visible at the default and at
/// `info`/`debug`/`trace`, silenced only by an explicit `--log-level error`.
pub(crate) fn warn(message: impl FnOnce() -> String) {
    if stderr_gate() >= LogLevel::Warn {
        eprintln!("{}", message());
    }
}

/// Resolves the effective stderr log level for `args` (CTX-0190).
///
/// Precedence, highest first: `--log-level`, then `--verbose` / `-v` /
/// `BITTY_VERBOSE=1`, then `BITTY_LOG`, then `RUST_LOG`, then the quiet
/// default ([`LogLevel::Warn`]). Impure (reads env); total (unknown values
/// fall back to the next layer, never panics).
pub(crate) fn effective_log_level(args: &Args) -> LogLevel {
    if let Some(level) = args.log_level {
        return level;
    }
    if args.verbose {
        return LogLevel::Debug;
    }
    // Reuse the standard `RUST_LOG` habit instead of inventing a second
    // system; `BITTY_LOG` wins when both are set.
    // MSRV 1.85: no let-chains; nest instead of `if ... && let ...`.
    if std::env::var("BITTY_VERBOSE").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        return LogLevel::Debug;
    }
    if let Ok(value) = std::env::var("BITTY_LOG") {
        if let Some(level) = log_level_from_env_value(&value) {
            return level;
        }
    }
    if let Ok(value) = std::env::var("RUST_LOG") {
        if let Some(level) = log_level_from_env_value(&value) {
            return level;
        }
    }
    LogLevel::default_level()
}
