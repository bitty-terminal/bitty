//! Owned error vocabulary for `bitty-ipc`.

use std::fmt;

/// Stable error class mirroring the security and isolation corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Frame or payload size / truncation failure.
    Framing,
    /// Bounded channel capacity exceeded.
    Channel,
    /// Transport (stdio pipe / IPC endpoint) failure.
    Transport,
    /// Request timeout / deadline exceeded.
    Timeout,
    /// Validation of method, params, or protocol fields.
    Validation,
    /// Scope / peer-credential denial.
    Scope,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Framing => "framing",
            Self::Channel => "channel",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Validation => "validation",
            Self::Scope => "scope",
        };
        f.write_str(label)
    }
}

/// Owned, headless-testable error for every `bitty-ipc` failure mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// Frame payload exceeds the 256 KiB bound.
    FrameTooLarge {
        /// Actual payload length in bytes.
        actual: usize,
        /// Configured limit.
        limit: usize,
    },
    /// Payload exceeds per-message or per-field limit.
    PayloadTooLarge {
        /// Field or context.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual length.
        actual: usize,
    },
    /// Frame header or body truncated.
    FrameTruncated {
        /// Expected bytes (header + length).
        expected: usize,
        /// Actual bytes available.
        actual: usize,
    },
    /// Frame contents malformed.
    InvalidFrame {
        /// Human-readable reason.
        reason: String,
    },
    /// Bounded channel at capacity.
    ChannelFull {
        /// Channel capacity.
        capacity: usize,
    },
    /// Channel closed or endpoint gone.
    ChannelClosed {
        /// Reason.
        reason: String,
    },
    /// Transport closed.
    TransportClosed {
        /// Reason.
        reason: String,
    },
    /// Transport buffer at capacity.
    TransportFull {
        /// Capacity (frames).
        capacity: usize,
    },
    /// Transport failure (peer died, broken pipe).
    Transport {
        /// Reason.
        reason: String,
    },
    /// Request deadline exceeded.
    Timeout {
        /// Request id that timed out.
        request_id: u64,
        /// Configured timeout in ms.
        timeout_ms: u64,
    },
    /// Too many pending requests.
    PendingLimitExceeded {
        /// Configured limit.
        limit: usize,
        /// Actual pending count.
        actual: usize,
    },
    /// Method name invalid.
    InvalidMethod {
        /// Attempted method.
        method: String,
        /// Reason.
        reason: String,
    },
    /// Generic validation failure.
    InvalidRequest {
        /// Reason.
        reason: String,
    },
    /// Scope denied for action.
    ScopeDenied {
        /// Scope that was checked.
        scope: String,
        /// Action attempted.
        action: String,
    },
    /// Limit exceeded for a named field.
    LimitExceeded {
        /// Field.
        field: String,
        /// Limit.
        limit: usize,
        /// Actual.
        actual: usize,
    },
}

impl IpcError {
    /// Convenience for frame-too-large.
    #[must_use]
    pub fn frame_too_large(actual: usize, limit: usize) -> Self {
        Self::FrameTooLarge { actual, limit }
    }

    /// Convenience for invalid frame.
    #[must_use]
    pub fn invalid_frame(reason: impl Into<String>) -> Self {
        Self::InvalidFrame {
            reason: reason.into(),
        }
    }

    /// Convenience for channel full.
    #[must_use]
    pub fn channel_full(capacity: usize) -> Self {
        Self::ChannelFull { capacity }
    }

    /// Convenience for transport closed.
    #[must_use]
    pub fn transport_closed(reason: impl Into<String>) -> Self {
        Self::TransportClosed {
            reason: reason.into(),
        }
    }

    /// Convenience for invalid method.
    #[must_use]
    pub fn invalid_method(method: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidMethod {
            method: method.into(),
            reason: reason.into(),
        }
    }

    /// Convenience for invalid request.
    #[must_use]
    pub fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: reason.into(),
        }
    }

    /// Convenience for scope denial.
    #[must_use]
    pub fn scope_denied(scope: impl Into<String>, action: impl Into<String>) -> Self {
        Self::ScopeDenied {
            scope: scope.into(),
            action: action.into(),
        }
    }

    /// Stable class for diagnostics and doctor aggregation.
    #[must_use]
    pub fn error_class(&self) -> ErrorClass {
        match self {
            Self::FrameTooLarge { .. }
            | Self::PayloadTooLarge { .. }
            | Self::FrameTruncated { .. }
            | Self::InvalidFrame { .. }
            | Self::LimitExceeded { .. } => ErrorClass::Framing,
            Self::ChannelFull { .. } | Self::ChannelClosed { .. } => ErrorClass::Channel,
            Self::TransportClosed { .. } | Self::TransportFull { .. } | Self::Transport { .. } => {
                ErrorClass::Transport
            }
            Self::Timeout { .. } => ErrorClass::Timeout,
            Self::InvalidMethod { .. } | Self::InvalidRequest { .. } => ErrorClass::Validation,
            Self::PendingLimitExceeded { .. } => ErrorClass::Channel,
            Self::ScopeDenied { .. } => ErrorClass::Scope,
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { actual, limit } => {
                write!(f, "frame too large: {actual} bytes exceeds limit {limit}")
            }
            Self::PayloadTooLarge {
                field,
                limit,
                actual,
            } => write!(f, "{field}: payload {actual} exceeds limit {limit}"),
            Self::FrameTruncated { expected, actual } => {
                write!(
                    f,
                    "frame truncated: expected {expected} bytes, got {actual}"
                )
            }
            Self::InvalidFrame { reason } => write!(f, "invalid frame: {reason}"),
            Self::ChannelFull { capacity } => write!(f, "channel full (capacity {capacity})"),
            Self::ChannelClosed { reason } => write!(f, "channel closed: {reason}"),
            Self::TransportClosed { reason } => write!(f, "transport closed: {reason}"),
            Self::TransportFull { capacity } => {
                write!(f, "transport full (capacity {capacity} frames)")
            }
            Self::Transport { reason } => write!(f, "transport error: {reason}"),
            Self::Timeout {
                request_id,
                timeout_ms,
            } => write!(f, "request {request_id} timed out after {timeout_ms} ms"),
            Self::PendingLimitExceeded { limit, actual } => {
                write!(f, "pending limit {limit} exceeded (actual {actual})")
            }
            Self::InvalidMethod { method, reason } => {
                write!(f, "invalid method '{method}': {reason}")
            }
            Self::InvalidRequest { reason } => write!(f, "invalid request: {reason}"),
            Self::ScopeDenied { scope, action } => {
                write!(f, "scope '{scope}' denied for action '{action}'")
            }
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(f, "{field}: limit {limit} exceeded (actual {actual})"),
        }
    }
}

impl std::error::Error for IpcError {}
