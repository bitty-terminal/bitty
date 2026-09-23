//! Candidate role contract for multi-agent work (OQ-057).
//!
//! `OQ-057` is still open: no ruling fixes the role-to-authority map,
//! per-role prompt binding, subagent dispatch limits, or the enforcement
//! points (including the execution-sandbox layer) for multi-agent work.
//! This module records the candidate direction only — Commander,
//! Implementer, Tester, Reviewer — as pure, bounded, fail-closed data.
//!
//! Nothing here is wired into any live path: roles never grant
//! authority by themselves, prompts never confer capability (there is
//! deliberately no `bind_prompt` constructor), and delegation only
//! narrows through the accepted intersection engine
//! ([`crate::effective`]). The chat-message [`Role`](bitty-agent)
//! distinction stays untouched; this kernel only answers "may role R
//! act at enforcement point P, and under which sandbox restrictions".
//! Unknown roles or points deny rather than default.
//!
//! # Non-goals
//!
//! Model routing, memory, skill growth, dispatch budgets, and the shell
//! write path (CRE-5) stay undecided until the OQ-057 ruling. There is
//! no `unsafe`, no I/O, and no new dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::capability::CapabilityFamily;
use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of a role label accepted by [`AgentRole::parse`].
pub const MAX_ROLE_LABEL_BYTES: usize = 32;

// ── roles ─────────────────────────────────────────────────────────────────

/// Candidate multi-agent roles (OQ-057).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    /// Plans and delegates; full host-mediated authority subject to grants.
    Commander,
    /// Implements tasks; may not re-delegate beyond its task grant.
    Implementer,
    /// Runs checks; sandboxed, no delegation, no writes outside scratch.
    Tester,
    /// Reads and reports; observation only, no mutation, no delegation.
    Reviewer,
}

impl AgentRole {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commander => "commander",
            Self::Implementer => "implementer",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
        }
    }

    /// Parse a role label; unknown roles fail closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_ROLE_LABEL_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "agent_role".to_string(),
                limit: MAX_ROLE_LABEL_BYTES,
                actual: s.len(),
            });
        }
        match s {
            "commander" => Ok(Self::Commander),
            "implementer" => Ok(Self::Implementer),
            "tester" => Ok(Self::Tester),
            "reviewer" => Ok(Self::Reviewer),
            _ => Err(PluginError::registry(format!(
                "unknown agent role '{s}' (OQ-057 candidate; deny by default)"
            ))),
        }
    }

    /// Enforcement points this role may act at (candidate map).
    ///
    /// Authority narrows down the table: Commander acts everywhere its
    /// grants allow; Implementer loses delegation; Tester is confined to
    /// sandboxed execution and reads; Reviewer reads only.
    #[must_use]
    pub const fn allowed_points(self) -> &'static [EnforcementPoint] {
        match self {
            Self::Commander => &[
                EnforcementPoint::ContextRead,
                EnforcementPoint::ToolCall,
                EnforcementPoint::Delegation,
                EnforcementPoint::SandboxExec,
            ],
            Self::Implementer => &[
                EnforcementPoint::ContextRead,
                EnforcementPoint::ToolCall,
                EnforcementPoint::SandboxExec,
            ],
            Self::Tester => &[EnforcementPoint::ContextRead, EnforcementPoint::SandboxExec],
            Self::Reviewer => &[EnforcementPoint::ContextRead],
        }
    }

    /// Whether this role may act at `point` (pure candidate check).
    #[must_use]
    pub const fn may(self, point: EnforcementPoint) -> bool {
        let allowed = self.allowed_points();
        let mut i = 0;
        while i < allowed.len() {
            if allowed[i] as u8 == point as u8 {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Execution-sandbox restrictions for this role (candidate).
    ///
    /// Restrictions only tighten down the table; the sandbox mechanism
    /// itself (CRE-5 shell-write closure) stays undecided until the
    /// ruling.
    #[must_use]
    pub const fn sandbox(self) -> SandboxRestrictions {
        match self {
            Self::Commander => SandboxRestrictions {
                no_fs_write: false,
                no_network: false,
                no_child_process: false,
                env_sealed: false,
            },
            Self::Implementer => SandboxRestrictions {
                no_fs_write: false,
                no_network: true,
                no_child_process: false,
                env_sealed: true,
            },
            Self::Tester => SandboxRestrictions {
                no_fs_write: true,
                no_network: true,
                no_child_process: true,
                env_sealed: true,
            },
            Self::Reviewer => SandboxRestrictions {
                no_fs_write: true,
                no_network: true,
                no_child_process: true,
                env_sealed: true,
            },
        }
    }

    /// Capability families this role may exercise *at most*, intersected
    /// with grants elsewhere (candidate ceiling, never a grant).
    ///
    /// Families without an accepted meaning for the role map to absence:
    /// the candidate never invents authority.
    #[must_use]
    pub const fn capability_ceiling(self) -> &'static [CapabilityFamily] {
        match self {
            Self::Commander => &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Network,
                CapabilityFamily::Terminal,
                CapabilityFamily::Agent,
                CapabilityFamily::Mcp,
                CapabilityFamily::Ai,
            ],
            Self::Implementer => &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Terminal,
                CapabilityFamily::Agent,
            ],
            Self::Tester => &[CapabilityFamily::Fs, CapabilityFamily::Terminal],
            Self::Reviewer => &[CapabilityFamily::Terminal],
        }
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── enforcement points ────────────────────────────────────────────────────

/// Candidate enforcement points where the role contract is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnforcementPoint {
    /// Reading terminal/workspace context (observation only).
    ContextRead,
    /// Invoking a granted tool.
    ToolCall,
    /// Dispatching a subagent (delegation; narrows only).
    Delegation,
    /// Spawning a sandboxed process (filesystem/network/process/env
    /// restrictions per [`AgentRole::sandbox`]).
    SandboxExec,
}

