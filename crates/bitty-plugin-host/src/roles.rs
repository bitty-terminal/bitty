//! Role contract for multi-agent work (OQ-057).
//!
//! `OQ-057` is adopted (register accepted 2026-09-23): the role-to-authority
//! map is Commander, Implementer, Tester, Reviewer, as pure, bounded,
//! fail-closed data with the enforcement points below (including the
//! execution-sandbox layer).
//!
//! One seam is enforced at a live call boundary: [`AgentRole::check_point`]
//! (and [`AgentRole::check_request`] per privileged request kind) denies
//! roles outside their mapped points, and
//! [`crate::effective::authorize_with_role`] runs that gate before the
//! six-layer grant intersection. Roles never grant authority by themselves,
//! prompts never confer capability (there is deliberately no `bind_prompt`
//! constructor), and delegation only narrows through the accepted
//! intersection engine ([`crate::effective`]). The chat-message
//! [`Role`](bitty-agent) distinction stays untouched; this kernel answers
//! "may role R act at enforcement point P, and under which sandbox
//! restrictions". Unknown roles or points deny rather than default.
//!
//! # Non-goals
//!
//! Model routing, memory, skill growth, dispatch budgets, and the shell
//! write path (CRE-5) stay open. There is no `unsafe`, no I/O, and no new
//! dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::capability::CapabilityFamily;
use crate::effective::RequestKind;
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

    /// Check this role at one enforcement point (OQ-057 adopted).
    ///
    /// Call-boundary gate enforced by [`crate::effective::authorize_with_role`]:
    /// roles act only at their mapped points, otherwise the request denies
    /// fail-closed with a grant error naming the role and the point only
    /// (never a prompt, plan, or payload).
    pub fn check_point(self, point: EnforcementPoint) -> Result<(), PluginError> {
        if self.may(point) {
            Ok(())
        } else {
            Err(PluginError::grant(format!(
                "role '{}' may not act at '{}' (OQ-057 adopted)",
                self.as_str(),
                point.as_str()
            )))
        }
    }

    /// Check this role for one privileged request kind.
    ///
    /// Routes through [`EnforcementPoint::for_request_kind`]: delegation-gated
    /// kinds need dispatch authority, sandboxed execution needs the sandbox
    /// point, and every other privileged kind invokes a granted tool.
    pub fn check_request(self, kind: RequestKind) -> Result<(), PluginError> {
        self.check_point(EnforcementPoint::for_request_kind(kind))
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

    /// Enforcement point guarding one privileged request kind (OQ-057 adopted).
    ///
    /// Delegation-gated kinds (spawning agents or subagents) need the dispatch
    /// point; sandboxed execution needs the sandbox point with its
    /// [`AgentRole::sandbox`] restrictions; every other privileged kind
    /// invokes a granted tool or lifecycle entry. Observation-only context
    /// reads travel outside the privileged kinds, so no kind maps to
    /// `ContextRead`.
    #[must_use]
    pub const fn for_request_kind(kind: RequestKind) -> EnforcementPoint {
        match kind {
            RequestKind::AgentSpawn => EnforcementPoint::Delegation,
            RequestKind::ExecutionRun => EnforcementPoint::SandboxExec,
            RequestKind::PanelAcquire
            | RequestKind::FsRead
            | RequestKind::FsWrite
            | RequestKind::NetworkConnect
            | RequestKind::PluginLifecycle => EnforcementPoint::ToolCall,
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

    #[test]
    fn request_kind_points_route_correctly() {
        assert_eq!(
            EnforcementPoint::for_request_kind(RequestKind::AgentSpawn),
            EnforcementPoint::Delegation
        );
        assert_eq!(
            EnforcementPoint::for_request_kind(RequestKind::ExecutionRun),
            EnforcementPoint::SandboxExec
        );
        for kind in [
            RequestKind::PanelAcquire,
            RequestKind::FsRead,
            RequestKind::FsWrite,
            RequestKind::NetworkConnect,
            RequestKind::PluginLifecycle,
        ] {
            assert_eq!(
                EnforcementPoint::for_request_kind(kind),
                EnforcementPoint::ToolCall,
                "{kind} must route to tool-call"
            );
        }
    }

    #[test]
    fn role_gate_denies_outside_points() {
        assert!(
            AgentRole::Commander
                .check_request(RequestKind::AgentSpawn)
                .is_ok()
        );
        // Implementers carry no dispatch authority.
        assert!(
            AgentRole::Implementer
                .check_request(RequestKind::AgentSpawn)
                .is_err()
        );
        assert!(
            AgentRole::Implementer
                .check_request(RequestKind::ExecutionRun)
                .is_ok()
        );
        // Testers execute sandboxed but invoke no granted tools.
        assert!(
            AgentRole::Tester
                .check_request(RequestKind::ExecutionRun)
                .is_ok()
        );
        assert!(
            AgentRole::Tester
                .check_request(RequestKind::FsRead)
                .is_err()
        );
        // Reviewers observe only: every privileged kind denies.
        for kind in [
            RequestKind::AgentSpawn,
            RequestKind::ExecutionRun,
            RequestKind::PanelAcquire,
            RequestKind::FsRead,
            RequestKind::FsWrite,
            RequestKind::NetworkConnect,
            RequestKind::PluginLifecycle,
        ] {
            assert!(
                AgentRole::Reviewer.check_request(kind).is_err(),
                "reviewer must deny {kind}"
            );
        }
    }

    #[test]
    fn role_denial_names_role_and_point_only() {
        let error = AgentRole::Reviewer
            .check_request(RequestKind::FsWrite)
            .expect_err("reviewer must deny writes");
        let text = error.to_string();
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains("tool-call"), "{text}");
    }
}
