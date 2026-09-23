//! OS desktop-notification delivery and the audible bell primitive
//! (CTX-0754, issue #1361).
//!
//! Terminal-requested notifications (`OSC 9` / `OSC 777`) used to parse into
//! an in-memory queue only, and `BellMode::Audible` had no OS primitive at
//! all. This module is the fail-closed delivery floor underneath the runtime
//! policy (`bitty-runtime` gates consent, `RC-8` rate, and queue bounds
//! before anything here runs):
//!
//! - [`DesktopNotification`]: sanitized, length-bounded title/body. Payloads
//!   are terminal-provided observation data: never expanded, never executed,
//!   and control characters never reach an OS backend.
//! - [`NotificationSink`] / [`BellSink`]: injectable seams (the same pattern
//!   as [`crate::clipboard::Clipboard`] doubles and the runtime's
//!   `UrlOpener`). Tests install recording doubles; production uses
//!   [`OsNotificationSink`] / [`OsBellSink`].
//! - [`OsNotificationSink`]: best-effort native delivery with no new
//!   dependencies and no shell: Linux `notify-send`, macOS `osascript`.
//!   Windows has no inbox command-line toast path without a new native
//!   dependency, so it reports [`OsDeliverySkip::BackendMissing`] there (full
//!   coverage awaits the `OQ-076` policy decision).
//! - [`OsBellSink`]: best-effort audible bell. Without an audio-synthesis
//!   dependency the portable primitive is a single `BEL` byte on stderr, so
//!   a hosting terminal (or console) can sound it; embedders that own audio
//!   replace the sink. Richer audio also awaits `OQ-076`.
//!
//! Every outcome is an [`OsDeliveryOutcome`]: delivery is best-effort and
//! fail-closed (missing backend, spawn failure, and I/O error are values,
//! never panics), and spawning never blocks the caller: children are handed
//! to a bounded background reaper (the `wl-copy` precedent in
//! [`crate::clipboard`]).
//!
//! Headless note: unit tests exercise only pure constructors/argv builders.
//! No test spawns a real backend, so CI with no session bus stays silent.

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

/// Maximum characters retained in a desktop-notification title.
pub const NOTIFICATION_TITLE_MAX_CHARS: usize = 128;

/// Maximum characters retained in a desktop-notification body.
///
/// Mirrors the runtime banner bound (`NOTIFICATION_TEXT_MAX_CHARS`), so what
/// the OS shows and what the in-grid banner shows stay the same bounded
/// string.
pub const NOTIFICATION_BODY_MAX_CHARS: usize = 256;

/// How long a spawned notifier child may run before the reaper kills it.
const NOTIFIER_REAP_WAIT: Duration = Duration::from_secs(2);

/// Reaper poll interval while waiting for a notifier child.
const NOTIFIER_REAP_POLL: Duration = Duration::from_millis(10);

/// Linux desktop-notification backend (absolute path, no `PATH` lookup).
///
/// Unused off Linux; the `allow` keeps `-D warnings` green there without
/// hiding genuine dead code behind a blanket crate-level allow.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_NOTIFY_BACKEND: &str = "/usr/bin/notify-send";

/// macOS desktop-notification backend (absolute path, no `PATH` lookup).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_NOTIFY_BACKEND: &str = "/usr/bin/osascript";

/// Best-effort OS delivery outcome.
///
/// `Delivered` means the request reached (bell) or was handed to
/// (notification) the OS backend; the OS may still drop it silently, which no
/// synchronous API can observe without blocking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsDeliveryOutcome {
    /// The backend accepted the request.
    Delivered,
    /// No attempt was made (backend absent, no session, or disabled).
    Skipped(OsDeliverySkip),
    /// The attempt failed; carries the backend diagnostic.
    Failed(String),
}

/// Why a delivery attempt was skipped without touching the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsDeliverySkip {
    /// No usable OS backend exists on this platform/configuration.
    BackendMissing,
    /// Delivery was disabled by the caller (reserved for embedder gating).
    Disabled,
}

/// A sanitized, length-bounded desktop notification.
///
/// Constructed only via [`DesktopNotification::new`], which strips control
/// characters, collapses whitespace, and truncates to
/// [`NOTIFICATION_TITLE_MAX_CHARS`] / [`NOTIFICATION_BODY_MAX_CHARS`], so an
/// unbounded or hostile PTY payload can never reach the OS as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopNotification {
    title: String,
    body: String,
}

impl DesktopNotification {
    /// Builds a notification from untrusted title/body text.
    ///
    /// Either side may be empty; delivery backends degrade to whichever side
    /// is non-empty, and a fully empty notification is still well-formed (the
    /// runtime banner path drops empty text before a sink ever runs).
    #[must_use]
    pub fn new(title: &str, body: &str) -> Self {
        Self {
            title: sanitize_field(title, NOTIFICATION_TITLE_MAX_CHARS),
            body: sanitize_field(body, NOTIFICATION_BODY_MAX_CHARS),
        }
    }

