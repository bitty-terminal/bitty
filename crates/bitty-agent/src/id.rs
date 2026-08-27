//! Owned agent identity.

use crate::error::AgentError;

/// Maximum agent id length (bytes).
pub const MAX_AGENT_ID_LEN: usize = 128;

/// Maximum segment length inside an agent id.
pub const MAX_AGENT_ID_SEGMENT_LEN: usize = 64;

/// Owner-qualified stable agent identifier, `owner.name`, e.g. `local.assistant`.
///
/// Validation mirrors `PluginId` (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*$`) so the
/// namespace stays disjoint and attributable, but the type is distinct so
/// plugin and agent identities never unify by accident.
///
/// # Bounds (threat `T-01`)
///
/// Length is bounded at `MAX_AGENT_ID_LEN`; each segment is bounded at
/// `MAX_AGENT_ID_SEGMENT_LEN`. No heap growth depends on untrusted input
/// beyond these caps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(String);

impl AgentId {
    /// Parse and validate an agent id.
    pub fn new(raw: &str) -> Result<Self, AgentError> {
        validate_agent_id(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// Raw id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Owner segment (before dot).
    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.split_once('.').map(|(a, _)| a).unwrap_or(&self.0)
    }

    /// Name segment (after dot).
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('.').map(|(_, b)| b).unwrap_or("")
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for AgentId {
    type Err = AgentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

fn validate_agent_id(raw: &str) -> Result<(), AgentError> {
    if raw.is_empty() {
        return Err(AgentError::InvalidAgentId {
            id: raw.to_string(),
            reason: "agent id must not be empty".to_string(),
        });
    }
    if raw.len() > MAX_AGENT_ID_LEN {
        return Err(AgentError::InvalidAgentId {
            id: raw.to_string(),
            reason: format!("agent id too long (max {MAX_AGENT_ID_LEN})"),
        });
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(AgentError::InvalidAgentId {
            id: raw.to_string(),
            reason: "agent id must not contain whitespace".to_string(),
        });
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 2 {
        return Err(AgentError::InvalidAgentId {
            id: raw.to_string(),
            reason: "agent id must be exactly owner.name (one dot)".to_string(),
        });
    }
    for seg in &parts {
        if seg.is_empty() {
            return Err(AgentError::InvalidAgentId {
                id: raw.to_string(),
                reason: "agent id segment must not be empty".to_string(),
            });
        }
        if seg.len() > MAX_AGENT_ID_SEGMENT_LEN {
            return Err(AgentError::InvalidAgentId {
                id: raw.to_string(),
                reason: format!("segment too long (max {MAX_AGENT_ID_SEGMENT_LEN})"),
            });
        }
        let first = seg.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return Err(AgentError::InvalidAgentId {
                id: raw.to_string(),
                reason: "segment must start with lowercase letter".to_string(),
            });
        }
        for b in seg.bytes() {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
                return Err(AgentError::InvalidAgentId {
                    id: raw.to_string(),
                    reason: "segment must be [a-z0-9_-]".to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids() {
        for id in ["local.assistant", "bitty.agent-1", "x.y_z-0"] {
            AgentId::new(id).unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }

    #[test]
    fn rejects_invalid() {
        for (id, _) in [
            ("", "empty"),
            ("local", "no dot"),
            ("Local.assistant", "uppercase"),
            ("local.assistant.extra", "two dots"),
            ("1local.assistant", "digit start"),
            ("local.1assistant", "digit start second"),
            ("local.assistant ", "whitespace"),
            ("a.b", "too short actually valid"), // a.b is valid (1 char segments allowed)
        ]
        .iter()
        .take(7)
        {
            assert!(AgentId::new(id).is_err(), "should reject {id}");
        }
        // a.b is actually valid
        AgentId::new("a.b").expect("a.b valid");
    }

    #[test]
    fn max_len_bound() {
        let long = format!("{}.{}", "a".repeat(64), "b".repeat(64));
        // 64+1+64 =129 >128 so should fail
        assert!(AgentId::new(&long).is_err());
        let ok = format!("{}.{}", "a".repeat(63), "b".repeat(64));
        assert_eq!(ok.len(), 128);
        AgentId::new(&ok).expect("128 valid");
    }
}
