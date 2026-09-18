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

/// Marker appended to untrusted terminal output when truncated at the cap.
pub const TERMINAL_OUTPUT_TRUNCATION_MARKER: &str = "\n[... truncated ...]";

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
            let budget =
                MAX_OBSERVATION_BYTES.saturating_sub(TERMINAL_OUTPUT_TRUNCATION_MARKER.len());
            let mut end = budget;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(TERMINAL_OUTPUT_TRUNCATION_MARKER);
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
        match &obs {
            AgentObservation::TerminalOutput { text } => {
                assert!(text.ends_with(TERMINAL_OUTPUT_TRUNCATION_MARKER));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn truncation_untruncated_preserves_text_and_no_marker() {
        // Small text
        let small = "short terminal output payload".to_string();
        let obs_small = AgentObservation::terminal_output_truncated(small.clone());
        assert_eq!(
            obs_small,
            AgentObservation::TerminalOutput {
                text: small.clone()
            }
        );
        match obs_small {
            AgentObservation::TerminalOutput { text } => {
                assert_eq!(text, small);
                assert!(!text.contains(TERMINAL_OUTPUT_TRUNCATION_MARKER));
            }
            _ => unreachable!(),
        }

        // Exact limit text
        let exact = "x".repeat(MAX_OBSERVATION_BYTES);
        let obs_exact = AgentObservation::terminal_output_truncated(exact.clone());
        assert_eq!(
            obs_exact,
            AgentObservation::TerminalOutput {
                text: exact.clone()
            }
        );
        match obs_exact {
            AgentObservation::TerminalOutput { text } => {
                assert_eq!(text, exact);
                assert!(!text.contains(TERMINAL_OUTPUT_TRUNCATION_MARKER));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn truncation_exceeding_max_bytes_budget_and_marker() {
        let big = "hello terminal world ".repeat(1000);
        assert!(big.len() > MAX_OBSERVATION_BYTES);
        let obs = AgentObservation::terminal_output_truncated(big);
        obs.validate().expect("truncated must be valid");
        assert!(obs.byte_len() <= MAX_OBSERVATION_BYTES);
        match obs {
            AgentObservation::TerminalOutput { text } => {
                assert!(text.ends_with(TERMINAL_OUTPUT_TRUNCATION_MARKER));
                assert!(text.len() <= MAX_OBSERVATION_BYTES);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn truncation_multibyte_utf8_char_boundaries() {
        // Test 2-byte ('é'), 3-byte ('中'), 4-byte ('🦀') characters arranged so
        // truncation budget hits right in the middle of a multi-byte code point.
        // None of these should panic and all must yield valid UTF-8 ending with marker.

        // 2-byte character: 'é' is 2 bytes (0xC3 0xA9)
        for prefix_len in 0..2 {
            let prefix = "a".repeat(prefix_len);
            let input = format!("{prefix}{}", "é".repeat(MAX_OBSERVATION_BYTES));
            let obs = AgentObservation::terminal_output_truncated(input);
            obs.validate().expect("2-byte UTF-8 valid");
            assert!(obs.byte_len() <= MAX_OBSERVATION_BYTES);
            match obs {
                AgentObservation::TerminalOutput { text } => {
                    assert!(text.ends_with(TERMINAL_OUTPUT_TRUNCATION_MARKER));
                    let content = &text[..text.len() - TERMINAL_OUTPUT_TRUNCATION_MARKER.len()];
                    assert!(content.starts_with(&prefix));
                    assert!(std::str::from_utf8(content.as_bytes()).is_ok());
                }
                _ => unreachable!(),
            }
        }

        // 3-byte character: '中' is 3 bytes (0xE4 0xB8 0xAD)
        for prefix_len in 0..3 {
            let prefix = "b".repeat(prefix_len);
            let input = format!("{prefix}{}", "中".repeat(MAX_OBSERVATION_BYTES));
            let obs = AgentObservation::terminal_output_truncated(input);
            obs.validate().expect("3-byte UTF-8 valid");
            assert!(obs.byte_len() <= MAX_OBSERVATION_BYTES);
            match obs {
                AgentObservation::TerminalOutput { text } => {
                    assert!(text.ends_with(TERMINAL_OUTPUT_TRUNCATION_MARKER));
                    let content = &text[..text.len() - TERMINAL_OUTPUT_TRUNCATION_MARKER.len()];
                    assert!(content.starts_with(&prefix));
                    assert!(std::str::from_utf8(content.as_bytes()).is_ok());
                }
                _ => unreachable!(),
            }
        }

        // 4-byte character: '🦀' is 4 bytes (0xF0 0x9F 0xA6 0x80)
        for prefix_len in 0..4 {
            let prefix = "c".repeat(prefix_len);
            let input = format!("{prefix}{}", "🦀".repeat(MAX_OBSERVATION_BYTES));
            let obs = AgentObservation::terminal_output_truncated(input);
            obs.validate().expect("4-byte UTF-8 valid");
            assert!(obs.byte_len() <= MAX_OBSERVATION_BYTES);
            match obs {
                AgentObservation::TerminalOutput { text } => {
                    assert!(text.ends_with(TERMINAL_OUTPUT_TRUNCATION_MARKER));
                    let content = &text[..text.len() - TERMINAL_OUTPUT_TRUNCATION_MARKER.len()];
                    assert!(content.starts_with(&prefix));
                    assert!(std::str::from_utf8(content.as_bytes()).is_ok());
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn untrusted_label() {
        assert!(AgentObservation::TerminalOutput { text: "hi".into() }.is_untrusted_surface());
        assert!(!AgentObservation::Bell.is_untrusted_surface());
    }
}
