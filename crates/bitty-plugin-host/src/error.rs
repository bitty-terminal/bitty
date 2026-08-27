//! Owned error vocabulary for `bitty-plugin-host`.

use std::fmt;

/// Stable error class mirroring the security and plugin corpus categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Manifest validation failure.
    Manifest,
    /// Capability validation failure.
    Capability,
    /// Grant lifecycle failure.
    Grant,
    /// Registry / lifecycle failure.
    Registry,
    /// Event pipeline failure.
    Event,
    /// Queue or resource bound exceeded.
    Budget,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Manifest => "manifest",
            Self::Capability => "capability",
            Self::Grant => "grant",
            Self::Registry => "registry",
            Self::Event => "event",
            Self::Budget => "budget",
        };
        f.write_str(label)
    }
}

/// Owned, headless-testable error for every `bitty-plugin-host` failure mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// A field in the manifest failed validation.
    Manifest {
        /// Dotted field path.
        field: String,
        /// Human-readable reason.
        message: String,
    },
    /// Capability identifier invalid or unknown.
    Capability {
        /// Attempted identifier.
        id: String,
        /// Reason.
        reason: String,
    },
    /// Plugin ID invalid.
    InvalidPluginId {
        /// Attempted id.
        id: String,
        /// Reason.
        reason: String,
    },
    /// A hard limit was exceeded.
    LimitExceeded {
        /// Field or resource.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual value.
        actual: usize,
    },
    /// Duplicate declaration (e.g. qualified command name).
    Duplicate {
        /// Kind of duplicate.
        kind: String,
        /// Value.
        value: String,
    },
    /// Registry rejected the operation.
    Registry {
        /// Reason.
        message: String,
    },
    /// Grant lifecycle rejected the operation.
    Grant {
        /// Reason.
        message: String,
    },
    /// Event pipeline rejected the operation.
    Event {
        /// Reason.
        message: String,
    },
    /// Referenced plugin not found.
    NotFound {
        /// Plugin id.
        id: String,
    },
    /// Operation requires a different lifecycle state.
    InvalidState {
        /// Plugin id.
        id: String,
        /// Current state label.
        current: String,
        /// Expected label.
        expected: String,
    },
}

impl PluginError {
    /// Convenience for manifest field errors.
    #[must_use]
    pub fn manifest(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Manifest {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Convenience for capability errors.
    #[must_use]
    pub fn capability(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Capability {
            id: id.into(),
            reason: reason.into(),
        }
    }

    /// Convenience for registry errors.
    #[must_use]
    pub fn registry(message: impl Into<String>) -> Self {
        Self::Registry {
            message: message.into(),
        }
    }

    /// Convenience for grant errors.
    #[must_use]
    pub fn grant(message: impl Into<String>) -> Self {
        Self::Grant {
            message: message.into(),
        }
    }

    /// Convenience for event errors.
    #[must_use]
    pub fn event(message: impl Into<String>) -> Self {
        Self::Event {
            message: message.into(),
        }
    }

    /// Stable class for diagnostics and `bitty plugin doctor` aggregation.
    #[must_use]
    pub fn error_class(&self) -> ErrorClass {
        match self {
            Self::Manifest { .. } | Self::InvalidPluginId { .. } | Self::LimitExceeded { .. } => {
                ErrorClass::Manifest
            }
            Self::Capability { .. } => ErrorClass::Capability,
            Self::Grant { .. } => ErrorClass::Grant,
            Self::Registry { .. }
            | Self::Duplicate { .. }
            | Self::NotFound { .. }
            | Self::InvalidState { .. } => ErrorClass::Registry,
            Self::Event { .. } => ErrorClass::Event,
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { field, message } => write!(f, "manifest {field}: {message}"),
            Self::Capability { id, reason } => write!(f, "capability '{id}': {reason}"),
            Self::InvalidPluginId { id, reason } => write!(f, "plugin id '{id}': {reason}"),
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => {
                write!(f, "{field}: limit {limit} exceeded (actual {actual})")
            }
            Self::Duplicate { kind, value } => write!(f, "duplicate {kind}: '{value}'"),
            Self::Registry { message } => write!(f, "registry: {message}"),
            Self::Grant { message } => write!(f, "grant: {message}"),
            Self::Event { message } => write!(f, "event: {message}"),
            Self::NotFound { id } => write!(f, "plugin not found: '{id}'"),
            Self::InvalidState {
                id,
                current,
                expected,
            } => {
                write!(f, "plugin '{id}' state '{current}' requires '{expected}'")
            }
        }
    }
}

impl std::error::Error for PluginError {}
