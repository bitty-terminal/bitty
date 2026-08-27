//! Owned agent session — headless, bounded, stub tool dispatch.
//!
//! A session owns its `AgentId`, declared tools (`ToolRegistry`), message
//! history, and the bounded side queue that observes terminal/runtime events
//! per `ADR-0003` rule 4. There is no LLM I/O, no process spawn, no network,
//! and no window/GPU coupling. The session is `std`-only and is exercised
//! headlessly on both the Linux CI and the `windows-latest` job.

use crate::error::AgentError;
use crate::id::AgentId;
use crate::message::{AgentMessage, MAX_MESSAGES_PER_SESSION, MAX_SESSION_BYTES, Role};
use crate::observation::AgentObservation;
use crate::queue::SideQueue;
use crate::tool::{ToolCall, ToolRegistry, ToolResult, ToolSpec};

/// Default side-queue capacity (candidate, not normative; `OQ-014` family).
pub const DEFAULT_SIDE_QUEUE_CAPACITY: usize = 64;

/// Default tool-registry cap is owned by `tool::MAX_TOOLS_PER_AGENT` (32).
/// Session history caps are owned by `message::*` (`128` messages / `256 KiB`).
/// Lifecycle state of an agent session (draft, not normative).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// Session created, no assistant turn yet.
    Created,
    /// At least one message has been recorded; agent may produce turns.
    Running,
    /// Last assistant turn requested tool calls and the host is expected to
    /// answer with a `Role::Tool` message.
    WaitingToolResult,
    /// Session completed normally (host or user terminated).
    Completed,
    /// Session failed (validation, budget, or host error).
    Failed,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::WaitingToolResult => "waiting-tool-result",
            Self::Completed => "completed",
            Self::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// Owned draft agent session.
///
/// # Ownership rules (ADR-0003 / ADR-0004)
///
/// - Depends on no other workspace crate (`std`-only). The `bitty-ipc` wire
///   transport (bounded `256 KiB` framing, request timeouts, stdio stub per
///   `OQ-018`) will map `AgentMessage`/`AgentObservation` onto IPC frames
///   without redefining caps. This keeps the crate headless and avoids a
///   hard path dependency while `CTX-0031` lands in parallel.
/// - Never holds GPU objects, window handles, PTY file descriptors, or hot-path
///   Rust objects. The only observation path is the bounded `SideQueue`.
/// - `#![forbid(unsafe_code)]`, `MSRV 1.85`, `edition = "2024"`.
/// - All structures are owned (`String`, `Vec`, …), never `&str`.
#[derive(Debug)]
pub struct AgentSession {
    agent_id: AgentId,
    state: SessionState,
    tools: ToolRegistry,
    messages: Vec<AgentMessage>,
    session_bytes: usize,
    side_queue: SideQueue<AgentObservation>,
    next_sequence: u64,
}

impl AgentSession {
    /// Create a new session.
    pub fn new(agent_id: AgentId, side_capacity: usize) -> Self {
        Self {
            agent_id,
            state: SessionState::Created,
            tools: ToolRegistry::new(),
            messages: Vec::new(),
            session_bytes: 0,
            side_queue: SideQueue::new(side_capacity),
            next_sequence: 1,
        }
    }

    /// Create with an explicit tool registry.
    pub fn with_tools(
        agent_id: AgentId,
        tools: ToolRegistry,
        side_capacity: usize,
    ) -> Result<Self, AgentError> {
        // Tools already validated by registry constructor.
        Ok(Self {
            agent_id,
            state: SessionState::Created,
            tools,
            messages: Vec::new(),
            session_bytes: 0,
            side_queue: SideQueue::new(side_capacity),
            next_sequence: 1,
        })
    }

