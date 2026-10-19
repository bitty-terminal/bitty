//! Owned error type for platform operations.

use core::fmt;

/// Errors surfaced by the platform layer.
///
/// All variants are owned; underlying OS/upstream diagnostics are captured as
/// strings because upstream error types must not escape the crate API
/// (ADR-0004 wrapper rule).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PlatformError {
    /// The process has no usable window system (common in headless CI).
    ///
    /// Carries the upstream diagnostic text.
    DisplayUnavailable(String),

    /// A window could not be created.
    WindowCreation(String),

    /// The event loop failed while running (after successful creation).
    EventLoopRun(String),

    /// A platform surface handle could not be obtained for GPU surface
    /// attachment (see [`crate::surface::SurfaceTarget`]).
    ///
    /// Carries the upstream diagnostic text.
    SurfaceHandle(String),

    /// A scale factor was not finite or not strictly positive.
    InvalidScaleFactor(f64),

    /// Clipboard is unavailable on this platform or display server.
    ///
    /// Since CTX-1366 this is also the typed refusal for a macOS text read:
    /// AppKit exposes no caller byte bound, so Bitty does not materialize the
    /// pasteboard value. Headless handles and writes are unaffected.
    ClipboardUnavailable(String),

    /// Clipboard operation failed.
    ClipboardOperation(String),

    /// Clipboard payload exceeds the bounded clipboard budget.
    ///
    /// Truncation is never silent: callers receive this typed error and the
    /// clipboard is left unchanged instead of storing a shortened value.
    ///
    /// For bounded native acquisition the value was never fully materialized,
    /// so `len` is the number of bytes actually observed or advertised, a
    /// lower bound on the foreign payload rather than its full length. The
    /// in-memory paths still report the exact length. Linux X11 and Windows
    /// over-cap reads return this error with no partial text (CTX-1366);
    /// Linux Wayland returns a truncated prefix through the bounded accessors
    /// instead, and the direct reads report this error there too.
    ClipboardPayloadTooLarge {
        /// Byte length of the rejected payload, or the observed lower bound
        /// for a bounded native acquisition.
        len: usize,
        /// Configured maximum in bytes (`CLIPBOARD_MAX_BYTES`).
        max: usize,
    },

    /// A bounded native clipboard acquisition exceeded its wall-clock budget
    /// and failed closed (CTX-1315).
    ///
    /// No text is returned and no input is delivered: the paste seam reports
    /// the failure and an authorized OSC 52 read answers with the existing
    /// framed-empty reply.
    ClipboardAcquisitionTimeout {
        /// Acquisition stage that was waiting when the deadline expired.
        stage: &'static str,
        /// Configured budget in milliseconds.
        timeout_ms: u64,
    },

    /// The URI is not in the supported, safe scheme allowlist.
    InvalidUrl,

    /// URL activation was not caused by a user gesture or was vetoed.
    UrlActivationDenied,

    /// The default URL handler could not be started.
    UrlLaunch(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisplayUnavailable(detail) => {
                write!(f, "no usable display server available: {detail}")
            }
            Self::WindowCreation(detail) => write!(f, "window creation failed: {detail}"),
            Self::EventLoopRun(detail) => write!(f, "event loop failed: {detail}"),
            Self::SurfaceHandle(detail) => {
                write!(f, "platform surface handle unavailable: {detail}")
            }
            Self::InvalidScaleFactor(value) => {
                write!(f, "invalid scale factor (must be finite and > 0): {value}")
            }
            Self::ClipboardUnavailable(detail) => {
                write!(f, "clipboard unavailable: {detail}")
            }
            Self::ClipboardOperation(detail) => {
                write!(f, "clipboard operation failed: {detail}")
            }
            Self::ClipboardPayloadTooLarge { len, max } => {
                write!(
                    f,
                    "clipboard payload exceeds the {max}-byte limit ({len} bytes observed)"
                )
            }
            Self::ClipboardAcquisitionTimeout { stage, timeout_ms } => {
                write!(
                    f,
                    "clipboard acquisition timed out after {timeout_ms} ms during {stage}"
                )
            }
            Self::InvalidUrl => write!(f, "URL rejected by scheme and character policy"),
            Self::UrlActivationDenied => {
                write!(f, "URL activation requires a user gesture and approval")
            }
            Self::UrlLaunch(detail) => write!(f, "URL launch failed: {detail}"),
        }
    }
}

impl std::error::Error for PlatformError {}
