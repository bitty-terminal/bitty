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
//! six-layer grant intersection. Delegation fan-out and depth are bounded
//! per role ([`AgentRole::check_dispatch`], enforced by
//! [`crate::effective::delegate_with_role`]). Roles never grant authority
//! by themselves, prompts never confer capability (there is deliberately no
//! `bind_prompt` constructor; [`deny_prompt_authority`] is the executable
//! form of that invariant and always denies), and delegation only narrows
//! through the accepted intersection engine ([`crate::effective`]).
//! Execution spawns carry a declared sandbox posture ([`SandboxDecl`])
//! checked against the role table ([`AgentRole::check_sandbox_exec`]); the
//! full shell-write closure mechanism (CRE-5) stays open work. The
//! chat-message [`Role`](bitty-agent) distinction stays untouched; this
//! kernel answers "may role R act at enforcement point P, and under which
//! sandbox restrictions". Unknown roles or points deny rather than default.
//!
//! # Non-goals
//!
//! Model routing, memory, and skill growth stay open. The sandbox mechanism
//! itself (CRE-5 shell-write closure) stays undecided: the declaration gate
//! constrains what a spawn may claim, it does not build the sandbox.
//! There is no `unsafe`, no I/O, and no new dependency (`std` only).

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fmt;

use crate::capability::CapabilityFamily;
use crate::effective::RequestKind;
use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of a role label accepted by [`AgentRole::parse`].
pub const MAX_ROLE_LABEL_BYTES: usize = 32;

/// Delegation fan-out ceiling for the Commander role (OQ-057 adopted).
///
/// Mirrors the host default agent ceiling
/// ([`crate::effective::HOST_DEFAULT_MAX_AGENTS`]): without an explicit
/// per-task grant, no delegation tree fans wider than this. Every other
/// role carries no dispatch authority, so their ceiling is zero.
pub const MAX_DISPATCH_FANOUT: u32 = 16;

