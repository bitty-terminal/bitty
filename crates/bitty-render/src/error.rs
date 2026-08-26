//! Owned error type for [`crate`].
//!
//! Errors reported by the wrapped upstream layers are flattened into owned
//! data before they cross back into this crate's API; no `wgpu` or `crossfont`
//! error type ever escapes the crate (ADR-0004). Failures that originate
//! inside this crate surface as dedicated variants with owned payloads only.

use std::fmt;

/// Everything that can go wrong across the render lifecycle owned by this
/// crate.
#[derive(Debug)]
pub enum RenderError {
    /// An input value failed validation before any upstream call was made.
    InvalidInput {
        /// Why the value was rejected.
        reason: &'static str,
    },
    /// No GPU adapter matched the requested backend options. Reported when
    /// running on a machine without a usable graphics stack (for example a
    /// headless CI runner).
    NoCompatibleAdapter,
    /// The adapter rejected the logical device request.
    DeviceRequest(String),
    /// No font matching the requested description could be found.
    FontNotFound(String),
    /// A font handle from a previous rasterizer session (or one never issued
    /// by this instance) was used to rasterize.
    UnknownFontHandle,
    /// Opaque failure reported by the wrapped upstream font rasterizer. The
    /// message carries the upstream error's primary display text; the
    /// upstream type itself never crosses this crate's API boundary.
    UpstreamRasterizer(String),
    /// Opaque failure reported by the wrapped upstream graphics layer. The
    /// message carries the upstream error's primary display text; the
    /// upstream type itself never crosses this crate's API boundary.
    UpstreamGraphics(String),
}

impl RenderError {
    /// Flattens an upstream rasterizer failure into owned data. The upstream
    /// error type is intentionally unnamed; callers pass whatever the wrapped
    /// layer returned and only its `Display` output survives.
    pub(crate) fn flatten_rasterizer<E: fmt::Display>(err: E) -> Self {
        RenderError::UpstreamRasterizer(err.to_string())
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::InvalidInput { reason } => write!(f, "invalid input: {reason}"),
            RenderError::NoCompatibleAdapter => {
                write!(f, "no compatible GPU adapter found")
            }
            RenderError::DeviceRequest(msg) => write!(f, "GPU device request failed: {msg}"),
            RenderError::FontNotFound(desc) => write!(f, "font not found: {desc}"),
            RenderError::UnknownFontHandle => {
                write!(f, "unknown font handle for this rasterizer session")
            }
            RenderError::UpstreamRasterizer(msg) => write!(f, "font rasterizer error: {msg}"),
            RenderError::UpstreamGraphics(msg) => write!(f, "graphics backend error: {msg}"),
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_owned_and_stable() {
        assert_eq!(
            RenderError::InvalidInput { reason: "empty" }.to_string(),
            "invalid input: empty"
        );
        assert_eq!(
            RenderError::NoCompatibleAdapter.to_string(),
            "no compatible GPU adapter found"
        );
        assert_eq!(
            RenderError::DeviceRequest("limit too high".into()).to_string(),
            "GPU device request failed: limit too high"
        );
        assert_eq!(
            RenderError::FontNotFound("monospace".into()).to_string(),
            "font not found: monospace"
        );
        assert_eq!(
            RenderError::UnknownFontHandle.to_string(),
            "unknown font handle for this rasterizer session"
        );
        assert_eq!(
            RenderError::UpstreamRasterizer("boom".into()).to_string(),
            "font rasterizer error: boom"
        );
        assert_eq!(
            RenderError::UpstreamGraphics("kaboom".into()).to_string(),
            "graphics backend error: kaboom"
        );
    }

    #[test]
    fn flatten_keeps_only_display_text() {
        struct Weird;
        impl fmt::Display for Weird {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "upstream said 42")
            }
        }
        let err = RenderError::flatten_rasterizer(Weird);
        assert!(matches!(err, RenderError::UpstreamRasterizer(ref m) if m == "upstream said 42"));
    }
}
