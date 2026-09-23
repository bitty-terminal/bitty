//! Effective-capability intersection engine (research 045 §8 §12 §13, OQ-057).
//!
//! Every agent, plugin, or Lua request is a request, never a grant. This
//! module computes, for each privileged request,
//!
//! ```text
//! EffectiveCapability =
//!     HostCeiling ∩ UserPolicy ∩ ProjectPolicy ∩ ParentDelegation ∩ TaskGrant ∩ AgentRequest
//! ```
//!
//! and enforces it through [`authorize`] (full six-layer check) and
//! [`delegate`] (parent-to-child attenuation). Lower layers only narrow:
//! a child never exceeds its parent, and declarations such as
//! `filesystem = "all"`, `root = true`, or `max_agents = 100` never
//! self-grant — unrepresentable wide declarations fail closed with
//! [`DenialKind::SelfGrant`], while typed over-requests are clamped to the
//! intersection and denied with a reason chain when exercised.
//!
//! # Layer model
//!
//! Each layer is a [`CapabilityScope`]: an exact set of allowed
//! [`CapabilityId`]s, an explicit deny set (deny wins across layers,
//! fail-closed), an optional `max_agents` ceiling (effective is the minimum),
//! and an optional `allow_root` flag (effective is the conjunction, default
//! deny). Capability matching is exact-set intersection, the same semantic
//! as [`crate::grant::GrantStore`]: a parameterized capability such as
//! `fs.read:~/projects/**` covers only itself, never a wider or narrower
//! spelling. Absent layers say nothing (unconstrained), except that
//! capabilities are deny-by-default: with no affirmative allow anywhere the
//! effective set is empty.
//!
//! Authority order (highest first) is
//! `HostCeiling > UserPolicy > ProjectPolicy > ParentDelegation > TaskGrant >
//! AgentRequest`, mirroring the Configuration Model RFC layer stack
//! (`SystemPolicy > User > TrustedLocal`, with the three runtime layers
//! below). Capabilities never widen via precedence: precedence only orders
//! the denial reason chain. Pinned ([`CapabilityScope::pins`]) entries are
//! non-overridable unanimity assertions: any layer omitting a pinned item is
//! a [`DenialKind::PolicyConflict`], never a silent clamp.
//!
//! # Non-goals
//!
//! Delegation budget enforcement, subagent attenuation as AI semantics, and
//! role/organization models stay in `bitty-ai`; this engine only
//! re-authorizes at the host. The engine adds no ambient authority: with an
//! empty stack every capability check denies.
//!
//! # Bounds (threat `T-01`)
//!
//! Scopes hold at most [`MAX_SCOPE_CAPS`] capabilities; requests carry at
//! most [`MAX_RAW_DECLARATIONS`] raw declarations; denial chains name at
//! most [`MAX_DENIAL_ITEMS`] excess items; the [`AuditLedger`] keeps at most
//! [`MAX_AUDIT_ENTRIES`] entries (drop-oldest, counter increments). No
//! wall-clock, no randomness: audit entries are sequence-numbered only.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use crate::capability::{CapabilityFamily, CapabilityId};
use crate::roles::AgentRole;
use crate::trust_levels::TrustLevel;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum capabilities (allow plus deny) in one [`CapabilityScope`].
pub const MAX_SCOPE_CAPS: usize = 256;

/// Maximum opaque raw declarations in one [`AgentRequest`].
pub const MAX_RAW_DECLARATIONS: usize = 8;

/// Maximum bytes of one raw declaration.
pub const MAX_RAW_DECLARATION_BYTES: usize = 256;

/// Maximum excess items named in a denial reason chain (remainder counted).
pub const MAX_DENIAL_ITEMS: usize = 8;

/// Maximum entries retained by the [`AuditLedger`] (drop-oldest).
pub const MAX_AUDIT_ENTRIES: usize = 1024;

/// Bound note for audit consumers (why entries may be missing).
pub const EFFECTIVE_AUDIT_BOUND_NOTE: &str =
    "audit ledger is bounded (drop-oldest); use dropped() for the evicted count";

/// Built-in host ceiling for agent fan-out (045 §12 example value).
///
/// Applies when no layer sets `max_agents`. Explicit layers only narrow via
/// minimum; nothing in a request can raise it.
pub const HOST_DEFAULT_MAX_AGENTS: u64 = 16;

// ── layers ────────────────────────────────────────────────────────────────

/// One of the six effective-capability layers (045 §8), highest authority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectiveLayer {
    /// Host security ceiling (non-overridable; Configuration Model `SystemPolicy`).
    HostCeiling,
    /// User policy (`$XDG_CONFIG_HOME/bitty/capabilities.conf`; layer `User`).
    UserPolicy,
    /// Project policy (`<root>/.bitty/capabilities.conf`; layer `TrustedLocal`).
    ProjectPolicy,
    /// Parent delegation (runtime; the delegating agent's effective set).
    ParentDelegation,
    /// Task grant (runtime; the task's granted set).
    TaskGrant,
    /// Agent/plugin/Lua request (runtime; always the narrowest voice).
    AgentRequest,
}

impl EffectiveLayer {
    /// Authority rank; lower wins diagnostics order (higher authority first).
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::HostCeiling => 0,
            Self::UserPolicy => 1,
            Self::ProjectPolicy => 2,
            Self::ParentDelegation => 3,
            Self::TaskGrant => 4,
            Self::AgentRequest => 5,
        }
    }

    /// Stable lowercase label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::HostCeiling => "host-ceiling",
            Self::UserPolicy => "user-policy",
            Self::ProjectPolicy => "project-policy",
            Self::ParentDelegation => "parent-delegation",
            Self::TaskGrant => "task-grant",
            Self::AgentRequest => "agent-request",
        }
    }

    /// Corresponding Configuration Model RFC layer (declared precedence map).
    ///
    /// The three runtime layers have no config counterpart; they sit below
    /// `TrustedLocal` and above nothing, in delegation order.
    #[must_use]
    pub const fn config_layer(self) -> &'static str {
        match self {
            Self::HostCeiling => "system-policy",
            Self::UserPolicy => "user",
            Self::ProjectPolicy => "trusted-local",
            Self::ParentDelegation => "runtime-delegation",
            Self::TaskGrant => "runtime-task",
            Self::AgentRequest => "runtime-request",
        }
    }

    /// All six layers in authority order (highest first).
    #[must_use]
    pub const fn all() -> &'static [EffectiveLayer] {
        &[
            Self::HostCeiling,
            Self::UserPolicy,
            Self::ProjectPolicy,
            Self::ParentDelegation,
            Self::TaskGrant,
            Self::AgentRequest,
        ]
    }
}

impl fmt::Display for EffectiveLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ── enforcement classification (045 §7) ───────────────────────────────────

/// Hard Safety / Policy / Strategy classification (045 §7 enforcement map).
///
/// Hard Safety is never突破able by policy or strategy; Policy is composable
/// by Lua within the hard ceiling; Strategy never reaches this engine (it is
/// listed in the map so reviewers can see it is deliberately out of scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnforcementClass {
    /// Unbreakable host boundary (resources, leases, authorization denials).
    HardSafety,
    /// Composable within the hard ceiling (capability grants, budgets).
    Policy,
    /// Lua/Agent choice only; never an engine input.
    Strategy,
}

impl EnforcementClass {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardSafety => "hard-safety",
            Self::Policy => "policy",
            Self::Strategy => "strategy",
        }
    }
}

impl fmt::Display for EnforcementClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Privileged request kinds gated by this engine (045 §13).
///
/// Closed vocabulary: `agent.spawn`, `execution.run`, `panel.acquire`,
/// `fs.read`, `fs.write`, `network.connect`, and the plugin lifecycle all
/// enter the single authorization path; raw Lua escapes (`os.execute`,
/// `io.open`) are not representable here and stay denied elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// Spawn an agent or subagent.
    AgentSpawn,
    /// Run an execution job.
    ExecutionRun,
    /// Acquire a panel (read or interactive write).
    PanelAcquire,
    /// Read through the filesystem surface.
    FsRead,
    /// Write through the filesystem surface.
    FsWrite,
    /// Open a network connection.
    NetworkConnect,
    /// Drive the plugin lifecycle (declare/resolve/register/activate).
    PluginLifecycle,
}

