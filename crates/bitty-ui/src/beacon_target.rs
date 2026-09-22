//! Beacon target handles (UX-28, U-8 Beacon family).
//!
//! [`TargetRef`] is the typed generation handle the Beacon hint layer
//! addresses: one variant per addressable surface (`Panel`, `Workspace`,
//! `CommandBlock`, `UiNode`, `Link`). Every handle carries a generation
//! captured at enumeration time; [`TargetRegistry::resolve`] revalidates it
//! against the live table and fails closed ([`TargetError::StaleTarget`])
//! when the generation moved or the id retired. There is deliberately no
//! `ViewId` variant and no `From<ViewId>` bridge: views are layout leaves,
//! never beacon targets.
//!
//! All types are bounded ([`MAX_TARGETS_PER_KIND`] per kind),
//! `#![forbid(unsafe_code)]`, deterministic, and headless: no I/O,
//! wall-clock, randomness, or render coupling.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use crate::panel::PanelId;

/// Maximum live targets retained per target kind.
pub const MAX_TARGETS_PER_KIND: usize = 1024;

/// Stable handle for a workspace. Distinct newtype; no `From` bridge to any
/// other id type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(pub u64);

impl WorkspaceId {
    /// Creates a workspace id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkspaceId({})", self.0)
    }
}

/// Stable handle for a command block (scrollback region promoted to a
/// beacon-addressable unit). Distinct newtype; no `From` bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandBlockId(pub u64);

impl CommandBlockId {
    /// Creates a command-block id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CommandBlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommandBlockId({})", self.0)
    }
}

/// Stable handle for a chrome/UI-tree node. Distinct newtype; no `From`
/// bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UiNodeId(pub u64);

impl UiNodeId {
    /// Creates a UI-node id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for UiNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UiNodeId({})", self.0)
    }
}

/// Stable handle for a link (OSC-8 hyperlink or chrome link). Distinct
/// newtype; no `From` bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(pub u64);

impl LinkId {
    /// Creates a link id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for LinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinkId({})", self.0)
    }
}

/// Generation handle for a panel beacon target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelRef {
    /// Target panel.
    pub id: PanelId,
    /// Generation captured at enumeration; must match the live table.
    pub generation: u64,
}

/// Generation handle for a workspace beacon target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceRef {
    /// Target workspace.
    pub id: WorkspaceId,
    /// Generation captured at enumeration; must match the live table.
    pub generation: u64,
}

/// Generation handle for a command-block beacon target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandBlockRef {
    /// Target command block.
    pub id: CommandBlockId,
    /// Generation captured at enumeration; must match the live table.
    pub generation: u64,
}

/// Generation handle for a chrome/UI-node beacon target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiNodeRef {
    /// Target UI node.
    pub id: UiNodeId,
    /// Generation captured at enumeration; must match the live table.
    pub generation: u64,
}

/// Generation handle for a link beacon target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkRef {
    /// Target link.
    pub id: LinkId,
    /// Generation captured at enumeration; must match the live table.
    pub generation: u64,
}

/// Typed beacon target: exactly the five addressable surfaces. `ViewId` is
/// deliberately excluded (views are layout leaves, never beacon targets).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetRef {
    /// A hosted panel.
    Panel(PanelRef),
    /// A workspace.
    Workspace(WorkspaceRef),
    /// A scrollback command block.
    CommandBlock(CommandBlockRef),
    /// A chrome/UI-tree node.
    UiNode(UiNodeRef),
    /// A hyperlink.
    Link(LinkRef),
}

impl std::fmt::Display for TargetRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panel(r) => write!(f, "Panel({}, gen {})", r.id, r.generation),
            Self::Workspace(r) => write!(f, "Workspace({}, gen {})", r.id, r.generation),
            Self::CommandBlock(r) => {
                write!(f, "CommandBlock({}, gen {})", r.id, r.generation)
            }
            Self::UiNode(r) => write!(f, "UiNode({}, gen {})", r.id, r.generation),
            Self::Link(r) => write!(f, "Link({}, gen {})", r.id, r.generation),
        }
    }
}