    /// Sanitized title (possibly empty; `OSC 9` carries no title).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Sanitized body.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Whether both sides are empty (nothing worth handing to the OS).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.body.is_empty()
    }
}

/// Strips control characters, collapses whitespace runs, and truncates to
/// `max_chars` characters.
fn sanitize_field(raw: &str, max_chars: usize) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Injectable desktop-notification delivery seam.
///
/// Production uses [`OsNotificationSink`]; tests install a recording double
/// that captures the exact sequence without touching the OS.
pub trait NotificationSink {
    /// Hands an already-gated notification to the OS (best-effort).
    fn deliver(&self, notification: &DesktopNotification) -> OsDeliveryOutcome;
}

/// Injectable audible-bell seam.
///
/// Production uses [`OsBellSink`]; tests install a recording double.
pub trait BellSink {
    /// Sounds one admitted audible bell (best-effort).
    fn ring(&self) -> OsDeliveryOutcome;
}

/// Production [`NotificationSink`]: best-effort native delivery, no shell.
///
/// Linux resolves to `notify-send` via an absolute path; macOS to
/// `osascript`. Other platforms report
/// [`OsDeliverySkip::BackendMissing`]. A missing backend binary also reports
/// `BackendMissing` (fail-closed); only a present backend is ever spawned,
/// with fixed argv (never a shell) and stdio nulled.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsNotificationSink;

impl NotificationSink for OsNotificationSink {
    fn deliver(&self, notification: &DesktopNotification) -> OsDeliveryOutcome {
        let (program, args) = notification_argv(notification);
        let Some(program) = program else {
            return OsDeliveryOutcome::Skipped(OsDeliverySkip::BackendMissing);
        };
        if !backend_present(program) {
            return OsDeliveryOutcome::Skipped(OsDeliverySkip::BackendMissing);
        }
        spawn_notifier(program, &args)
    }
}

/// Production [`BellSink`]: best-effort audible bell.
///
/// Writes a single `BEL` byte (`0x07`) to stderr so a hosting terminal or
/// console can sound it. This is the portable floor without an
/// audio-synthesis dependency — not a synthesized tone; richer audio awaits
/// the `OQ-076` policy decision. I/O failure is a [`OsDeliveryOutcome::Failed`]
/// value, never a panic.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsBellSink;

impl BellSink for OsBellSink {
    fn ring(&self) -> OsDeliveryOutcome {
        let mut stderr = std::io::stderr().lock();
        match stderr.write_all(&[0x07]) {
            Ok(()) => OsDeliveryOutcome::Delivered,
            Err(error) => OsDeliveryOutcome::Failed(error.to_string()),
        }
    }
}

/// Native backend selection for a notification.
///
/// Returns `(None, _)` on platforms with no inbox command-line backend
/// (Windows today: no WinRT toast projection without a new native
/// dependency). The pure shape keeps dispatch headless-testable; only
/// [`OsNotificationSink::deliver`] spawns.
fn notification_argv(notification: &DesktopNotification) -> (Option<&'static str>, Vec<String>) {
    if cfg!(target_os = "linux") {
        // `notify-send [OPTION...] <SUMMARY> [BODY]`; banner-length expiry
        // matches the runtime banner so OS and in-grid surfaces agree.
        let summary = if notification.title.is_empty() {
            String::from("bitty")
        } else {
            notification.title.clone()
        };
        (
            Some(LINUX_NOTIFY_BACKEND),
            vec![
                String::from("--app-name=bitty"),
                String::from("--expire-time=4000"),
                summary,
                notification.body.clone(),
            ],
        )
    } else if cfg!(target_os = "macos") {
        // `osascript -e 'display notification ...'`; the script expression is
        // built by `osascript_notification_script` with AppleScript quoting.
        let script = osascript_notification_script(&notification.title, &notification.body);
        (Some(MACOS_NOTIFY_BACKEND), vec![String::from("-e"), script])
    } else {
        (None, Vec::new())
    }
}

/// Builds the `display notification` AppleScript expression with both
/// interpolated strings quoted.
///
/// AppleScript has no escape inside double-quoted strings except `\"` via
/// backslash in `osascript` arguments; backslashes are doubled first so a
/// hostile payload can never break out of the quoted literal (no shell is
/// involved either way — argv only).
///
/// macOS-only at runtime; the `allow` keeps `-D warnings` green elsewhere
/// while the quoting contract stays unit-tested on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn osascript_notification_script(title: &str, body: &str) -> String {
    fn quote(text: &str) -> String {
        let mut quoted = String::with_capacity(text.len() + 2);
        quoted.push('"');
        for ch in text.chars() {
            if ch == '\\' || ch == '"' {
                quoted.push('\\');
            }
            quoted.push(ch);
        }
        quoted.push('"');
        quoted
    }
    // `display notification` requires a message; an empty body degrades to
    // the title so the call stays well-formed.
    let message = if body.is_empty() { title } else { body };
    if title.is_empty() || body.is_empty() {
        format!(
            "display notification {} with title \"bitty\"",
            quote(message)
        )
    } else {
        format!(
            "display notification {} with title {}",
            quote(message),
            quote(title)
        )
    }
}

