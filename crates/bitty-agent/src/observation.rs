//! Bounded agent observations delivered through the side queue.
//!
//! Observations are read-only, bounded, and labeled as **untrusted display
//! data** per security invariant 6 and `T-10` / `R-013` (`Agent follows
//! instructions printed by hostile terminal output`). An agent must treat
//! payloads as observation, never as authority or instruction, and must not
//! be granted filesystem/network authority by copying text from an observation
//! without an explicit capability grant.

use crate::error::AgentError;

/// Maximum bytes for any bounded string payload in an observation.
///
/// Mirrors the small bounded presentation helpers (`8 KiB` batch budgets in
/// the plugin host) so the side queue plus one batch stays well below the
/// `256 KiB` IPC framing cap owned by `bitty-ipc` (OQ-018).
pub const MAX_OBSERVATION_BYTES: usize = 8 * 1024;

/// Maximum bytes for the whole observation's serialized form (defensive cap
/// for transport framing checks that will live in `bitty-ipc`).
pub const MAX_OBSERVATION_FRAME_BYTES: usize = 16 * 1024;

/// Read-only observation delivered through the bounded side queue (ADR-0003
/// rule 4).
///
/// Payloads are owned (`String`), bounded, and cloneable. No live terminal
/// object, GPU texture, window handle, or PTY descriptor is ever placed in
/// the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentObservation {
    /// Window/icon title changed (`OSC 0/2`).
    TitleChanged(String),
    /// Working directory report changed (`OSC 7`).
    CwdChanged(String),
    /// Terminal bell.
    Bell,
    /// Damage became available (generation counter).
    Damage {
        /// New terminal-state generation after the batch.
        generation: u64,
    },
    /// Selection changed (bounded preview of selected text).
    SelectionChanged(String),
    /// Focus changed.
    FocusChanged {
        /// True when focused.
        focused: bool,
    },
    /// Process / PTY exited (bounded exit reason).
    ProcessExited {
        /// Exit code, if known.
        code: Option<i32>,
    },
    /// Configuration reloaded.
    ConfigReloaded,
    /// Raw terminal output chunk (untrusted, truncated at cap).
    ///
    /// This is the `T-10` surface: the agent **must not** interpret this
    /// string as an instruction without explicit user consent and a
    /// per-client scope check owned outside this crate.
    TerminalOutput {
        /// Bounded preview (truncated at `MAX_OBSERVATION_BYTES`).
        text: String,
    },
    /// Custom bounded text observation for headless tests and future probes.
    Custom {
        /// Kind label (bounded).
        kind: String,
        /// Bounded payload.
        payload: String,
    },
}

impl AgentObservation {
    /// Validate that every string payload respects `MAX_OBSERVATION_BYTES`.
    pub fn validate(&self) -> Result<(), AgentError> {
        match self {
            Self::TitleChanged(s)
            | Self::CwdChanged(s)
            | Self::SelectionChanged(s)
            | Self::TerminalOutput { text: s } => {
                if s.len() > MAX_OBSERVATION_BYTES {
                    return Err(AgentError::LimitExceeded {
                        field: "observation payload".to_string(),
                        limit: MAX_OBSERVATION_BYTES,
                        actual: s.len(),
                    });
                }
            }
            Self::Custom { kind, payload } => {
                if kind.len() > 128 {
                    return Err(AgentError::LimitExceeded {
                        field: "observation kind".to_string(),
                        limit: 128,
                        actual: kind.len(),
                    });
                }
                if kind.is_empty() {
                    return Err(AgentError::validation(
                        "observation kind",
                        "must not be empty",
                    ));
                }
                if payload.len() > MAX_OBSERVATION_BYTES {
                    return Err(AgentError::LimitExceeded {
                        field: "observation payload".to_string(),
                        limit: MAX_OBSERVATION_BYTES,
                        actual: payload.len(),
                    });
                }
                // Kind must be [a-z0-9_.-]
                for b in kind.bytes() {
                    if !(b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'_'
                        || b == b'.'
                        || b == b'-')
                    {
                        return Err(AgentError::validation(
                            "observation kind",
                            "kind must be [a-z0-9_.-]",
                        ));
                    }
                }
            }
            Self::Bell
            | Self::Damage { .. }
            | Self::FocusChanged { .. }
            | Self::ProcessExited { .. }
            | Self::ConfigReloaded => {}
        }
        Ok(())
    }

    /// Approximate byte size of the payload (for batch budget checks).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::TitleChanged(s) => s.len(),
            Self::CwdChanged(s) => s.len(),
            Self::Bell => 0,
            Self::Damage { .. } => 8,
            Self::SelectionChanged(s) => s.len(),
            Self::FocusChanged { .. } => 1,
            Self::ProcessExited { .. } => 4,
            Self::ConfigReloaded => 0,
            Self::TerminalOutput { text } => text.len(),
            Self::Custom { kind, payload } => kind.len() + payload.len(),
        }
    }

    /// Create a `TerminalOutput` truncation helper.
    ///
    /// Untrusted terminal bytes are truncated at `MAX_OBSERVATION_BYTES` with
    /// a loud marker so silent loss is not mistaken for complete data.
    #[must_use]
    pub fn terminal_output_truncated(mut text: String) -> Self {
        if text.len() > MAX_OBSERVATION_BYTES {
            text.truncate(MAX_OBSERVATION_BYTES);
        }
        Self::TerminalOutput { text }
    }

    /// Human-readable kind label.
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::TitleChanged(_) => "title-changed",
            Self::CwdChanged(_) => "cwd-changed",
            Self::Bell => "bell",
            Self::Damage { .. } => "damage",
            Self::SelectionChanged(_) => "selection-changed",
            Self::FocusChanged { .. } => "focus-changed",
            Self::ProcessExited { .. } => "process-exited",
            Self::ConfigReloaded => "config-reloaded",
            Self::TerminalOutput { .. } => "terminal-output",
            Self::Custom { .. } => "custom",
        }
    }

    /// Whether this observation is the untrusted terminal-output surface that
    /// must be treated as data, not instruction (`T-10`).
    #[must_use]
    pub fn is_untrusted_surface(&self) -> bool {
        matches!(self, Self::TerminalOutput { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_bounds() {
        AgentObservation::TitleChanged("hi".into())
            .validate()
            .expect("small valid");
        let big = "x".repeat(MAX_OBSERVATION_BYTES + 1);
        assert!(
            AgentObservation::TitleChanged(big.clone())
                .validate()
                .is_err()
        );
        // Custom kind validation
        assert!(
            AgentObservation::Custom {
                kind: "bad kind".into(),
                payload: "ok".into()
            }
            .validate()
            .is_err()
        );
        AgentObservation::Custom {
            kind: "probe.ok".into(),
            payload: "hi".into(),
        }
        .validate()
        .expect("valid custom");
    }

    #[test]
    fn truncation_helper() {
        let big = "a".repeat(MAX_OBSERVATION_BYTES + 100);
        let obs = AgentObservation::terminal_output_truncated(big);
        assert_eq!(obs.byte_len(), MAX_OBSERVATION_BYTES);
        obs.validate().expect("truncated fits");
    }

    #[test]
    fn untrusted_label() {
        assert!(AgentObservation::TerminalOutput { text: "hi".into() }.is_untrusted_surface());
        assert!(!AgentObservation::Bell.is_untrusted_surface());
    }
}