/// Beacon target resolution failure. All variants fail closed: the caller
/// must drop the hint session, never fall back to a default target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// The id is unknown to the live table.
    UnknownTarget(String),
    /// The generation moved or the id retired after enumeration.
    StaleTarget(String),
    /// The kind table is at [`MAX_TARGETS_PER_KIND`].
    TooManyTargets {
        /// Enforced cap.
        max: usize,
        /// Live entries in the kind table.
        current: usize,
    },
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTarget(detail) => write!(f, "unknown beacon target: {detail}"),
            Self::StaleTarget(detail) => write!(f, "stale beacon target: {detail}"),
            Self::TooManyTargets { max, current } => {
                write!(f, "too many beacon targets: max {max}, current {current}")
            }
        }
    }
}

impl std::error::Error for TargetError {}

/// Live beacon-target table. Each kind maps id to its current generation;
/// [`TargetRef`] handles resolve only when the captured generation matches.
/// Registration bumps the generation so previously enumerated handles go
/// stale; retirement removes the id (recording a bounded tombstone) so all
/// outstanding handles resolve [`TargetError::StaleTarget`].
#[derive(Clone, Debug, Default)]
pub struct TargetRegistry {
    panels: HashMap<u64, u64>,
    workspaces: HashMap<u64, u64>,
    blocks: HashMap<u64, u64>,
    nodes: HashMap<u64, u64>,
    links: HashMap<u64, u64>,
    /// `(kind_tag, id)` tombstones for retired ids, so retirement reports
    /// `StaleTarget` rather than `UnknownTarget`. Bounded to
    /// [`MAX_TARGETS_PER_KIND`] entries; beyond that new retirements fall
    /// back to `UnknownTarget` — still fail-closed, only less precise.
    retired: HashSet<(u8, u64)>,
    /// Monotonic generation issuer. Every registration — fresh or
    /// re-register — takes the next value, so generations never repeat
    /// within a registry lifetime (mod `u64` wrap) and a stale handle can
    /// never false-accept after its id is re-registered.
    issuer: u64,
}

/// Kind tags for [`TargetRegistry`] tombstones.
const KIND_PANEL: u8 = 0;
/// Kind tags for [`TargetRegistry`] tombstones.
const KIND_WORKSPACE: u8 = 1;
/// Kind tags for [`TargetRegistry`] tombstones.
const KIND_BLOCK: u8 = 2;
/// Kind tags for [`TargetRegistry`] tombstones.
const KIND_NODE: u8 = 3;
/// Kind tags for [`TargetRegistry`] tombstones.
const KIND_LINK: u8 = 4;

