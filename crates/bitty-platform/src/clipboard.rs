//! Clipboard primitives via `arboard` with headless fallback.
//!
//! Wayland-first clipboard + primary sync (CTX-0160, issue #260):
//!
//! - The `arboard` dependency enables the `wayland-data-control` feature (see
//!   `Cargo.toml`). On Linux, `arboard` selects the Wayland data-control
//!   backend when `WAYLAND_DISPLAY` is set and falls back to X11 otherwise;
//!   when neither display is reachable `arboard::Clipboard::new` fails and this
//!   module degrades to an in-memory buffer (fail-soft headless).
//! - Every `set_text` writes the regular clipboard selection **and**
//!   best-effort syncs the primary selection (middle-click) on Linux. Primary
//!   sync is fail-soft: a primary failure (e.g. a Wayland compositor without
//!   primary-selection support, which requires version 2+) never fails the
//!   overall write when the regular clipboard succeeded. The authoritative
//!   clipboard error is still surfaced to the caller.
//! - Primary writes on Wayland go through the `wl-copy --primary` CLI
//!   (CTX-0158 fix): `arboard`'s in-process fork-daemon for Wayland `copy`
//!   is unsound from bitty's multithreaded runtime — live proof showed the
//!   primary `set` returning `Ok` while `wl-paste --primary` stayed empty,
//!   so middle-click found nothing. `wl-copy` is single-threaded at fork
//!   time and serves reliably. When `wl-copy` is missing or fails, the write
//!   falls back to the `arboard` primary path, so behavior never regresses
//!   below the CTX-0160 contract.
//! - Reads are authoritative per selection: `get_text` reads the regular
//!   clipboard and surfaces `PlatformError::ClipboardOperation` on failure;
//!   `get_primary` reads the primary selection the same way. There is no
//!   silent cross-selection fallback, so read failures are visible instead of
//!   being swallowed. Callers that want best-effort use `get_text_lossy` /
//!   `get_primary_lossy`, which fall back to the in-memory buffers.
//! - The secondary selection is never used: it is unavailable on Wayland and
//!   returns an error there by design.
//!
//! Reference (DEC-0017: reference-first is a merge gate): Alacritty
//! `alacritty/src/clipboard.rs` (Wayland-first via
//! `RawDisplayHandle::Wayland` with
//! `wayland_clipboard::create_clipboards_from_external`, X11 fallback
//! otherwise) and Ghostty `src/terminal/clipboard.zig` (`Location::standard /
//! selection / primary`, text MIME union `isTextMime`, sync `Write`/`Read`
//! effects with capability-gated replies). Snapshots are read-only and
//! untrusted under `recording/references/` (never executed, never imported).
//! The sync direction here mirrors `wl-copy` / `wl-paste` (regular selection)
//! plus `wl-copy --primary` / `wl-paste --primary` (primary selection).
//!
//! This module wraps `arboard::Clipboard` behind an owned `Clipboard` type
//! that never panics on headless machines. When a display server is absent
//! (`arboard::Clipboard::new` fails) or an operation fails, the clipboard
//! falls back to an in-memory buffer so `bitty-runtime` selection copy/paste
//! remains headless-testable (`cargo test` on CI without X11/Wayland).
//!
//! The `Clipboard` is owned per `Runtime` instance; callers may also thread a
//! `Clipboard::new_headless()` that never touches the system clipboard, so
//! tests stay deterministic even on machines that do have a display.
//!
//! Security: `arboard` clipboard access is gated behind this seam so the
//! runtime can enforce `clipboard.read` / `clipboard.write` separately from
//! the platform primitive. This file itself never grants ambient access.

#![forbid(unsafe_code)]

use crate::error::PlatformError;

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
use arboard::{ClearExtLinux, GetExtLinux, LinuxClipboardKind, SetExtLinux};

