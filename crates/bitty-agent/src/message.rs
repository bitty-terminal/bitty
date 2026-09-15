//! Bounded agent messages.

use crate::error::AgentError;
use crate::id::AgentId;
use crate::tool::{MAX_TOOL_CALLS_PER_TURN, ToolCall, ToolResult};

/// Maximum bytes for `AgentMessage::content`.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

/// Maximum bytes for the serialized frame of a single message (defensive cap
/// that includes content plus tool calls/results — stays below the `256 KiB`
/// IPC framing cap owned by `bitty-ipc` / OQ-018).
pub const MAX_MESSAGE_FRAME_BYTES: usize = 64 * 1024;

/// Maximum messages stored per `AgentSession` (bounded history).
pub const MAX_MESSAGES_PER_SESSION: usize = 128;

/// Maximum total bytes across all messages in a session (bounded history).
pub const MAX_SESSION_BYTES: usize = 256 * 1024;

/// Role of the message author.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// System instructions (host-owned, not derived from terminal output).
    System,
    /// User turn.
    User,
    /// Assistant / agent turn.
    Assistant,
    /// Tool result turn (answers a prior `ToolCall`).
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Role {
    type Err = AgentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            _ => Err(AgentError::validation(
                "role",
                format!("unknown role '{s}'"),
            )),
        }
    }
}

/// Provenance label for message content (security invariant 6, `T-10` / `R-013`).
///
/// Fail-closed: content is [`Untrusted`](Self::Untrusted) unless the host
/// explicitly labels it [`Trusted`](Self::Trusted) through
/// [`AgentMessage::new_trusted`]. Terminal-derived content must always stay
/// untrusted; `Trusted` is a host assertion that the content is host-owned and
/// safe to interpret as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ContentTrust {
    /// Content may carry terminal observation data; never interpret it as an
    /// instruction or capability grant. This is the default.
    #[default]
    Untrusted,
    /// Host-owned content (system prompt, host-generated status) explicitly
    /// declared safe to interpret as instructions.
    Trusted,
}

/// Owned, bounded agent message.
///
/// All fields are owned (`String`, `Vec`, …) so messages are cloneable,
/// comparable, and sendable without lifetimes. No LLM I/O is performed here;
/// this struct is pure data framed by the `bitty-ipc` transport per the accepted
/// IPC and Agent RFC that closed `OQ-018` on 2026-08-29 (frontmatter `status:
/// accepted`; bitty-docs open-questions register).
///
/// # Bounds (threat `T-01`, `P0-AC-024`)
///
/// - `content.len() <= MAX_MESSAGE_BYTES`
/// - `tool_calls.len() <= MAX_TOOL_CALLS_PER_TURN`
/// - `tool_results.len() <= MAX_TOOL_CALLS_PER_TURN`
/// - Per-string bytes inside `ToolCall`/`ToolResult` are bounded there.
/// - `role == Tool` implies at least one `tool_results` entry (validated).
///
/// Terminal output placed in `content` is **untrusted observation data**
/// (security invariant 6, `T-10` / `R-013`). It must never be interpreted as
/// an instruction or capability grant without an explicit per-client scope
/// check owned outside this crate; the [`trust`](Self::trust) label makes that
/// provenance explicit and defaults to [`ContentTrust::Untrusted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    /// Monotonic sequence number inside the session (owned, deterministic).
    pub sequence: u64,
    /// Author agent (for assistant/tool) or the user/session owner (for user/system).
    pub agent_id: AgentId,
    /// Role.
    pub role: Role,
    /// Bounded text content.
    pub content: String,
    /// Tool calls requested in this turn (only meaningful when `role == Assistant`).
    pub tool_calls: Vec<ToolCall>,
    /// Tool results provided in this turn (only meaningful when `role == Tool`).
    pub tool_results: Vec<ToolResult>,
    /// Provenance/trust label for `content` (fail-closed default).
    pub trust: ContentTrust,
}