impl TargetRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panels: HashMap::new(),
            workspaces: HashMap::new(),
            blocks: HashMap::new(),
            nodes: HashMap::new(),
            links: HashMap::new(),
            retired: HashSet::new(),
            issuer: 0,
        }
    }

    /// Total live entries across all kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
            + self.workspaces.len()
            + self.blocks.len()
            + self.nodes.len()
            + self.links.len()
    }

    /// True when no target is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Registers (or re-registers) a panel, returning its fresh handle.
    /// Re-registration issues a new generation, staling prior handles.
    pub fn insert_panel(&mut self, id: PanelId) -> Result<PanelRef, TargetError> {
        let generation = Self::issue(&mut self.panels, &mut self.issuer, id.0)?;
        Ok(PanelRef { id, generation })
    }

    /// Registers (or re-registers) a workspace, returning its fresh handle.
    pub fn insert_workspace(&mut self, id: WorkspaceId) -> Result<WorkspaceRef, TargetError> {
        let generation = Self::issue(&mut self.workspaces, &mut self.issuer, id.0)?;
        Ok(WorkspaceRef { id, generation })
    }

    /// Registers (or re-registers) a command block, returning its handle.
    pub fn insert_block(&mut self, id: CommandBlockId) -> Result<CommandBlockRef, TargetError> {
        let generation = Self::issue(&mut self.blocks, &mut self.issuer, id.0)?;
        Ok(CommandBlockRef { id, generation })
    }

    /// Registers (or re-registers) a UI node, returning its handle.
    pub fn insert_node(&mut self, id: UiNodeId) -> Result<UiNodeRef, TargetError> {
        let generation = Self::issue(&mut self.nodes, &mut self.issuer, id.0)?;
        Ok(UiNodeRef { id, generation })
    }

    /// Registers (or re-registers) a link, returning its handle.
    pub fn insert_link(&mut self, id: LinkId) -> Result<LinkRef, TargetError> {
        let generation = Self::issue(&mut self.links, &mut self.issuer, id.0)?;
        Ok(LinkRef { id, generation })
    }

    /// Retires a panel id; outstanding handles resolve stale. Returns true
    /// when the id was live.
    pub fn retire_panel(&mut self, id: PanelId) -> bool {
        let live = self.panels.remove(&id.0).is_some();
        if live {
            self.note_retired(KIND_PANEL, id.0);
        }
        live
    }

    /// Retires a workspace id. Returns true when the id was live.
    pub fn retire_workspace(&mut self, id: WorkspaceId) -> bool {
        let live = self.workspaces.remove(&id.0).is_some();
        if live {
            self.note_retired(KIND_WORKSPACE, id.0);
        }
        live
    }

    /// Retires a command-block id. Returns true when the id was live.
    pub fn retire_block(&mut self, id: CommandBlockId) -> bool {
        let live = self.blocks.remove(&id.0).is_some();
        if live {
            self.note_retired(KIND_BLOCK, id.0);
        }
        live
    }

    /// Retires a UI-node id. Returns true when the id was live.
    pub fn retire_node(&mut self, id: UiNodeId) -> bool {
        let live = self.nodes.remove(&id.0).is_some();
        if live {
            self.note_retired(KIND_NODE, id.0);
        }
        live
    }

    /// Retires a link id. Returns true when the id was live.
    pub fn retire_link(&mut self, id: LinkId) -> bool {
        let live = self.links.remove(&id.0).is_some();
        if live {
            self.note_retired(KIND_LINK, id.0);
        }
        live
    }

    /// Issues a fresh monotonic generation for `raw` inside `table`,
    /// enforcing the per-kind cap on first insert. Associated function (no
    /// `self`) so callers can borrow the kind table and the issuer
    /// disjointly.
    fn issue(
        table: &mut HashMap<u64, u64>,
        issuer: &mut u64,
        raw: u64,
    ) -> Result<u64, TargetError> {
        if !table.contains_key(&raw) && table.len() >= MAX_TARGETS_PER_KIND {
            return Err(TargetError::TooManyTargets {
                max: MAX_TARGETS_PER_KIND,
                current: table.len(),
            });
        }
        let generation = *issuer;
        *issuer = issuer.wrapping_add(1);
        table.insert(raw, generation);
        Ok(generation)
    }

    /// Records a retirement tombstone, bounded by [`MAX_TARGETS_PER_KIND`].
    fn note_retired(&mut self, kind: u8, raw: u64) {
        if self.retired.len() < MAX_TARGETS_PER_KIND {
            self.retired.insert((kind, raw));
        }
    }

    /// Resolves `target` against the live table. Returns the target itself
    /// when its generation is current; fails closed with
    /// [`TargetError::StaleTarget`] (retired or bumped) or
    /// [`TargetError::UnknownTarget`] (never registered) otherwise.
    pub fn resolve(&self, target: &TargetRef) -> Result<TargetRef, TargetError> {
        let (live, tombstone) = match target {
            TargetRef::Panel(r) => (self.panels.get(&r.id.0).copied(), (KIND_PANEL, r.id.0)),
            TargetRef::Workspace(r) => (
                self.workspaces.get(&r.id.0).copied(),
                (KIND_WORKSPACE, r.id.0),
            ),
            TargetRef::CommandBlock(r) => (self.blocks.get(&r.id.0).copied(), (KIND_BLOCK, r.id.0)),
            TargetRef::UiNode(r) => (self.nodes.get(&r.id.0).copied(), (KIND_NODE, r.id.0)),
            TargetRef::Link(r) => (self.links.get(&r.id.0).copied(), (KIND_LINK, r.id.0)),
        };
        match live {
            None if self.retired.contains(&tombstone) => {
                Err(TargetError::StaleTarget(target.to_string()))
            }
            None => Err(TargetError::UnknownTarget(target.to_string())),
            Some(generation) if generation == captured_generation(target) => Ok(*target),
            Some(_) => Err(TargetError::StaleTarget(target.to_string())),
        }
    }
}