impl RequestKind {
    /// Stable wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentSpawn => "agent.spawn",
            Self::ExecutionRun => "execution.run",
            Self::PanelAcquire => "panel.acquire",
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::NetworkConnect => "network.connect",
            Self::PluginLifecycle => "plugin.lifecycle",
        }
    }

    /// Enforcement class per the 045 §7 table.
    #[must_use]
    pub const fn enforcement_class(self) -> EnforcementClass {
        match self {
            // Panel writer exclusivity and execution resource ceilings are
            // hard safety (Core guarantees, §3/§7).
            Self::ExecutionRun | Self::PanelAcquire => EnforcementClass::HardSafety,
            // Capability authorization and budgets compose within the ceiling.
            Self::AgentSpawn
            | Self::FsRead
            | Self::FsWrite
            | Self::NetworkConnect
            | Self::PluginLifecycle => EnforcementClass::Policy,
        }
    }

    /// Owning boundary per 045 §7 (`bitty` enforces, `bitty-ai`/Lua composes).
    #[must_use]
    pub const fn owner(self) -> &'static str {
        match self {
            Self::AgentSpawn => "bitty-ai + Lua (host re-authorizes)",
            Self::ExecutionRun
            | Self::PanelAcquire
            | Self::FsRead
            | Self::FsWrite
            | Self::NetworkConnect
            | Self::PluginLifecycle => "bitty",
        }
    }

    /// Capability families a request of this kind may name.
    ///
    /// Reserved for future kind routing; today every kind accepts every
    /// family (the engine intersects exact capabilities and the `fs` head
    /// rule below, while grant/host gates enforce exact grants). Kept so the
    /// vocabulary is closed and reviewers can see the reservation.
    #[must_use]
    pub const fn allowed_families(self) -> &'static [CapabilityFamily] {
        &[]
    }

    /// Exact capability heads allowed, when narrower than the family.
    ///
    /// `fs.read` requests must name `fs.read` heads only (and symmetrically
    /// for `fs.write`); all other kinds accept any head in their families.
    #[must_use]
    pub const fn allowed_heads(self) -> Option<&'static [&'static str]> {
        match self {
            Self::FsRead => Some(&["fs.read"]),
            Self::FsWrite => Some(&["fs.write"]),
            Self::AgentSpawn
            | Self::ExecutionRun
            | Self::PanelAcquire
            | Self::NetworkConnect
            | Self::PluginLifecycle => None,
        }
    }

    /// All seven kinds in a deterministic order.
    #[must_use]
    pub const fn all() -> &'static [RequestKind] {
        &[
            Self::AgentSpawn,
            Self::ExecutionRun,
            Self::PanelAcquire,
            Self::FsRead,
            Self::FsWrite,
            Self::NetworkConnect,
            Self::PluginLifecycle,
        ]
    }
}

impl fmt::Display for RequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the Hard Safety / Policy / Strategy enforcement map (045 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementEntry {
    /// Request kind this row classifies.
    pub kind: RequestKind,
    /// Enforcement class.
    pub class: EnforcementClass,
    /// Owning boundary.
    pub owner: &'static str,
    /// Why this class (045 §7 reference).
    pub note: &'static str,
}

/// The enforcement map: every privileged request kind classified.
///
/// Strategy rows (scheduling order, retry policy, team shape) are Lua-only
/// and deliberately absent: they never reach the engine.
pub const ENFORCEMENT_MAP: &[EnforcementEntry] = &[
    EnforcementEntry {
        kind: RequestKind::AgentSpawn,
        class: EnforcementClass::Policy,
        owner: "bitty-ai + Lua (host re-authorizes)",
        note: "045 §7: max agents is a hard ceiling plus policy; count strategy stays Lua",
    },
    EnforcementEntry {
        kind: RequestKind::ExecutionRun,
        class: EnforcementClass::HardSafety,
        owner: "bitty",
        note: "045 §7: CPU/RAM/GPU/disk ceilings and privilege denial are hard safety",
    },
    EnforcementEntry {
        kind: RequestKind::PanelAcquire,
        class: EnforcementClass::HardSafety,
        owner: "bitty",
        note: "045 §7: panel writer exclusivity is Core-guaranteed mutual exclusion",
    },
    EnforcementEntry {
        kind: RequestKind::FsRead,
        class: EnforcementClass::Policy,
        owner: "bitty",
        note: "045 §7: sensitive-path authorization composes user/project policy",
    },
    EnforcementEntry {
        kind: RequestKind::FsWrite,
        class: EnforcementClass::Policy,
        owner: "bitty",
        note: "045 §7: sensitive-path authorization composes user/project policy",
    },
    EnforcementEntry {
        kind: RequestKind::NetworkConnect,
        class: EnforcementClass::Policy,
        owner: "bitty",
        note: "045 §7: network destinations compose user/project policy",
    },
    EnforcementEntry {
        kind: RequestKind::PluginLifecycle,
        class: EnforcementClass::Policy,
        owner: "bitty",
        note: "045 §7: grant lifecycle composes consent within the host ceiling",
    },
];

/// Enforcement class for one request kind.
#[must_use]
pub const fn enforcement_class_for(kind: RequestKind) -> EnforcementClass {
    kind.enforcement_class()
}

// ── scopes, requests, effective sets ──────────────────────────────────────

/// One layer's capability voice.
///
/// `caps` affirmatively allows (exact [`CapabilityId`] match);
/// `denied` explicitly excludes (wins across layers, fail-closed);
/// `max_agents` ceilings fan-out (`None` says nothing, effective is the
/// minimum); `allow_root` gates privilege (`None` says nothing, effective is
/// the conjunction, default deny); `pins` names non-overridable entries that
/// every layer must preserve (see module docs). `source` attributes the layer
/// for reason chains (e.g. `user:/path/capabilities.conf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityScope {
    /// Affirmatively allowed capabilities (exact match).
    pub caps: BTreeSet<CapabilityId>,
    /// Explicitly denied capabilities (wins over any allow).
    pub denied: BTreeSet<CapabilityId>,
    /// Fan-out ceiling (`None` says nothing).
    pub max_agents: Option<u64>,
    /// Privilege gate (`None` says nothing).
    pub allow_root: Option<bool>,
    /// Non-overridable entries: capability raw strings, `max_agents`, `allow_root`.
    pub pins: BTreeSet<String>,
    /// Source attribution for diagnostics.
    pub source: String,
}

impl CapabilityScope {
    /// Scope that says nothing (unconstrained layer).
    pub fn unconstrained(source: impl Into<String>) -> Self {
        Self {
            caps: BTreeSet::new(),
            denied: BTreeSet::new(),
            max_agents: None,
            allow_root: None,
            pins: BTreeSet::new(),
            source: bounded_source(source.into()),
        }
    }

    /// Whether this scope constrains capabilities at all.
    #[must_use]
    pub fn constrains_caps(&self) -> bool {
        !self.caps.is_empty() || !self.denied.is_empty()
    }

    /// Whether `cap` survives this scope (allowed and not denied).
    #[must_use]
    pub fn allows(&self, cap: &CapabilityId) -> bool {
        self.caps.contains(cap) && !self.denied.contains(cap)
    }

    /// Validate bounds (fail-closed before a scope enters a stack).
    pub fn validate(&self) -> Result<(), EffectiveDenial> {
        if self.caps.len().saturating_add(self.denied.len()) > MAX_SCOPE_CAPS {
            return Err(EffectiveDenial::invalid(
                RequestKind::PluginLifecycle,
                format!(
                    "scope '{}' exceeds {MAX_SCOPE_CAPS} capabilities",
                    self.source
                ),
            ));
        }
        if self.source.len() > MAX_RAW_DECLARATION_BYTES {
            return Err(EffectiveDenial::invalid(
                RequestKind::PluginLifecycle,
                "scope source exceeds bounds",
            ));
        }
        Ok(())
    }
}

/// Clamp source attribution to a bounded length (fail-closed at validate).
fn bounded_source(source: String) -> String {
    if source.len() > MAX_RAW_DECLARATION_BYTES {
        source[..MAX_RAW_DECLARATION_BYTES].to_string()
    } else {
        source
    }
}

/// An agent/plugin/Lua request: narrow typed fields plus opaque raws.
///
/// Typed fields (`scope.caps`, `scope.max_agents`, `scope.allow_root`) are
/// intersected with the stack. `raw_wide` carries declarations that are not
/// exactly representable as validated capabilities — e.g.
/// `filesystem = "all"` — and any non-empty `raw_wide` fails closed with
/// [`DenialKind::SelfGrant`]: wide declarations never self-grant, they must
/// be re-expressed narrowly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// Narrow typed request voice (also the `AgentRequest` stack layer).
    pub scope: CapabilityScope,
    /// Opaque wide declarations (any entry denies the whole request).
    pub raw_wide: Vec<String>,
}

impl AgentRequest {
    /// Request with no authority claimed (least authority).
    pub fn empty(source: impl Into<String>) -> Self {
        Self {
            scope: CapabilityScope::unconstrained(source),
            raw_wide: Vec::new(),
        }
    }

    /// Validate bounds and shape before authorization.
    pub fn validate(&self, kind: RequestKind) -> Result<(), EffectiveDenial> {
        self.scope.validate()?;
        if self.raw_wide.len() > MAX_RAW_DECLARATIONS {
            return Err(EffectiveDenial::invalid(
                kind,
                format!("request exceeds {MAX_RAW_DECLARATIONS} raw declarations"),
            ));
        }
        for raw in &self.raw_wide {
            if raw.len() > MAX_RAW_DECLARATION_BYTES {
                return Err(EffectiveDenial::invalid(
                    kind,
                    "raw declaration exceeds bounds",
                ));
            }
        }
        Ok(())
    }
}

