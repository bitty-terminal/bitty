//! Tool-call stubs — no LLM I/O, no process spawn, no network.
//!
//! This module owns the **vocabulary** that will travel over the `bitty-ipc`
//! transport when the `OQ-018` RFC lands. It validates and stores tool
//! declarations and individual calls/results, but it never contacts an LLM,
//! never executes a tool, and never performs I/O. Real dispatch will be owned
//! by the runtime's capability-checked service layer (or by an external
//! helper process behind `bitty-ipc`), not by this crate.
//!
//! # Bounds (threat `T-01`)
//!
//! Every field is bounded and owned so untrusted agent payloads cannot grow
//! memory without limit. The side queue and message caps are independent — a
//! flood of tool calls cannot exceed `MAX_TOOL_CALLS_PER_TURN` and each
//! argument/result is capped.

use crate::error::AgentError;

/// Maximum tool name length (bytes).
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Maximum tool description length (bytes).
pub const MAX_TOOL_DESCRIPTION_LEN: usize = 1024;

/// Maximum input-schema length (bytes) — JSON Schema text, bounded.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 8 * 1024;

/// Maximum tool arguments length (bytes) — JSON text, bounded.
pub const MAX_TOOL_ARGS_BYTES: usize = 16 * 1024;

/// Maximum tool result length (bytes).
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Maximum tool calls per turn / per message.
pub const MAX_TOOL_CALLS_PER_TURN: usize = 8;

/// Maximum declared tools per agent session.
pub const MAX_TOOLS_PER_AGENT: usize = 32;

/// Maximum tool-call id length.
pub const MAX_TOOL_CALL_ID_LEN: usize = 128;

/// Owned description of a tool an agent may call (stub, not executable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Tool name, e.g. `read_file` or `run_command` (candidate names, not normative).
    pub name: String,
    /// Human-readable description (bounded).
    pub description: String,
    /// JSON Schema for arguments as bounded text (no schema validation beyond bounds here).
    pub input_schema: String,
}

impl ToolSpec {
    /// Validate this spec.
    pub fn validate(&self) -> Result<(), AgentError> {
        validate_tool_name(&self.name)?;
        if self.description.len() > MAX_TOOL_DESCRIPTION_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool description".to_string(),
                limit: MAX_TOOL_DESCRIPTION_LEN,
                actual: self.description.len(),
            });
        }
        if self.input_schema.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool input_schema".to_string(),
                limit: MAX_TOOL_SCHEMA_BYTES,
                actual: self.input_schema.len(),
            });
        }
        if self.description.contains('\0') || self.input_schema.contains('\0') {
            return Err(AgentError::validation("tool spec", "must not contain NUL"));
        }
        Ok(())
    }
}

/// A single tool call requested by the agent (stub, not executed here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Stable call id (owned, bounded).
    pub id: String,
    /// Tool name (must match a declared `ToolSpec::name` to be considered valid — not enforced here).
    pub name: String,
    /// Arguments as bounded JSON text (untrusted, never `eval`ed here).
    pub arguments: String,
}

impl ToolCall {
    /// Create and validate a tool call.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let id = id.into();
        let name = name.into();
        let arguments = arguments.into();
        let call = Self {
            id,
            name,
            arguments,
        };
        call.validate()?;
        Ok(call)
    }

    /// Validate this call.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.id.is_empty() {
            return Err(AgentError::validation("tool call id", "must not be empty"));
        }
        if self.id.len() > MAX_TOOL_CALL_ID_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool call id".to_string(),
                limit: MAX_TOOL_CALL_ID_LEN,
                actual: self.id.len(),
            });
        }
        if self.id.contains('\0') {
            return Err(AgentError::validation(
                "tool call id",
                "must not contain NUL",
            ));
        }
        validate_tool_name(&self.name)?;
        if self.arguments.len() > MAX_TOOL_ARGS_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool arguments".to_string(),
                limit: MAX_TOOL_ARGS_BYTES,
                actual: self.arguments.len(),
            });
        }
        if self.arguments.contains('\0') {
            return Err(AgentError::validation(
                "tool arguments",
                "must not contain NUL",
            ));
        }
        Ok(())
    }

    /// Approximate byte size (for batch budgets).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.id.len() + self.name.len() + self.arguments.len()
    }
}

/// Result of a tool call (stub, produced by the host outside this crate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Call id this result answers.
    pub call_id: String,
    /// Bounded result content (JSON or text, untrusted).
    pub content: String,
    /// Whether the tool reported an error (host-owned flag, not inferred from content).
    pub is_error: bool,
}

impl ToolResult {
    /// Create and validate a tool result.
    pub fn new(
        call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<Self, AgentError> {
        let call_id = call_id.into();
        let content = content.into();
        let r = Self {
            call_id,
            content,
            is_error,
        };
        r.validate()?;
        Ok(r)
    }

    /// Validate this result.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.call_id.is_empty() {
            return Err(AgentError::validation(
                "tool result call_id",
                "must not be empty",
            ));
        }
        if self.call_id.len() > MAX_TOOL_CALL_ID_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool result call_id".to_string(),
                limit: MAX_TOOL_CALL_ID_LEN,
                actual: self.call_id.len(),
            });
        }
        if self.content.len() > MAX_TOOL_RESULT_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool result".to_string(),
                limit: MAX_TOOL_RESULT_BYTES,
                actual: self.content.len(),
            });
        }
        if self.call_id.contains('\0') || self.content.contains('\0') {
            return Err(AgentError::validation(
                "tool result",
                "must not contain NUL",
            ));
        }
        Ok(())
    }

    /// Approximate byte size.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.call_id.len() + self.content.len()
    }
}

