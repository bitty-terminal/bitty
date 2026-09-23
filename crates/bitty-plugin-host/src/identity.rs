//! Candidate core ontology and identity model (OQ-084).
//!
//! `OQ-084` is still open: no accepted document fixes the first-class
//! concepts (Instance, Workspace, Panel, Surface, ExecutionContext,
//! Terminal, Session, Resource, Service, Agent), their ownership,
//! lifetime, persistence, permission, or the relationships among
//! `PanelId`, `WorkspaceId`, `ResourceId`, `ExecutionContextId`,
//! `AgentId`, and `GenerationId`. This module records the candidate
//! direction only, as pure, bounded, fail-closed identity data.
//!
//! Nothing here is wired into any live path: these identifiers are not
//! the Stable Ids, not the plugin [`crate::manifest::PluginId`], and not
//! the agent `AgentId` in `bitty-agent`. They confer no authority; they
//! only name entities so a future ruling has a tested shape to accept
//! or replace. Malformed identifiers fail validation instead of being
//! normalized.
//!
//! # Non-goals
//!
//! Persistence, permission checks, cross-entity relationships, and the
//! Panel/Execution and Restore/Persistence refinements stay undecided
//! until the OQ-084 ruling. There is no `unsafe`, no I/O, and no new
//! dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of one ontology identifier value.
pub const MAX_ONTOLOGY_ID_BYTES: usize = 128;

// ── entity kinds ──────────────────────────────────────────────────────────

/// Candidate first-class entity kinds (OQ-084).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntityKind {
    Instance,
    Workspace,
    Panel,
    Surface,
    ExecutionContext,
    Terminal,
    Session,
    Resource,
    Service,
    Agent,
}

impl EntityKind {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Workspace => "workspace",
            Self::Panel => "panel",
            Self::Surface => "surface",
            Self::ExecutionContext => "execution-context",
            Self::Terminal => "terminal",
            Self::Session => "session",
            Self::Resource => "resource",
            Self::Service => "service",
            Self::Agent => "agent",
        }
    }

    /// Parse a kind label; unknown kinds fail closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_ONTOLOGY_ID_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "entity_kind".to_string(),
                limit: MAX_ONTOLOGY_ID_BYTES,
                actual: s.len(),
            });
        }
        match s {
            "instance" => Ok(Self::Instance),
            "workspace" => Ok(Self::Workspace),
            "panel" => Ok(Self::Panel),
            "surface" => Ok(Self::Surface),
            "execution-context" => Ok(Self::ExecutionContext),
            "terminal" => Ok(Self::Terminal),
            "session" => Ok(Self::Session),
            "resource" => Ok(Self::Resource),
            "service" => Ok(Self::Service),
            "agent" => Ok(Self::Agent),
            _ => Err(PluginError::registry(format!(
                "unknown entity kind '{s}' (OQ-084 candidate; deny by default)"
            ))),
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── identifiers ───────────────────────────────────────────────────────────

/// Candidate ontology identifier: a kind plus an opaque value.
///
/// Values are opaque to the host: non-empty, bounded, and restricted to
/// `A–Z a–z 0–9 - _ .`. Anything else fails validation — the host never
/// normalizes an identifier into a different entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OntologyId {
    kind: EntityKind,
    value: String,
}

impl OntologyId {
    /// Build an identifier, validating the value.
    pub fn new(kind: EntityKind, value: impl Into<String>) -> Result<Self, PluginError> {
        let value = value.into();
        validate_id_value(&value)?;
        Ok(Self { kind, value })
    }

    /// Entity kind.
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// Opaque value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Parse `kind:value` text; malformed input fails closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_ONTOLOGY_ID_BYTES + 32 {
            return Err(PluginError::LimitExceeded {
                field: "ontology_id".to_string(),
                limit: MAX_ONTOLOGY_ID_BYTES + 32,
                actual: s.len(),
            });
        }
        let (kind_text, value) = s.split_once(':').ok_or_else(|| {
            PluginError::registry(format!(
                "ontology id '{s}' must be 'kind:value' (OQ-084 candidate)"
            ))
        })?;
        let kind = EntityKind::parse(kind_text)?;
        Self::new(kind, value)
    }
}

impl fmt::Display for OntologyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.value)
    }
}

/// Validate one identifier value (shared by constructors).
fn validate_id_value(value: &str) -> Result<(), PluginError> {
    if value.is_empty() {
        return Err(PluginError::registry(
            "ontology id value must not be empty".to_string(),
        ));
    }
    if value.len() > MAX_ONTOLOGY_ID_BYTES {
        return Err(PluginError::LimitExceeded {
            field: "ontology_id.value".to_string(),
            limit: MAX_ONTOLOGY_ID_BYTES,
            actual: value.len(),
        });
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(PluginError::registry(format!(
            "ontology id value '{value}' uses characters outside [A-Za-z0-9-_.]"
        )));
    }
    Ok(())
}

