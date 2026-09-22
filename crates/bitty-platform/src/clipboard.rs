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
//!   time and serves reliably. The payload is written to the child's stdin
//!   pipe — never argv, which any same-UID process can read through
//!   `/proc/<pid>/cmdline` — and the child is reaped by a background thread
//!   with a bounded wait, so a wedged compositor cannot stall the caller
//!   (CTX-0388). When `wl-copy` cannot be started or fed, the write falls
//!   back to the `arboard` primary path, so behavior never regresses below
//!   the CTX-0160 contract.
//! - Reads are authoritative per selection: `get_text` reads the regular
//!   clipboard and surfaces `PlatformError::ClipboardOperation` on failure;
//!   `get_primary` reads the primary selection the same way. There is no
//!   silent cross-selection fallback, so read failures are visible instead of
//!   being swallowed. Callers that want best-effort use `get_text_lossy` /
//!   `get_primary_lossy`, which return an empty string when the system read
//!   fails rather than replaying a stale in-memory value.
//! - Payloads are bounded by [`CLIPBOARD_MAX_BYTES`] without silent
//!   truncation: an over-limit write, or an over-limit system read through
//!   the direct [`Clipboard::get_text`] / [`Clipboard::get_primary`] APIs,
//!   fails with `PlatformError::ClipboardPayloadTooLarge`, and a rejected
//!   write leaves both selections unchanged (CTX-0478). The bounded reads
//!   [`Clipboard::get_text_bounded`] / [`Clipboard::get_primary_bounded`],
//!   used by the paste and OSC 52 reply seams, clip an over-limit system
//!   value at a UTF-8 char boundary instead of rejecting it, so an oversized
//!   clipboard never pastes (or answers) nothing (CTX-0478 review).
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

use std::time::Duration;

use crate::error::PlatformError;

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
use arboard::{ClearExtLinux, GetExtLinux, LinuxClipboardKind, SetExtLinux};

