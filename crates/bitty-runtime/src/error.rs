//! Owned error type for `bitty-runtime`.
//!
//! No upstream error type (portable-pty, winit, wgpu, vte) escapes this crate;
//! every failure is flattened into [`RuntimeError`] with an owned message.

use std::fmt;

/// Everything that can go wrong across the owned runtime.
#[derive(Debug)]
pub enum RuntimeError {
    /// Configuration value was outside its valid range.
    InvalidConfig(&'static str),
    /// PTY layer rejected the request.
    Pty(String),
    /// Platform layer rejected the request.
    Platform(String),
    /// Renderer layer rejected the request.
    Render(String),
    /// Surface configuration rejected the size.
    InvalidSize(&'static str),
    /// A bounded queue was constructed with zero capacity.
    InvalidQueueCapacity,
    /// Plugin host rejected the request.
    Plugin(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid runtime config: {msg}"),
            Self::Pty(msg) => write!(f, "pty error: {msg}"),
            Self::Platform(msg) => write!(f, "platform error: {msg}"),
            Self::Render(msg) => write!(f, "render error: {msg}"),
            Self::InvalidSize(msg) => write!(f, "invalid size: {msg}"),
            Self::InvalidQueueCapacity => write!(f, "cold queue capacity must be > 0"),
            Self::Plugin(msg) => write!(f, "plugin error: {msg}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<bitty_pty::PtyError> for RuntimeError {
    fn from(value: bitty_pty::PtyError) -> Self {
        Self::Pty(value.to_string())
    }
}

impl From<bitty_render::RenderError> for RuntimeError {
    fn from(value: bitty_render::RenderError) -> Self {
        Self::Render(value.to_string())
    }
}

impl From<bitty_platform::PlatformError> for RuntimeError {
    fn from(value: bitty_platform::PlatformError) -> Self {
        Self::Platform(value.to_string())
    }
}

impl From<bitty_plugin_host::PluginError> for RuntimeError {
    fn from(value: bitty_plugin_host::PluginError) -> Self {
        Self::Plugin(value.to_string())
    }
}