// ── lifetime and ownership ────────────────────────────────────────────────

/// Candidate lifetime of an entity (OQ-084).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lifetime {
    /// Dies with the creating operation (never persisted).
    Ephemeral,
    /// Lives while its session/workspace lives.
    SessionScoped,
    /// Survives restarts (persistence contract still undecided).
    Persistent,
}

impl Lifetime {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::SessionScoped => "session-scoped",
            Self::Persistent => "persistent",
        }
    }
}

impl fmt::Display for Lifetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Candidate ownership link: an entity plus its optional owner.
///
/// The owner must name a *different* entity (self-ownership fails
/// closed). Chain resolution itself stays undecided until the ruling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownership {
    entity: OntologyId,
    owner: Option<OntologyId>,
    lifetime: Lifetime,
}

impl Ownership {
    /// Build an ownership link; self-ownership fails closed.
    pub fn new(
        entity: OntologyId,
        owner: Option<OntologyId>,
        lifetime: Lifetime,
    ) -> Result<Self, PluginError> {
        if let Some(ref owner_id) = owner {
            if owner_id == &entity {
                return Err(PluginError::registry(format!(
                    "entity '{entity}' cannot own itself (OQ-084 candidate)"
                )));
            }
        }
        Ok(Self {
            entity,
            owner,
            lifetime,
        })
    }

    /// Owned entity.
    #[must_use]
    pub fn entity(&self) -> &OntologyId {
        &self.entity
    }

    /// Owner, if any.
    #[must_use]
    pub fn owner(&self) -> Option<&OntologyId> {
        self.owner.as_ref()
    }

    /// Lifetime.
    #[must_use]
    pub fn lifetime(&self) -> Lifetime {
        self.lifetime
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for kind in [
            EntityKind::Instance,
            EntityKind::Workspace,
            EntityKind::Panel,
            EntityKind::Surface,
            EntityKind::ExecutionContext,
            EntityKind::Terminal,
            EntityKind::Session,
            EntityKind::Resource,
            EntityKind::Service,
            EntityKind::Agent,
        ] {
            assert_eq!(EntityKind::parse(kind.as_str()), Ok(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn unknown_kind_denies() {
        assert!(EntityKind::parse("pane").is_err());
        assert!(EntityKind::parse("").is_err());
        assert!(EntityKind::parse("Panel").is_err());
    }

    #[test]
    fn id_text_round_trip() {
        let id = OntologyId::new(EntityKind::Panel, "panel-01").expect("valid id");
        assert_eq!(id.kind(), EntityKind::Panel);
        assert_eq!(id.value(), "panel-01");
        assert_eq!(OntologyId::parse("panel:panel-01"), Ok(id.clone()));
        assert_eq!(id.to_string(), "panel:panel-01");
    }

    #[test]
    fn malformed_ids_deny() {
        assert!(OntologyId::parse("panel").is_err());
        assert!(OntologyId::parse("pane:x").is_err());
        assert!(OntologyId::parse(":x").is_err());
        assert!(OntologyId::new(EntityKind::Agent, "").is_err());
        assert!(OntologyId::new(EntityKind::Agent, "has space").is_err());
        assert!(OntologyId::new(EntityKind::Agent, "semi;colon").is_err());
        assert!(OntologyId::new(EntityKind::Agent, "uniçode").is_err());
    }

    #[test]
    fn oversize_id_denies() {
        let long = "a".repeat(MAX_ONTOLOGY_ID_BYTES + 1);
        assert!(OntologyId::new(EntityKind::Session, long).is_err());
    }

    #[test]
    fn self_ownership_denies() {
        let id = OntologyId::new(EntityKind::Workspace, "ws1").expect("valid id");
        assert!(Ownership::new(id.clone(), Some(id), Lifetime::SessionScoped).is_err());
    }

    #[test]
    fn ownership_links() {
        let panel = OntologyId::new(EntityKind::Panel, "p1").expect("valid id");
        let ws = OntologyId::new(EntityKind::Workspace, "w1").expect("valid id");
        let link = Ownership::new(panel.clone(), Some(ws.clone()), Lifetime::SessionScoped)
            .expect("valid link");
        assert_eq!(link.entity(), &panel);
        assert_eq!(link.owner(), Some(&ws));
        let root = Ownership::new(ws, None, Lifetime::Persistent).expect("valid root");
        assert_eq!(root.owner(), None);
        assert_eq!(root.lifetime(), Lifetime::Persistent);
    }

    #[test]
    fn lifetime_labels_stable() {
        assert_eq!(Lifetime::Ephemeral.to_string(), "ephemeral");
        assert_eq!(Lifetime::SessionScoped.to_string(), "session-scoped");
        assert_eq!(Lifetime::Persistent.to_string(), "persistent");
    }
}
