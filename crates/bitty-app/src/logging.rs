//! Stderr verbosity gating (`--verbose` / `--log-level` / `BITTY_LOG` / `RUST_LOG`).

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
/// Accepts bare levels (`debug`, `trace`, ...) and `RUST_LOG`-style filters
/// (`bitty=debug`, `info,bitty-app=trace`, `warn`). Scans case-insensitively
/// for the most verbose level named anywhere in the value so existing
/// `RUST_LOG=debug` / `RUST_LOG=trace` habits keep working without a second
/// system. Returns `None` when no known level appears.
pub(crate) fn log_level_from_env_value(value: &str) -> Option<LogLevel> {
    let lower = value.to_lowercase();
    if lower.contains("trace") {
        Some(LogLevel::Trace)
    } else if lower.contains("debug") {
        Some(LogLevel::Debug)
    } else if lower.contains("info") {
        Some(LogLevel::Info)
    } else if lower.contains("warn") {
        Some(LogLevel::Warn)
    } else if lower.contains("error") {
        Some(LogLevel::Error)
    } else {
        None
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