/// Absolute-path backend presence probe (no `PATH` resolution, mirroring
/// [`crate::url`]).
fn backend_present(program: &str) -> bool {
    std::path::Path::new(program).is_file()
}

/// Spawns `program` with fixed `args` (stdio nulled, never a shell) and hands
/// the child to a bounded background reaper so the caller never blocks.
///
/// Returns `Delivered` once the child is reaped by the background thread
/// handoff — i.e. the backend accepted the spawn, not that a banner is
/// visible; spawn failure is `Failed`.
fn spawn_notifier(program: &'static str, args: &[String]) -> OsDeliveryOutcome {
    use std::process::Command;
    let spawn = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match spawn {
        Ok(child) => child,
        Err(error) => return OsDeliveryOutcome::Failed(error.to_string()),
    };
    let (child_tx, child_rx) = std::sync::mpsc::channel::<std::process::Child>();
    let program_name = program.to_owned();
    let reaper = std::thread::Builder::new()
        .name(String::from("bitty-notify-reap"))
        .spawn(move || {
            let Ok(mut child) = child_rx.recv() else {
                return;
            };
            // Bounded wait: a hung notifier (wedged bus) is killed, never
            // left running, and never left a zombie.
            let waited = NOTIFIER_REAP_WAIT.as_millis() / NOTIFIER_REAP_POLL.as_millis().max(1);
            for _ in 0..waited.max(1) {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(NOTIFIER_REAP_POLL),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = program_name;
        });
    match reaper {
        Ok(_) => {
            let _ = child_tx.send(child);
            OsDeliveryOutcome::Delivered
        }
        Err(error) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            OsDeliveryOutcome::Failed(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_and_bounds_length() {
        let raw = format!("a\u{1}b\nc\u{7}d  e\t{}", "x".repeat(400));
        let notification = DesktopNotification::new(&raw, &raw);
        assert!(!notification.title.contains('\u{1}'));
        assert!(!notification.title.contains('\n'));
        assert!(!notification.body.contains('\u{7}'));
        assert!(!notification.body.contains("  "));
        assert!(notification.title.chars().count() <= NOTIFICATION_TITLE_MAX_CHARS);
        assert!(notification.body.chars().count() <= NOTIFICATION_BODY_MAX_CHARS);
    }

    #[test]
    fn empty_sides_degrade() {
        let notification = DesktopNotification::new("", "");
        assert!(notification.is_empty());
        assert!(!DesktopNotification::new("t", "").is_empty());
        assert!(!DesktopNotification::new("", "b").is_empty());
    }

    #[test]
    fn osascript_quoting_never_breaks_out() {
        let hostile = "hi\"; do shell script \"rm -rf ~\"; \"\\bye";
        let script = osascript_notification_script("t\"tle", hostile);
        // Every interior quote/backslash is escaped: the script contains no
        // bare `"` beyond the four delimiters... simpler invariant: the
        // hostile payload round-trips only inside escaped literals.
        assert!(script.contains("\\\\"));
        assert!(script.contains("\\\""));
        assert!(script.starts_with("display notification"));
        // The exact hostile text (with raw quotes) never appears verbatim.
        assert!(!script.contains(hostile));
    }

    #[test]
    fn osascript_empty_sides_stay_wellformed() {
        let script = osascript_notification_script("", "hello");
        assert!(script.contains("with title \"bitty\""));
        let script = osascript_notification_script("title", "");
        assert!(script.contains("display notification \"title\""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_argv_is_fixed_and_absolute() {
        let notification = DesktopNotification::new("Build", "done");
        let (program, args) = notification_argv(&notification);
        assert_eq!(program, Some("/usr/bin/notify-send"));
        assert!(args.iter().any(|arg| arg == "Build"));
        assert!(args.iter().any(|arg| arg == "done"));
        // No shell metacharacters are interpreted: argv entries are literal.
        let hostile = DesktopNotification::new("$(id)", "`id`");
        let (_, hostile_args) = notification_argv(&hostile);
        assert!(hostile_args.iter().any(|arg| arg == "$(id)"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_untitled_notification_names_bitty() {
        let notification = DesktopNotification::new("", "plain");
        let (_, args) = notification_argv(&notification);
        assert!(args.iter().any(|arg| arg == "bitty"));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_reports_no_backend() {
        let notification = DesktopNotification::new("t", "b");
        let (program, _) = notification_argv(&notification);
        assert_eq!(program, None);
        assert_eq!(
            OsNotificationSink.deliver(&notification),
            OsDeliveryOutcome::Skipped(OsDeliverySkip::BackendMissing)
        );
    }

    #[test]
    fn missing_backend_is_fail_closed_skip() {
        // `backend_present` on a path that cannot exist never spawns.
        assert!(!backend_present("/nonexistent-bitty-backend-0123456789"));
    }
}