impl EnforcementPoint {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextRead => "context-read",
            Self::ToolCall => "tool-call",
            Self::Delegation => "delegation",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

impl fmt::Display for EnforcementPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── sandbox restrictions ──────────────────────────────────────────────────

/// Candidate execution-sandbox restrictions for a role (OQ-057).
///
/// Pure flags describing what the sandbox would forbid; no sandbox
/// exists yet and this kernel enforces nothing by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SandboxRestrictions {
    no_fs_write: bool,
    no_network: bool,
    no_child_process: bool,
    env_sealed: bool,
}

impl SandboxRestrictions {
    /// Whether filesystem writes are forbidden.
    #[must_use]
    pub const fn no_fs_write(self) -> bool {
        self.no_fs_write
    }

    /// Whether network access is forbidden.
    #[must_use]
    pub const fn no_network(self) -> bool {
        self.no_network
    }

    /// Whether spawning child processes is forbidden.
    #[must_use]
    pub const fn no_child_process(self) -> bool {
        self.no_child_process
    }

    /// Whether the environment is sealed (no inherited secrets).
    #[must_use]
    pub const fn env_sealed(self) -> bool {
        self.env_sealed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip() {
        for role in [
            AgentRole::Commander,
            AgentRole::Implementer,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            assert_eq!(AgentRole::parse(role.as_str()), Ok(role));
            assert_eq!(role.to_string(), role.as_str());
        }
    }

    #[test]
    fn unknown_role_denies() {
        assert!(AgentRole::parse("owner").is_err());
        assert!(AgentRole::parse("").is_err());
        assert!(AgentRole::parse("Commander").is_err());
    }

    #[test]
    fn authority_narrows_down_the_table() {
        assert!(AgentRole::Commander.may(EnforcementPoint::Delegation));
        assert!(!AgentRole::Implementer.may(EnforcementPoint::Delegation));
        assert!(!AgentRole::Tester.may(EnforcementPoint::ToolCall));
        assert!(!AgentRole::Tester.may(EnforcementPoint::Delegation));
        assert!(!AgentRole::Reviewer.may(EnforcementPoint::ToolCall));
        assert!(!AgentRole::Reviewer.may(EnforcementPoint::SandboxExec));
        for role in [
            AgentRole::Commander,
            AgentRole::Implementer,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            assert!(
                role.may(EnforcementPoint::ContextRead),
                "{role} must at least read context"
            );
        }
    }

    #[test]
    fn sandbox_tightens_down_the_table() {
        let commander = AgentRole::Commander.sandbox();
        assert!(!commander.no_fs_write());
        assert!(!commander.no_network());
        let reviewer = AgentRole::Reviewer.sandbox();
        assert!(reviewer.no_fs_write());
        assert!(reviewer.no_network());
        assert!(reviewer.no_child_process());
        assert!(reviewer.env_sealed());
        assert!(AgentRole::Implementer.sandbox().no_network());
        assert!(!AgentRole::Implementer.sandbox().no_fs_write());
    }

    #[test]
    fn reviewer_ceiling_is_read_only() {
        assert_eq!(
            AgentRole::Reviewer.capability_ceiling(),
            &[CapabilityFamily::Terminal]
        );
        assert!(
            AgentRole::Commander
                .capability_ceiling()
                .contains(&CapabilityFamily::Agent)
        );
        assert!(
            !AgentRole::Tester
                .capability_ceiling()
                .contains(&CapabilityFamily::Network)
        );
    }

    #[test]
    fn point_labels_stable() {
        assert_eq!(EnforcementPoint::ContextRead.to_string(), "context-read");
        assert_eq!(EnforcementPoint::ToolCall.to_string(), "tool-call");
        assert_eq!(EnforcementPoint::Delegation.to_string(), "delegation");
        assert_eq!(EnforcementPoint::SandboxExec.to_string(), "sandbox-exec");
    }
}