/// Maximum bytes allowed for a clipboard payload (mirrors
/// `BoundedBytes::MAX_LEN` / `CLIPBOARD_MAX_PAYLOAD_BYTES` = 4096 plus a
/// small slack for paste verification). Writes are rejected before any
/// `arboard` call, so an over-limit write never reaches the OS.
///
/// Scope (R-004 residual): this is a post-acquisition retained/inspection
/// bound, not a strict peak-memory bound. A native read materializes the
/// OS-provided `String` first and the bounded accessors clip afterwards, so
/// only retained/inspected/pasted/answered bytes are capped. See the
/// [`Clipboard`] type docs.
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
///
/// Bound scope (R-004 residual): [`CLIPBOARD_MAX_BYTES`] is a
/// post-acquisition retained/inspection bound, not a strict peak-memory
/// bound. Native reads materialize the OS-provided `String` before the
/// bounded accessors clip it, so a hostile clipboard can transiently exceed
/// the cap in memory; what is retained, inspected, pasted, or answered is
/// always within the cap. Whether the last bounded read clipped is
/// observable via [`Self::last_bounded_read_truncated`].
pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
    headless_buf: String,
    primary_buf: String,
    headless_only: bool,
    /// Why the system clipboard could not be opened, when [`Self::new`]
    /// degraded to the headless buffer. `None` when a system handle exists or
    /// the handle was constructed headless on purpose. Kept so a swallowed
    /// display/backend failure stays observable (CTX-0478).
    init_error: Option<String>,
    /// Test seam: raw read result substituted for the platform backend on
    /// every regular/primary read. `None` in production; set by
    /// [`Clipboard::simulate_system_text_for_test`].
    simulated_read: Option<String>,
    /// Whether the most recent bounded read ([`Self::get_text_bounded`] /
    /// [`Self::get_primary_bounded`]) clipped an over-limit value.
    ///
    /// Set on every bounded read, untouched by the direct reads, so the
    /// paste seam can attribute a truncation to the platform layer exactly
    /// once (R-004 truncated-paste telemetry).
    bounded_read_truncated: bool,
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard")
            .field("is_headless", &self.is_headless())
            .field("headless_len", &self.headless_buf.len())
            .field("primary_len", &self.primary_buf.len())
            .field("backend_hint", &self.backend_hint())
            .field("headless_reason", &self.headless_reason())
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
                init_error: None,
                simulated_read: None,
                bounded_read_truncated: false,
            },
            Err(err) => Self {
                inner: None,
                headless_buf: String::new(),
                primary_buf: String::new(),
                headless_only: false,
                init_error: Some(err.to_string()),
                simulated_read: None,
                bounded_read_truncated: false,
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
                init_error: None,
                simulated_read: None,
                bounded_read_truncated: false,
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
            init_error: None,
            simulated_read: None,
            bounded_read_truncated: false,
        }
    }

    /// Why the system clipboard could not be opened, when this handle
    /// degraded to the headless buffer via [`Self::new`].
    ///
    /// `None` when a system handle exists or when the handle is headless by
    /// construction ([`Self::new_headless`], [`Self::new_strict`]).
    #[must_use]
    pub fn headless_reason(&self) -> Option<&str> {
        self.init_error.as_deref()
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

    /// Whether the most recent bounded read clipped an over-limit value.
    ///
    /// Set by [`Self::get_text_bounded`] / [`Self::get_primary_bounded`] on
    /// every call (`true` only when the system value exceeded
    /// [`CLIPBOARD_MAX_BYTES`] and was cut at a UTF-8 char boundary);
    /// `false` after construction and untouched by the direct rejecting
    /// reads. The paste seam merges this into its truncation telemetry so a
    /// platform-layer clip is attributed exactly once (R-004).
    #[must_use]
    pub fn last_bounded_read_truncated(&self) -> bool {
        self.bounded_read_truncated
    }

    /// Writes `text` to the regular clipboard and syncs the primary selection.
    ///
    /// `text` is bounded by [`CLIPBOARD_MAX_BYTES`] before any system call;
    /// an over-limit payload is rejected with
    /// [`PlatformError::ClipboardPayloadTooLarge`] and both selections are
    /// left unchanged (no silent truncation, CTX-0478). When headless, both
    /// buffers are updated and `Ok` is returned.
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
        ensure_within_limit(text.len())?;
        if self.headless_only {
            self.headless_buf = text.clone();
            self.primary_buf = text;
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            let clipboard_result = set_clipboard_text(inner, text.clone());
            // Best-effort primary sync (Linux only; no-op elsewhere).
            // Routed through `set_primary_selection` (wl-copy-first on
            // Wayland) so the sync benefits from the fork-safe CLI path.
            let _primary_result = set_primary_selection(inner, &text);
            match clipboard_result {
                Ok(()) => {
                    self.headless_buf = text.clone();
                    self.primary_buf = text;
                    Ok(())
                }
                Err(err) => {
                    self.headless_buf = text.clone();
                    self.primary_buf = text;
                    Err(PlatformError::ClipboardOperation(err))
                }
            }
        } else {
            self.headless_buf = text.clone();
            self.primary_buf = text;
            Ok(())
        }
    }

    /// Writes `text` to the primary selection only (middle-click /
    /// `wl-paste --primary`).
    ///
    /// Bounded by [`CLIPBOARD_MAX_BYTES`] with a typed rejection (see
    /// [`Self::set_text`]). On Wayland the write prefers the `wl-copy
    /// --primary` CLI (fork-safe from multithreaded processes) and falls back
    /// to the `arboard` primary path when the CLI is missing or cannot be
    /// started. On non-Linux platforms there is no primary selection: the
    /// primary buffer is updated headlessly and `Ok` is returned without
    /// touching the OS.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the primary write fails (e.g.
    /// Wayland compositor without primary-selection support), returns
    /// `PlatformError::ClipboardOperation`. The primary buffer is still
    /// updated.
    pub fn set_primary(&mut self, text: String) -> Result<(), PlatformError> {
        ensure_within_limit(text.len())?;
        if self.headless_only {
            self.primary_buf = text;
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            match set_primary_selection(inner, &text) {
                Ok(()) => {
                    self.primary_buf = text;
                    Ok(())
                }
                Err(err) => {
                    self.primary_buf = text;
                    Err(PlatformError::ClipboardOperation(err))
                }
            }
        } else {
            self.primary_buf = text;
            Ok(())
        }
    }

    /// Best-effort write that never returns an error: updates both buffers
    /// and attempts the system clipboard (+ primary sync), dropping any
    /// system error.
    pub fn set_text_lossy(&mut self, text: String) {
        let _ = self.set_text(text);
    }

    /// Reads text from the regular clipboard, bounded by
    /// [`CLIPBOARD_MAX_BYTES`] after the system call. When headless, returns
    /// the buffer contents.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the read fails, returns
    /// `PlatformError::ClipboardOperation`; an over-limit system value is
    /// rejected with `PlatformError::ClipboardPayloadTooLarge` instead of
    /// being truncated. Paste and reply seams that must stay total use
    /// [`Self::get_text_bounded`], which clips instead of rejecting. A
    /// headless handle with no simulated system read never fails. There is
    /// no silent fallback to the primary selection: use
    /// [`Self::get_primary`] explicitly or [`Self::get_text_lossy`] for
    /// best-effort reads.
    pub fn get_text(&mut self) -> Result<String, PlatformError> {
        let text = self
            .read_text_raw()
            .map_err(PlatformError::ClipboardOperation)?;
        ensure_within_limit(text.len())?;
        self.headless_buf = text.clone();
        Ok(text)
    }

    /// Reads text from the primary selection (middle-click /
    /// `wl-paste --primary`), bounded by [`CLIPBOARD_MAX_BYTES`].
    ///
    /// On non-Linux platforms returns the primary buffer without touching
    /// the OS. A headless handle with no simulated system read never fails.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the primary read fails, returns
    /// `PlatformError::ClipboardOperation`; an over-limit system value is
    /// rejected with `PlatformError::ClipboardPayloadTooLarge` instead of
    /// being truncated.
    pub fn get_primary(&mut self) -> Result<String, PlatformError> {
        let text = self
            .read_primary_raw()
            .map_err(PlatformError::ClipboardOperation)?;
        ensure_within_limit(text.len())?;
        self.primary_buf = text.clone();
        Ok(text)
    }

    /// Bounded regular-clipboard read for the paste and OSC 52 reply seams.
    ///
    /// Identical to [`Self::get_text`] except that an over-limit system value
    /// is clipped at a UTF-8 char boundary within [`CLIPBOARD_MAX_BYTES`]
    /// instead of failing with [`PlatformError::ClipboardPayloadTooLarge`]:
    /// a large clipboard pastes its bounded prefix instead of pasting nothing
    /// (CTX-0478 review). The result is always at most
    /// [`CLIPBOARD_MAX_BYTES`] bytes; the runtime paste gate re-applies its
    /// own char-boundary bound before inspection and delivery.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the read fails, returns
    /// `PlatformError::ClipboardOperation`.
    ///
    /// Records whether the value was clipped in
    /// [`Self::last_bounded_read_truncated`].
    pub fn get_text_bounded(&mut self) -> Result<String, PlatformError> {
        let text = self
            .read_text_raw()
            .map_err(PlatformError::ClipboardOperation)?;
        let truncated = text.len() > CLIPBOARD_MAX_BYTES;
        let text = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
        self.bounded_read_truncated = truncated;
        self.headless_buf = text.clone();
        Ok(text)
    }

    /// Bounded primary-selection read for the middle-click paste seam.
    ///
    /// Identical to [`Self::get_primary`] except that an over-limit system
    /// value is clipped at a UTF-8 char boundary within
    /// [`CLIPBOARD_MAX_BYTES`] instead of failing with
    /// [`PlatformError::ClipboardPayloadTooLarge`] (CTX-0478 review).
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the read fails, returns
    /// `PlatformError::ClipboardOperation`.
    ///
    /// Records whether the value was clipped in
    /// [`Self::last_bounded_read_truncated`].
    pub fn get_primary_bounded(&mut self) -> Result<String, PlatformError> {
        let text = self
            .read_primary_raw()
            .map_err(PlatformError::ClipboardOperation)?;
        let truncated = text.len() > CLIPBOARD_MAX_BYTES;
        let text = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
        self.bounded_read_truncated = truncated;
        self.primary_buf = text.clone();
        Ok(text)
    }

    /// Raw regular-clipboard text from the read funnel: a simulated system
    /// read when a test seeded one, otherwise the platform backend when a
    /// system handle exists, otherwise the headless buffer.
    ///
    /// The direct and bounded reads share this funnel, so the bound decision
    /// is identical for a simulated and a real system read.
    fn read_text_raw(&mut self) -> Result<String, String> {
        if let Some(text) = self.simulated_read.clone() {
            return Ok(text);
        }
        if let Some(inner) = self.inner.as_mut() {
            get_clipboard_text(inner)
        } else {
            Ok(self.headless_buf.clone())
        }
    }

    /// Raw primary-selection text from the read funnel; see
    /// [`Self::read_text_raw`].
    fn read_primary_raw(&mut self) -> Result<String, String> {
        if let Some(text) = self.simulated_read.clone() {
            return Ok(text);
        }
        if let Some(inner) = self.inner.as_mut() {
            get_primary_text(inner)
        } else {
            Ok(self.primary_buf.clone())
        }
    }

    /// Test seam: makes every subsequent regular and primary read return
    /// `text` as if the system clipboard had produced it, without touching
    /// the OS.
    ///
    /// The simulated value flows through the same read funnel as a real
    /// system read: the direct reads reject an over-limit value with
    /// [`PlatformError::ClipboardPayloadTooLarge`] while the bounded reads
    /// clip it. Production writes reject over-limit payloads, so this is the
    /// only way to put an over-limit *system* read in front of the runtime
    /// paste seam without a display server (CTX-0478 review).
    pub fn simulate_system_text_for_test(&mut self, text: String) {
        self.simulated_read = Some(text);
    }

    /// Best-effort regular-clipboard read that never returns an error.
    ///
    /// Returns an empty string when the system read fails: replaying the last
    /// in-memory value would present a stale clipboard as current (CTX-0478).
    /// An over-limit system value is clipped to its bounded prefix by the
    /// bounded read instead of being dropped (CTX-0478 review), so the OSC 52
    /// read reply stays non-empty for a large clipboard. Headless reads still
    /// return the buffer.
    #[must_use]
    pub fn get_text_lossy(&mut self) -> String {
        self.get_text_bounded().unwrap_or_default()
    }

    /// Best-effort primary read that returns an empty string when the system
    /// read fails, rather than replaying a stale buffer (CTX-0478). An
    /// over-limit system value is clipped to its bounded prefix instead of
    /// being dropped (CTX-0478 review).
    #[must_use]
    pub fn get_primary_lossy(&mut self) -> String {
        self.get_primary_bounded().unwrap_or_default()
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

/// Clips `text` to at most `max_bytes`, cutting at a UTF-8 char boundary so
/// the result is always valid UTF-8.
///
/// Used by the bounded reads, which deliberately clip an over-limit system
/// value to a pasteable prefix instead of rejecting the paste. Mirrors the
/// runtime paste gate's `truncate_paste_text` char-boundary semantics.
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

/// Rejects a payload larger than [`CLIPBOARD_MAX_BYTES`] with a typed error.
///
/// The old behavior silently truncated at a UTF-8 boundary; truncation is now
/// explicit and the caller decides how to handle the rejection (CTX-0478).
/// The direct reads keep this rejection; the bounded reads clip instead.
fn ensure_within_limit(len: usize) -> Result<(), PlatformError> {
    if len > CLIPBOARD_MAX_BYTES {
        return Err(PlatformError::ClipboardPayloadTooLarge {
            len,
            max: CLIPBOARD_MAX_BYTES,
        });
    }
    Ok(())
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
/// `wl-copy --primary` CLI and falls back to the `arboard` primary path when
/// the CLI is missing or cannot be handed the payload (spawn or stdin-pipe
/// failure), so behavior never regresses below the CTX-0160 contract.
/// Everywhere else (X11, headless-adjacent) this is exactly the `arboard`
/// primary write.
fn set_primary_selection(inner: &mut arboard::Clipboard, text: &str) -> Result<(), String> {
    if is_wayland_session() && wl_copy_primary(text).is_ok() {
        return Ok(());
    }
    set_primary_text(inner, text.to_owned())
}

/// How long the background reaper waits for the `wl-copy` child before it
/// kills and reaps it. Enforced off the caller (CTX-0388), never on the
/// frame loop.
const WL_COPY_WAIT: Duration = Duration::from_secs(2);
/// Reaper poll cadence: one non-blocking `try_wait` per interval on the
/// background thread. Kept off the calling thread so the main path never
/// sleeps.
const WL_COPY_POLL: Duration = Duration::from_millis(10);

/// Writes `text` to the Wayland primary selection via the `wl-copy` CLI.
///
/// Fixed argv (`wl-copy --primary`, no shell) with the payload fed over the
/// child's **stdin pipe** — argv is world-readable through
/// `/proc/<pid>/cmdline`, so clipboard text must never travel as an argument
/// (CTX-0388). The child is handed to a background reaper with a bounded
/// wait ([`WL_COPY_WAIT`]); the caller never sleeps or polls, so a wedged
/// compositor cannot stall the frame loop. Spawn and stdin-write failures
/// return synchronously and the caller falls back to `arboard`; a late
/// non-zero exit or timeout is logged by the reaper (the caller has already
/// returned by then). `text` must already be truncated to
/// [`CLIPBOARD_MAX_BYTES`].
fn wl_copy_primary(text: &str) -> Result<(), String> {
    wl_copy_spawn("wl-copy", text, WL_COPY_WAIT, WL_COPY_POLL)
}

/// Spawns `program` with the fixed `--primary` argv, feeds `text` on the
/// child's stdin pipe, and reaps it off the caller's thread with a bounded
/// wait.
///
/// The `program`/`wait`/`poll` parameters exist so the argv/stdin and
/// bounded-reap contracts are testable against a fake script without
/// resolving from `PATH` (edition 2024 makes `set_var` unsafe and this crate
/// forbids unsafe code).
fn wl_copy_spawn(program: &str, text: &str, wait: Duration, poll: Duration) -> Result<(), String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .arg("--primary")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    // Feed the payload on the child's stdin pipe, then drop the handle so the
    // child reads EOF. A failed write (child exited early) fails closed; the
    // child is killed and reaped before returning so no zombie remains.
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(String::from("wl-copy stdin pipe was not created"));
    };
    if let Err(err) = stdin.write_all(text.as_bytes()) {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(err.to_string());
    }
    drop(stdin);
    // Hand the child off; the caller returns immediately (never waits).
    spawn_wl_copy_reaper(program, child, wait, poll)
}