/// The five authorizing layers (the request itself is passed separately).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStack {
    /// Host security ceiling.
    pub host: CapabilityScope,
    /// User policy (XDG).
    pub user: CapabilityScope,
    /// Project policy (`.bitty`; `None` when absent).
    pub project: Option<CapabilityScope>,
    /// Parent delegation.
    pub parent: CapabilityScope,
    /// Task grant.
    pub task: CapabilityScope,
}

impl EffectiveStack {
    /// Stack where every layer says nothing (deny-by-default still denies caps).
    pub fn unconstrained() -> Self {
        Self {
            host: CapabilityScope::unconstrained("host-ceiling:built-in"),
            user: CapabilityScope::unconstrained("user:absent"),
            project: None,
            parent: CapabilityScope::unconstrained("parent:absent"),
            task: CapabilityScope::unconstrained("task:absent"),
        }
    }

    /// Validate every present scope.
    pub fn validate(&self) -> Result<(), EffectiveDenial> {
        self.host.validate()?;
        self.user.validate()?;
        if let Some(project) = &self.project {
            project.validate()?;
        }
        self.parent.validate()?;
        self.task.validate()?;
        Ok(())
    }

    /// Present layers in authority order (highest first; request excluded).
    fn layers_in_order(&self) -> Vec<(EffectiveLayer, &CapabilityScope)> {
        let mut layers = vec![
            (EffectiveLayer::HostCeiling, &self.host),
            (EffectiveLayer::UserPolicy, &self.user),
        ];
        if let Some(project) = &self.project {
            layers.push((EffectiveLayer::ProjectPolicy, project));
        }
        layers.push((EffectiveLayer::ParentDelegation, &self.parent));
        layers.push((EffectiveLayer::TaskGrant, &self.task));
        layers
    }
}

/// Intersect every layer: deny-by-default capabilities, minimum ceiling,
/// conjunctive root, union of explicit denies.
fn intersect(layers: &[(EffectiveLayer, &CapabilityScope)]) -> EffectiveCapability {
    let mut caps: Option<BTreeSet<CapabilityId>> = None;
    let mut denied: BTreeSet<CapabilityId> = BTreeSet::new();
    let mut ceilings: Vec<u64> = Vec::new();
    let mut roots: Vec<bool> = Vec::new();
    for (_, scope) in layers {
        // Deny-by-default: a scope constrains only when it says something;
        // the first constraining scope seeds the intersection.
        if scope.constrains_caps() {
            let allowed: BTreeSet<CapabilityId> =
                scope.caps.difference(&scope.denied).cloned().collect();
            caps = Some(match caps {
                Some(current) => current.intersection(&allowed).cloned().collect(),
                None => allowed,
            });
        }
        denied.extend(scope.denied.iter().cloned());
        if let Some(ceiling) = scope.max_agents {
            ceilings.push(ceiling);
        }
        if let Some(root) = scope.allow_root {
            roots.push(root);
        }
    }
    // No affirmative allow anywhere: the effective set is empty (never a grant).
    let mut caps = caps.unwrap_or_default();
    // Explicit denies win across layers, including over the seeded allow.
    caps.retain(|cap| !denied.contains(cap));
    EffectiveCapability {
        caps,
        max_agents: ceilings
            .into_iter()
            .min()
            .unwrap_or(HOST_DEFAULT_MAX_AGENTS),
        allow_root: !roots.is_empty() && roots.iter().all(|root| *root),
    }
}

/// The computed intersection: what the request may exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCapability {
    /// Capabilities surviving every layer (deny-by-default: empty unless allowed).
    pub caps: BTreeSet<CapabilityId>,
    /// Effective fan-out ceiling (minimum of present ceilings, else host default).
    pub max_agents: u64,
    /// Effective privilege gate (conjunction of present flags, else deny).
    pub allow_root: bool,
}

impl EffectiveCapability {
    /// Whether `cap` may be exercised.
    #[must_use]
    pub fn contains(&self, cap: &CapabilityId) -> bool {
        self.caps.contains(cap)
    }

    /// Whether spawning `count` agents fits the ceiling.
    #[must_use]
    pub const fn allows_count(&self, count: u64) -> bool {
        count <= self.max_agents
    }

    /// Whether privilege escalation is allowed.
    #[must_use]
    pub const fn allows_root(&self) -> bool {
        self.allow_root
    }

    /// Whether `self` is attenuated within `parent` (child ⊆ parent).
    #[must_use]
    pub fn is_subset_of(&self, parent: &EffectiveCapability) -> bool {
        self.caps.is_subset(&parent.caps)
            && self.max_agents <= parent.max_agents
            && (!self.allow_root || parent.allow_root)
    }
}

// ── typed denial ──────────────────────────────────────────────────────────

/// Why authorization refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DenialKind {
    /// A wide declaration (`filesystem = "all"`) or delegation excess tried
    /// to self-grant authority the layers do not confer.
    SelfGrant,
    /// The request exceeds the computed intersection.
    ExceedsPolicy,
    /// A non-overridable (pinned) entry was narrowed.
    PolicyConflict,
    /// Project policy exists but the project is not trusted.
    UntrustedProject,
    /// The request itself is malformed (wrong family, over bounds).
    InvalidRequest,
}

impl DenialKind {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfGrant => "self-grant",
            Self::ExceedsPolicy => "exceeds-policy",
            Self::PolicyConflict => "policy-conflict",
            Self::UntrustedProject => "untrusted-project",
            Self::InvalidRequest => "invalid-request",
        }
    }
}

impl fmt::Display for DenialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reason-chain step: which layer decided what, in authority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialStep {
    /// Layer that produced this step (`None` for request-shape findings).
    pub layer: Option<EffectiveLayer>,
    /// Owned detail (bounded: capped item lists, never values).
    pub detail: String,
}

/// Typed denial with the full reason chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDenial {
    /// Why the request was refused.
    pub kind: DenialKind,
    /// Which privileged kind was requested.
    pub request_kind: RequestKind,
    /// Reason chain in layer-authority order.
    pub chain: Vec<DenialStep>,
}

impl EffectiveDenial {
    /// Denial naming the narrowest context first (request-shape findings).
    pub(crate) fn invalid(request_kind: RequestKind, detail: impl Into<String>) -> Self {
        Self {
            kind: DenialKind::InvalidRequest,
            request_kind,
            chain: vec![DenialStep {
                layer: None,
                detail: detail.into(),
            }],
        }
    }

    /// Rendered reason chain, one line per step.
    #[must_use]
    pub fn reason_chain(&self) -> Vec<String> {
        self.chain
            .iter()
            .map(|step| match step.layer {
                Some(layer) => format!("{}: {}", layer.label(), step.detail),
                None => format!("request: {}", step.detail),
            })
            .collect()
    }
}

impl fmt::Display for EffectiveDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "denied {} ({})", self.request_kind, self.kind)?;
        for line in self.reason_chain() {
            writeln!(f, "  - {line}")?;
        }
        Ok(())
    }
}

impl std::error::Error for EffectiveDenial {}

// ── authorization ─────────────────────────────────────────────────────────

/// Compute the intersection and authorize `request` for `kind`.
///
/// Fail-closed order: malformed request → untrusted project → wide
/// self-grant declarations → kind/family mismatch → intersection → pin
/// unanimity → excess over effective. Every refusal carries the reason
/// chain; success returns exactly what may be exercised (never the request
/// verbatim).
pub fn authorize(
    stack: &EffectiveStack,
    request: &AgentRequest,
    kind: RequestKind,
    project_trusted: bool,
) -> Result<EffectiveCapability, EffectiveDenial> {
    stack.validate().map_err(|invalid| EffectiveDenial {
        kind: DenialKind::InvalidRequest,
        request_kind: kind,
        chain: invalid.chain,
    })?;
    request.validate(kind)?;
    // Untrusted project: a present project layer must never contribute
    // authority (deny before any intersection math).
    if stack.project.is_some() && !project_trusted {
        return Err(EffectiveDenial {
            kind: DenialKind::UntrustedProject,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: Some(EffectiveLayer::ProjectPolicy),
                detail: "project policy present but project is not trusted".to_string(),
            }],
        });
    }
    // Wide declarations are never expressible narrowly: fail closed.
    if !request.raw_wide.is_empty() {
        return Err(SelfGrant::from_wide(kind, &request.raw_wide));
    }
    // Kind/family gate: the request vocabulary must fit the privileged kind.
    check_kind_fit(&request.scope, kind)?;
    let layers = stack.layers_in_order();
    // Pin unanimity: a pinned item must survive every layer (non-overridable).
    check_pins(&layers, kind)?;
    let effective = intersect(&layers);
    // The request's scalar claims can never raise the intersection: a higher
    // `max_agents` or `allow_root = true` above effective denies as
    // self-grant, not as a silent clamp.
    check_scalar_self_grant(&request.scope, &effective, kind)?;
    // Excess capabilities over the intersection fail closed with the chain.
    check_excess(&request.scope, &effective, kind, &layers)?;
    Ok(EffectiveCapability {
        caps: request.scope.caps.clone(),
        max_agents: floor_ceiling(request.scope.max_agents, effective.max_agents),
        allow_root: request.scope.allow_root.unwrap_or(false) && effective.allow_root,
    })
}