/// Delegation depth ceiling for the Commander role (OQ-057 adopted).
///
/// Bounds how deep a commander-rooted delegation chain may nest before a
/// fresh authorization is required. Every other role carries no dispatch
/// authority, so their ceiling is zero.
pub const MAX_DELEGATION_DEPTH: u32 = 3;

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

    /// Maximum subagent fan-out this role may dispatch at once (OQ-057
    /// adopted dispatch limit).
    ///
    /// Only the Commander dispatches; every other role carries no dispatch
    /// authority and reads zero here (their dispatch attempts already fail
    /// at [`Self::check_point`] with the delegation point).
    #[must_use]
    pub const fn max_dispatch(self) -> u32 {
        match self {
            Self::Commander => MAX_DISPATCH_FANOUT,
            Self::Implementer | Self::Tester | Self::Reviewer => 0,
        }
    }

    /// Maximum delegation-chain depth this role may nest (OQ-057 adopted
    /// dispatch limit).
    ///
    /// Only the Commander dispatches; every other role reads zero.
    #[must_use]
    pub const fn max_delegation_depth(self) -> u32 {
        match self {
            Self::Commander => MAX_DELEGATION_DEPTH,
            Self::Implementer | Self::Tester | Self::Reviewer => 0,
        }
    }

    /// Check a dispatch of `child_count` subagents at chain `depth`
    /// (OQ-057 adopted).
    ///
    /// Call-boundary gate enforced by
    /// [`crate::effective::delegate_with_role`]: the role must admit the
    /// delegation point first, then the fan-out and depth must fit the
    /// per-role ceilings. Over-ceiling dispatches deny fail-closed with a
    /// limit error naming the bound only (never a plan or payload).
    pub fn check_dispatch(self, child_count: u32, depth: u32) -> Result<(), PluginError> {
        self.check_point(EnforcementPoint::Delegation)?;
        let ceiling = self.max_dispatch();
        if child_count > ceiling {
            return Err(PluginError::LimitExceeded {
                field: "dispatch_fanout".to_string(),
                limit: ceiling as usize,
                actual: child_count as usize,
            });
        }
        let max_depth = self.max_delegation_depth();
        if depth > max_depth {
            return Err(PluginError::LimitExceeded {
                field: "delegation_depth".to_string(),
                limit: max_depth as usize,
                actual: depth as usize,
            });
        }
        Ok(())
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

    /// Check an execution spawn carrying `decl` against this role's sandbox
    /// posture (OQ-057 adopted, CRE-5 declaration gate).
    ///
    /// The role must admit the sandbox point first, then the declared
    /// posture must fit [`Self::sandbox`]: a spawn claiming filesystem
    /// writes, network, a child process, or a broken environment seal
    /// denies when the role forbids it. The gate constrains what a spawn
    /// may claim; the sandbox mechanism itself stays open work.
    pub fn check_sandbox_exec(self, decl: &SandboxDecl) -> Result<(), PluginError> {
        self.check_point(EnforcementPoint::SandboxExec)?;
        self.sandbox().check_decl(decl)
    }

    /// Capability families this role may exercise *at most*, intersected
    /// with grants elsewhere (candidate ceiling, never a grant).
    ///
    /// Families without an accepted meaning for the role map to absence:
    /// the candidate never invents authority.
    ///
    /// CTX-0916 S4 (DEC-0102): the Agent/Mcp/Ai families left the Core seed
    /// (zero-AI default), so no role ceiling names them anymore. AI authority
    /// returns only through the ceiling-contribution API (S5) with an
    /// explicitly extended catalog; until then every ceiling below is AI-free
    /// fail-closed.
    #[must_use]
    pub const fn capability_ceiling(self) -> &'static [CapabilityFamily] {
        match self {
            Self::Commander => &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Network,
                CapabilityFamily::Terminal,
            ],
            Self::Implementer => &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Terminal,
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

/// Prompt text never confers capability (OQ-057 adopted).
///
/// Executable form of the "prompts never grant authority" invariant: this
/// function always denies with a grant error naming the invariant only
/// (never prompt content). There is deliberately no `bind_prompt`
/// constructor on [`AgentRole`]; any future call site tempted to authorize
/// from prompt text must route through this denial instead.
pub fn deny_prompt_authority() -> Result<(), PluginError> {
    Err(PluginError::grant(
        "prompt text confers no authority (OQ-057 adopted; bind the role, never the prompt)",
    ))
}

/// Declared sandbox posture for one execution spawn (OQ-057 adopted,
/// CRE-5 declaration gate).
///
/// The spawner declares what the spawn will be allowed; the role table
/// ([`SandboxRestrictions::check_decl`]) admits or denies the claim. The
/// declaration is self-attested — it constrains claims, it does not build
/// the sandbox mechanism (open work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SandboxDecl {
    fs_write: bool,
    network: bool,
    child_process: bool,
    env_sealed: bool,
}

impl SandboxDecl {
    /// A fully sealed spawn: no filesystem writes, no network, no child
    /// process, environment sealed. Admitted by every role that reaches
    /// the sandbox point.
    #[must_use]
    pub const fn sealed() -> Self {
        Self {
            fs_write: false,
            network: false,
            child_process: false,
            env_sealed: true,
        }
    }

    /// A custom posture claim; each `true` needs the role to allow it.
    #[must_use]
    pub const fn new(fs_write: bool, network: bool, child_process: bool, env_sealed: bool) -> Self {
        Self {
            fs_write,
            network,
            child_process,
            env_sealed,
        }
    }

    /// Whether the spawn claims filesystem writes.
    #[must_use]
    pub const fn fs_write(self) -> bool {
        self.fs_write
    }

    /// Whether the spawn claims network access.
    #[must_use]
    pub const fn network(self) -> bool {
        self.network
    }

    /// Whether the spawn claims a child process beyond itself.
    #[must_use]
    pub const fn child_process(self) -> bool {
        self.child_process
    }