/// Maximum bytes allowed for a clipboard payload (mirrors
/// `BoundedBytes::MAX_LEN` / `CLIPBOARD_MAX_PAYLOAD_BYTES` = 4096 plus a
/// small slack for paste verification). Enforced before `arboard` calls so
/// unbounded paste data cannot grow the heap without limit (T-01).
pub const CLIPBOARD_MAX_BYTES: usize = 8192;

/// Whether a Wayland display is advertised via `WAYLAND_DISPLAY`.
///
/// This is the same signal `arboard` (with `wayland-data-control`) uses to
/// prefer the Wayland data-control backend over X11. It is a hint, not a
/// guarantee: `arboard` still falls back to X11 when the Wayland connection
/// fails, and headless handles report their own state via [`Clipboard::is_headless`].
#[must_use]
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Human-readable hint for the display backend `arboard` will prefer.
///
/// Returns `"wayland"` when [`is_wayland_session`] holds, `"x11"` otherwise.
/// This describes backend preference, not the live connection: a Wayland hint
/// with an unreachable compositor still falls back to X11 inside `arboard`,
/// and a missing display degrades to the headless buffer (see
/// [`Clipboard::backend_hint`]).
#[must_use]
pub fn display_backend_hint() -> &'static str {
    if is_wayland_session() {
        "wayland"
    } else {
        "x11"
    }
}

/// Owned clipboard handle with headless fallback.
///
/// On construction the inner `arboard::Clipboard` is attempted. If that
/// fails (headless CI, missing display server, permission error) the handle
/// degrades to an in-memory buffer: `set_text`/`get_text` operate on the
/// buffer and never return an error for that reason, so headless CI stays
/// green. When a system clipboard is available, operations are forwarded to
/// `arboard` and the buffer is kept in sync so `get_text` after a failed
/// system read can still return the last `set_text` value.
///
/// The primary selection (middle-click / `wl-paste --primary`) is tracked in
/// a second buffer and synced best-effort on Linux. See the module docs for
/// the exact sync and error contract.
pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
    headless_buf: String,
    primary_buf: String,
    headless_only: bool,
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard")
            .field("is_headless", &self.is_headless())
            .field("headless_len", &self.headless_buf.len())
            .field("primary_len", &self.primary_buf.len())
            .field("backend_hint", &self.backend_hint())
            .finish()
    }
}

impl Clipboard {
    /// Attempts to open the system clipboard, falling back to memory when
    /// no display server is present.
    ///
    /// This never returns an error: even when `arboard::Clipboard::new()`
    /// fails the returned handle is usable headlessly. Callers that need
    /// strict failure should use [`Self::new_strict`].
    ///
    /// Backend selection is Wayland-first on Linux: with the
    /// `wayland-data-control` feature, `arboard` uses the Wayland backend
    /// when `WAYLAND_DISPLAY` is set and falls back to X11 otherwise.
    #[must_use]
    pub fn new() -> Self {
        match arboard::Clipboard::new() {
            Ok(inner) => Self {
                inner: Some(inner),
                headless_buf: String::new(),
                primary_buf: String::new(),
                headless_only: false,
            },
            Err(_) => Self {
                inner: None,
                headless_buf: String::new(),
                primary_buf: String::new(),
                headless_only: false,
            },
        }
    }

    /// Like [`Self::new`] but fails openly when no display server exists.
    ///
    /// Exposed for callers that want to surface `ClipboardUnavailable` instead
    /// of degrading. The headless buffers are still initialized empty.
    pub fn new_strict() -> Result<Self, PlatformError> {
        match arboard::Clipboard::new() {
            Ok(inner) => Ok(Self {
                inner: Some(inner),
                headless_buf: String::new(),
                primary_buf: String::new(),
                headless_only: false,
            }),
            Err(err) => Err(PlatformError::ClipboardUnavailable(err.to_string())),
        }
    }

    /// Forced headless clipboard that never touches the OS (deterministic
    /// for unit/integration tests, even on live desktops).
    #[must_use]
    pub fn new_headless() -> Self {
        Self {
            inner: None,
            headless_buf: String::new(),
            primary_buf: String::new(),
            headless_only: true,
        }
    }