// ── adopted-contract wrappers (OQ-085 trust, OQ-057 roles) ─────────────────

/// Trust-level pre-check shared by [`authorize_with_trust`] and
/// [`authorize_with_trust_and_role`]: every requested capability's family
/// must pass [`TrustLevel::check_family`] for `level`.
fn check_trust_gate(
    request: &AgentRequest,
    kind: RequestKind,
    level: TrustLevel,
) -> Result<(), EffectiveDenial> {
    for cap in &request.scope.caps {
        if let Err(error) = level.check_family(cap.family()) {
            return Err(EffectiveDenial {
                kind: DenialKind::PolicyConflict,
                request_kind: kind,
                chain: vec![DenialStep {
                    layer: None,
                    detail: error.to_string(),
                }],
            });
        }
    }
    Ok(())
}

/// Role-contract pre-check shared by [`authorize_with_role`] and
/// [`authorize_with_trust_and_role`]: `role` must admit the enforcement
/// point guarding `kind`.
fn check_role_gate(kind: RequestKind, role: AgentRole) -> Result<(), EffectiveDenial> {
    if let Err(error) = role.check_request(kind) {
        return Err(EffectiveDenial {
            kind: DenialKind::PolicyConflict,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: None,
                detail: error.to_string(),
            }],
        });
    }
    Ok(())
}

/// Authorize with the adopted trust-level admission gate first (OQ-085).
///
/// Every requested capability's family must pass
/// [`TrustLevel::check_family`] for `level` before the six-layer
/// intersection runs; the first family outside the level's admitted domains
/// denies fail-closed with [`DenialKind::PolicyConflict`], naming the level
/// and the family only (never a value). Families the adopted matrix does not
/// cover pass through to their grants. On success this is exactly
/// [`authorize`]: trust narrows, never grants.
pub fn authorize_with_trust(
    stack: &EffectiveStack,
    request: &AgentRequest,
    kind: RequestKind,
    project_trusted: bool,
    level: TrustLevel,
) -> Result<EffectiveCapability, EffectiveDenial> {
    check_trust_gate(request, kind, level)?;
    authorize(stack, request, kind, project_trusted)
}

/// Authorize with the adopted role contract gate first (OQ-057).
///
/// `role` must admit the enforcement point guarding `kind`
/// ([`crate::roles::EnforcementPoint::for_request_kind`]) before the six-layer
/// intersection runs; otherwise the request denies fail-closed with
/// [`DenialKind::PolicyConflict`], naming the role and the point only (never
/// a prompt, plan, or payload). The role gate never grants: on success this
/// is exactly [`authorize`], so capability ceilings still intersect with
/// grants elsewhere.
pub fn authorize_with_role(
    stack: &EffectiveStack,
    request: &AgentRequest,
    kind: RequestKind,
    project_trusted: bool,
    role: AgentRole,
) -> Result<EffectiveCapability, EffectiveDenial> {
    check_role_gate(kind, role)?;
    authorize(stack, request, kind, project_trusted)
}

/// Authorize with both adopted gates first: trust-level admission (OQ-085)
/// then the role contract (OQ-057), fail-closed in that order.
///
/// Either gate denies with [`DenialKind::PolicyConflict`] before the
/// six-layer intersection runs; on success this is exactly [`authorize`]:
/// neither gate ever grants. This is the single entry point for host seams
/// ([`crate::host::PluginHost::authorize_effective`]) so callers cannot
/// adopt the intersection while skipping a gate.
pub fn authorize_with_trust_and_role(
    stack: &EffectiveStack,
    request: &AgentRequest,
    kind: RequestKind,
    project_trusted: bool,
    level: TrustLevel,
    role: AgentRole,
) -> Result<EffectiveCapability, EffectiveDenial> {
    check_trust_gate(request, kind, level)?;
    check_role_gate(kind, role)?;
    authorize(stack, request, kind, project_trusted)
}

/// Attenuate `request` under an already-computed `parent` set.
///
/// The child is `parent ∩ request`, and any request excess over the parent
/// fails closed with [`DenialKind::SelfGrant`]: children narrow, never
/// widen. On success the returned set is always a subset of `parent`
/// (see [`EffectiveCapability::is_subset_of`]).
pub fn delegate(
    parent: &EffectiveCapability,
    request: &AgentRequest,
    kind: RequestKind,
) -> Result<EffectiveCapability, EffectiveDenial> {
    request.validate(kind)?;
    if !request.raw_wide.is_empty() {
        return Err(SelfGrant::from_wide(kind, &request.raw_wide));
    }
    check_kind_fit(&request.scope, kind)?;
    // Excess capabilities widen: deny before intersecting.
    let mut excess: Vec<String> = request
        .scope
        .caps
        .difference(&parent.caps)
        .take(MAX_DENIAL_ITEMS + 1)
        .map(|cap| cap.as_str().to_string())
        .collect();
    excess.sort();
    if !excess.is_empty() {
        let (named, rest) = split_items(&excess);
        return Err(EffectiveDenial {
            kind: DenialKind::SelfGrant,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: Some(EffectiveLayer::AgentRequest),
                detail: format!(
                    "request exceeds parent delegation ({named}{rest}); child must narrow"
                ),
            }],
        });
    }
    if let Some(want) = request.scope.max_agents {
        if want > parent.max_agents {
            return Err(EffectiveDenial {
                kind: DenialKind::SelfGrant,
                request_kind: kind,
                chain: vec![DenialStep {
                    layer: Some(EffectiveLayer::AgentRequest),
                    detail: format!(
                        "max_agents {want} exceeds parent delegation {}",
                        parent.max_agents
                    ),
                }],
            });
        }
    }
    if request.scope.allow_root.unwrap_or(false) && !parent.allow_root {
        return Err(EffectiveDenial {
            kind: DenialKind::SelfGrant,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: Some(EffectiveLayer::AgentRequest),
                detail: "root=true exceeds parent delegation (parent denies root)".to_string(),
            }],
        });
    }
    Ok(EffectiveCapability {
        caps: request.scope.caps.clone(),
        max_agents: floor_ceiling(request.scope.max_agents, parent.max_agents),
        allow_root: request.scope.allow_root.unwrap_or(false) && parent.allow_root,
    })
}

/// Attenuate `request` under `parent` after the adopted role dispatch gate
/// (OQ-057).
///
/// `role` must admit a dispatch of `child_count` subagents at chain
/// `depth` ([`AgentRole::check_dispatch`]) before [`delegate`] runs;
/// otherwise the delegation denies fail-closed with
/// [`DenialKind::PolicyConflict`], naming the bound only (never a plan or
/// payload). On success this is exactly [`delegate`]: the role gate never
/// grants, children still narrow under the parent.
pub fn delegate_with_role(
    parent: &EffectiveCapability,
    request: &AgentRequest,
    kind: RequestKind,
    role: AgentRole,
    child_count: u32,
    depth: u32,
) -> Result<EffectiveCapability, EffectiveDenial> {
    if let Err(error) = role.check_dispatch(child_count, depth) {
        return Err(EffectiveDenial {
            kind: DenialKind::PolicyConflict,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: None,
                detail: error.to_string(),
            }],
        });
    }
    delegate(parent, request, kind)
}

/// Wide-declaration denials: unrepresentable claims, never widened.
struct SelfGrant;

impl SelfGrant {
    fn from_wide(kind: RequestKind, raw: &[String]) -> EffectiveDenial {
        let mut items: Vec<String> = raw.iter().take(MAX_DENIAL_ITEMS + 1).cloned().collect();
        items.sort();
        let (named, rest) = split_items(&items);
        EffectiveDenial {
            kind: DenialKind::SelfGrant,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: Some(EffectiveLayer::AgentRequest),
                detail: format!(
                    "wide declaration ({named}{rest}) is not a grant; re-express narrowly"
                ),
            }],
        }
    }
}

/// Split a sorted excess list into the named items plus an `and N more` tail.
fn split_items(sorted: &[String]) -> (String, String) {
    let named: Vec<&str> = sorted
        .iter()
        .take(MAX_DENIAL_ITEMS)
        .map(String::as_str)
        .collect();
    let rest = if sorted.len() > MAX_DENIAL_ITEMS {
        format!(" and {} more", sorted.len() - MAX_DENIAL_ITEMS)
    } else {
        String::new()
    };
    (named.join(", "), rest)
}

/// Lower a request ceiling to the effective one (`None` inherits effective).
fn floor_ceiling(want: Option<u64>, effective: u64) -> u64 {
    want.map_or(effective, |value| value.min(effective))
}