    /// Whether the spawn claims a sealed environment (no inherited secrets).
    #[must_use]
    pub const fn env_sealed(self) -> bool {
        self.env_sealed
    }
}

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

    /// Check a declared spawn posture against these restrictions (OQ-057
    /// adopted, CRE-5 declaration gate).
    ///
    /// Each claim the declaration makes must be one the restrictions allow;
    /// the first forbidden claim denies fail-closed with a grant error
    /// naming the restriction only (never a program, argument, or path).
    pub fn check_decl(self, decl: &SandboxDecl) -> Result<(), PluginError> {
        if self.no_fs_write && decl.fs_write() {
            return Err(PluginError::grant(
                "sandbox declaration claims filesystem writes the role forbids (OQ-057 adopted)",
            ));
        }
        if self.no_network && decl.network() {
            return Err(PluginError::grant(
                "sandbox declaration claims network access the role forbids (OQ-057 adopted)",
            ));
        }
        if self.no_child_process && decl.child_process() {
            return Err(PluginError::grant(
                "sandbox declaration claims a child process the role forbids (OQ-057 adopted)",
            ));
        }
        if self.env_sealed && !decl.env_sealed() {
            return Err(PluginError::grant(
                "sandbox declaration breaks the environment seal the role requires (OQ-057 adopted)",
            ));
        }
        Ok(())
    }
}

// ── role-ceiling contributions (CTX-0916 S5, DEC-0100) ──────────────────────

/// Maximum families accepted by a single
/// [`RoleCeilingCatalog::register_ceiling`] call.
///
/// One role row holds a handful of families; sixteen is generous headroom
/// while keeping extension input bounded (mirrors the S1 per-call bound
/// pattern on [`crate::capability::CapabilityCatalog`]).
pub const MAX_CEILING_FAMILIES_PER_CALL: usize = 16;

/// Maximum contributed (non-Core) ceiling families across all roles.
///
/// Bounds total extension input (mirrors the S1 total bound pattern).
/// Raise only by reviewed change.
pub const MAX_CONTRIBUTED_CEILING_FAMILIES: usize = 64;

/// Maximum bytes of one contributed ceiling family label.
///
/// Mirrors the capability segment bound enforced by manifest shape
/// validation.
pub const MAX_CEILING_FAMILY_LEN: usize = 64;

/// Core-owned role-ceiling contribution table (CTX-0916 slice S5, DEC-0100).
///
/// S5 is purely additive: the Core defaults are exactly
/// [`AgentRole::capability_ceiling`] (AI-free since S4) and stay unchanged.
/// Loaded extensions contribute additional families per role through
/// [`RoleCeilingCatalog::register_ceiling`]; the opt-in
/// [`crate::effective::authorize_with_role_and_ceilings`] path intersects the
/// effective (Core plus contributed) ceiling with grants elsewhere, so
/// contributed authority still narrows and never grants by itself. The
/// default [`crate::effective::authorize_with_role`] path stays gate-only
/// (pre-S5 behavior) and skips the ceiling gate.
///
/// Fail-closed rules for [`RoleCeilingCatalog::register_ceiling`] (mirroring
/// the S1 [`crate::capability::CapabilityCatalog::register`] patterns):
///
/// - Additive only: contributing a family the role already admits (Core
///   default or prior contribution) is a [`PluginError::Duplicate`] error,
///   never an overwrite. Rows are per-role: the same family may be
///   contributed to several roles; duplication is per (role, family) pair.
/// - Every family label is shape-validated like manifest segment validation
///   (length, character class, lowercase start). Membership in any catalog is
///   deliberately NOT required: `bitty-ai` contributes `ai`/`mcp`/`agent`
///   (S6), which Core catalogs intentionally do not know.
/// - The whole call is validated before any mutation, so a failed call
///   leaves the catalog unchanged.
/// - Per-call and total bounds ([`MAX_CEILING_FAMILIES_PER_CALL`],
///   [`MAX_CONTRIBUTED_CEILING_FAMILIES`]) keep extension input bounded.
///
/// ```rust
/// use bitty_plugin_host::roles::{AgentRole, RoleCeilingCatalog};
///
/// let mut ceilings = RoleCeilingCatalog::core();
/// assert!(ceilings.allows(AgentRole::Commander, "fs"));
/// assert!(!ceilings.allows(AgentRole::Commander, "ai"));
/// ceilings.register_ceiling(AgentRole::Commander, &["ai"]).unwrap();
/// assert!(ceilings.allows(AgentRole::Commander, "ai"));
/// // A fresh Core seed is unaffected by the contribution.
/// assert!(!RoleCeilingCatalog::core().allows(AgentRole::Commander, "ai"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCeilingCatalog {
    /// Contributed families per role, indexed by [`role_index`]; the Core
    /// defaults live in [`AgentRole::capability_ceiling`] and are never
    /// copied here, so they cannot drift.
    extra: [BTreeSet<String>; 4],
}

