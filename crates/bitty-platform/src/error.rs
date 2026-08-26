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

    /// A scale factor was not finite or not strictly positive.
    InvalidScaleFactor(f64),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisplayUnavailable(detail) => {
                write!(f, "no usable display server available: {detail}")
            }
            Self::WindowCreation(detail) => write!(f, "window creation failed: {detail}"),
            Self::EventLoopRun(detail) => write!(f, "event loop failed: {detail}"),
            Self::InvalidScaleFactor(value) => {
                write!(f, "invalid scale factor (must be finite and > 0): {value}")
            }
        }
    }
}

impl std::error::Error for PlatformError {}