/// Generation captured inside `target`.
fn captured_generation(target: &TargetRef) -> u64 {
    match target {
        TargetRef::Panel(r) => r.generation,
        TargetRef::Workspace(r) => r.generation,
        TargetRef::CommandBlock(r) => r.generation,
        TargetRef::UiNode(r) => r.generation,
        TargetRef::Link(r) => r.generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::PanelId;

    fn all_kinds(registry: &mut TargetRegistry) -> Vec<TargetRef> {
        vec![
            TargetRef::Panel(registry.insert_panel(PanelId::new(1)).expect("panel")),
            TargetRef::Workspace(
                registry
                    .insert_workspace(WorkspaceId::new(2))
                    .expect("workspace"),
            ),
            TargetRef::CommandBlock(
                registry
                    .insert_block(CommandBlockId::new(3))
                    .expect("block"),
            ),
            TargetRef::UiNode(registry.insert_node(UiNodeId::new(4)).expect("node")),
            TargetRef::Link(registry.insert_link(LinkId::new(5)).expect("link")),
        ]
    }

    #[test]
    fn fresh_handles_resolve() {
        let mut registry = TargetRegistry::new();
        assert!(registry.is_empty());
        for target in all_kinds(&mut registry) {
            assert_eq!(registry.resolve(&target), Ok(target));
        }
        assert_eq!(registry.len(), 5);
    }

    #[test]
    fn retired_id_fails_closed() {
        let mut registry = TargetRegistry::new();
        let targets = all_kinds(&mut registry);
        assert!(registry.retire_panel(PanelId::new(1)));
        assert!(registry.retire_workspace(WorkspaceId::new(2)));
        assert!(registry.retire_block(CommandBlockId::new(3)));
        assert!(registry.retire_node(UiNodeId::new(4)));
        assert!(registry.retire_link(LinkId::new(5)));
        for target in &targets {
            assert!(matches!(
                registry.resolve(target),
                Err(TargetError::StaleTarget(_))
            ));
        }
    }

    #[test]
    fn reregister_stales_prior_handle() {
        let mut registry = TargetRegistry::new();
        let first = registry.insert_link(LinkId::new(9)).expect("link");
        let second = registry.insert_link(LinkId::new(9)).expect("link");
        assert_ne!(first.generation, second.generation);
        assert!(matches!(
            registry.resolve(&TargetRef::Link(first)),
            Err(TargetError::StaleTarget(_))
        ));
        assert_eq!(
            registry.resolve(&TargetRef::Link(second)),
            Ok(TargetRef::Link(second))
        );
    }

    #[test]
    fn reregister_after_retire_never_reuses_generation() {
        let mut registry = TargetRegistry::new();
        let first = registry.insert_panel(PanelId::new(7)).expect("panel");
        assert!(registry.retire_panel(PanelId::new(7)));
        let second = registry.insert_panel(PanelId::new(7)).expect("panel");
        assert_ne!(first.generation, second.generation);
        // The pre-retirement handle must not false-accept against the new
        // registration.
        assert!(matches!(
            registry.resolve(&TargetRef::Panel(first)),
            Err(TargetError::StaleTarget(_))
        ));
        assert_eq!(
            registry.resolve(&TargetRef::Panel(second)),
            Ok(TargetRef::Panel(second))
        );
    }

    #[test]
    fn never_registered_is_unknown_not_stale() {
        let registry = TargetRegistry::new();
        let ghost = TargetRef::Panel(PanelRef {
            id: PanelId::new(4242),
            generation: 0,
        });
        assert!(matches!(
            registry.resolve(&ghost),
            Err(TargetError::UnknownTarget(_))
        ));
    }

    #[test]
    fn target_enum_covers_exactly_five_surfaces() {
        // Exhaustiveness is compile-checked: adding a sixth variant (e.g. a
        // ViewId leaf) breaks this match. ViewId has no mapping here by
        // construction — there is no variant and no From<ViewId> impl.
        let mut registry = TargetRegistry::new();
        for target in all_kinds(&mut registry) {
            let kind = match target {
                TargetRef::Panel(_) => "panel",
                TargetRef::Workspace(_) => "workspace",
                TargetRef::CommandBlock(_) => "block",
                TargetRef::UiNode(_) => "node",
                TargetRef::Link(_) => "link",
            };
            assert!(registry.resolve(&target).is_ok(), "{kind} must resolve");
        }
    }

    #[test]
    fn kind_table_enforces_cap() {
        let mut registry = TargetRegistry::new();
        for raw in 0..(MAX_TARGETS_PER_KIND as u64) {
            registry.insert_node(UiNodeId::new(raw)).expect("node");
        }
        let err = registry
            .insert_node(UiNodeId::new(MAX_TARGETS_PER_KIND as u64))
            .expect_err("cap must hold");
        assert!(matches!(err, TargetError::TooManyTargets { .. }));
    }
}