/// Index one role into [`RoleCeilingCatalog::extra`].
fn role_index(role: AgentRole) -> usize {
    match role {
        AgentRole::Commander => 0,
        AgentRole::Implementer => 1,
        AgentRole::Tester => 2,
        AgentRole::Reviewer => 3,
    }
}

impl RoleCeilingCatalog {
    /// Core seed: no contributions, so every role admits exactly its
    /// [`AgentRole::capability_ceiling`] (AI-free since S4, fail-closed).
    #[must_use]
    pub fn core() -> Self {
        Self {
            extra: [
                BTreeSet::new(),
                BTreeSet::new(),
                BTreeSet::new(),
                BTreeSet::new(),
            ],
        }
    }

    /// Whether `role` admits `family` under the effective (Core plus
    /// contributed) ceiling.
    ///
    /// `family` is a bare family label (for example `"fs"`, or `"ai"` once
    /// contributed); unknown labels simply do not match and deny at the
    /// enforcement gate.
    #[must_use]
    pub fn allows(&self, role: AgentRole, family: &str) -> bool {
        if role
            .capability_ceiling()
            .iter()
            .any(|member| member.as_str() == family)
        {
            return true;
        }
        self.extra[role_index(role)].contains(family)
    }

    /// Contributed (non-Core) families for `role`, sorted.
    #[must_use]
    pub fn contributed_for(&self, role: AgentRole) -> Vec<&str> {
        self.extra[role_index(role)]
            .iter()
            .map(String::as_str)
            .collect()
    }

    /// Effective (Core plus contributed) ceiling families for `role`, sorted.
    ///
    /// Core defaults come first from [`AgentRole::capability_ceiling`] only
    /// as labels; contributions extend, never replace, them.
    #[must_use]
    pub fn effective_for(&self, role: AgentRole) -> Vec<&str> {
        let mut effective: BTreeSet<&str> = role
            .capability_ceiling()
            .iter()
            .copied()
            .map(CapabilityFamily::as_str)
            .collect();
        effective.extend(self.contributed_for(role));
        effective.into_iter().collect()
    }

