//! Owned error vocabulary for `bitty-agent`.

use std::fmt;

/// Stable error class for the draft agent contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Agent identity validation failure.
    Identity,
    /// Message validation or bounding failure.
    Message,
    /// Tool-call validation or stub failure.
    Tool,
    /// Bounded queue or budget exceeded.
    Budget,
    /// Session or lifecycle failure.
    Session,
    /// Observation validation failure.
    Observation,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Identity => "identity",
            Self::Message => "message",
            Self::Tool => "tool",
            Self::Budget => "budget",
            Self::Session => "session",
            Self::Observation => "observation",
        };
        f.write_str(label)
    }
}

/// Owned, headless-testable error for every `bitty-agent` failure mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// A field failed validation.
    Validation {
        /// Field or context.
        field: String,
        /// Human-readable reason.
        message: String,
    },
    /// Agent id invalid.
    InvalidAgentId {
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
    /// Duplicate declaration.
    Duplicate {
        /// Kind of duplicate.
        kind: String,
        /// Value.
        value: String,
    },
    /// Session rejected the operation.
    Session {
        /// Reason.
        message: String,
    },
    /// Tool rejected the operation.
    Tool {
        /// Reason.
        message: String,
    },
    /// Referenced agent or session not found.
    NotFound {
        /// Identifier.
        id: String,
    },
}

impl AgentError {
    /// Convenience for field validation errors.
    #[must_use]
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Convenience for session errors.
    #[must_use]
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session {
            message: message.into(),
        }
    }

    /// Convenience for tool errors.
    #[must_use]
    pub fn tool(message: impl Into<String>) -> Self {
        Self::Tool {
            message: message.into(),
        }
    }

    /// Stable class for diagnostics and `doctor` aggregation.
    #[must_use]
    pub fn error_class(&self) -> ErrorClass {
        match self {
            Self::Validation { .. } | Self::InvalidAgentId { .. } | Self::LimitExceeded { .. } => {
                ErrorClass::Identity
            }
            Self::Duplicate { kind, .. } if kind == "tool" => ErrorClass::Tool,
            Self::Duplicate { .. } => ErrorClass::Session,
            Self::Session { .. } | Self::NotFound { .. } => ErrorClass::Session,
            Self::Tool { .. } => ErrorClass::Tool,
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { field, message } => write!(f, "{field}: {message}"),
            Self::InvalidAgentId { id, reason } => write!(f, "agent id '{id}': {reason}"),
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(f, "{field}: limit {limit} exceeded (actual {actual})"),
            Self::Duplicate { kind, value } => write!(f, "duplicate {kind}: '{value}'"),
            Self::Session { message } => write!(f, "session: {message}"),
            Self::Tool { message } => write!(f, "tool: {message}"),
            Self::NotFound { id } => write!(f, "not found: '{id}'"),
        }
    }
}

impl std::error::Error for AgentError {}
