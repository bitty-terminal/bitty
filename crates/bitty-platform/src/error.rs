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
    ClipboardUnavailable(String),

    /// Clipboard operation failed.
    ClipboardOperation(String),

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
            Self::InvalidUrl => write!(f, "URL rejected by scheme and character policy"),
            Self::UrlActivationDenied => {
                write!(f, "URL activation requires a user gesture and approval")
            }
            Self::UrlLaunch(detail) => write!(f, "URL launch failed: {detail}"),
        }
    }
}

impl std::error::Error for PlatformError {}