    /// Session owner id.
    #[must_use]
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// Whether the session is terminal (`Completed` or `Failed`).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, SessionState::Completed | SessionState::Failed)
    }

    /// Declared tools (read-only).
    #[must_use]
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Insert a tool spec (validates, checks duplicates and cap).
    pub fn declare_tool(&mut self, spec: ToolSpec) -> Result<(), AgentError> {
        if self.is_terminal() {
            return Err(AgentError::session(format!(
                "cannot declare tool in terminal state {}",
                self.state
            )));
        }
        self.tools.insert(spec)
    }

    /// Message history (read-only, ordered by sequence).
    #[must_use]
    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    /// Number of messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// True when no message has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Total bytes across all messages.
    #[must_use]
    pub const fn session_bytes(&self) -> usize {
        self.session_bytes
    }

    /// Side-queue capacity.
    #[must_use]
    pub const fn side_capacity(&self) -> usize {
        self.side_queue.capacity()
    }

    /// Side-queue length.
    #[must_use]
    pub fn side_len(&self) -> usize {
        self.side_queue.len()
    }

    /// Side-queue dropped counter.
    #[must_use]
    pub fn side_dropped(&self) -> u64 {
        self.side_queue.dropped()
    }

    /// Push an observation into the bounded side queue (never blocks producer).
    ///
    /// When the queue is full the oldest observation is dropped and the counter
    /// increments — mirroring the cold-queue / side-queue policy in
    /// `bitty-runtime` and `bitty-plugin-host`. The queue never holds hot-path
    /// objects (threat `T-07`).
    pub fn push_observation(&mut self, obs: AgentObservation) -> Result<(), AgentError> {
        obs.validate()?;
        self.side_queue.push(obs);
        Ok(())
    }

    /// Drain all side-queue observations in FIFO order.
    pub fn drain_observations(&mut self) -> Vec<AgentObservation> {
        self.side_queue.drain()
    }

    /// Drain up to `limit` observations (bounded batch).
    pub fn drain_observations_bounded(&mut self, limit: usize) -> Vec<AgentObservation> {
        self.side_queue.drain_bounded(limit)
    }

    /// Record a message (validates bounds, advances state machine).
    ///
    /// Sequence numbers are assigned by the session monotonically; the caller
    /// supplies `role`, `content`, `tool_calls`, and `tool_results` without a
    /// sequence. Tool calls are validated against the declared registry
    /// syntactically (unknown tool -> error), but no tool is executed.
    pub fn push_message(
        &mut self,
        role: Role,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    ) -> Result<u64, AgentError> {
        if self.is_terminal() {
            return Err(AgentError::session(format!(
                "cannot push message in terminal state {}",
                self.state
            )));
        }
        if self.messages.len() >= MAX_MESSAGES_PER_SESSION {
            return Err(AgentError::LimitExceeded {
                field: "messages per session".to_string(),
                limit: MAX_MESSAGES_PER_SESSION,
                actual: self.messages.len() + 1,
            });
        }
        // Validate tool calls against registry (syntactic, no I/O).
        for c in &tool_calls {
            if !self.tools.contains(&c.name) && !self.tools.specs().is_empty() {
                // When a registry is non-empty, unknown tools are rejected.
                // When empty (no tools declared), allow any call to keep the
                // stub testable without pre-declaration — but still validate
                // per-call bounds.
                c.validate()?;
                return Err(AgentError::Tool {
                    message: format!("unknown tool '{}'", c.name),
                });
            }
            c.validate()?;
        }
        for r in &tool_results {
            r.validate()?;
        }

        let seq = self.next_sequence;
        let msg = AgentMessage::new(
            seq,
            self.agent_id.clone(),
            role,
            content,
            tool_calls,
            tool_results,
        )?;
        let msg_bytes = msg.byte_len();
        if self.session_bytes.saturating_add(msg_bytes) > MAX_SESSION_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "session bytes".to_string(),
                limit: MAX_SESSION_BYTES,
                actual: self.session_bytes + msg_bytes,
            });
        }

        // State machine transitions.
        match (self.state, role, msg.tool_calls.is_empty()) {
            (SessionState::Created, _, _) => self.state = SessionState::Running,
            (SessionState::Running, Role::Assistant, false) => {
                self.state = SessionState::WaitingToolResult;
            }
            (SessionState::WaitingToolResult, Role::Tool, _) => {
                self.state = SessionState::Running;
            }
            (SessionState::WaitingToolResult, _, _) if role != Role::Tool => {
                // Non-tool answer while awaiting tool results keeps the
                // session in Running (policy: host may interleave user turns).
                self.state = SessionState::Running;
            }
            _ => {}
        }

        self.messages.push(msg);
        self.session_bytes = self.session_bytes.saturating_add(msg_bytes);
        self.next_sequence = self.next_sequence.wrapping_add(1);
        Ok(seq)
    }

    /// Convenience: push a `Role::User` message.
    pub fn push_user(&mut self, content: impl Into<String>) -> Result<u64, AgentError> {
        self.push_message(Role::User, content, vec![], vec![])
    }

    /// Convenience: push a `Role::Assistant` message optionally with tool calls.
    pub fn push_assistant(
        &mut self,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Result<u64, AgentError> {
        self.push_message(Role::Assistant, content, tool_calls, vec![])
    }

    /// Convenience: push a `Role::Tool` result message.
    pub fn push_tool_results(&mut self, results: Vec<ToolResult>) -> Result<u64, AgentError> {
        if results.is_empty() {
            return Err(AgentError::validation(
                "tool results",
                "must provide at least one result",
            ));
        }
        self.push_message(Role::Tool, "", vec![], results)
    }

    /// Stub tool dispatch: syntactically validates each call and returns
    /// deterministic placeholder results without any I/O, LLM, or capability
    /// grant.
    ///
    /// Real dispatch will be capability-checked by the host outside this
    /// crate. This method exists so tests can drive the `Assistant -> Tool`
    /// loop headlessly without describing it as implemented host behavior.
    pub fn stub_dispatch(&self, calls: &[ToolCall]) -> Result<Vec<ToolResult>, AgentError> {
        let mut out = Vec::with_capacity(calls.len());
        for c in calls {
            out.push(self.tools.stub_invoke(c)?);
        }
        Ok(out)
    }

    /// Mark the session as completed normally.
    pub fn complete(&mut self) -> Result<(), AgentError> {
        if self.is_terminal() {
            return Err(AgentError::session(format!(
                "already terminal ({})",
                self.state
            )));
        }
        self.state = SessionState::Completed;
        Ok(())
    }

    /// Mark the session as failed (with reason for diagnostics — bounded).
    pub fn fail(&mut self, reason: impl Into<String>) -> Result<(), AgentError> {
        if self.is_terminal() {
            return Err(AgentError::session(format!(
                "already terminal ({})",
                self.state
            )));
        }
        let reason = reason.into();
        if reason.len() > 1024 {
            return Err(AgentError::LimitExceeded {
                field: "fail reason".to_string(),
                limit: 1024,
                actual: reason.len(),
            });
        }
        self.state = SessionState::Failed;
        let _ = reason;
        Ok(())
    }

    /// Clear side queue and reset dropped counter.
    pub fn clear_observations(&mut self) {
        self.side_queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSpec;

    fn agent_id() -> AgentId {
        AgentId::new("local.assistant").unwrap()
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "stub".into(),
            input_schema: "{}".into(),
        }
    }

    #[test]
    fn lifecycle_user_assistant_tool() {
        let mut s = AgentSession::new(agent_id(), 4);
        assert_eq!(s.state(), SessionState::Created);
        s.push_user("hi").unwrap();
        assert_eq!(s.state(), SessionState::Running);
        // Declare a tool.
        s.declare_tool(spec("read_file")).unwrap();
        let call = crate::tool::ToolCall::new("id1", "read_file", "{}").unwrap();
        s.push_assistant("calling", vec![call.clone()]).unwrap();
        assert_eq!(s.state(), SessionState::WaitingToolResult);
        // Dispatch stub.
        let results = s.stub_dispatch(&[call]).unwrap();
        s.push_tool_results(results).unwrap();
        assert_eq!(s.state(), SessionState::Running);
        s.complete().unwrap();
        assert!(s.is_terminal());
        assert!(s.push_user("after complete").is_err());
    }

    #[test]
    fn side_queue_bounded() {
        let mut s = AgentSession::new(agent_id(), 2);
        s.push_observation(AgentObservation::Bell).unwrap();
        s.push_observation(AgentObservation::Bell).unwrap();
        s.push_observation(AgentObservation::TitleChanged("a".into()))
            .unwrap(); // evicts oldest
        assert_eq!(s.side_len(), 2);
        assert_eq!(s.side_dropped(), 1);
        let drained = s.drain_observations();
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn unknown_tool_rejected_when_registry_nonempty() {
        let mut s = AgentSession::new(agent_id(), 4);
        s.declare_tool(spec("read_file")).unwrap();
        let bad = crate::tool::ToolCall::new("id1", "unknown_tool", "{}").unwrap();
        assert!(s.push_assistant("hi", vec![bad]).is_err());
    }

    #[test]
    fn session_bytes_cap() {
        let mut s = AgentSession::new(agent_id(), 4);
        let big = "x".repeat(32 * 1024);
        // Each message up to 32 KiB; 8 such messages reaches 256 KiB session cap.
        for _ in 0..8 {
            s.push_user(big.clone()).unwrap();
        }
        // 9th should exceed cap (9*32KiB = 288KiB > 256KiB)
        assert!(s.push_user(big).is_err());
    }

    #[test]
    fn messages_cap() {
        let mut s = AgentSession::new(agent_id(), 4);
        for _ in 0..MAX_MESSAGES_PER_SESSION {
            s.push_user("hi").unwrap();
        }
        assert!(s.push_user("one more").is_err());
    }
}