/// Check that every requested capability fits the privileged kind.
///
/// Today only the `fs.read` / `fs.write` head split is enforced here (a
/// read request naming `fs.write` is malformed, never silently kept);
/// exact grant enforcement stays with the grant store and host gates.
fn check_kind_fit(scope: &CapabilityScope, kind: RequestKind) -> Result<(), EffectiveDenial> {
    let heads = kind.allowed_heads();
    let Some(allowed) = heads else {
        return Ok(());
    };
    let mut bad: Vec<String> = scope
        .caps
        .iter()
        .chain(scope.denied.iter())
        .filter(|cap| {
            let head = cap.as_str().split(':').next().unwrap_or(cap.as_str());
            !allowed.contains(&head)
        })
        .take(MAX_DENIAL_ITEMS + 1)
        .map(|cap| cap.as_str().to_string())
        .collect();
    bad.sort();
    if bad.is_empty() {
        return Ok(());
    }
    let (named, rest) = split_items(&bad);
    Err(EffectiveDenial {
        kind: DenialKind::InvalidRequest,
        request_kind: kind,
        chain: vec![DenialStep {
            layer: Some(EffectiveLayer::AgentRequest),
            detail: format!("{kind} request names out-of-kind capabilities ({named}{rest})"),
        }],
    })
}

/// Check non-overridable pins: a pinned item must survive every layer.
fn check_pins(
    layers: &[(EffectiveLayer, &CapabilityScope)],
    kind: RequestKind,
) -> Result<(), EffectiveDenial> {
    // Collect pins together with the pinning layer (first pin wins the label).
    let mut pins: Vec<(EffectiveLayer, &str, String)> = Vec::new();
    for (layer, scope) in layers {
        for pin in &scope.pins {
            pins.push((*layer, layer.label(), pin.clone()));
        }
    }
    for (_, policy_label, pin) in pins {
        let mut violator: Option<EffectiveLayer> = None;
        if pin == "max_agents" {
            let expected = layers
                .iter()
                .find(|(_, scope)| scope.pins.contains(pin.as_str()))
                .and_then(|(_, scope)| scope.max_agents);
            for (layer, scope) in layers {
                if scope.pins.contains(pin.as_str()) {
                    continue;
                }
                if scope
                    .max_agents
                    .is_some_and(|value| Some(value) != expected)
                {
                    violator = Some(*layer);
                    break;
                }
            }
        } else if pin == "allow_root" {
            let expected = layers
                .iter()
                .find(|(_, scope)| scope.pins.contains(pin.as_str()))
                .and_then(|(_, scope)| scope.allow_root);
            for (layer, scope) in layers {
                if scope.pins.contains(pin.as_str()) {
                    continue;
                }
                if scope
                    .allow_root
                    .is_some_and(|value| Some(value) != expected)
                {
                    violator = Some(*layer);
                    break;
                }
            }
        } else if let Ok(pinned) = CapabilityId::parse(pin.as_str()) {
            for (layer, scope) in layers {
                if scope.constrains_caps() && !scope.caps.contains(&pinned) {
                    violator = Some(*layer);
                    break;
                }
            }
        } else {
            // Unparseable pins fail closed: the policy itself is malformed.
            return Err(EffectiveDenial {
                kind: DenialKind::PolicyConflict,
                request_kind: kind,
                chain: vec![DenialStep {
                    layer: None,
                    detail: format!("policy pin '{pin}' from {policy_label} is malformed"),
                }],
            });
        }
        if let Some(violator) = violator {
            return Err(EffectiveDenial {
                kind: DenialKind::PolicyConflict,
                request_kind: kind,
                chain: vec![
                    DenialStep {
                        layer: layers
                            .iter()
                            .find(|(_, scope)| scope.pins.contains(pin.as_str()))
                            .map(|(layer, _)| *layer),
                        detail: format!("non-overridable pin '{pin}'"),
                    },
                    DenialStep {
                        layer: Some(violator),
                        detail: format!("narrows pinned '{pin}'"),
                    },
                ],
            });
        }
    }
    Ok(())
}

/// Check that scalar claims never raise the intersection.
fn check_scalar_self_grant(
    scope: &CapabilityScope,
    effective: &EffectiveCapability,
    kind: RequestKind,
) -> Result<(), EffectiveDenial> {
    if let Some(want) = scope.max_agents {
        if want > effective.max_agents {
            return Err(EffectiveDenial {
                kind: DenialKind::SelfGrant,
                request_kind: kind,
                chain: vec![DenialStep {
                    layer: Some(EffectiveLayer::AgentRequest),
                    detail: format!(
                        "max_agents {want} exceeds effective ceiling {}",
                        effective.max_agents
                    ),
                }],
            });
        }
    }
    if scope.allow_root.unwrap_or(false) && !effective.allow_root {
        return Err(EffectiveDenial {
            kind: DenialKind::SelfGrant,
            request_kind: kind,
            chain: vec![DenialStep {
                layer: Some(EffectiveLayer::AgentRequest),
                detail: "root=true exceeds effective policy (root denied)".to_string(),
            }],
        });
    }
    Ok(())
}

/// Check request capabilities against the intersection, layer-ordered chain.
fn check_excess(
    scope: &CapabilityScope,
    effective: &EffectiveCapability,
    kind: RequestKind,
    layers: &[(EffectiveLayer, &CapabilityScope)],
) -> Result<(), EffectiveDenial> {
    let mut excess: Vec<String> = scope
        .caps
        .difference(&effective.caps)
        .take(MAX_DENIAL_ITEMS + 1)
        .map(|cap| cap.as_str().to_string())
        .collect();
    excess.sort();
    if excess.is_empty() {
        return Ok(());
    }
    let (named, rest) = split_items(&excess);
    // Name every constraining voice in layer-authority order so the chain
    // shows who narrowed, then the request excess last.
    let mut chain: Vec<DenialStep> = layers
        .iter()
        .filter(|(_, scope)| scope.constrains_caps())
        .map(|(layer, scope)| DenialStep {
            layer: Some(*layer),
            detail: format!("allows {} capabilities", scope.caps.len()),
        })
        .collect();
    chain.push(DenialStep {
        layer: Some(EffectiveLayer::AgentRequest),
        detail: format!("request exceeds intersection ({named}{rest})"),
    });
    Err(EffectiveDenial {
        kind: DenialKind::ExceedsPolicy,
        request_kind: kind,
        chain,
    })
}

// ── audit ledger ──────────────────────────────────────────────────────────

/// Allow/deny outcome recorded in the [`AuditLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditDecision {
    /// The request was granted (effective set returned).
    Allow,
    /// The request was denied (typed denial returned).
    Deny,
}

impl AuditDecision {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// One audit entry: what was asked, what was decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Monotonic sequence (no wall-clock).
    pub seq: u64,
    /// Privileged kind requested.
    pub kind: RequestKind,
    /// Outcome.
    pub decision: AuditDecision,
    /// Requested capability strings (capped at [`MAX_DENIAL_ITEMS`]).
    pub requested: Vec<String>,
    /// Effective capability strings on allow (capped; empty on deny).
    pub effective: Vec<String>,
    /// Denial kind on deny (`None` on allow).
    pub denial: Option<DenialKind>,
}

/// Bounded append-only ledger of effective grants and denials.
///
/// Drop-oldest when full (accepted v1 default, mirroring the event
/// pipeline); `dropped` counts evicted entries for `bitty plugin doctor`.
#[derive(Debug, Clone, Default)]
pub struct AuditLedger {
    entries: VecDeque<AuditEntry>,
    dropped: u64,
    next_seq: u64,
}

impl AuditLedger {
    /// Empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an allowed authorization.
    pub fn push_allow(&mut self, kind: RequestKind, requested: &[String], effective: &[String]) {
        self.push(AuditDecision::Allow, kind, requested, effective, None);
    }

    /// Record a denied authorization.
    pub fn push_deny(&mut self, kind: RequestKind, requested: &[String], denial: DenialKind) {
        self.push(AuditDecision::Deny, kind, requested, &[], Some(denial));
    }