/// Moves `child` to a named background reaper thread and returns without
/// waiting. Thread creation is the only fallible step; a failed handoff kills
/// and reaps the child so it is never leaked.
fn spawn_wl_copy_reaper(
    program: &str,
    child: std::process::Child,
    wait: Duration,
    poll: Duration,
) -> Result<(), String> {
    let (child_tx, child_rx) = std::sync::mpsc::channel::<std::process::Child>();
    let program = program.to_owned();
    std::thread::Builder::new()
        .name(String::from("bitty-wl-copy-reap"))
        .spawn(move || {
            let Ok(mut child) = child_rx.recv() else {
                return;
            };
            if let Err(err) = reap_wl_copy(&mut child, wait, poll) {
                eprintln!("warning: {program}: {err}");
            }
        })
        .map_err(|err| err.to_string())?;
    match child_tx.send(child) {
        Ok(()) => Ok(()),
        Err(std::sync::mpsc::SendError(mut child)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(String::from("wl-copy reaper exited before handoff"))
        }
    }
}

/// Waits up to `wait` for `child` to exit, polling with `poll` pauses; kills
/// and reaps it on timeout. Runs on the background reaper thread only (the
/// caller must never invoke this on a frame path).
fn reap_wl_copy(
    child: &mut std::process::Child,
    wait: Duration,
    poll: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("wl-copy --primary exited with {status}")),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(String::from("wl-copy --primary timed out"));
            }
            None => std::thread::sleep(poll),
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
    #[cfg(unix)]
    use std::time::Duration;

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
    fn headless_rejects_over_max_without_truncating() {
        // CTX-0478: an over-limit payload is a typed error, never a silently
        // shortened write; the previous value is preserved.
        let mut cb = Clipboard::new_headless();
        cb.set_text("keep".to_string()).expect("seed");
        let long = "a".repeat(CLIPBOARD_MAX_BYTES + 100);
        let err = cb.set_text(long).expect_err("over-limit set must fail");
        match err {
            PlatformError::ClipboardPayloadTooLarge { len, max } => {
                assert_eq!(max, CLIPBOARD_MAX_BYTES);
                assert_eq!(len, CLIPBOARD_MAX_BYTES + 100);
            }
            other => panic!("expected ClipboardPayloadTooLarge, got {other:?}"),
        }
        assert_eq!(cb.headless_contents(), "keep");
        assert_eq!(cb.primary_contents(), "keep");
        // The primary-only path is bounded the same way.
        let emoji = "😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 10);
        assert!(matches!(
            cb.set_primary(emoji),
            Err(PlatformError::ClipboardPayloadTooLarge { .. })
        ));
        assert_eq!(cb.primary_contents(), "keep");
        // At the exact limit the write still succeeds.
        let exact = "b".repeat(CLIPBOARD_MAX_BYTES);
        cb.set_text(exact.clone()).expect("limit is inclusive");
        assert_eq!(cb.headless_contents(), exact);
    }

    #[test]
    fn headless_handles_report_no_init_error() {
        // CTX-0478: a headless handle (by construction) has no swallowed
        // backend failure to report.
        let cb = Clipboard::new_headless();
        assert_eq!(cb.headless_reason(), None);
        assert!(format!("{cb:?}").contains("headless_reason"));
    }

    #[test]
    fn degraded_handle_reports_the_init_error() {
        // CTX-0478 review (non-blocking): the `Some` path of `headless_reason`
        // was untested. `Clipboard::new` records the swallowed `arboard` init
        // failure when no display is reachable; that state cannot be forced
        // deterministically on a desktop, so it is constructed directly here
        // (the mapping from `new` is exercised in degraded environments).
        let cb = Clipboard {
            inner: None,
            headless_buf: String::new(),
            primary_buf: String::new(),
            headless_only: false,
            init_error: Some(String::from("display unavailable")),
            simulated_read: None,
            bounded_read_truncated: false,
        };
        assert_eq!(cb.headless_reason(), Some("display unavailable"));
        assert!(cb.is_headless());
        assert!(format!("{cb:?}").contains("display unavailable"));
    }

    #[test]
    fn direct_read_rejects_over_limit_while_bounded_read_clips() {
        // CTX-0478 review regression: an over-limit system read used to fail
        // the paste (the typed rejection propagated out of `get_text`). The
        // direct reads keep that typed rejection; the bounded reads used by
        // the paste/OSC 52 seams clip at a char boundary instead.
        let mut cb = Clipboard::new_headless();
        cb.simulate_system_text_for_test("x".repeat(CLIPBOARD_MAX_BYTES + 64));
        assert!(matches!(
            cb.get_text(),
            Err(PlatformError::ClipboardPayloadTooLarge { len, max })
                if len == CLIPBOARD_MAX_BYTES + 64 && max == CLIPBOARD_MAX_BYTES
        ));
        let text = cb.get_text_bounded().expect("bounded read must succeed");
        assert_eq!(text.len(), CLIPBOARD_MAX_BYTES);
        assert!(text.bytes().all(|byte| byte == b'x'));
        // The read-back buffer stays in sync with the clipped value.
        assert_eq!(cb.headless_contents(), text);
        assert_eq!(cb.get_text_lossy(), text);
        assert!(matches!(
            cb.get_primary(),
            Err(PlatformError::ClipboardPayloadTooLarge { .. })
        ));
        let primary = cb
            .get_primary_bounded()
            .expect("bounded primary read must succeed");
        assert_eq!(primary.len(), CLIPBOARD_MAX_BYTES);
        assert_eq!(cb.primary_contents(), primary);
        assert_eq!(cb.get_primary_lossy(), primary);
        // At the cap both reads agree; nothing is clipped.
        cb.simulate_system_text_for_test(String::from("ok"));
        assert_eq!(cb.get_text().expect("at-limit read"), "ok");
        assert_eq!(cb.get_text_bounded().expect("at-limit bounded read"), "ok");
    }

    #[test]
    fn bounded_read_cuts_emoji_on_char_boundary() {
        let mut cb = Clipboard::new_headless();
        cb.simulate_system_text_for_test("😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 10));
        assert!(
            cb.get_text().is_err(),
            "direct read must reject an over-limit system value"
        );
        let text = cb.get_text_bounded().expect("bounded read must succeed");
        assert!(text.len() <= CLIPBOARD_MAX_BYTES);
        assert_eq!(text.len() % 4, 0, "emoji must be cut on a char boundary");
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
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

    // -- CTX-0388 / Issue #644: wl-copy stdin + off-thread reap ------------
    //
    // A fake `wl-copy` script stands in for the CLI so the argv/stdin and
    // bounded-reap contracts are deterministic and display-free. The script
    // path is injected, never resolved from PATH (setting env vars is
    // `unsafe` under edition 2024 and forbidden in this crate).

    /// Per-test scratch directory holding a fake `wl-copy` script; removed
    /// on drop. Derived from `temp_dir()` (no host path baked into source).
    #[cfg(unix)]
    struct ScratchDir(std::path::PathBuf);

    #[cfg(unix)]
    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("bitty-wlcopy-test-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        fn script(&self, name: &str, body: &str) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt as _;
            let path = self.path(name);
            std::fs::write(&path, body).expect("write fake script");
            let mut perms = std::fs::metadata(&path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
            path
        }
    }

    #[cfg(unix)]
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Polls until `path` contains exactly `expected` (bounded).
    #[cfg(unix)]
    fn wait_for_content(path: &std::path::Path, expected: &str, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if std::fs::read_to_string(path).is_ok_and(|text| text == expected) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Linux `ETXTBSY` ("Text file busy"), the errno for execing a script
    /// another forked child still holds open for writing.
    #[cfg(unix)]
    const ETXTBSY: i32 = 26;

    /// Spawns a test-authored script, retrying the Linux `ETXTBSY` race: a
    /// concurrently forking test thread can briefly inherit our just-written
    /// script's write fd, so `exec` fails with "Text file busy". Production
    /// runs a long-installed `wl-copy`, so this race is test-only.
    #[cfg(unix)]
    fn spawn_script(program: &std::path::Path) -> std::process::Child {
        for _ in 0..100 {
            match std::process::Command::new(program)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => return child,
                Err(err) if err.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => panic!("spawn {program:?}: {err}"),
            }
        }
        panic!("spawn {program:?} kept failing with ETXTBSY");
    }

    /// [`wl_copy_spawn`] with the same test-only `ETXTBSY` retry (the spawn
    /// happens inside the production helper, so the retry wraps the call).
    #[cfg(unix)]
    fn wl_copy_spawn_retry(
        program: &str,
        text: &str,
        wait: Duration,
        poll: Duration,
    ) -> Result<(), String> {
        for _ in 0..100 {
            match wl_copy_spawn(program, text, wait, poll) {
                Err(err) if err.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                other => return other,
            }
        }
        Err(String::from("fake wl-copy kept failing with ETXTBSY"))
    }

    #[cfg(unix)]
    #[test]
    fn wl_copy_feeds_text_on_stdin_and_keeps_argv_clean() {
        // The payload must travel over the child's stdin pipe, never argv:
        // argv is readable by any same-UID process via /proc/<pid>/cmdline.
        let scratch = ScratchDir::new("stdin-argv");
        let argv_dump = scratch.path("argv.txt");
        let stdin_dump = scratch.path("stdin.txt");
        let script = scratch.script(
            "fake-wl-copy",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\n",
                argv_dump.display(),
                stdin_dump.display()
            ),
        );
        let secret = String::from("argv-leak-secret-42");
        wl_copy_spawn_retry(
            &script.to_string_lossy(),
            &secret,
            Duration::from_secs(5),
            Duration::from_millis(5),
        )
        .expect("spawn must succeed");
        assert!(
            wait_for_content(&stdin_dump, &secret, 5),
            "fake wl-copy must receive the payload on stdin"
        );
        let argv = std::fs::read_to_string(&argv_dump).expect("argv dump");
        assert_eq!(argv, "--primary\n", "argv must carry no payload");
        assert!(
            !argv.contains(&secret),
            "payload must never appear in argv: {argv:?}"
        );
        let stdin = std::fs::read_to_string(&stdin_dump).expect("stdin dump");
        assert_eq!(stdin, secret, "payload must arrive over stdin");
    }

    #[cfg(unix)]
    #[test]
    fn wl_copy_spawn_does_not_wait_for_the_child_on_the_caller() {
        // The old path polled `try_wait` with a 10 ms sleep on the caller
        // (main) thread for up to 2 s; the fixed path hands the child to a
        // reaper thread and returns immediately.
        let scratch = ScratchDir::new("nonblocking");
        let script = scratch.script("slow-wl-copy", "#!/bin/sh\nsleep 10\n");
        let started = std::time::Instant::now();
        wl_copy_spawn_retry(
            &script.to_string_lossy(),
            "payload",
            Duration::from_millis(50),
            Duration::from_millis(5),
        )
        .expect("spawn must succeed");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "spawn path must not block on the child (took {elapsed:?})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wl_copy_reaper_bounds_and_kills_a_wedged_child() {
        // Reaping is bounded and off the caller; a wedged child is killed
        // and reaped instead of hanging the write path.
        let scratch = ScratchDir::new("reaper-timeout");
        let script = scratch.script("wedged-wl-copy", "#!/bin/sh\nsleep 30\n");
        let mut child = spawn_script(&script);
        let started = std::time::Instant::now();
        let result = reap_wl_copy(
            &mut child,
            Duration::from_millis(100),
            Duration::from_millis(5),
        );
        let elapsed = started.elapsed();
        let err = result.expect_err("a wedged child must time out");
        assert!(err.contains("timed out"), "timeout must be named: {err}");
        assert!(elapsed >= Duration::from_millis(100), "waits the bound");
        assert!(elapsed < Duration::from_secs(2), "never hangs: {elapsed:?}");
        // Killed and reaped: no live child and no success status left behind.
        let status = child
            .try_wait()
            .expect("try_wait must not error")
            .expect("child must be reaped");
        assert!(!status.success(), "killed child is not a success");
    }

    #[cfg(unix)]
    #[test]
    fn wl_copy_reaper_surfaces_nonzero_exit() {
        let scratch = ScratchDir::new("reaper-nonzero");
        let script = scratch.script("failing-wl-copy", "#!/bin/sh\nexit 3\n");
        let mut child = spawn_script(&script);
        let err = reap_wl_copy(&mut child, Duration::from_secs(5), Duration::from_millis(5))
            .expect_err("non-zero exit must surface");
        assert!(err.contains("exited with"), "names the exit status: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn wl_copy_missing_program_fails_closed_without_panic() {
        // A missing binary makes the caller fall back to `arboard`.
        let err = wl_copy_spawn(
            "/definitely/not/a/real/wl-copy-binary",
            "x",
            Duration::from_millis(50),
            Duration::from_millis(5),
        )
        .expect_err("missing binary must fail closed");
        assert!(!err.is_empty(), "failure must carry a reason");
    }
}