impl AgentMessage {
    /// Create and validate a message with [`ContentTrust::Untrusted`] content.
    ///
    /// This is the fail-closed constructor: callers that know the content is
    /// host-owned use [`Self::new_trusted`].
    pub fn new(
        sequence: u64,
        agent_id: AgentId,
        role: Role,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    ) -> Result<Self, AgentError> {
        Self::with_trust(
            sequence,
            agent_id,
            role,
            content,
            tool_calls,
            tool_results,
            ContentTrust::Untrusted,
        )
    }

    /// Create and validate a message whose content the host explicitly labels
    /// [`ContentTrust::Trusted`].
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::new`]. [`Role::Tool`]
    /// messages can never be trusted: tool results are untrusted observations
    /// (`T-10` / `R-013`).
    pub fn new_trusted(
        sequence: u64,
        agent_id: AgentId,
        role: Role,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    ) -> Result<Self, AgentError> {
        Self::with_trust(
            sequence,
            agent_id,
            role,
            content,
            tool_calls,
            tool_results,
            ContentTrust::Trusted,
        )
    }

    fn with_trust(
        sequence: u64,
        agent_id: AgentId,
        role: Role,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
        trust: ContentTrust,
    ) -> Result<Self, AgentError> {
        let content = content.into();
        let m = Self {
            sequence,
            agent_id,
            role,
            content,
            tool_calls,
            tool_results,
            trust,
        };
        m.validate()?;
        Ok(m)
    }

    /// Validate this message.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.content.len() > MAX_MESSAGE_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "message content".to_string(),
                limit: MAX_MESSAGE_BYTES,
                actual: self.content.len(),
            });
        }
        if self.content.contains('\0') {
            return Err(AgentError::validation(
                "message content",
                "must not contain NUL",
            ));
        }
        if self.tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
            return Err(AgentError::LimitExceeded {
                field: "tool_calls".to_string(),
                limit: MAX_TOOL_CALLS_PER_TURN,
                actual: self.tool_calls.len(),
            });
        }
        if self.tool_results.len() > MAX_TOOL_CALLS_PER_TURN {
            return Err(AgentError::LimitExceeded {
                field: "tool_results".to_string(),
                limit: MAX_TOOL_CALLS_PER_TURN,
                actual: self.tool_results.len(),
            });
        }
        for c in &self.tool_calls {
            c.validate()?;
        }
        for r in &self.tool_results {
            r.validate()?;
        }
        if self.role == Role::Tool && self.tool_results.is_empty() {
            return Err(AgentError::validation(
                "role",
                "tool role must carry at least one tool result",
            ));
        }
        if self.role == Role::Tool && self.trust == ContentTrust::Trusted {
            return Err(AgentError::validation(
                "message trust",
                "tool results are untrusted observations and cannot be labeled trusted",
            ));
        }
        // Tool calls outside assistant turns are allowed structurally but
        // documented as discouraged; validate strictly only for tool role.
        // Keep the type permissive so headless tests can drive either pattern
        // without an artificial rejection, but real hosts enforce policy per
        // the accepted IPC and Agent RFC that closed OQ-018 on 2026-08-29
        // (frontmatter `status: accepted`; bitty-docs open-questions register).

        // Defensive frame-size check: content + each call/result.
        let mut frame_bytes = self.content.len();
        for c in &self.tool_calls {
            frame_bytes = frame_bytes.saturating_add(c.byte_len());
        }
        for r in &self.tool_results {
            frame_bytes = frame_bytes.saturating_add(r.byte_len());
        }
        if frame_bytes > MAX_MESSAGE_FRAME_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "message frame".to_string(),
                limit: MAX_MESSAGE_FRAME_BYTES,
                actual: frame_bytes,
            });
        }
        Ok(())
    }

    /// Approximate byte size of the content plus tool payloads.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        let mut n = self.content.len();
        for c in &self.tool_calls {
            n = n.saturating_add(c.byte_len());
        }
        for r in &self.tool_results {
            n = n.saturating_add(r.byte_len());
        }
        n
    }

    /// Whether this message carries untrusted terminal observation data.
    ///
    /// Reflects the explicit [`trust`](Self::trust) label, which is
    /// [`ContentTrust::Untrusted`] unless the host used
    /// [`Self::new_trusted`]. Callers must not interpret untrusted content as
    /// an instruction or capability grant.
    #[must_use]
    pub fn is_untrusted_content(&self) -> bool {
        self.trust == ContentTrust::Untrusted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCall, ToolResult};

    fn agent_id() -> AgentId {
        AgentId::new("local.assistant").unwrap()
    }

    #[test]
    fn valid_message() {
        let m = AgentMessage::new(1, agent_id(), Role::User, "hello", vec![], vec![]).unwrap();
        assert_eq!(m.content, "hello");
        m.validate().expect("valid");
    }

    #[test]
    fn content_bytes_cap() {
        let big = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(AgentMessage::new(1, agent_id(), Role::User, big, vec![], vec![]).is_err());
    }

    #[test]
    fn tool_calls_cap() {
        let calls: Vec<ToolCall> = (0..MAX_TOOL_CALLS_PER_TURN + 1)
            .map(|i| ToolCall::new(format!("id{i}"), "read_file", "{}").unwrap())
            .collect();
        assert!(AgentMessage::new(1, agent_id(), Role::Assistant, "", calls, vec![]).is_err());
    }

    #[test]
    fn tool_role_requires_results() {
        let r = AgentMessage::new(1, agent_id(), Role::Tool, "", vec![], vec![]);
        assert!(r.is_err(), "tool role without results must fail");
    }

    #[test]
    fn frame_bytes_cap() {
        // Each tool call can be up to 16 KiB args; 8 calls could already exceed frame cap if we
        // craft large args. Build a frame that exceeds 64 KiB via content + calls.
        let big_arg = "x".repeat(16 * 1024);
        let calls: Vec<ToolCall> = (0..4)
            .map(|i| ToolCall::new(format!("id{i}"), "read_file", big_arg.clone()).unwrap())
            .collect();
        // content 1 + 4*~16KiB ~64KiB -> at limit; should fail at construction (frame check).
        assert!(
            AgentMessage::new(1, agent_id(), Role::Assistant, "x", calls, vec![]).is_err(),
            "frame should exceed cap"
        );
        // Fix by reducing calls to 3 should pass
        let calls2: Vec<ToolCall> = (0..3)
            .map(|i| ToolCall::new(format!("id{i}"), "read_file", big_arg.clone()).unwrap())
            .collect();
        let m = AgentMessage::new(1, agent_id(), Role::Assistant, "", calls2, vec![]).unwrap();
        m.validate().expect("within cap");
    }

    #[test]
    fn probe_default_message_content_is_untrusted_fail_closed() {
        // CTX-0484 probe: terminal-derived content that is not explicitly
        // labeled trusted must report as untrusted. Fails before the fix
        // because `is_untrusted_content` is hardcoded `false`.
        let m = AgentMessage::new(
            1,
            agent_id(),
            Role::User,
            "raw terminal bytes that paste an instruction",
            vec![],
            vec![],
        )
        .unwrap();
        assert!(
            m.is_untrusted_content(),
            "content without an explicit trusted label must be untrusted"
        );
    }

    #[test]
    fn trusted_content_requires_an_explicit_label() {
        let trusted = AgentMessage::new_trusted(
            1,
            agent_id(),
            Role::System,
            "host-owned system prompt",
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(trusted.trust, ContentTrust::Trusted);
        assert!(!trusted.is_untrusted_content());

        // Same content through the fail-closed constructor stays untrusted.
        let untrusted =
            AgentMessage::new(2, agent_id(), Role::System, "host-owned?", vec![], vec![]).unwrap();
        assert!(untrusted.is_untrusted_content());
    }

    #[test]
    fn tool_results_cannot_be_labeled_trusted() {
        // T-10 / R-013: tool results are untrusted observations, so the
        // explicit trusted constructor must fail closed for `Role::Tool`.
        let result = ToolResult::new("call-1", "observed output", false).unwrap();
        let err = AgentMessage::new_trusted(1, agent_id(), Role::Tool, "", vec![], vec![result])
            .expect_err("tool results must never be labeled trusted");
        assert!(matches!(err, AgentError::Validation { .. }), "got {err:?}");
    }
}