/// In-crate stub registry that tracks declared tools and validates calls
/// syntactically without ever executing them.
///
/// Real execution (capability-checked, rate-limited, per-client scoped) will
/// be owned by the runtime/IPc layer. This registry is intentionally
/// headless and `std`-only so the vocabulary can be tested without a display
/// server, GPU, or network.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Create from specs (validates each).
    pub fn from_specs(specs: Vec<ToolSpec>) -> Result<Self, AgentError> {
        if specs.len() > MAX_TOOLS_PER_AGENT {
            return Err(AgentError::LimitExceeded {
                field: "tools per agent".to_string(),
                limit: MAX_TOOLS_PER_AGENT,
                actual: specs.len(),
            });
        }
        for s in &specs {
            s.validate()?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &specs {
            if !seen.insert(s.name.clone()) {
                return Err(AgentError::Duplicate {
                    kind: "tool".to_string(),
                    value: s.name.clone(),
                });
            }
        }
        Ok(Self { specs })
    }

    /// Insert a spec (validates, checks duplicates and cap).
    pub fn insert(&mut self, spec: ToolSpec) -> Result<(), AgentError> {
        spec.validate()?;
        if self.specs.len() >= MAX_TOOLS_PER_AGENT {
            return Err(AgentError::LimitExceeded {
                field: "tools per agent".to_string(),
                limit: MAX_TOOLS_PER_AGENT,
                actual: self.specs.len() + 1,
            });
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err(AgentError::Duplicate {
                kind: "tool".to_string(),
                value: spec.name,
            });
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Declared specs (read-only).
    #[must_use]
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Whether a tool name is declared.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// Syntactically validate a call against the registry (declared-name check +
    /// per-call bounds). No I/O, no execution.
    pub fn validate_call(&self, call: &ToolCall) -> Result<(), AgentError> {
        call.validate()?;
        if !self.contains(&call.name) {
            return Err(AgentError::Tool {
                message: format!("unknown tool '{}'", call.name),
            });
        }
        Ok(())
    }

    /// Stub `invoke` — never contacts an LLM or the filesystem. Returns a
    /// deterministic placeholder result that records the call as observed.
    ///
    /// Callers that need real tool execution must go through the capability-
    /// checked host outside this crate.
    pub fn stub_invoke(&self, call: &ToolCall) -> Result<ToolResult, AgentError> {
        self.validate_call(call)?;
        // Deterministic stub payload: no wall-clock, no randomness.
        let content = format!(
            "{{\"stub\":true,\"tool\":\"{}\",\"args_len\":{}}}",
            call.name,
            call.arguments.len()
        );
        ToolResult::new(call.id.clone(), content, false)
    }
}

fn validate_tool_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty() {
        return Err(AgentError::validation("tool name", "must not be empty"));
    }
    if name.len() > MAX_TOOL_NAME_LEN {
        return Err(AgentError::LimitExceeded {
            field: "tool name".to_string(),
            limit: MAX_TOOL_NAME_LEN,
            actual: name.len(),
        });
    }
    if name.contains('\0') {
        return Err(AgentError::validation("tool name", "must not contain NUL"));
    }
    // Grammar: start with [a-z], then [a-z0-9_.-]
    let first = name.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(AgentError::validation(
            "tool name",
            "must start with lowercase letter",
        ));
    }
    for b in name.bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-') {
            return Err(AgentError::validation("tool name", "must be [a-z0-9_.-]"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "stub".into(),
            input_schema: "{}".into(),
        }
    }

    #[test]
    fn valid_tool_names() {
        for n in ["read_file", "run-command", "a.b", "tool1", "my-tool_2"] {
            validate_tool_name(n).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(validate_tool_name("").is_err());
        assert!(validate_tool_name("1tool").is_err());
        assert!(validate_tool_name("Tool").is_err());
        assert!(validate_tool_name("tool with space").is_err());
        assert!(validate_tool_name(&"a".repeat(MAX_TOOL_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn registry_insert_and_duplicate() {
        let mut r = ToolRegistry::new();
        r.insert(spec("read_file")).expect("insert");
        assert!(r.contains("read_file"));
        assert!(matches!(
            r.insert(spec("read_file")),
            Err(AgentError::Duplicate { .. })
        ));
    }

    #[test]
    fn validate_call_unknown_tool() {
        let r = ToolRegistry::from_specs(vec![spec("read_file")]).unwrap();
        let call = ToolCall::new("id1", "unknown_tool", "{}").unwrap();
        assert!(r.validate_call(&call).is_err());
    }

    #[test]
    fn stub_invoke_deterministic() {
        let r = ToolRegistry::from_specs(vec![spec("read_file")]).unwrap();
        let call = ToolCall::new("id1", "read_file", "{\"path\":\"/tmp/x\"}").unwrap();
        let res = r.stub_invoke(&call).unwrap();
        assert_eq!(res.call_id, "id1");
        assert!(!res.is_error);
        assert!(res.content.contains("\"stub\":true"));
        // Same call -> same result deterministically.
        let res2 = r.stub_invoke(&call).unwrap();
        assert_eq!(res, res2);
    }

    #[test]
    fn args_bytes_cap() {
        let big = "x".repeat(MAX_TOOL_ARGS_BYTES + 1);
        assert!(ToolCall::new("id", "read_file", big).is_err());
    }

    #[test]
    fn result_bytes_cap() {
        let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
        assert!(ToolResult::new("id", big, false).is_err());
    }
}