    /// Bounded append shared by both outcomes.
    fn push(
        &mut self,
        decision: AuditDecision,
        kind: RequestKind,
        requested: &[String],
        effective: &[String],
        denial: Option<DenialKind>,
    ) {
        if self.entries.len() >= MAX_AUDIT_ENTRIES {
            self.entries.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.entries.push_back(AuditEntry {
            seq,
            kind,
            decision,
            requested: requested.iter().take(MAX_DENIAL_ITEMS).cloned().collect(),
            effective: effective.iter().take(MAX_DENIAL_ITEMS).cloned().collect(),
            denial,
        });
    }

    /// Retained entries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &AuditEntry> + '_ {
        self.entries.iter()
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted by the bound so far.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(s: &str) -> CapabilityId {
        CapabilityId::parse(s).unwrap()
    }

    fn scope_with(source: &str, caps: &[&str]) -> CapabilityScope {
        let mut scope = CapabilityScope::unconstrained(source);
        for c in caps {
            scope.caps.insert(cap(c));
        }
        scope
    }

    #[test]
    fn intersection_narrows_across_all_six_layers_red() {
        // 045 §8: EffectiveCapability = host ∩ user ∩ project ∩ parent ∩ task ∩ request.
        let stack = EffectiveStack {
            host: scope_with("host", &["terminal.semantic-read", "ui.rich"]),
            user: scope_with("user", &["terminal.semantic-read"]),
            project: Some(scope_with(
                "project",
                &["terminal.semantic-read", "ui.rich"],
            )),
            parent: scope_with("parent", &["terminal.semantic-read", "ui.rich"]),
            task: scope_with("task", &["terminal.semantic-read", "ui.rich"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        let effective = authorize(&stack, &request, RequestKind::PluginLifecycle, true).unwrap();
        assert_eq!(
            effective.caps,
            [cap("terminal.semantic-read")].into_iter().collect()
        );
    }

    #[test]
    fn trust_gate_denies_before_intersection() {
        // Every layer allows the filesystem capability, so plain `authorize`
        // succeeds; the trust pre-check still denies for external tools.
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/docs/*.md"]),
            user: scope_with("user", &["fs.read:~/docs/*.md"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/docs/*.md"]),
            task: scope_with("task", &["fs.read:~/docs/*.md"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/docs/*.md"]),
            raw_wide: Vec::new(),
        };
        let denial = authorize_with_trust(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            TrustLevel::ExternalTool,
        )
        .expect_err("external tools admit no filesystem domain");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
        assert_eq!(denial.request_kind, RequestKind::FsRead);
        let chain = denial.reason_chain().join("\n");
        assert!(chain.contains("external-tool"), "{chain}");
        // Core passes the gate and authorizes exactly like `authorize`.
        let trusted = authorize_with_trust(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            TrustLevel::Core,
        )
        .expect("core admits filesystem");
        assert_eq!(
            trusted.caps,
            [cap("fs.read:~/docs/*.md")].into_iter().collect()
        );
    }

    #[test]
    fn trust_gate_defers_unmapped_families_to_grants() {
        // `ui.rich` has no adopted domain: even external tools pass the gate
        // and the grant intersection decides.
        let stack = EffectiveStack {
            host: scope_with("host", &["ui.rich"]),
            user: scope_with("user", &["ui.rich"]),
            project: None,
            parent: scope_with("parent", &["ui.rich"]),
            task: scope_with("task", &["ui.rich"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["ui.rich"]),
            raw_wide: Vec::new(),
        };
        let effective = authorize_with_trust(
            &stack,
            &request,
            RequestKind::PluginLifecycle,
            true,
            TrustLevel::ExternalTool,
        )
        .expect("unmapped families defer to grants");
        assert_eq!(effective.caps, [cap("ui.rich")].into_iter().collect());
    }

    #[test]
    fn role_gate_denies_before_intersection() {
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/docs/*.md"]),
            user: scope_with("user", &["fs.read:~/docs/*.md"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/docs/*.md"]),
            task: scope_with("task", &["fs.read:~/docs/*.md"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/docs/*.md"]),
            raw_wide: Vec::new(),
        };
        // Reviewers observe only: the tool-call point denies first.
        let denial = authorize_with_role(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            AgentRole::Reviewer,
        )
        .expect_err("reviewers invoke no tools");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
        assert_eq!(denial.request_kind, RequestKind::FsRead);
        let chain = denial.reason_chain().join("\n");
        assert!(chain.contains("reviewer"), "{chain}");
        assert!(chain.contains("tool-call"), "{chain}");
        // Commanders pass the gate and authorize exactly like `authorize`.
        let effective = authorize_with_role(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            AgentRole::Commander,
        )
        .expect("commanders act at every point their grants allow");
        assert_eq!(
            effective.caps,
            [cap("fs.read:~/docs/*.md")].into_iter().collect()
        );
    }

    #[test]
    fn combined_gates_deny_either_gate_before_intersection() {
        // The host seam entry point: trust denies first, then role, and an
        // admitted pair authorizes exactly like `authorize`.
        let stack = EffectiveStack {
            host: scope_with("host", &["fs.read:~/docs/*.md"]),
            user: scope_with("user", &["fs.read:~/docs/*.md"]),
            project: None,
            parent: scope_with("parent", &["fs.read:~/docs/*.md"]),
            task: scope_with("task", &["fs.read:~/docs/*.md"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["fs.read:~/docs/*.md"]),
            raw_wide: Vec::new(),
        };
        let trust_denial = authorize_with_trust_and_role(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            TrustLevel::ExternalTool,
            AgentRole::Commander,
        )
        .expect_err("external tools admit no filesystem domain");
        assert_eq!(trust_denial.kind, DenialKind::PolicyConflict);
        assert!(
            trust_denial
                .reason_chain()
                .join("\n")
                .contains("external-tool"),
            "{}",
            trust_denial.reason_chain().join("\n")
        );
        let role_denial = authorize_with_trust_and_role(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            TrustLevel::Core,
            AgentRole::Reviewer,
        )
        .expect_err("reviewers invoke no tools");
        assert_eq!(role_denial.kind, DenialKind::PolicyConflict);
        let effective = authorize_with_trust_and_role(
            &stack,
            &request,
            RequestKind::FsRead,
            true,
            TrustLevel::Core,
            AgentRole::Commander,
        )
        .expect("admitted pair authorizes");
        assert_eq!(
            effective.caps,
            [cap("fs.read:~/docs/*.md")].into_iter().collect()
        );
    }

    #[test]
    fn attenuation_child_is_subset_of_parent_red() {
        // 045 §9: Capability(child) ⊆ Capability(parent); excess fails closed.
        let parent = EffectiveCapability {
            caps: [cap("terminal.semantic-read")].into_iter().collect(),
            max_agents: 3,
            allow_root: false,
        };
        // Narrow child succeeds.
        let narrow = AgentRequest {
            scope: scope_with("child", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        let child = delegate(&parent, &narrow, RequestKind::PluginLifecycle).unwrap();
        assert!(child.is_subset_of(&parent));
        // Wide child is denied as self-grant.
        let wide = AgentRequest {
            scope: scope_with("child", &["terminal.semantic-read", "ui.rich"]),
            raw_wide: Vec::new(),
        };
        let denial = delegate(&parent, &wide, RequestKind::PluginLifecycle).unwrap_err();
        assert_eq!(denial.kind, DenialKind::SelfGrant);
    }

    #[test]
    fn delegation_with_role_enforces_dispatch_limits() {
        // OQ-057: the role dispatch gate runs before attenuation.
        use crate::roles::MAX_DISPATCH_FANOUT;
        let parent = EffectiveCapability {
            caps: [cap("terminal.semantic-read")].into_iter().collect(),
            max_agents: 3,
            allow_root: false,
        };
        let narrow = AgentRequest {
            scope: scope_with("child", &["terminal.semantic-read"]),
            raw_wide: Vec::new(),
        };
        // Commander within ceiling attenuates exactly like `delegate`.
        let child = delegate_with_role(
            &parent,
            &narrow,
            RequestKind::PluginLifecycle,
            AgentRole::Commander,
            2,
            1,
        )
        .expect("commander within ceiling attenuates");
        assert!(child.is_subset_of(&parent));
        // Commander over fan-out denies before attenuation.
        let denial = delegate_with_role(
            &parent,
            &narrow,
            RequestKind::PluginLifecycle,
            AgentRole::Commander,
            MAX_DISPATCH_FANOUT + 1,
            0,
        )
        .expect_err("fan-out over ceiling must deny");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
        // Non-commanders carry no dispatch authority.
        let denial = delegate_with_role(
            &parent,
            &narrow,
            RequestKind::AgentSpawn,
            AgentRole::Implementer,
            0,
            0,
        )
        .expect_err("implementer dispatches nothing");
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
    }

    #[test]
    fn self_grant_wide_declarations_fail_closed_red() {
        // 045 §8: filesystem = "all" / root = true / max_agents = 100 never self-grant.
        let stack = EffectiveStack::unconstrained();
        let request = AgentRequest {
            scope: CapabilityScope::unconstrained("lua"),
            raw_wide: vec![
                "filesystem=all".to_string(),
                "root=true".to_string(),
                "max_agents=100".to_string(),
            ],
        };
        let denial = authorize(&stack, &request, RequestKind::AgentSpawn, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::SelfGrant);
        // Raised ceilings and root are also clamped: request above the
        // minimum denies rather than widening.
        let mut raising = CapabilityScope::unconstrained("lua");
        raising.max_agents = Some(100);
        raising.allow_root = Some(true);
        let request = AgentRequest {
            scope: raising,
            raw_wide: Vec::new(),
        };
        let denial = authorize(&stack, &request, RequestKind::AgentSpawn, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::SelfGrant);
    }

    #[test]
    fn user_and_project_narrowing_red() {
        // 045 §12: user/project layers only narrow; task/request excess denied.
        let stack = EffectiveStack {
            host: scope_with("host", &["terminal.semantic-read", "ui.rich"]),
            user: scope_with("user", &["terminal.semantic-read"]),
            project: Some(scope_with("project", &["terminal.semantic-read"])),
            parent: scope_with("parent", &["terminal.semantic-read"]),
            task: scope_with("task", &["terminal.semantic-read", "ui.rich"]),
        };
        let request = AgentRequest {
            scope: scope_with("req", &["terminal.semantic-read", "ui.rich"]),
            raw_wide: Vec::new(),
        };
        let denial = authorize(&stack, &request, RequestKind::PluginLifecycle, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
        assert!(
            denial
                .reason_chain()
                .iter()
                .any(|line| line.contains("user-policy"))
        );
    }

    #[test]
    fn non_overridable_pin_rejects_override_red() {
        // Non-overridable host pin: narrowing max_agents below the pin conflicts.
        let mut host = CapabilityScope::unconstrained("system-policy:policy.lua");
        host.max_agents = Some(8);
        host.pins.insert("max_agents".to_string());
        let stack = EffectiveStack {
            host,
            user: {
                let mut user = CapabilityScope::unconstrained("user:init.lua");
                user.max_agents = Some(4);
                user
            },
            project: None,
            parent: CapabilityScope::unconstrained("parent:absent"),
            task: CapabilityScope::unconstrained("task:absent"),
        };
        let request = AgentRequest::empty("lua");
        let denial = authorize(&stack, &request, RequestKind::AgentSpawn, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
    }

    #[test]
    fn audit_ledger_records_allow_and_deny_red() {
        let mut ledger = AuditLedger::new();
        let stack = EffectiveStack::unconstrained();
        let request = AgentRequest::empty("lua");
        let allowed = authorize(&stack, &request, RequestKind::FsRead, true).unwrap();
        assert!(allowed.caps.is_empty());
        ledger.push_allow(RequestKind::FsRead, &[], &[]);
        let bad = AgentRequest {
            scope: scope_with("lua", &["fs.read:~/x"]),
            raw_wide: Vec::new(),
        };
        let denial = authorize(&stack, &bad, RequestKind::FsRead, true).unwrap_err();
        ledger.push_deny(
            RequestKind::FsRead,
            &["fs.read:~/x".to_string()],
            denial.kind,
        );
        assert_eq!(ledger.len(), 2);
        let entries: Vec<_> = ledger.iter().collect();
        assert_eq!(entries[0].decision, AuditDecision::Allow);
        assert_eq!(entries[1].decision, AuditDecision::Deny);
        assert!(entries[1].denial.is_some());
    }

    #[test]
    fn untrusted_project_denies_red() {
        let stack = EffectiveStack {
            host: CapabilityScope::unconstrained("host"),
            user: CapabilityScope::unconstrained("user"),
            project: Some(scope_with("project", &["terminal.semantic-read"])),
            parent: CapabilityScope::unconstrained("parent"),
            task: CapabilityScope::unconstrained("task"),
        };
        let request = AgentRequest::empty("lua");
        let denial = authorize(&stack, &request, RequestKind::FsRead, false).unwrap_err();
        assert_eq!(denial.kind, DenialKind::UntrustedProject);
    }

    #[test]
    fn reason_chain_is_layer_ordered_red() {
        let denial = EffectiveDenial {
            kind: DenialKind::ExceedsPolicy,
            request_kind: RequestKind::FsRead,
            chain: vec![
                DenialStep {
                    layer: Some(EffectiveLayer::AgentRequest),
                    detail: "excess fs.read:~/x".to_string(),
                },
                DenialStep {
                    layer: Some(EffectiveLayer::HostCeiling),
                    detail: "allows nothing".to_string(),
                },
            ],
        };
        let lines = denial.reason_chain();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("agent-request:"));
    }
}

// ── policy file loading (XDG user + `.bitty` project) ─────────────────────

/// User policy file name under the XDG config root (`$XDG_CONFIG_HOME/bitty/`).
pub const USER_POLICY_FILE_NAME: &str = "capabilities.conf";

/// Project policy directory name under the project root (`<root>/.bitty/`).
pub const PROJECT_POLICY_DIR_NAME: &str = ".bitty";

/// Project policy file name (`<root>/.bitty/capabilities.conf`).
pub const PROJECT_POLICY_FILE_NAME: &str = "capabilities.conf";

/// Maximum policy file size in bytes (fail-closed).
pub const MAX_POLICY_FILE_BYTES: usize = 64 * 1024;

/// Maximum policy file lines (fail-closed).
pub const MAX_POLICY_FILE_LINES: usize = 1024;

/// Maximum bytes per policy line (fail-closed).
pub const MAX_POLICY_LINE_BYTES: usize = 1024;

/// Provenance of a loaded policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyProvenance {
    /// XDG user configuration.
    User,
    /// Project-local `.bitty` configuration.
    Project,
}

impl PolicyProvenance {
    /// Effective layer this provenance maps to.
    #[must_use]
    pub const fn layer(self) -> EffectiveLayer {
        match self {
            Self::User => EffectiveLayer::UserPolicy,
            Self::Project => EffectiveLayer::ProjectPolicy,
        }
    }

    /// Configuration Model RFC layer label (declared precedence map).
    #[must_use]
    pub const fn config_layer(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "trusted-local",
        }
    }

    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Pure policy-file path for one file name with injected environment values.
///
/// Mirrors the XDG derivation in `bitty-config` (`$XDG_CONFIG_HOME` else
/// `$HOME/.config`); returns `None` only when neither yields a usable root
/// (no panic, no I/O). Paths stay caller-owned; the loader below takes the
/// resolved content, never the path.
#[must_use]
pub fn user_policy_path_with_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<String> {
    if let Some(xdg) = xdg_config_home {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(format!("{trimmed}/bitty/{USER_POLICY_FILE_NAME}"));
        }
    }
    if let Some(home) = home {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Some(format!("{trimmed}/.config/bitty/{USER_POLICY_FILE_NAME}"));
        }
    }
    None
}

/// Pure project policy path for a project root (no I/O).
#[must_use]
pub fn project_policy_path(project_root: &str) -> String {
    format!("{project_root}/{PROJECT_POLICY_DIR_NAME}/{PROJECT_POLICY_FILE_NAME}")
}

/// Parse policy file text into a [`CapabilityScope`].
///
/// Grammar (one directive per line, `#` comments, blank lines ignored):
///
/// ```text
/// allow <capability-id>        # affirmatively allow (exact, validated)
/// deny <capability-id>         # explicitly deny (wins across layers)
/// max_agents <n>               # fan-out ceiling (decimal u64)
/// allow_root <true|false>      # privilege gate
/// pin <capability-id|max_agents|allow_root>  # non-overridable entry
/// ```
///
/// Capability ids validate through the closed [`CapabilityId`] grammar, so
/// `allow fs.read` (missing parameter) fails closed exactly like a manifest
/// would. `pin` names an allow/deny entry that must already be present in
/// this same scope (`pin` then asserts unanimity across layers), or one of
/// the two scalar names. Unknown directives, over-bound input, and control
/// bytes fail closed with [`EffectiveDenial`] (never a panic, never silent).
/// Secrets never appear here: values are capability ids and small scalars,
/// and denial details quote names only.
///
/// `source` attributes the layer for reason chains (e.g.
/// `user:/path/capabilities.conf`).
pub fn parse_policy(
    text: &str,
    provenance: PolicyProvenance,
    source: &str,
) -> Result<CapabilityScope, EffectiveDenial> {
    let kind = RequestKind::PluginLifecycle;
    if text.len() > MAX_POLICY_FILE_BYTES {
        return Err(EffectiveDenial::invalid(
            kind,
            format!("policy file exceeds {MAX_POLICY_FILE_BYTES} bytes"),
        ));
    }
    let mut scope = CapabilityScope::unconstrained(source);
    let mut line_no = 0usize;
    for raw_line in text.lines() {
        line_no += 1;
        if line_no > MAX_POLICY_FILE_LINES {
            return Err(EffectiveDenial::invalid(
                kind,
                format!("policy file exceeds {MAX_POLICY_FILE_LINES} lines"),
            ));
        }
        if raw_line.len() > MAX_POLICY_LINE_BYTES {
            return Err(EffectiveDenial::invalid(
                kind,
                format!("policy line {line_no} exceeds {MAX_POLICY_LINE_BYTES} bytes"),
            ));
        }
        if raw_line.bytes().any(|b| b < 0x20 && b != b'\t') || raw_line.contains('\x7f') {
            return Err(EffectiveDenial::invalid(
                kind,
                format!("policy line {line_no} carries control bytes"),
            ));
        }
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let directive = parts.next().unwrap_or("");
        let argument = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(EffectiveDenial::invalid(
                kind,
                format!("policy line {line_no}: too many fields"),
            ));
        }
        match directive {
            "allow" | "deny" => {
                if argument.is_empty() {
                    return Err(EffectiveDenial::invalid(
                        kind,
                        format!("policy line {line_no}: '{directive}' needs a capability id"),
                    ));
                }
                let id = CapabilityId::parse(argument).map_err(|error| {
                    EffectiveDenial::invalid(
                        kind,
                        format!("policy line {line_no}: invalid capability '{argument}': {error}"),
                    )
                })?;
                // Kind cross-check for the fs head split (a user/project
                // policy may scope reads and writes independently).
                let target = if argument.starts_with("fs.read:") {
                    Some(RequestKind::FsRead)
                } else if argument.starts_with("fs.write:") {
                    Some(RequestKind::FsWrite)
                } else {
                    None
                };
                if let Some(expected) = target {
                    let probe = CapabilityScope {
                        caps: [id.clone()].into_iter().collect(),
                        denied: BTreeSet::new(),
                        max_agents: None,
                        allow_root: None,
                        pins: BTreeSet::new(),
                        source: String::new(),
                    };
                    check_kind_fit(&probe, expected)?;
                }
                if scope.caps.len().saturating_add(scope.denied.len()) >= MAX_SCOPE_CAPS {
                    return Err(EffectiveDenial::invalid(
                        kind,
                        format!("policy exceeds {MAX_SCOPE_CAPS} capabilities"),
                    ));
                }
                if directive == "allow" {
                    scope.caps.insert(id);
                } else {
                    scope.denied.insert(id);
                }
            }
            "max_agents" => {
                let value: u64 = argument.parse().map_err(|_| {
                    EffectiveDenial::invalid(
                        kind,
                        format!("policy line {line_no}: max_agents needs a decimal integer"),
                    )
                })?;
                scope.max_agents = Some(value);
            }
            "allow_root" => match argument {
                "true" => scope.allow_root = Some(true),
                "false" => scope.allow_root = Some(false),
                _ => {
                    return Err(EffectiveDenial::invalid(
                        kind,
                        format!("policy line {line_no}: allow_root needs 'true' or 'false'"),
                    ));
                }
            },
            "pin" => {
                if argument.is_empty() {
                    return Err(EffectiveDenial::invalid(
                        kind,
                        format!("policy line {line_no}: 'pin' needs an entry"),
                    ));
                }
                if argument == "max_agents" {
                    if scope.max_agents.is_none() {
                        return Err(EffectiveDenial::invalid(
                            kind,
                            format!(
                                "policy line {line_no}: pin 'max_agents' needs the value first"
                            ),
                        ));
                    }
                } else if argument == "allow_root" {
                    if scope.allow_root.is_none() {
                        return Err(EffectiveDenial::invalid(
                            kind,
                            format!(
                                "policy line {line_no}: pin 'allow_root' needs the value first"
                            ),
                        ));
                    }
                } else {
                    let pinned = CapabilityId::parse(argument).map_err(|error| {
                        EffectiveDenial::invalid(
                            kind,
                            format!("policy line {line_no}: invalid pin '{argument}': {error}"),
                        )
                    })?;
                    if !scope.caps.contains(&pinned) && !scope.denied.contains(&pinned) {
                        return Err(EffectiveDenial::invalid(
                            kind,
                            format!(
                                "policy line {line_no}: pin '{argument}' names no allow/deny entry"
                            ),
                        ));
                    }
                }
                scope.pins.insert(argument.to_string());
            }
            _ => {
                return Err(EffectiveDenial::invalid(
                    kind,
                    format!("policy line {line_no}: unknown directive '{directive}'"),
                ));
            }
        }
    }
    // Provenance prefix keeps the RFC layer visible in reason chains.
    scope.source = format!("{}:{source}", provenance.as_str());
    scope.validate()?;
    Ok(scope)
}

#[cfg(test)]
mod policy_tests {
    use super::{
        AgentRequest, CapabilityScope, DenialKind, EffectiveStack, PolicyProvenance, RequestKind,
        authorize, parse_policy, project_policy_path, user_policy_path_with_env,
    };

    fn stack_with(user_text: &str, project_text: Option<&str>) -> EffectiveStack {
        let user = parse_policy(user_text, PolicyProvenance::User, "user:test").unwrap();
        let project = project_text
            .map(|text| parse_policy(text, PolicyProvenance::Project, "project:test").unwrap());
        EffectiveStack {
            host: CapabilityScope::unconstrained("host"),
            user,
            project,
            parent: CapabilityScope::unconstrained("parent"),
            task: CapabilityScope::unconstrained("task"),
        }
    }

    #[test]
    fn user_policy_narrows_request() {
        let stack = stack_with("allow terminal.semantic-read\n", None);
        let mut scope = CapabilityScope::unconstrained("lua");
        scope
            .caps
            .insert(super::CapabilityId::parse("terminal.semantic-read").unwrap());
        let request = AgentRequest {
            scope,
            raw_wide: Vec::new(),
        };
        let effective = authorize(&stack, &request, RequestKind::PluginLifecycle, true).unwrap();
        assert!(
            effective
                .caps
                .iter()
                .any(|c| c.as_str() == "terminal.semantic-read")
        );
        // Omitted capability is not granted.
        assert!(!effective.caps.iter().any(|c| c.as_str() == "ui.rich"));
    }

    #[test]
    fn project_policy_narrows_below_user() {
        let stack = stack_with(
            "allow terminal.semantic-read\nallow ui.rich\n",
            Some("allow terminal.semantic-read\n"),
        );
        let mut scope = CapabilityScope::unconstrained("lua");
        for raw in ["terminal.semantic-read", "ui.rich"] {
            scope.caps.insert(super::CapabilityId::parse(raw).unwrap());
        }
        let request = AgentRequest {
            scope,
            raw_wide: Vec::new(),
        };
        let denial = authorize(&stack, &request, RequestKind::PluginLifecycle, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
        assert!(
            denial
                .reason_chain()
                .iter()
                .any(|line| line.contains("project-policy"))
        );
    }

    #[test]
    fn non_overridable_pin_survives_lower_layers() {
        // Host pins max_agents; a project policy lowering it conflicts.
        let mut host = CapabilityScope::unconstrained("system-policy:policy.lua");
        host.max_agents = Some(8);
        host.pins.insert("max_agents".to_string());
        let stack = EffectiveStack {
            host,
            user: CapabilityScope::unconstrained("user"),
            project: Some(
                parse_policy("max_agents 4\n", PolicyProvenance::Project, "project:test").unwrap(),
            ),
            parent: CapabilityScope::unconstrained("parent"),
            task: CapabilityScope::unconstrained("task"),
        };
        let request = AgentRequest::empty("lua");
        let denial = authorize(&stack, &request, RequestKind::AgentSpawn, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::PolicyConflict);
    }

    #[test]
    fn untrusted_project_never_contributes() {
        let stack = stack_with(
            "allow terminal.semantic-read\n",
            Some("allow terminal.semantic-read\nallow ui.rich\n"),
        );
        let request = AgentRequest::empty("lua");
        let denial = authorize(&stack, &request, RequestKind::FsRead, false).unwrap_err();
        assert_eq!(denial.kind, DenialKind::UntrustedProject);
    }

    #[test]
    fn deny_entries_win_across_layers() {
        let stack = stack_with("allow ui.rich\ndeny ui.rich\n", None);
        let mut scope = CapabilityScope::unconstrained("lua");
        scope
            .caps
            .insert(super::CapabilityId::parse("ui.rich").unwrap());
        let request = AgentRequest {
            scope,
            raw_wide: Vec::new(),
        };
        let denial = authorize(&stack, &request, RequestKind::PluginLifecycle, true).unwrap_err();
        assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
    }

    #[test]
    fn malformed_policy_fails_closed() {
        assert!(parse_policy("allow fs.read\n", PolicyProvenance::User, "u").is_err());
        assert!(parse_policy("frobnicate x\n", PolicyProvenance::User, "u").is_err());
        assert!(parse_policy("max_agents lots\n", PolicyProvenance::User, "u").is_err());
        assert!(parse_policy("allow_root maybe\n", PolicyProvenance::User, "u").is_err());
        assert!(parse_policy("pin ui.rich\n", PolicyProvenance::User, "u").is_err());
        assert!(
            parse_policy(
                "allow ui.rich\nallow ui.rich extra\n",
                PolicyProvenance::User,
                "u"
            )
            .is_err()
        );
    }

    #[test]
    fn policy_paths_derive_from_environment() {
        assert_eq!(
            user_policy_path_with_env(Some("/xdg"), Some("/home/u")),
            Some("/xdg/bitty/capabilities.conf".to_string())
        );
        assert_eq!(
            user_policy_path_with_env(None, Some("/home/u")),
            Some("/home/u/.config/bitty/capabilities.conf".to_string())
        );
        assert_eq!(user_policy_path_with_env(None, None), None);
        assert_eq!(
            project_policy_path("/repo"),
            "/repo/.bitty/capabilities.conf".to_string()
        );
    }
}