    /// Whether the clipboard is operating headlessly (no system handle).
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.inner.is_none() || self.headless_only
    }

    /// Backend preference hint for this handle.
    ///
    /// Returns `"headless"` when [`Self::is_headless`] holds, otherwise the
    /// process-wide [`display_backend_hint`]. As with that function, this is
    /// a preference hint: `arboard` may still fall back from Wayland to X11
    /// at connection time.
    #[must_use]
    pub fn backend_hint(&self) -> &'static str {
        if self.is_headless() {
            "headless"
        } else {
            display_backend_hint()
        }
    }

    /// Current contents of the headless regular-clipboard buffer (for tests).
    #[must_use]
    pub fn headless_contents(&self) -> &str {
        &self.headless_buf
    }

    /// Current contents of the headless primary-selection buffer (for tests).
    #[must_use]
    pub fn primary_contents(&self) -> &str {
        &self.primary_buf
    }

    /// Writes `text` to the regular clipboard and syncs the primary selection.
    ///
    /// `text` is truncated to [`CLIPBOARD_MAX_BYTES`] before any system call
    /// (T-01). When headless, both buffers are updated and `Ok` is returned.
    ///
    /// On Linux the primary selection is synced best-effort after the
    /// regular clipboard write: a primary failure never fails the call when
    /// the regular clipboard succeeded (Wayland primary selection requires
    /// compositor support and is optional). Both buffers are still updated so
    /// headless reads stay in sync.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the regular-clipboard write
    /// fails, the error is returned as `PlatformError::ClipboardOperation`
    /// but both buffers are still updated so future `get_text` remains
    /// possible. Callers that want best-effort should use
    /// [`Self::set_text_lossy`] and keep the buffers.
    pub fn set_text(&mut self, text: String) -> Result<(), PlatformError> {
        let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
        if self.headless_only {
            self.headless_buf = truncated.clone();
            self.primary_buf = truncated;
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            let clipboard_result = set_clipboard_text(inner, truncated.clone());
            // Best-effort primary sync (Linux only; no-op elsewhere).
            // Routed through `set_primary_selection` (wl-copy-first on
            // Wayland) so the sync benefits from the fork-safe CLI path.
            let _primary_result = set_primary_selection(inner, &truncated);
            match clipboard_result {
                Ok(()) => {
                    self.headless_buf = truncated.clone();
                    self.primary_buf = truncated;
                    Ok(())
                }
                Err(err) => {
                    self.headless_buf = truncated.clone();
                    self.primary_buf = truncated;
                    Err(PlatformError::ClipboardOperation(err))
                }
            }
        } else {
            self.headless_buf = truncated.clone();
            self.primary_buf = truncated;
            Ok(())
        }
    }

    /// Writes `text` to the primary selection only (middle-click /
    /// `wl-paste --primary`).
    ///
    /// Truncated to [`CLIPBOARD_MAX_BYTES`] before the system call. On
    /// Wayland the write prefers the `wl-copy --primary` CLI (fork-safe from
    /// multithreaded processes) and falls back to the `arboard` primary path
    /// when the CLI is missing or fails. On non-Linux platforms there is no
    /// primary selection: the primary buffer is updated headlessly and `Ok`
    /// is returned without touching the OS.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the primary write fails (e.g.
    /// Wayland compositor without primary-selection support), returns
    /// `PlatformError::ClipboardOperation`. The primary buffer is still
    /// updated.
    pub fn set_primary(&mut self, text: String) -> Result<(), PlatformError> {
        let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
        if self.headless_only {
            self.primary_buf = truncated;
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            match set_primary_selection(inner, &truncated) {
                Ok(()) => {
                    self.primary_buf = truncated;
                    Ok(())
                }
                Err(err) => {
                    self.primary_buf = truncated;
                    Err(PlatformError::ClipboardOperation(err))
                }
            }
        } else {
            self.primary_buf = truncated;
            Ok(())
        }
    }

    /// Best-effort write that never returns an error: updates both buffers
    /// and attempts the system clipboard (+ primary sync), dropping any
    /// system error.
    pub fn set_text_lossy(&mut self, text: String) {
        let _ = self.set_text(text);
    }

    /// Reads text from the regular clipboard, truncating to
    /// [`CLIPBOARD_MAX_BYTES`] after the system call. When headless, returns
    /// the buffer contents.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the read fails, returns
    /// `PlatformError::ClipboardOperation`. Headless `get_text` never fails.
    /// There is no silent fallback to the primary selection: use
    /// [`Self::get_primary`] explicitly or [`Self::get_text_lossy`] for
    /// best-effort reads.
    pub fn get_text(&mut self) -> Result<String, PlatformError> {
        if self.headless_only {
            return Ok(self.headless_buf.clone());
        }
        if let Some(inner) = self.inner.as_mut() {
            match get_clipboard_text(inner) {
                Ok(text) => {
                    let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
                    self.headless_buf = truncated.clone();
                    Ok(truncated)
                }
                Err(err) => Err(PlatformError::ClipboardOperation(err)),
            }
        } else {
            Ok(self.headless_buf.clone())
        }
    }

    /// Reads text from the primary selection (middle-click /
    /// `wl-paste --primary`), truncating to [`CLIPBOARD_MAX_BYTES`].
    ///
    /// On non-Linux platforms returns the primary buffer without touching
    /// the OS. Headless `get_primary` never fails.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the primary read fails, returns
    /// `PlatformError::ClipboardOperation`.
    pub fn get_primary(&mut self) -> Result<String, PlatformError> {
        if self.headless_only {
            return Ok(self.primary_buf.clone());
        }
        if let Some(inner) = self.inner.as_mut() {
            match get_primary_text(inner) {
                Ok(text) => {
                    let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
                    self.primary_buf = truncated.clone();
                    Ok(truncated)
                }
                Err(err) => Err(PlatformError::ClipboardOperation(err)),
            }
        } else {
            Ok(self.primary_buf.clone())
        }
    }

    /// Best-effort regular-clipboard read that returns `Ok` even on system
    /// error: falls back to the headless buffer.
    #[must_use]
    pub fn get_text_lossy(&mut self) -> String {
        self.get_text()
            .unwrap_or_else(|_| self.headless_buf.clone())
    }

    /// Best-effort primary read that falls back to the primary buffer on
    /// system error.
    #[must_use]
    pub fn get_primary_lossy(&mut self) -> String {
        self.get_primary()
            .unwrap_or_else(|_| self.primary_buf.clone())
    }

    /// Clears both system and headless clipboard to empty string.
    ///
    /// Clears the regular clipboard and best-effort clears the primary
    /// selection on Linux. System errors are dropped to preserve the
    /// historical `clear()` signature; use [`Self::try_clear`] when the
    /// caller needs the failure surfaced.
    pub fn clear(&mut self) {
        let _ = self.try_clear();
    }

    /// Clears the regular clipboard and primary selection, surfacing the
    /// first system failure.
    ///
    /// Both headless buffers are always cleared. When a system clipboard is
    /// present, the regular clipboard is cleared first and its error (if any)
    /// is returned after still attempting the primary clear, so a primary
    /// failure cannot mask a clipboard failure and vice versa.
    ///
    /// # Errors
    ///
    /// Returns `PlatformError::ClipboardOperation` when a system clear fails.
    /// Headless clears never fail.
    pub fn try_clear(&mut self) -> Result<(), PlatformError> {
        self.headless_buf.clear();
        self.primary_buf.clear();
        if self.headless_only {
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            let clipboard_result = clear_clipboard_text(inner);
            let primary_result = clear_primary_text(inner);
            match (clipboard_result, primary_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(err), _) => Err(PlatformError::ClipboardOperation(err)),
                (Ok(()), Err(err)) => Err(PlatformError::ClipboardOperation(err)),
            }
        } else {
            Ok(())
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate_to_bytes(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn set_clipboard_text(inner: &mut arboard::Clipboard, text: String) -> Result<(), String> {
    inner
        .set()
        .clipboard(LinuxClipboardKind::Clipboard)
        .text(text)
        .map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn set_clipboard_text(inner: &mut arboard::Clipboard, text: String) -> Result<(), String> {
    inner.set_text(text).map_err(|err| err.to_string())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn set_primary_text(inner: &mut arboard::Clipboard, text: String) -> Result<(), String> {
    inner
        .set()
        .clipboard(LinuxClipboardKind::Primary)
        .text(text)
        .map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn set_primary_text(_inner: &mut arboard::Clipboard, _text: String) -> Result<(), String> {
    Ok(())
}

/// Primary-selection write with a fork-safe Wayland fast path (CTX-0158).
///
/// When a Wayland session is advertised, the write first tries the
/// `wl-copy --primary` CLI and only falls back to the `arboard` primary
/// path when the CLI is missing or fails, so behavior never regresses below
/// the CTX-0160 contract. Everywhere else (X11, headless-adjacent) this is
/// exactly the `arboard` primary write.
fn set_primary_selection(inner: &mut arboard::Clipboard, text: &str) -> Result<(), String> {
    if is_wayland_session() && wl_copy_primary(text).is_ok() {
        return Ok(());
    }
    set_primary_text(inner, text.to_owned())
}

/// Writes `text` to the Wayland primary selection via the `wl-copy` CLI.
///
/// Fixed argv (`wl-copy --primary -- <text>`), no shell, stdio nulled, and
/// the wait is bounded (2 s) so a wedged compositor cannot hang the caller;
/// on timeout the child is killed and reaped. Any failure (missing binary,
/// non-zero exit, timeout) is an `Err` string and the caller falls back to
/// `arboard`. `text` must already be truncated to [`CLIPBOARD_MAX_BYTES`].
fn wl_copy_primary(text: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const WL_COPY_WAIT: Duration = Duration::from_secs(2);
    const WL_COPY_POLL: Duration = Duration::from_millis(10);

    let mut child = Command::new("wl-copy")
        .arg("--primary")
        .arg("--")
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + WL_COPY_WAIT;
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("wl-copy --primary exited with {status}")),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("wl-copy --primary timed out".to_string());
            }
            None => std::thread::sleep(WL_COPY_POLL),
        }
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn get_clipboard_text(inner: &mut arboard::Clipboard) -> Result<String, String> {
    inner
        .get()
        .clipboard(LinuxClipboardKind::Clipboard)
        .text()
        .map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn get_clipboard_text(inner: &mut arboard::Clipboard) -> Result<String, String> {
    inner.get_text().map_err(|err| err.to_string())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn get_primary_text(inner: &mut arboard::Clipboard) -> Result<String, String> {
    inner
        .get()
        .clipboard(LinuxClipboardKind::Primary)
        .text()
        .map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn get_primary_text(_inner: &mut arboard::Clipboard) -> Result<String, String> {
    Err("primary selection is unavailable on this platform".to_string())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn clear_clipboard_text(inner: &mut arboard::Clipboard) -> Result<(), String> {
    inner.set_text(String::new()).map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn clear_clipboard_text(inner: &mut arboard::Clipboard) -> Result<(), String> {
    inner.set_text(String::new()).map_err(|err| err.to_string())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn clear_primary_text(inner: &mut arboard::Clipboard) -> Result<(), String> {
    inner
        .clear_with()
        .clipboard(LinuxClipboardKind::Primary)
        .map_err(|err| err.to_string())
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
)))]
fn clear_primary_text(_inner: &mut arboard::Clipboard) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_clipboard_roundtrip_is_deterministic() {
        let mut cb = Clipboard::new_headless();
        assert!(cb.is_headless());
        assert_eq!(cb.headless_contents(), "");
        cb.set_text("hello world".to_string())
            .expect("headless set must succeed");
        assert_eq!(cb.get_text().expect("headless get"), "hello world");
        assert_eq!(cb.headless_contents(), "hello world");
        cb.set_text("second".to_string()).expect("overwrite");
        assert_eq!(cb.get_text_lossy(), "second");
        cb.clear();
        assert_eq!(cb.get_text_lossy(), "");
    }

    #[test]
    fn headless_set_syncs_primary_buffer() {
        let mut cb = Clipboard::new_headless();
        cb.set_text("synced".to_string()).expect("set");
        assert_eq!(cb.headless_contents(), "synced");
        assert_eq!(cb.primary_contents(), "synced");
        assert_eq!(cb.get_primary().expect("primary get"), "synced");
        assert_eq!(cb.get_primary_lossy(), "synced");
    }

    #[test]
    fn headless_primary_roundtrip_is_independent() {
        let mut cb = Clipboard::new_headless();
        cb.set_text("clipboard".to_string()).expect("set clipboard");
        cb.set_primary("primary".to_string()).expect("set primary");
        assert_eq!(cb.get_text().expect("clipboard get"), "clipboard");
        assert_eq!(cb.get_primary().expect("primary get"), "primary");
        // Overwriting the clipboard re-syncs primary; explicit primary writes
        // do not clobber the regular clipboard buffer.
        cb.set_text("both".to_string()).expect("resync");
        assert_eq!(cb.headless_contents(), "both");
        assert_eq!(cb.primary_contents(), "both");
    }

    #[test]
    fn headless_clear_empties_both_selections() {
        let mut cb = Clipboard::new_headless();
        cb.set_text("data".to_string()).expect("set");
        cb.try_clear().expect("try_clear headless");
        assert_eq!(cb.headless_contents(), "");
        assert_eq!(cb.primary_contents(), "");
        cb.set_text("again".to_string()).expect("set again");
        cb.clear();
        assert_eq!(cb.get_text_lossy(), "");
        assert_eq!(cb.get_primary_lossy(), "");
    }

    #[test]
    fn headless_truncates_at_max_bytes_on_char_boundary() {
        let mut cb = Clipboard::new_headless();
        let long = "a".repeat(CLIPBOARD_MAX_BYTES + 100);
        cb.set_text(long).expect("headless set");
        assert_eq!(cb.headless_contents().len(), CLIPBOARD_MAX_BYTES);
        assert_eq!(cb.primary_contents().len(), CLIPBOARD_MAX_BYTES);
        // 4-byte emoji boundary
        let emoji = "😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 10);
        cb.set_text(emoji).expect("emoji set");
        assert!(cb.headless_contents().len() <= CLIPBOARD_MAX_BYTES);
        assert!(
            cb.headless_contents().len() % 4 == 0
                || cb.headless_contents().len() < CLIPBOARD_MAX_BYTES
        );
        cb.set_primary("😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 10))
            .expect("emoji primary");
        assert!(cb.primary_contents().len() <= CLIPBOARD_MAX_BYTES);
    }

    #[test]
    fn backend_hint_reflects_wayland_env() {
        let headless = Clipboard::new_headless();
        assert_eq!(headless.backend_hint(), "headless");
        assert_eq!(
            display_backend_hint(),
            if is_wayland_session() {
                "wayland"
            } else {
                "x11"
            }
        );
    }

    #[test]
    fn new_never_panics_even_without_display() {
        let _cb = Clipboard::new();
    }

    #[test]
    fn lossy_helpers_never_error() {
        let mut cb = Clipboard::new_headless();
        cb.set_text_lossy("lossy".to_string());
        assert_eq!(cb.get_text_lossy(), "lossy");
        assert_eq!(cb.get_primary_lossy(), "lossy");
    }
}