    /// Number of contributed (non-Core) families across all roles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.extra.iter().map(BTreeSet::len).sum()
    }

    /// Whether no families have been contributed (the Core seed always is).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extra.iter().all(BTreeSet::is_empty)
    }

    /// Additively contribute ceiling families for one role (CTX-0916
    /// extension hook).
    ///
    /// `families` names bare family labels admitted for `role` in addition
    /// to its Core [`AgentRole::capability_ceiling`]. Fail-closed: anything
    /// invalid rejects the whole call and the catalog is left unchanged
    /// (see the type docs).
    pub fn register_ceiling(
        &mut self,
        role: AgentRole,
        families: &[&str],
    ) -> Result<(), PluginError> {
        if families.is_empty() {
            return Err(PluginError::registry(
                "ceiling registration must declare at least one family",
            ));
        }
        if families.len() > MAX_CEILING_FAMILIES_PER_CALL {
            return Err(PluginError::LimitExceeded {
                field: "role_ceiling.register".to_string(),
                limit: MAX_CEILING_FAMILIES_PER_CALL,
                actual: families.len(),
            });
        }
        if self.len() + families.len() > MAX_CONTRIBUTED_CEILING_FAMILIES {
            return Err(PluginError::LimitExceeded {
                field: "role_ceiling.catalog".to_string(),
                limit: MAX_CONTRIBUTED_CEILING_FAMILIES,
                actual: self.len() + families.len(),
            });
        }
        // Validate everything before mutating so a failed call leaves the
        // catalog unchanged.
        let mut seen_in_call = BTreeSet::new();
        for family in families {
            validate_ceiling_family(family)?;
            if self.allows(role, family) {
                return Err(PluginError::Duplicate {
                    kind: "ceiling-family".to_string(),
                    value: (*family).to_string(),
                });
            }
            if !seen_in_call.insert(*family) {
                return Err(PluginError::Duplicate {
                    kind: "ceiling-family".to_string(),
                    value: (*family).to_string(),
                });
            }
        }
        let slot = &mut self.extra[role_index(role)];
        for family in families {
            slot.insert((*family).to_string());
        }
        Ok(())
    }
}

impl Default for RoleCeilingCatalog {
    fn default() -> Self {
        Self::core()
    }
}

