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

/// Owned, bounded agent message.
///
/// All fields are owned (`String`, `Vec`, …) so messages are cloneable,
/// comparable, and sendable without lifetimes. No LLM I/O is performed here;
/// this struct is pure data that will be framed by the `bitty-ipc` transport
/// when the `OQ-018` RFC lands.
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
/// check owned outside this crate.
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
}

impl AgentMessage {
    /// Create and validate a message.
    pub fn new(
        sequence: u64,
        agent_id: AgentId,
        role: Role,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    ) -> Result<Self, AgentError> {
        let content = content.into();
        let m = Self {
            sequence,
            agent_id,
            role,
            content,
            tool_calls,
            tool_results,
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
        // Tool calls outside assistant turns are allowed structurally but
        // documented as discouraged; validate strictly only for tool role.
        // Keep the type permissive so headless tests can drive either pattern
        // without an artificial rejection, but real hosts should enforce
        // policy per the future CLI/IPC RFC.

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
    /// Heuristic stub: `User` messages that contain raw terminal text should
    /// be treated as untrusted. The definitive signal is outside this crate —
    /// observations arriving via `AgentObservation::TerminalOutput` — but this
    /// helper makes the invariant visible at the message layer.
    #[must_use]
    pub fn is_untrusted_content(&self) -> bool {
        // No content sniffing here beyond the type-level contract: callers
        // must label terminal output explicitly before placing it in a message.
        // This stub exists so policy checks can be added centrally later.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCall;

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
}