/// Ceiling family-label shape validation mirroring manifest segment rules:
/// non-empty, bounded, lowercase start, `[a-z0-9_-]` body. Catalog
/// membership is deliberately not required (see the type docs).
fn validate_ceiling_family(family: &str) -> Result<(), PluginError> {
    if family.is_empty() {
        return Err(PluginError::registry("ceiling family must not be empty"));
    }
    if family.len() > MAX_CEILING_FAMILY_LEN {
        return Err(PluginError::LimitExceeded {
            field: "role_ceiling.family".to_string(),
            limit: MAX_CEILING_FAMILY_LEN,
            actual: family.len(),
        });
    }
    let first = family.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(PluginError::registry(
            "ceiling family must start with lowercase letter",
        ));
    }
    for byte in family.bytes() {
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_') {
            return Err(PluginError::registry("ceiling family must be [a-z0-9_-]"));
        }
    }
    Ok(())
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
        // CTX-0916 S4 (DEC-0102): no role ceiling names AI families anymore
        // (zero-AI default, fail-closed). Commander keeps the widest Core
        // ceiling; AI authority returns only via the S5 contribution API.
        assert_eq!(
            AgentRole::Commander.capability_ceiling(),
            &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Network,
                CapabilityFamily::Terminal,
            ]
        );
        assert_eq!(
            AgentRole::Implementer.capability_ceiling(),
            &[
                CapabilityFamily::Fs,
                CapabilityFamily::Process,
                CapabilityFamily::Terminal,
            ]
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

    #[test]
    fn dispatch_limits_admit_commander_within_ceiling() {
        assert_eq!(AgentRole::Commander.max_dispatch(), MAX_DISPATCH_FANOUT);
        assert_eq!(
            AgentRole::Commander.max_delegation_depth(),
            MAX_DELEGATION_DEPTH
        );
        assert!(AgentRole::Commander.check_dispatch(1, 0).is_ok());
        assert!(
            AgentRole::Commander
                .check_dispatch(MAX_DISPATCH_FANOUT, MAX_DELEGATION_DEPTH)
                .is_ok()
        );
        for role in [
            AgentRole::Implementer,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            assert_eq!(role.max_dispatch(), 0, "{role} dispatches nothing");
            assert_eq!(role.max_delegation_depth(), 0, "{role} nests nothing");
        }
    }

    #[test]
    fn dispatch_over_ceiling_denies_fail_closed() {
        let error = AgentRole::Commander
            .check_dispatch(MAX_DISPATCH_FANOUT + 1, 0)
            .expect_err("fan-out over ceiling must deny");
        let text = error.to_string();
        assert!(text.contains("dispatch_fanout"), "{text}");
        let error = AgentRole::Commander
            .check_dispatch(1, MAX_DELEGATION_DEPTH + 1)
            .expect_err("depth over ceiling must deny");
        let text = error.to_string();
        assert!(text.contains("delegation_depth"), "{text}");
        // Non-commanders deny at the delegation point before any bound.
        let error = AgentRole::Implementer
            .check_dispatch(0, 0)
            .expect_err("implementer dispatches nothing");
        let text = error.to_string();
        assert!(text.contains("delegation"), "{text}");
        assert!(
            AgentRole::Reviewer.check_dispatch(0, 0).is_err(),
            "reviewer dispatches nothing"
        );
    }

    #[test]
    fn prompt_text_never_confers_authority() {
        let error = deny_prompt_authority().expect_err("prompts never authorize");
        let text = error.to_string();
        assert!(text.contains("no authority"), "{text}");
        assert!(text.contains("OQ-057"), "{text}");
    }

    #[test]
    fn core_ceiling_catalog_matches_capability_ceiling() {
        // CTX-0916 S5 (DEC-0100): the Core seed contributes nothing, so the
        // effective ceiling is exactly `capability_ceiling` per role
        // (AI-free since S4).
        let core = RoleCeilingCatalog::core();
        assert!(core.is_empty());
        assert_eq!(core.len(), 0);
        assert_eq!(RoleCeilingCatalog::default(), core);
        for role in [
            AgentRole::Commander,
            AgentRole::Implementer,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            let mut expected: Vec<&str> = role
                .capability_ceiling()
                .iter()
                .copied()
                .map(CapabilityFamily::as_str)
                .collect();
            expected.sort_unstable();
            assert_eq!(
                core.effective_for(role),
                expected,
                "{role} Core ceiling drifted"
            );
            assert!(core.contributed_for(role).is_empty());
            for family in expected {
                assert!(core.allows(role, family), "{role} must admit {family}");
            }
            assert!(
                !core.allows(role, "ai"),
                "{role} must deny ai on the Core seed"
            );
        }
    }

    #[test]
    fn register_ceiling_accepts_extension_families_additively() {
        // The S6 handoff shape: `bitty-ai` contributes its families to the
        // Commander row without touching the Core defaults.
        let mut ceilings = RoleCeilingCatalog::core();
        ceilings
            .register_ceiling(AgentRole::Commander, &["ai", "mcp", "agent"])
            .expect("ai families contribute");
        assert_eq!(ceilings.len(), 3);
        assert!(!ceilings.is_empty());
        for family in ["ai", "mcp", "agent"] {
            assert!(
                ceilings.allows(AgentRole::Commander, family),
                "commander must admit contributed {family}"
            );
            assert!(
                !ceilings.allows(AgentRole::Implementer, family),
                "contribution must not leak to other roles"
            );
        }
        assert_eq!(
            ceilings.contributed_for(AgentRole::Commander),
            vec!["agent", "ai", "mcp"]
        );
        // Core families stay admitted alongside contributions.
        assert!(ceilings.allows(AgentRole::Commander, "fs"));
        // A fresh Core seed is unaffected by the contribution.
        let fresh = RoleCeilingCatalog::core();
        assert!(!fresh.allows(AgentRole::Commander, "ai"));
        assert!(fresh.is_empty());
    }

    #[test]
    fn register_ceiling_rejects_duplicates_without_mutation() {
        let mut ceilings = RoleCeilingCatalog::core();
        let before = ceilings.clone();

        // Contributing a Core-default family is a duplicate, never an
        // overwrite ...
        let error = ceilings
            .register_ceiling(AgentRole::Commander, &["fs"])
            .expect_err("core family re-registration must deny");
        assert!(matches!(error, PluginError::Duplicate { .. }), "{error}");
        // ... as is re-contributing an already contributed family ...
        ceilings
            .register_ceiling(AgentRole::Tester, &["acme"])
            .expect("first contribution registers");
        let error = ceilings
            .register_ceiling(AgentRole::Tester, &["acme"])
            .expect_err("second contribution must deny");
        assert!(matches!(error, PluginError::Duplicate { .. }), "{error}");
        // ... while the same family stays contributable to another role's
        // row (rows are per-role; S6 contributes shared AI families to
        // several roles) ...
        ceilings
            .register_ceiling(AgentRole::Commander, &["acme"])
            .expect("cross-role contribution registers");
        // ... and duplicates within one call reject before any mutation.
        let error = ceilings
            .register_ceiling(AgentRole::Reviewer, &["new-a", "new-a"])
            .expect_err("in-call duplicate must deny");
        assert!(matches!(error, PluginError::Duplicate { .. }), "{error}");
        assert!(!ceilings.allows(AgentRole::Reviewer, "new-a"));

        // Only the two accepted contributions landed.
        assert_eq!(ceilings.len(), before.len() + 2);
        assert!(ceilings.allows(AgentRole::Tester, "acme"));
        assert!(ceilings.allows(AgentRole::Commander, "acme"));
    }

    #[test]
    fn register_ceiling_rejects_bad_shapes_without_mutation() {
        let mut ceilings = RoleCeilingCatalog::core();
        let before = ceilings.clone();

        assert!(
            ceilings
                .register_ceiling(AgentRole::Commander, &[])
                .is_err(),
            "empty registration must deny"
        );
        for bad in [
            "",
            "Ai",
            "9lives",
            "has space",
            "with:colon",
            "wild*card",
            "UPPER",
        ] {
            assert!(
                ceilings
                    .register_ceiling(AgentRole::Commander, &[bad])
                    .is_err(),
                "'{bad}' must deny"
            );
        }
        let long = "f".repeat(MAX_CEILING_FAMILY_LEN + 1);
        assert!(
            ceilings
                .register_ceiling(AgentRole::Commander, &[long.as_str()])
                .is_err(),
            "oversize family must deny"
        );
        // A mixed call (one valid, one invalid) mutates nothing.
        assert!(
            ceilings
                .register_ceiling(AgentRole::Commander, &["valid-new", "Bad"])
                .is_err(),
            "mixed call must deny"
        );
        assert!(!ceilings.allows(AgentRole::Commander, "valid-new"));

        assert_eq!(ceilings, before);
    }

    #[test]
    fn register_ceiling_enforces_bounds() {
        let mut ceilings = RoleCeilingCatalog::core();

        // Per-call bound.
        let owned: Vec<String> = (0..MAX_CEILING_FAMILIES_PER_CALL + 1)
            .map(|i| format!("ext{i}"))
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let error = ceilings
            .register_ceiling(AgentRole::Commander, &refs)
            .expect_err("over per-call bound must deny");
        assert!(
            matches!(error, PluginError::LimitExceeded { .. }),
            "{error}"
        );
        assert!(ceilings.is_empty());

        // Total bound: fill to the limit across roles, then trip it.
        let mut made = 0;
        for role in [
            AgentRole::Commander,
            AgentRole::Implementer,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            while ceilings.len() + MAX_CEILING_FAMILIES_PER_CALL <= MAX_CONTRIBUTED_CEILING_FAMILIES
            {
                let owned: Vec<String> = (0..MAX_CEILING_FAMILIES_PER_CALL)
                    .map(|i| format!("r{role}-{made}-{i}"))
                    .collect();
                let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                ceilings
                    .register_ceiling(role, &refs)
                    .expect("bounded fill registers");
                made += 1;
            }
        }
        assert_eq!(ceilings.len(), MAX_CONTRIBUTED_CEILING_FAMILIES);
        let error = ceilings
            .register_ceiling(AgentRole::Commander, &["one-more"])
            .expect_err("over total bound must deny");
        assert!(
            matches!(error, PluginError::LimitExceeded { .. }),
            "{error}"
        );
        assert!(!ceilings.allows(AgentRole::Commander, "one-more"));
    }
}
