//! `registry` — Panel Runtime domain (`PanelRegistry`, event bus, overlays,
//! commands, capabilities) per CTX-0102.
//!
//! Split from `super` (`registry.rs`) as a pure move under CTX-0308:
//! byte-identical logic, only module wiring changed.

use super::*;

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Panel Runtime domain (CTX-0102) — generic Panel Runtime per 9032d1e
// ---------------------------------------------------------------------------
//
// Implements PanelId distinct newtype, generation monotonic, lifecycle
// Declared->Created->Mounted->Focused->Suspended->Disposed, command registry
// owner.name:command, overlay max 4+1, focus MRU per Window/Workspace,
// EventBus 64/1024/2MiB/8192 DropOldest, panel.* capability per
// (PanelId,generation). Single-process winit window, one registry per
// window, no bittyd/remote. Bounded, no unsafe, headless testable.
//
// Placement: `Instance -> Window -> Workspace -> LayoutTree -> View`
// stays authoritative; Panel is typed View content (`ViewContent::Panel`)
// per Option A, reusing ViewId generation, focus MRU, visibility.
// The panel runtime owns panel lifecycle, the workspace owns layout,
// the terminal registry owns PTY descriptors; no view/panel holds PTY fd,
// GPU object, or OS window handle.

// Re-export canonical Panel types from bitty-ui for single definition.
pub use bitty_ui::panel::{
    MAX_COMMANDS_PER_PANEL_TYPE, MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN,
    MAX_OVERLAYS_PER_WINDOW, Overlay, OverlayError, OverlayKind, OverlayManager, PanelFocus,
    QualifiedCommand, ViewContent,
};
pub use bitty_ui::panel::{PanelId, PanelState, PanelType};
pub use bitty_ui::panel_recovery::{
    MAX_RECOVERY_KEYS_PER_PANEL, MAX_RECOVERY_RECORDS_PER_PANEL, PlaceholderReason,
    PlaceholderScene, RECOVERY_ATTEMPT_CAP, RecoveryAction, RecoveryBudget, RecoveryOutcome,
    RecoveryReason, RecoveryRecord, RecoveryStep,
};

// PanelId distinctness is already enforced by `bitty_ui::PanelId` being a
// newtype with no From bridge to `ViewId`/`TerminalId`. Re-exported here so
// `registry::PanelId` is the same canonical type but still pairwise
// incompatible at type level across crates (requires explicit import).

pub const MAX_PANELS_PER_WORKSPACE: usize = 32;
pub const MAX_PANELS_PER_WINDOW: usize = 64;
pub const DEFAULT_MAX_PANELS_PER_WORKSPACE: usize = 16;
pub const DEFAULT_MAX_PANELS_PER_WINDOW: usize = 32;
pub const MAX_TOPICS_TOTAL: usize = 256;
pub const MAX_SUBSCRIPTIONS_PER_PANEL: usize = 32;
pub const MAX_PANEL_COMMANDS_PER_TYPE: usize = 32;
pub const BUS_PER_SUBSCRIPTION_LIMIT: usize = 64;
pub const BUS_PER_PANEL_LIMIT: usize = 1024;
pub const BUS_PER_PANEL_BYTES_LIMIT: usize = 256 * 1024;
pub const BUS_GLOBAL_LIMIT: usize = 8192;
pub const BUS_GLOBAL_BYTES_LIMIT: usize = 2 * 1024 * 1024;
pub const BUS_EVENT_MAX_BYTES: usize = 8 * 1024;
pub const BUS_BATCH_MAX_EVENTS: usize = 32;
pub const BUS_BATCH_MAX_BYTES: usize = 8 * 1024;

/// Panel generation is the same monotonic `Generation` type; panels bump the
/// same registry generation counter so stale `(PanelId, Generation)` handles
/// are detectable, mirroring terminal/view rules.
pub type PanelGeneration = Generation;

/// Handle for a panel instance: `(PanelId, Generation)` pair that must be
/// validated on every cross-component call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelHandle {
    pub id: PanelId,
    pub generation: Generation,
}

/// Panel-specific error type; leaves previous valid state intact (fail-closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelError {
    TooManyPanels {
        max: usize,
        current: usize,
    },
    TooManyTopics {
        max: usize,
        current: usize,
    },
    TooManySubscriptions {
        max: usize,
        current: usize,
    },
    PayloadTooLarge {
        bytes: usize,
        max: usize,
    },
    UnknownPanelType {
        value: String,
    },
    UnknownTopic {
        topic: String,
    },
    UndisclosedTopic {
        topic: String,
    },
    AlreadyMounted {
        view_id: ViewId,
        existing: ViewContent,
    },
    PanelAlreadyMounted {
        panel_id: PanelId,
        current_view: ViewId,
    },
    StaleHandle {
        expected_generation: Generation,
        found_generation: Generation,
        id_raw: u64,
    },
    RegistryDisposed {
        generation: Generation,
    },
    GenerationExhausted {
        current: Generation,
    },
    OverlayBusy,
    TooManyOverlays {
        max: usize,
        current: usize,
    },
    CapabilityDenied {
        panel_id: PanelId,
        capability: String,
    },
    InvalidCommand {
        reason: String,
    },
    DuplicateCommand {
        command: String,
        owner: PanelId,
    },
    TooManyCommands {
        max: usize,
        current: usize,
    },
    NotFound {
        kind: &'static str,
        id_raw: u64,
    },
    /// A panel lease transition was refused; the lease is unchanged
    /// (RUN-21, #1052). `reason` carries the stable kernel audit name
    /// (`already_occupied` / `not_occupied` / `not_holder`) plus the
    /// human-readable cause.
    LeaseDenied {
        panel_id: PanelId,
        reason: String,
    },
    /// A panel title or description failed the lease-kernel bounds
    /// (RUN-21, #1052): empty or over-long titles, control characters, or
    /// over-long descriptions. Nothing was stored.
    InvalidDescription {
        reason: String,
    },
    InvalidState {
        current: PanelState,
        expected: &'static str,
    },
    /// A bind found a previous controller generation for the panel but no
    /// retired-ceiling tombstone that names it (OQ-084 D.2 phase 0). The
    /// ceiling record was lost, so the bind is refused instead of granting the
    /// whole successor resolution. Nothing was installed and no binding was
    /// written.
    CeilingLost {
        panel_id: PanelId,
        bind_generation: Generation,
        /// The record's retained last controller generation, when it names
        /// one.
        last_controller_generation: Option<Generation>,
        /// The tombstone generation when a tombstone exists but does not name
        /// the record's last controller; `None` when no tombstone was found.
        tombstone_generation: Option<Generation>,
    },
    /// A panel verb named a controller generation that is not the one the
    /// record binds (OQ-084 D.2, "Two typed refusals, neither borrowed from
    /// `StaleHandle`"). The verb touched no panel state, so the binding in
    /// force is unchanged and the panel is left exactly as it was.
    ///
    /// Reachable wherever a binding exists: a verb that presents the allocation
    /// epoch while a controller is bound, and a bind that presents a generation
    /// the record already binds, both land here. While `PanelRecord::binding`
    /// is `None` the allocation epoch is the accepted key, so every call site on
    /// a panel that never bound a controller is unaffected. The generation edge
    /// only ever rebinds a container that already carries a binding; nothing
    /// writes a container's first binding yet (`bitty.panel.create` carries no
    /// `PluginId` and no generation, OQ-084 follow-up (e)), so tests mint that
    /// one.
    ControllerConflict {
        panel_id: PanelId,
        /// The generation the record's binding names, or `None` while no
        /// controller is bound — the allocation epoch is then the only
        /// accepted key, and a foreign generation is what is refused.
        bound_generation: Option<Generation>,
        /// The generation the refused verb presented.
        presented_generation: Generation,
    },
    /// A panel verb presented the controller generation whose binding was
    /// released, so the verb was in flight when its own generation was unbound
    /// (OQ-084 D.2 phase 1 step 5, C.5). Nothing of that generation was
    /// touched.
    ///
    /// Unreachable on a live path for the same reason as
    /// [`PanelError::ControllerConflict`]: only a bound-then-released panel can
    /// produce it.
    ControllerReleased {
        panel_id: PanelId,
        /// The ended controller generation the refused verb presented.
        released_generation: Generation,
    },
    /// A placeholder reason outside the Decision E vocabulary (OQ-084 E.2).
    ///
    /// The token was refused before any panel state was read or written: no
    /// placeholder was presented, no binding was released, and no recovery
    /// counter moved. A reason outside the set is a defect and is counted as
    /// such (`error_count("RecoveryReasonRejected")`), never passed through and
    /// never accepted from a user.
    RecoveryReasonRejected {
        panel_id: PanelId,
        /// The refused token, already bounded by the vocabulary's spelling cap.
        spelling: String,
    },
    /// The Core-owned placeholder block failed the accepted scene bounds
    /// (`SCN-1..3`). The table is fixed, so this is a Core defect rather than a
    /// caller error, and nothing was stored.
    PlaceholderSceneRejected {
        panel_id: PanelId,
        /// The scene admission detail, for the defect report.
        detail: String,
    },
    ResourceExhausted {
        reason: String,
    },
}

impl std::fmt::Display for PanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPanels { max, current } => {
                write!(f, "too many panels: max {max}, current {current}")
            }
            Self::TooManyTopics { max, current } => {
                write!(f, "too many topics: max {max}, current {current}")
            }
            Self::TooManySubscriptions { max, current } => {
                write!(f, "too many subscriptions: max {max}, current {current}")
            }
            Self::PayloadTooLarge { bytes, max } => {
                write!(f, "payload too large: {bytes} > {max}")
            }
            Self::UnknownPanelType { value } => write!(f, "unknown panel type '{value}'"),
            Self::UnknownTopic { topic } => write!(f, "unknown topic '{topic}'"),
            Self::UndisclosedTopic { topic } => write!(f, "undisclosed topic '{topic}'"),
            Self::AlreadyMounted { view_id, existing } => {
                write!(f, "view {view_id} already hosts {existing:?}")
            }
            Self::PanelAlreadyMounted {
                panel_id,
                current_view,
            } => write!(f, "panel {panel_id} already mounted at {current_view}"),
            Self::StaleHandle {
                expected_generation,
                found_generation,
                id_raw,
            } => write!(
                f,
                "stale panel handle id {id_raw}: expected {expected_generation}, found {found_generation}"
            ),
            Self::RegistryDisposed { generation } => {
                write!(f, "panel registry disposed at {generation}")
            }
            Self::GenerationExhausted { current } => {
                write!(f, "generation exhausted at {current}")
            }
            Self::OverlayBusy => f.write_str("modal overlay already active (OverlayBusy)"),
            Self::TooManyOverlays { max, current } => {
                write!(f, "too many overlays: max {max}, current {current}")
            }
            Self::CapabilityDenied {
                panel_id,
                capability,
            } => write!(f, "panel {panel_id} missing capability '{capability}'"),
            Self::InvalidCommand { reason } => write!(f, "invalid command: {reason}"),
            Self::DuplicateCommand { command, owner } => {
                write!(f, "duplicate command '{command}' already owned by {owner}")
            }
            Self::TooManyCommands { max, current } => {
                write!(f, "too many commands: max {max}, current {current}")
            }
            Self::NotFound { kind, id_raw } => write!(f, "{kind} {id_raw} not found"),
            Self::LeaseDenied { panel_id, reason } => {
                write!(f, "panel {panel_id} lease denied: {reason}")
            }
            Self::InvalidDescription { reason } => {
                write!(f, "invalid panel description: {reason}")
            }
            Self::InvalidState { current, expected } => {
                write!(f, "invalid state {current}: expected {expected}")
            }
            Self::CeilingLost {
                panel_id,
                bind_generation,
                last_controller_generation,
                tombstone_generation,
            } => {
                write!(
                    f,
                    "panel {panel_id} ceiling lost at bind {bind_generation}: "
                )?;
                match (last_controller_generation, tombstone_generation) {
                    (Some(last), None) => {
                        write!(f, "last controller {last} has no retired-ceiling tombstone")
                    }
                    (None, Some(tombstone)) => write!(
                        f,
                        "tombstone names {tombstone} but the record names no last controller"
                    ),
                    (Some(last), Some(tombstone)) => {
                        write!(f, "tombstone names {tombstone}, last controller is {last}")
                    }
                    (None, None) => f.write_str("no predecessor ceiling"),
                }
            }
            Self::ControllerConflict {
                panel_id,
                bound_generation,
                presented_generation,
            } => {
                write!(f, "panel {panel_id} controller conflict: ")?;
                match bound_generation {
                    Some(bound) => write!(f, "bound {bound}, presented {presented_generation}"),
                    None => write!(f, "no controller bound, presented {presented_generation}"),
                }
            }
            Self::ControllerReleased {
                panel_id,
                released_generation,
            } => write!(
                f,
                "panel {panel_id} controller released: generation {released_generation} is unbound"
            ),
            Self::RecoveryReasonRejected { panel_id, spelling } => write!(
                f,
                "panel {panel_id} placeholder reason {spelling:?} is outside the vocabulary"
            ),
            Self::PlaceholderSceneRejected { panel_id, detail } => write!(
                f,
                "panel {panel_id} placeholder scene failed the scene bounds: {detail}"
            ),
            Self::ResourceExhausted { reason } => write!(f, "resource exhausted: {reason}"),
        }
    }
}

impl std::error::Error for PanelError {}

/// Config for `PanelRegistry`; validated before creation via `ConfigPlan`-like checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRegistryConfig {
    pub max_panels_per_workspace: usize,
    pub max_panels_per_window: usize,
    pub max_topics_total: usize,
    pub max_subscriptions_per_panel: usize,
}

impl Default for PanelRegistryConfig {
    fn default() -> Self {
        Self {
            max_panels_per_workspace: DEFAULT_MAX_PANELS_PER_WORKSPACE,
            max_panels_per_window: DEFAULT_MAX_PANELS_PER_WINDOW,
            max_topics_total: MAX_TOPICS_TOTAL,
            max_subscriptions_per_panel: MAX_SUBSCRIPTIONS_PER_PANEL,
        }
    }
}

impl PanelRegistryConfig {
    pub fn validate(&self) -> Result<(), PanelError> {
        if !(1..=MAX_PANELS_PER_WORKSPACE).contains(&self.max_panels_per_workspace) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_workspace must be in [1, 32]".to_string(),
            });
        }
        if !(1..=MAX_PANELS_PER_WINDOW).contains(&self.max_panels_per_window) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_window must be in [1, 64]".to_string(),
            });
        }
        if self.max_topics_total == 0 || self.max_topics_total > MAX_TOPICS_TOTAL {
            return Err(PanelError::ResourceExhausted {
                reason: "max_topics_total must be in [1, 256]".to_string(),
            });
        }
        if self.max_subscriptions_per_panel == 0
            || self.max_subscriptions_per_panel > MAX_SUBSCRIPTIONS_PER_PANEL
        {
            return Err(PanelError::ResourceExhausted {
                reason: "max_subscriptions_per_panel must be in [1, 32]".to_string(),
            });
        }
        Ok(())
    }
}

/// The one controller binding a panel record holds (OQ-084 A.2, A.3, D.2
/// phase 2 step 2).
///
/// Phase 2 step 2 writes the binding as ONE pair, `(owner, controller
/// generation)`, so both halves live in one value: a record can hold neither an
/// owner without a controller generation nor a controller generation without an
/// owner, and it cannot carry an owner from one bind beside a controller
/// generation from another.
///
/// The pair, not the plugin id alone, is the owner part of the
/// retired-ceiling tombstone key `(owner, PanelId, N)`: two plugins can present
/// the same controller generation number, so a key without the owner could hand
/// a successor a wider ceiling than its own predecessor held (A.2, `N-13`).
/// Until a cross-owner adoption rule is decided (B.6) the owner generation and
/// the controller generation written together at phase 2 step 2 are the same
/// value, which is why one generation field carries both; the retired-ceiling
/// tombstone key is `(owner, PanelId)` and the owner half is this field's
/// `plugin_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelBinding {
    /// Plugin that created the panel content and holds this binding.
    plugin_id: bitty_plugin_host::PluginId,
    /// Controller generation this binding names, and (while B.6 is undecided)
    /// the owner generation the tombstone pairs with `plugin_id`.
    controller_generation: Generation,
}

/// The predecessor ceiling phase 0 captured at bind entry (OQ-084 D.2 phase 0).
///
/// One immutable snapshot, stamped with the panel id, the predecessor's owner
/// and controller generation, the captured ceiling set, and the bind attempt it
/// belongs to. [`PanelRegistry::capture_rebind_source`] builds it; nothing
/// inside a bind may re-read the live grant table after this exists, and the
/// derivation in [`PanelRegistry::rebind_panel`] reads this snapshot only
/// (D.1: "a bind is a pure function of that captured snapshot").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebindCapture {
    panel_id: PanelId,
    /// The bind attempt this snapshot is stamped with — the successor
    /// controller generation the bind presents.
    attempt: Generation,
    /// The predecessor controller generation, or `None` on a panel that has
    /// never been bound (the first-bind case, where `panel.create` is the
    /// authorization and the whole resolution is granted).
    predecessor_generation: Option<Generation>,
    /// The predecessor's owner: the live binding while the bind displaces a
    /// still-bound predecessor, otherwise the owner the panel record retained
    /// beside the ended generation (OQ-084 A.2). `None` is "there is no
    /// predecessor owner to compare" — a panel that never bound a controller,
    /// or one whose edge ended the allocation epoch — never "unknown".
    predecessor_owner: Option<bitty_plugin_host::PluginId>,
    /// `None` means **absent** — no predecessor, so there is no ceiling and the
    /// first bind commits `R`. `Some` is a real ceiling and may be empty: the
    /// predecessor existed and held nothing, so nothing of it survives.
    ceiling: Option<BTreeSet<String>>,
}

impl RebindCapture {
    /// The panel this snapshot was taken for.
    #[must_use]
    pub const fn panel_id(&self) -> PanelId {
        self.panel_id
    }

    /// The successor generation this bind attempt presents.
    #[must_use]
    pub const fn attempt(&self) -> Generation {
        self.attempt
    }

    /// The predecessor controller generation, or `None` for a first bind.
    #[must_use]
    pub const fn predecessor_generation(&self) -> Option<Generation> {
        self.predecessor_generation
    }

    /// The predecessor's owner, when the record still names it.
    #[must_use]
    pub fn predecessor_owner(&self) -> Option<&bitty_plugin_host::PluginId> {
        self.predecessor_owner.as_ref()
    }

    /// The captured ceiling: `None` when no predecessor exists, `Some(∅)` when
    /// the predecessor held nothing.
    #[must_use]
    pub fn ceiling(&self) -> Option<&BTreeSet<String>> {
        self.ceiling.as_ref()
    }

    /// Whether this bind has no predecessor at all (the first bind).
    #[must_use]
    pub const fn is_first_bind(&self) -> bool {
        self.predecessor_generation.is_none() && self.ceiling.is_none()
    }
}

/// What one committed rebind did (OQ-084 D.2 phase 2).
///
/// Returned only after all three commit steps ran, so a caller that holds this
/// value knows the panel is bound to `controller_generation` with `grants`
/// installed and the predecessor's tombstone retired. A refused bind returns a
/// `PanelError` and no value, and leaves the previous valid state intact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRebind {
    panel_id: PanelId,
    predecessor_generation: Option<Generation>,
    controller_generation: Generation,
    grants: BTreeSet<String>,
}

impl PanelRebind {
    /// The panel the bind moved.
    #[must_use]
    pub const fn panel_id(&self) -> PanelId {
        self.panel_id
    }

    /// The generation the bind displaced, or `None` on a first bind.
    #[must_use]
    pub const fn predecessor_generation(&self) -> Option<Generation> {
        self.predecessor_generation
    }

    /// The controller generation the panel is now bound to.
    #[must_use]
    pub const fn controller_generation(&self) -> Generation {
        self.controller_generation
    }

    /// The installed panel-scoped grant set, `(R ∩ P) ∪ C` (D.1).
    #[must_use]
    pub fn grants(&self) -> &BTreeSet<String> {
        &self.grants
    }
}

/// The unbound container's placeholder (OQ-084 Decision E, bitty#2476
/// residual 4).
///
/// The container is retained (the record, its state, its workspace, and its
/// attachment are untouched), the controller binding is absent (the unbind edge
/// cleared it, or none was ever written), and the content is absent. The
/// reason is the one Decision E rule 2 spelling the last transition carried, so
/// a panel has exactly one findable reason at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelPlaceholder {
    reason: PlaceholderReason,
    /// The controller generation that ended when the placeholder appeared
    /// (`None` for a container that never had a controller: the unloaded case).
    unbound_generation: Option<Generation>,
}

impl PanelPlaceholder {
    /// The reason this placeholder names.
    #[must_use]
    pub const fn reason(self) -> PlaceholderReason {
        self.reason
    }

    /// The controller generation the placeholder succeeded, if any.
    #[must_use]
    pub const fn unbound_generation(self) -> Option<Generation> {
        self.unbound_generation
    }

    /// The Core-owned surface for this reason (fixed table, no plugin text).
    #[must_use]
    pub const fn scene(self) -> PlaceholderScene {
        PlaceholderScene::for_reason(self.reason)
    }
}

/// What [`PanelRegistry::present_placeholder`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceholderPresentation {
    /// Which of Decision E's three outcomes the reason selected.
    pub outcome: RecoveryOutcome,
    /// The placeholder now on the record; `None` for the `Drop` outcome, which
    /// retires the container instead.
    pub placeholder: Option<PanelPlaceholder>,
    /// The FS-4-style record this transition emitted, when it emitted one.
    pub record: Option<RecoveryRecord>,
    /// Whether this call emitted the single `saturated` note for the binding.
    pub saturated: bool,
}

/// Internal panel record.
#[allow(dead_code)]
#[derive(Debug)]
struct PanelRecord {
    id: PanelId,
    /// Allocation epoch: minted once at `create_panel` and never rewritten by a
    /// bind (OQ-084 B.1/B.2). Distinct from `controller_generation`.
    generation: Generation,
    state: PanelState,
    panel_type: PanelType,
    workspace: Option<WorkspaceId>,
    view: Option<ViewId>,
    title: Option<String>,
    /// The controller binding written by D.2 phase 2 step 2, if any.
    ///
    /// One value, never two fields: phase 2 step 2 writes the pair
    /// `(owner, controller generation)` as one fact, so the record cannot hold
    /// half a binding nor an owner and a controller generation that came from
    /// different binds.
    ///
    /// `None` until a bind writes one. The generation edge rebinds a container
    /// that already carries a binding; nothing in the tree writes the *first*
    /// binding of a `bitty.panel.create` container, because the create seam
    /// carries no owner (OQ-084 follow-up (e)).
    binding: Option<PanelBinding>,
    /// Most recently ended controller generation, retained across the unbind
    /// edge (OQ-084 D.2 phase 0).
    ///
    /// `None` until the panel's first controller generation ends, or until an
    /// allocation-epoch edge ends a panel that had no controller at all. Phase 0
    /// uses it to distinguish "never bound" (the first bind commits `R`) from "a
    /// previous controller ended but its retired-ceiling tombstone is missing".
    /// An edge that ended the allocation epoch instead of a binding leaves the
    /// field as the last controller edge wrote it: the epoch is never a
    /// controller generation (B.1/B.2), so it never replaces this value
    /// (bitty#2476 residual 7(f)). This is the predecessor read, not the live
    /// binding: `controller_generation` names the binding in force now.
    last_controller_generation: Option<Generation>,
    /// The plugin that owned the most recently ended controller generation,
    /// retained beside [`Self::last_controller_generation`] (OQ-084 A.2,
    /// D.2 phase 0, `N-13`).
    ///
    /// The retired-ceiling tombstone is keyed by `(owner, PanelId)`, because
    /// two plugins can carry the same controller generation number, so a key of
    /// `(PanelId, generation)` alone could hand a successor a ceiling its own
    /// predecessor never held. Phase 0 can only check the tombstone against this
    /// field while no controller is bound: the binding is gone by then, so
    /// `record.owner()` no longer names the predecessor.
    ///
    /// `None` before the first edge, after panel disposal, and on a panel whose
    /// only ended edge is an allocation-epoch one — an edge that ended no
    /// binding has no owner to stamp. An edge that ended the allocation epoch
    /// after a controller edge stamps neither half of the pair, so it leaves
    /// this field and `last_controller_generation` exactly as the controller
    /// edge wrote them (bitty#2476 residual 7(f)). Written and retained with
    /// `last_controller_generation`, never on its own.
    last_controller_owner: Option<bitty_plugin_host::PluginId>,
    /// The placeholder presented while no controller is bound (OQ-084 E,
    /// bitty#2476 residual 4).
    ///
    /// `None` while a controller owns the content, before the first unbind, and
    /// after a rebind clears it. `Some` is the "container retained, controller
    /// unbound, content absent" state Decision C and E describe: a fact about
    /// the binding and the content, never a seventh `PanelState`.
    placeholder: Option<PanelPlaceholder>,
}

impl PanelRecord {
    /// The plugin that holds this panel's content, or `None` while no
    /// controller is bound (OQ-084 A.2, D.2 phase 2 step 2).
    ///
    /// Derived from the record's one binding, so an owner never exists without
    /// the controller generation that was written with it.
    fn owner(&self) -> Option<&bitty_plugin_host::PluginId> {
        self.binding.as_ref().map(|binding| &binding.plugin_id)
    }

    /// The controller generation the record's binding names (OQ-084 A.3, D.2
    /// phase 2 step 2), or `None` while no controller is bound.
    ///
    /// Deliberately distinct from `generation`: the allocation epoch names the
    /// container and never moves, while this names the generation that may
    /// touch the panel's content and is replaced by a rebind. `None` is a valid
    /// state (A.5, no bound controller); every panel-scoped grant lookup then
    /// falls back to the allocation epoch, which is the only key a caller can
    /// mint while nothing binds a controller.
    fn controller_generation(&self) -> Option<Generation> {
        self.binding
            .as_ref()
            .map(|binding| binding.controller_generation)
    }

    /// The generation that keys this panel's panel-scoped grant row (OQ-084
    /// D.2 phase 2 step 1).
    ///
    /// The bound controller generation when one exists, otherwise the
    /// allocation epoch. Before a controller can be bound the two agree, so
    /// this preserves the single-generation behaviour exactly.
    fn grant_key_generation(&self) -> Generation {
        self.controller_generation().unwrap_or(self.generation)
    }

    /// Ends the binding `generation` held, if it held one (OQ-084 A.2, C.2:
    /// "the panel becomes unbound").
    ///
    /// The unbind edge drops the binding together with the generation's grant
    /// row, so the record cannot keep claiming a controller it has released:
    /// what survives is [`Self::last_controller_generation`], which is what
    /// makes [`PanelError::ControllerReleased`] decidable at use. A release of
    /// a generation the binding does not name leaves the binding untouched.
    fn release_controller_binding(&mut self, generation: Generation) {
        if self
            .controller_generation()
            .is_some_and(|bound| bound == generation)
        {
            self.binding = None;
        }
    }
}

/// Validated event topic: `owner.name:topic` with `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z][a-z0-9_.-]*$`, `<=64` bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventTopic(String);

impl EventTopic {
    pub fn parse(raw: &str) -> Result<Self, PanelError> {
        if raw.is_empty() {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if raw.len() > 64 {
            return Err(PanelError::ResourceExhausted {
                reason: "topic exceeds 64 bytes".to_string(),
            });
        }
        let (owner_part, topic) = raw
            .split_once(':')
            .ok_or_else(|| PanelError::UnknownTopic {
                topic: raw.to_string(),
            })?;
        if topic.is_empty() || topic.len() > 32 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.'
        }) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        let segs: Vec<&str> = owner_part.split('.').collect();
        if segs.len() != 2 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        for seg in &segs {
            if seg.is_empty() || seg.len() > 16 {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
        }
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        // Forbid bitty.* impersonation for non-Core topics? Core topics are bitty.panel:*
        // Allow bitty.panel:* only from runtime; other bitty.* rejected
        if owner_part == "bitty" && !raw.starts_with("bitty.panel:") {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this topic is coalescable (latest-wins).
    #[must_use]
    pub fn is_coalescable(&self) -> bool {
        let s = self.0.as_str();
        s.contains("focus") || s.contains("cwd") || s.contains("title") || s.contains("file.open")
    }
}

/// Bounded payload text `<= 8KiB`, truncated or rejected at boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoundedPayload(String);

impl BoundedPayload {
    pub fn try_new(s: impl Into<String>) -> Result<Self, PanelError> {
        let raw = s.into();
        if raw.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: raw.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        Ok(Self(raw))
    }

    pub fn new_truncated(s: &str) -> Self {
        if s.len() <= BUS_EVENT_MAX_BYTES {
            return Self(s.to_owned());
        }
        // Truncate at char boundary
        let mut end = BUS_EVENT_MAX_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        Self(s[..end].to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One bus event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusEvent {
    pub topic: EventTopic,
    pub payload: BoundedPayload,
    pub generation: Generation,
    /// Monotonic publish sequence (CTX-0534): aggregate eviction must victimize
    /// the genuinely oldest queued event, not the first queue in map order.
    pub sequence: u64,
}

/// Drop policy for bus queues; v1 default is `DropOldest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusDropPolicy {
    DropOldest,
    DropNewest,
}

/// Per-subscription bounded FIFO queue (64) with DropOldest/Newest.
#[derive(Debug)]
struct BusQueue {
    inner: VecDeque<BusEvent>,
    capacity: usize,
    dropped: u64,
    drop_policy: BusDropPolicy,
}

impl BusQueue {
    fn new(capacity: usize, drop_policy: BusDropPolicy) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
            drop_policy,
        }
    }

    fn push(&mut self, event: BusEvent) -> bool {
        // Coalescing: if topic is coalescable and queue holds undelivered copy, replace latest
        if event.topic.is_coalescable() {
            if let Some(pos) = self.inner.iter().position(|e| e.topic == event.topic) {
                // Remove existing coalescable entry and push latest to back (latest-wins)
                self.inner.remove(pos);
                self.inner.push_back(event);
                return true;
            }
        }
        if self.inner.len() >= self.capacity {
            match self.drop_policy {
                BusDropPolicy::DropOldest => {
                    self.inner.pop_front();
                    self.dropped = self.dropped.wrapping_add(1);
                    self.inner.push_back(event);
                    true
                }
                BusDropPolicy::DropNewest => {
                    self.dropped = self.dropped.wrapping_add(1);
                    false
                }
            }
        } else {
            self.inner.push_back(event);
            true
        }
    }

    fn drain_batch(&mut self, max_events: usize, max_bytes: usize) -> Vec<BusEvent> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        while let Some(front) = self.inner.front() {
            if out.len() >= max_events {
                break;
            }
            let payload_len = front.payload.len();
            if bytes + payload_len > max_bytes && !out.is_empty() {
                break;
            }
            // If single event exceeds max_bytes, drain it only if it's the first? But spec says strict: never exceed max_bytes even for first? However for panel bus we follow same as plugin-host drain_batch strict: never exceed max_bytes even for first event; remainder stays queued.
            // So if first event alone exceeds max_bytes, we do not drain it.
            if payload_len > max_bytes {
                break;
            }
            if bytes + payload_len > max_bytes {
                break;
            }
            let Some(ev) = self.inner.pop_front() else {
                break;
            };
            bytes += payload_len;
            out.push(ev);
        }
        out
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn bytes(&self) -> usize {
        self.inner.iter().map(|e| e.payload.len()).sum()
    }
}

/// Panel EventBus with three-level budgets mirroring plugin-host but for panel traffic.
#[derive(Debug)]
pub struct PanelEventBus {
    queues: HashMap<(PanelId, String), BusQueue>,
    total_dropped: u64,
    drop_policy: BusDropPolicy,
    topics: HashSet<String>,
    /// Monotonic publish sequence handed to each queued event (CTX-0534).
    next_publish_seq: u64,
}

impl PanelEventBus {
    #[must_use]
    pub fn new(drop_policy: BusDropPolicy) -> Self {
        Self {
            queues: HashMap::new(),
            total_dropped: 0,
            drop_policy,
            topics: HashSet::new(),
            next_publish_seq: 0,
        }
    }

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        let topic = EventTopic::parse(raw)?;
        if self.topics.len() >= MAX_TOPICS_TOTAL && !self.topics.contains(topic.as_str()) {
            return Err(PanelError::TooManyTopics {
                max: MAX_TOPICS_TOTAL,
                current: self.topics.len(),
            });
        }
        self.topics.insert(topic.as_str().to_string());
        Ok(topic)
    }

    pub fn subscribe(&mut self, panel_id: PanelId, topic: &EventTopic) -> Result<(), PanelError> {
        if !self.topics.contains(topic.as_str()) {
            return Err(PanelError::UnknownTopic {
                topic: topic.as_str().to_string(),
            });
        }
        // Count subscriptions per panel
        let count = self
            .queues
            .keys()
            .filter(|(pid, _)| *pid == panel_id)
            .count();
        if count >= MAX_SUBSCRIPTIONS_PER_PANEL {
            return Err(PanelError::TooManySubscriptions {
                max: MAX_SUBSCRIPTIONS_PER_PANEL,
                current: count,
            });
        }
        let key = (panel_id, topic.as_str().to_string());
        self.queues
            .entry(key)
            .or_insert_with(|| BusQueue::new(BUS_PER_SUBSCRIPTION_LIMIT, self.drop_policy));
        Ok(())
    }

    /// Publish a payload to all subscribers of `topic`. Enforces per-panel
    /// 1024/256KiB and global 8192/2MiB as admission invariants: under
    /// `DropOldest` eviction loops (oldest event first, across every queue)
    /// until the arrival fits or the queues are empty; under `DropNewest` the
    /// arrival is refused when it would exceed. `publish` never returns with
    /// any aggregate over its cap (CTX-0534).
    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        if payload.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: payload.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        // Gather subscribers for topic
        let subscribers: Vec<(PanelId, String)> = self
            .queues
            .keys()
            .filter(|(_, t)| t == topic.as_str())
            .cloned()
            .collect();
        if subscribers.is_empty() {
            return Ok(());
        }
        for (panel_id, topic_str) in subscribers {
            let key = (panel_id, topic_str);
            if !self.queues.contains_key(&key) {
                continue;
            }
            let payload_len = payload.len();
            match self.drop_policy {
                BusDropPolicy::DropNewest => {
                    if self.admission_exceeds(panel_id, payload_len) {
                        self.record_aggregate_drop(&key);
                        continue;
                    }
                }
                BusDropPolicy::DropOldest => {
                    // Loop until the arrival is admissible or nothing remains
                    // to evict. Global first so the host-wide ceiling wins,
                    // then the panel aggregate. The attempt bound is the total
                    // capacity across both scopes; the progress guard covers
                    // intrinsic over-budget payloads (which cannot fit even
                    // with empty queues).
                    let mut attempts = 0usize;
                    while self.admission_exceeds(panel_id, payload_len) {
                        let before = self.global_totals();
                        if self.global_exceeds(payload_len) {
                            self.evict_oldest_globally();
                        } else if self.panel_exceeds(panel_id, payload_len) {
                            self.evict_oldest_for_panel(panel_id);
                        } else {
                            break;
                        }
                        if self.global_totals() == before {
                            break;
                        }
                        attempts += 1;
                        if attempts > BUS_GLOBAL_LIMIT + BUS_PER_PANEL_LIMIT {
                            break;
                        }
                    }
                    if self.admission_exceeds(panel_id, payload_len) {
                        // Queues empty but the payload still cannot fit: apply
                        // the documented drop-newest fallback and count it on
                        // the target queue.
                        self.record_aggregate_drop(&key);
                        continue;
                    }
                }
            }
            let event = BusEvent {
                topic: topic.clone(),
                payload: payload.clone(),
                generation: Generation::INITIAL, // placeholder; real generation tracked per panel elsewhere
                sequence: self.next_publish_seq,
            };
            self.next_publish_seq = self.next_publish_seq.wrapping_add(1);
            if let Some(queue) = self.queues.get_mut(&key) {
                let before_dropped = queue.dropped;
                let _pushed = queue.push(event);
                // A full per-subscription queue applies the same policy and
                // counts its own drop; fold that into the global counter.
                let newly_dropped = queue.dropped.wrapping_sub(before_dropped);
                self.total_dropped = self.total_dropped.wrapping_add(newly_dropped);
            }
        }
        Ok(())
    }

    /// Whether admitting `payload_len` more bytes for `panel_id` would breach
    /// any per-panel or global event/byte cap.
    fn admission_exceeds(&self, panel_id: PanelId, payload_len: usize) -> bool {
        self.panel_exceeds(panel_id, payload_len) || self.global_exceeds(payload_len)
    }

    /// Whether the per-panel event/byte aggregate would breach its cap.
    fn panel_exceeds(&self, panel_id: PanelId, payload_len: usize) -> bool {
        let (panel_events, panel_bytes) = self.panel_totals(panel_id);
        panel_events + 1 > BUS_PER_PANEL_LIMIT
            || panel_bytes + payload_len > BUS_PER_PANEL_BYTES_LIMIT
    }

    /// Whether the global event/byte aggregate would breach its cap.
    fn global_exceeds(&self, payload_len: usize) -> bool {
        let (global_events, global_bytes) = self.global_totals();
        global_events + 1 > BUS_GLOBAL_LIMIT || global_bytes + payload_len > BUS_GLOBAL_BYTES_LIMIT
    }

    /// Live queued (events, bytes) aggregate for one panel, computed from the
    /// queues so it can never drift from the stored state.
    fn panel_totals(&self, panel_id: PanelId) -> (usize, usize) {
        self.queues
            .iter()
            .filter(|((pid, _), _)| *pid == panel_id)
            .fold((0usize, 0usize), |(events, bytes), (_, q)| {
                (events + q.len(), bytes + q.bytes())
            })
    }

    /// Live queued (events, bytes) aggregate across every queue.
    fn global_totals(&self) -> (usize, usize) {
        self.queues
            .values()
            .fold((0usize, 0usize), |(events, bytes), q| {
                (events + q.len(), bytes + q.bytes())
            })
    }

    /// Count one refused/dropped arrival against `key` and the total.
    fn record_aggregate_drop(&mut self, key: &(PanelId, String)) {
        if let Some(q) = self.queues.get_mut(key) {
            q.dropped = q.dropped.wrapping_add(1);
            self.total_dropped = self.total_dropped.wrapping_add(1);
        }
    }

    /// Evict the genuinely oldest queued event across `panel_id`'s queues.
    /// Returns `false` when the panel has nothing queued.
    fn evict_oldest_for_panel(&mut self, panel_id: PanelId) -> bool {
        let victim = self
            .queues
            .iter()
            .filter(|((pid, _), _)| *pid == panel_id)
            .filter_map(|(key, q)| q.inner.front().map(|ev| (key.clone(), ev.sequence)))
            .min_by_key(|(_, seq)| *seq)
            .map(|(key, _)| key);
        let Some(key) = victim else {
            return false;
        };
        self.pop_oldest(&key)
    }

    /// Evict the genuinely oldest queued event across every queue.
    /// Returns `false` when nothing is queued anywhere.
    fn evict_oldest_globally(&mut self) -> bool {
        let victim = self
            .queues
            .iter()
            .filter_map(|(key, q)| q.inner.front().map(|ev| (key.clone(), ev.sequence)))
            .min_by_key(|(_, seq)| *seq)
            .map(|(key, _)| key);
        let Some(key) = victim else {
            return false;
        };
        self.pop_oldest(&key)
    }

    /// Pop the front of `key`, counting the drop exactly once.
    fn pop_oldest(&mut self, key: &(PanelId, String)) -> bool {
        if let Some(q) = self.queues.get_mut(key) {
            if q.inner.pop_front().is_some() {
                q.dropped = q.dropped.wrapping_add(1);
                self.total_dropped = self.total_dropped.wrapping_add(1);
                return true;
            }
        }
        false
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        let key = (panel_id, topic.to_string());
        if let Some(q) = self.queues.get_mut(&key) {
            q.drain_batch(max_events, max_bytes)
        } else {
            Vec::new()
        }
    }

    #[must_use]
    pub fn total_queued_events(&self) -> usize {
        self.queues.values().map(BusQueue::len).sum()
    }

    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.queues.values().map(BusQueue::bytes).sum()
    }

    #[must_use]
    pub fn queued_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.panel_totals(panel_id).0
    }

    #[must_use]
    pub fn queued_bytes_for_panel(&self, panel_id: PanelId) -> usize {
        self.panel_totals(panel_id).1
    }

    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.total_dropped
    }

    /// Removes every queue owned by `panel_id` and reports how many queued
    /// events that released.
    pub fn clear_panel_counted(&mut self, panel_id: PanelId) -> usize {
        let mut released = 0;
        self.queues.retain(|(pid, _), queue| {
            if *pid == panel_id {
                released += queue.len();
                false
            } else {
                true
            }
        });
        released
    }

    pub fn clear_panel(&mut self, panel_id: PanelId) {
        // `total_dropped` is maintained at drop time (pop/record/push), so
        // removing the queues here must not re-add their per-queue counters.
        self.clear_panel_counted(panel_id);
    }

    pub fn topics_len(&self) -> usize {
        self.topics.len()
    }
}

/// Absolute upper bound on retired generation ceilings in one registry
/// (OQ-084 B-4).
///
/// This is a structural ceiling, not an eviction threshold. A panel holds at
/// most one tombstone, only a live panel record can own a tombstone, and a
/// registry admits at most [`MAX_PANELS_PER_WINDOW`] live records. A registry
/// therefore caps its table at its configured `max_panels_per_window` (always
/// `<= MAX_PANELS_PER_WINDOW`) and refuses a write that would evict a ceiling
/// instead of dropping one that still protects a panel.
pub const MAX_RETAINED_GRANT_CEILINGS: usize = MAX_PANELS_PER_WINDOW;

/// The retired-ceiling tombstone for a departed controller generation
/// (OQ-084 B-4, D.2 phase 0).
///
/// The edge release removes the live `capabilities[(PanelId, generation)]` row,
/// so a later bind that read the live table would see an empty `P` and either
/// grant nothing or treat "no row" as "no ceiling". This record is what phase 0
/// of a bind reads instead: the ceiling the predecessor actually held, stamped
/// with the generation that ended.
///
/// The table holds at most one entry per `(owner, panel)`. A new tombstone for
/// the same panel replaces the previous one; the entry is dropped when a
/// successor successfully binds the panel or when the panel is disposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedGrantCeiling {
    /// The plugin that owned the ended controller generation (OQ-084 A.2).
    ///
    /// Mirrors the owner half of the map key so a reader holding the tombstone
    /// can compare it with the record's `last_controller_owner` without
    /// reconstructing the key. `None` for an entry written by an edge that
    /// ended the allocation epoch rather than a bound controller.
    pub owner: Option<bitty_plugin_host::PluginId>,
    /// The ended controller generation whose edge released the live row.
    pub ended_generation: Generation,
    /// The grant rows that generation held at the edge (possibly empty: a
    /// predecessor that held nothing is a ceiling of `∅`, not an absent one).
    pub grants: BTreeSet<String>,
}

/// Per-family release record written by the panel's unbind edge (OQ-084 A.2).
///
/// The edge releases four families for one `(PanelId, generation)`: the
/// panel-scoped grant row, the panel's queued bus events, the commands the
/// generation registered, and the overlays attributed to it. This record
/// names the ended generation and counts what each family released, so
/// "released nothing" (`Some(0)` everywhere) stays distinguishable from
/// "the release never ran" (no record).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationReleaseRecord {
    /// The ended controller generation this record describes.
    pub generation: Generation,
    /// Panel-scoped capabilities released with the grant row.
    pub grants: usize,
    /// Queued bus events drained.
    pub events: usize,
    /// Panel commands unregistered.
    pub commands: usize,
    /// Overlays attributed to the generation and dismissed.
    pub overlays: usize,
}

impl GenerationReleaseRecord {
    /// Total items released across every family.
    #[must_use]
    pub fn total(&self) -> usize {
        self.grants + self.events + self.commands + self.overlays
    }

    /// Whether at least one family released something.
    #[must_use]
    pub fn released_any(&self) -> bool {
        self.total() > 0
    }
}

/// Why a panel container was retired (OQ-084 C.9).
///
/// The vocabulary is closed and matches FS-4's structured-record spelling:
/// one lowercase stable string per reason, so a diagnostic consumer never
/// parses prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelDisposalReason {
    /// The panel's workspace was closed. C.9 names this reason `quit`: the
    /// user quit the workspace, not the plugin generation.
    Quit,
}

impl PanelDisposalReason {
    /// Stable identifier for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quit => "quit",
        }
    }
}

impl std::fmt::Display for PanelDisposalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happened to the panel container (OQ-084 C.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelDisposalAction {
    /// The container was retired and its families released.
    Disposed,
}

impl PanelDisposalAction {
    /// Stable identifier for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disposed => "disposed",
        }
    }
}

impl std::fmt::Display for PanelDisposalAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FS-4-style record for one retired panel container (OQ-084 C.9).
///
/// C.9 requires "one FS-4-style record per panel with owner, controller
/// generation, reason `quit`, and action `disposed`". [`GenerationReleaseRecord`]
/// is not that carrier: it counts the four families an *unbind edge* releases
/// for a generation and survives on the live panel, while this record describes
/// the *container's* retirement, is written exactly once per disposed panel,
/// names the owner and the workspace the container belonged to, and must exist
/// after the panel record is gone.
///
/// `owner` is `None` exactly when the record held no binding at the edge — a
/// container whose generation never bound a controller, or one whose binding an
/// earlier edge already released — and `controller_generation` is then the
/// allocation epoch (OQ-084 A.5). The binder that writes the pair is
/// `PanelRegistry::rebind_panel` (D.2 phase 2 step 2), and the generation edge
/// reaches it through the panel-generation port whenever the app installs one,
/// so a container the edge rebound is disposed naming its successor's owner and
/// controller generation. The record reports what the binding actually named at
/// the edge rather than inventing an owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelDisposalRecord {
    /// The retired container.
    pub panel: PanelId,
    /// The workspace the container was attributed to (C.9's disposal path).
    pub workspace: WorkspaceId,
    /// Plugin that held the panel's content binding, if one was bound.
    pub owner: Option<bitty_plugin_host::PluginId>,
    /// The controller generation the disposal released: the bound controller
    /// generation when one exists, otherwise the allocation epoch.
    pub controller_generation: Generation,
    /// The panel state at the edge (`Focused`/`Mounted`/`Suspended`/`Created`),
    /// which names the rule-8 release arm that ran.
    pub state: PanelState,
    /// Why the container was retired.
    pub reason: PanelDisposalReason,
    /// What happened to it.
    pub action: PanelDisposalAction,
}

/// Outcome of one panel's disposal inside a workspace sweep (OQ-084 C.9).
///
/// Disposal is all-or-nothing per panel: `Ok` means every rule-8 family for
/// that panel was released, `Err` means the panel kept its previous valid
/// state and nothing of it was released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelDisposalOutcome {
    /// The panel the outcome belongs to.
    pub panel: PanelId,
    /// The per-panel result.
    pub result: Result<(), PanelError>,
}

/// Per-panel result of disposing every panel of one workspace (OQ-084 C.9).
///
/// The sweep is ordered (focused panel first, then workspace MRU) and
/// fail-fast: the first refused panel stops it. `remaining` names every panel
/// the sweep did not attempt, so a partial close is never mistaken for a
/// complete one — a caller that only inspected `outcomes` would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceDisposeReport {
    /// The workspace whose panels were swept.
    pub workspace: WorkspaceId,
    /// One entry per attempted panel, in disposal order.
    pub outcomes: Vec<PanelDisposalOutcome>,
    /// Panels not attempted because an earlier panel was refused.
    pub remaining: Vec<PanelId>,
    /// One FS-4-style record per disposed panel, in disposal order.
    pub records: Vec<PanelDisposalRecord>,
}

impl WorkspaceDisposeReport {
    /// Whether every panel attributed to the workspace was disposed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.refused().is_none() && self.remaining.is_empty()
    }

    /// The refused panel and the typed refusal that stopped the sweep.
    #[must_use]
    pub fn refused(&self) -> Option<(PanelId, &PanelError)> {
        self.outcomes
            .iter()
            .find_map(|outcome| match &outcome.result {
                Ok(()) => None,
                Err(error) => Some((outcome.panel, error)),
            })
    }

    /// Panels the sweep disposed, in disposal order.
    #[must_use]
    pub fn disposed(&self) -> Vec<PanelId> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.result.is_ok())
            .map(|outcome| outcome.panel)
            .collect()
    }

    /// How many panels the sweep disposed.
    #[must_use]
    pub fn disposed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.result.is_ok())
            .count()
    }
}

/// The facts a workspace sweep stamps on a panel's disposal record before the
/// panel record leaves the registry (OQ-084 C.9).
///
/// Read through [`PanelRegistry::panel_disposal_facts`]: the sweep needs the
/// panel's allocation epoch (to name it in the disposal), the binding
/// generation, the owner, and the state at the edge, and every one of them is
/// unavailable once the container is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelDisposalFacts {
    /// The container's allocation epoch (OQ-084 B.1), the fence
    /// [`PanelRegistry::dispose_panel`] validates.
    pub allocation_generation: Generation,
    /// The generation the disposal releases: the bound controller generation
    /// when one is bound, otherwise the allocation epoch (D.2 phase 2 step 1).
    pub controller_generation: Generation,
    /// Plugin that held the content binding, if one was bound (A.2).
    pub owner: Option<bitty_plugin_host::PluginId>,
    /// The panel state at the edge.
    pub state: PanelState,
}

/// Generic Panel Runtime per window/process. One registry per window,
/// single-process winit, holds no PTY fd, GPU object, or OS window handle
/// (those remain with `bitty-pty`, `bitty-render`, `bitty-platform`).
pub struct PanelRegistry {
    registry_generation: Generation,
    next_panel_raw: u64,
    config: PanelRegistryConfig,
    panels: HashMap<u64, PanelRecord>,
    panel_to_view: HashMap<PanelId, ViewId>,
    view_to_panel: HashMap<ViewId, PanelId>,
    workspace_panels: HashMap<WorkspaceId, Vec<PanelId>>,
    focus_per_workspace: HashMap<WorkspaceId, bitty_ui::panel::PanelFocus>,
    active_workspace: Option<WorkspaceId>,
    command_registry: UiCommandRegistry,
    overlay_manager: UiOverlayManager,
    event_bus: PanelEventBus,
    capabilities: HashMap<(PanelId, Generation), BTreeSet<String>>,
    /// Retired-ceiling tombstones (OQ-084 A.2 `B-4`, D.2 phase 0), keyed
    /// `(owner, PanelId)` — the owner and panel of the binding the edge ended,
    /// because two plugins can carry the same controller generation number and
    /// a key without the owner could hand a successor a ceiling its own
    /// predecessor never held (`N-13`). At most one entry per `(owner, panel)`;
    /// written by [`PanelRegistry::release_generation_scope`] before the live
    /// row is removed, read by [`PanelRegistry::derive_rebind_grants`] at bind
    /// entry, and dropped by a successful successor bind or panel disposal.
    /// Never evicted while the panel record is alive.
    ///
    /// The owner half is `Option` because the epoch-keyed edge of a panel that
    /// never bound a controller has no owner to stamp: that edge writes the
    /// entry under `None`, which is also what the record's
    /// `last_controller_owner` carries, so the two still compare.
    retained_ceilings:
        BTreeMap<(Option<bitty_plugin_host::PluginId>, PanelId), RetainedGrantCeiling>,
    /// Per-family release records (OQ-084 A.2): at most one per live panel,
    /// written by [`PanelRegistry::release_generation_scope`], replaced by the
    /// panel's next edge, and dropped with the container. Created even when a
    /// family released nothing.
    release_records: BTreeMap<PanelId, GenerationReleaseRecord>,
    /// Overlay attribution (OQ-084 A.2/D.4): which `(PanelId, generation)`
    /// asked for each live overlay, so the unbind edge can dismiss exactly
    /// that generation's surfaces. Bounded by the `4+1` overlay envelope.
    overlay_attribution: BTreeMap<u64, (PanelId, Generation)>,
    /// The capped recovery counter, keyed `(PanelId, controller generation,
    /// reason)` and capped at eight per key (OQ-084 E.1, bitty#2476 residual
    /// 4). Bounded per panel and cleared when a container is retired.
    recovery: RecoveryBudget,
    errors: HashMap<String, u64>,
    disposed: bool,
    total_created: u64,
}

impl std::fmt::Debug for PanelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelRegistry")
            .field("generation", &self.registry_generation)
            .field("panels_active", &self.panels.len())
            .field("total_created", &self.total_created)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl PanelRegistry {
    /// Creates a panel registry after `PanelRegistryConfig` validation.
    ///
    /// # Errors
    /// `ResourceExhausted` for bad bounds, `GenerationExhausted` if reserved.
    pub fn new(config: PanelRegistryConfig) -> Result<Self, PanelError> {
        config.validate()?;
        if Generation::INITIAL.is_exhausted() {
            return Err(PanelError::GenerationExhausted {
                current: Generation::INITIAL,
            });
        }
        Ok(Self {
            registry_generation: Generation::INITIAL,
            next_panel_raw: 1,
            config,
            panels: HashMap::new(),
            panel_to_view: HashMap::new(),
            view_to_panel: HashMap::new(),
            workspace_panels: HashMap::new(),
            focus_per_workspace: HashMap::new(),
            active_workspace: None,
            command_registry: UiCommandRegistry::new(),
            overlay_manager: UiOverlayManager::new(),
            event_bus: PanelEventBus::new(BusDropPolicy::DropOldest),
            capabilities: HashMap::new(),
            retained_ceilings: BTreeMap::new(),
            release_records: BTreeMap::new(),
            overlay_attribution: BTreeMap::new(),
            recovery: RecoveryBudget::new(),
            errors: HashMap::new(),
            disposed: false,
            total_created: 0,
        })
    }

    fn ensure_not_disposed(&self) -> Result<(), PanelError> {
        if self.disposed {
            return Err(PanelError::RegistryDisposed {
                generation: self.registry_generation,
            });
        }
        Ok(())
    }

    fn bump_error(&mut self, variant: &str) {
        *self.errors.entry(variant.to_owned()).or_insert(0) += 1;
    }

    #[must_use]
    pub fn generation(&self) -> Generation {
        self.registry_generation
    }

    #[must_use]
    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    #[must_use]
    pub fn config(&self) -> &PanelRegistryConfig {
        &self.config
    }

    /// Resolves the generation that keys this panel's grant row for a caller
    /// presenting `presented` (OQ-084 D.2 phase 2 step 1, A.3).
    ///
    /// `Some` only when `presented` names the panel's current binding: the
    /// controller generation when one is bound, otherwise the allocation
    /// epoch, which is the only key a caller can mint until a bind writes one.
    /// A stale generation therefore resolves to nothing even when the table
    /// still holds a row under it, so no caller ever reads another
    /// generation's grants.
    fn resolved_grant_generation(
        &self,
        panel_id: PanelId,
        presented: Generation,
    ) -> Option<Generation> {
        let record = self.panels.get(&panel_id.0)?;
        let key = record.grant_key_generation();
        (presented == key).then_some(key)
    }

    /// Revalidates a panel verb's presented generation against the record's
    /// controller binding at use, immediately before the verb touches panel
    /// state, and returns the record's state (OQ-084 D.2, "Revalidate at use,
    /// not only at entry"; C.5 "a mount that started against `N` must fail
    /// closed").
    ///
    /// The accepted generation is the record's binding key — the controller
    /// generation once one is bound, otherwise the allocation epoch, the same
    /// key [`Self::resolved_grant_generation`] accepts (A.3). Everything else
    /// is refused by the typed class the record defines rather than borrowed
    /// from `StaleHandle`:
    ///
    /// - [`PanelError::ControllerReleased`] when the presented generation's own
    ///   binding was released: it is the record's retained
    ///   `last_controller_generation`, and the allocation epoch is never a
    ///   controller generation (B.1/B.2), so presenting it against a foreign
    ///   binding is a conflict instead.
    /// - [`PanelError::ControllerConflict`] when the record binds a controller
    ///   or has ended one and the presented generation is neither.
    /// - `StaleHandle` — unchanged — when the record binds nothing and has
    ///   ended nothing: its binding key is then the allocation epoch, a foreign
    ///   generation has never been accepted here, and this is the refusal that
    ///   has always answered it.
    ///
    /// Fail-open by construction while nothing binds: a panel that never bound a
    /// controller has `binding == None` and `last_controller_generation ==
    /// None`, so every caller presents the allocation epoch, hits the first arm,
    /// and behaves exactly as it did before the fence existed. Both new refusals
    /// become reachable only on a panel that carries a binding: the generation
    /// edge rebinds such a container through the panel-generation port when the
    /// app installs one, and nothing writes a container's first binding yet
    /// (`bitty.panel.create` carries no `PluginId` and no generation, OQ-084
    /// follow-up (e)), so tests mint that one.
    ///
    /// Applied at the mount surface, which is the one C.5 names; the remaining
    /// panel verbs are wired by residual 2's phase 1/phase 2 work.
    fn panel_state_at_controller_fence(
        &mut self,
        panel_id: PanelId,
        presented: Generation,
    ) -> Result<PanelState, PanelError> {
        self.ensure_not_disposed()?;
        let (state, key, bound, last, epoch) = match self.panels.get(&panel_id.0) {
            Some(record) => (
                record.state,
                record.grant_key_generation(),
                record.controller_generation(),
                record.last_controller_generation,
                record.generation,
            ),
            None => {
                return Err(PanelError::NotFound {
                    kind: "panel",
                    id_raw: panel_id.0,
                });
            }
        };
        if state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            });
        }
        if presented == key {
            return Ok(state);
        }
        if last == Some(presented) && presented != epoch {
            self.bump_error("ControllerReleased");
            return Err(PanelError::ControllerReleased {
                panel_id,
                released_generation: presented,
            });
        }
        if bound.is_none() && last.is_none() {
            // Not bumped: `get_panel` has always answered this call without
            // touching the audit counter, and this arm keeps that byte for byte.
            return Err(PanelError::StaleHandle {
                expected_generation: epoch,
                found_generation: presented,
                id_raw: panel_id.0,
            });
        }
        self.bump_error("ControllerConflict");
        Err(PanelError::ControllerConflict {
            panel_id,
            bound_generation: bound,
            presented_generation: presented,
        })
    }

    /// Validates `(id, generation)` before returning a reference.
    fn get_panel(&self, id: PanelId, generation: Generation) -> Result<&PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.generation != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: rec.generation,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    fn get_panel_mut(
        &mut self,
        id: PanelId,
        generation: Generation,
    ) -> Result<&mut PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let current_gen = {
            let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            })?;
            rec.generation
        };
        if current_gen != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: current_gen,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        let rec = self.panels.get_mut(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    /// Creates a panel of `panel_type`. Validates closed type set and
    /// `max_panels_per_workspace` / `max_panels_per_window` before allocation.
    ///
    /// # Errors
    /// `TooManyPanels`, `UnknownPanelType`, `GenerationExhausted`, `RegistryDisposed`.
    pub fn create_panel(
        &mut self,
        panel_type: PanelType,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            self.bump_error("GenerationExhausted");
            return Err(PanelError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        if self.panels.len() >= self.config.max_panels_per_window {
            self.bump_error("TooManyPanels");
            return Err(PanelError::TooManyPanels {
                max: self.config.max_panels_per_window,
                current: self.panels.len(),
            });
        }
        if let Some(ws) = workspace {
            let count = self.workspace_panels.get(&ws).map_or(0, |v| v.len());
            if count >= self.config.max_panels_per_workspace {
                self.bump_error("TooManyPanels");
                return Err(PanelError::TooManyPanels {
                    max: self.config.max_panels_per_workspace,
                    current: count,
                });
            }
        }
        let next_gen =
            self.registry_generation
                .next()
                .map_err(|_| PanelError::GenerationExhausted {
                    current: self.registry_generation,
                })?;
        self.registry_generation = next_gen;
        let pid = PanelId::new(self.next_panel_raw);
        self.next_panel_raw = self.next_panel_raw.wrapping_add(1).max(1);
        let gen_val = self.registry_generation;
        let rec = PanelRecord {
            id: pid,
            generation: gen_val,
            state: UiPanelState::Created,
            panel_type,
            workspace,
            view: None,
            title: None,
            // No binding: `create_panel` mints a container, and D.2 phase 2
            // step 2 is what would bind an owner together with a controller
            // generation.
            binding: None,
            last_controller_generation: None,
            last_controller_owner: None,
            placeholder: None,
        };
        self.panels.insert(pid.0, rec);
        if let Some(ws) = workspace {
            self.workspace_panels.entry(ws).or_default().push(pid);
            self.focus_per_workspace.entry(ws).or_default();
            if self.active_workspace.is_none() {
                self.active_workspace = Some(ws);
            }
        }
        self.total_created += 1;
        Ok(PanelHandle {
            id: pid,
            generation: gen_val,
        })
    }

    /// Creates a panel from a string type name; validates closed set.
    pub fn create_panel_by_type_str(
        &mut self,
        type_str: &str,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        let pt = UiPanelType::parse(type_str).ok_or_else(|| PanelError::UnknownPanelType {
            value: type_str.to_string(),
        })?;
        self.create_panel(pt, workspace)
    }

    /// Returns panel state for a handle.
    pub fn panel_state(
        &self,
        id: PanelId,
        generation: Generation,
    ) -> Result<PanelState, PanelError> {
        Ok(self.get_panel(id, generation)?.state)
    }

    /// Returns the view currently attached to a panel, if any. `Mounted`/
    /// `Focused` records always carry `Some`; `Created`/`Suspended` records
    /// are viewless (CTX-0535).
    pub fn panel_view(
        &self,
        id: PanelId,
        generation: Generation,
    ) -> Result<Option<ViewId>, PanelError> {
        Ok(self.get_panel(id, generation)?.view)
    }

    /// Mounts `panel` to an empty `ViewId`. Validates handles, single-owner
    /// mapping, and transitions `Created -> Mounted`.
    ///
    /// Revalidates the caller's controller generation at use, immediately
    /// before the commit touches panel state (OQ-084 D.2, "Revalidate at use,
    /// not only at entry"; C.5): a mount issued by a controller generation the
    /// record does not bind, or by one whose binding was released, fails closed
    /// and leaves the panel in its pre-mount state. While no controller is
    /// bound the allocation epoch is the accepted generation, so this is a
    /// no-op on every existing call site (bitty#2476 residual 3).
    ///
    /// # Errors
    /// `StaleHandle`, `AlreadyMounted`, `PanelAlreadyMounted`, `InvalidState`,
    /// `ControllerConflict`, `ControllerReleased`.
    pub fn mount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        view_id: ViewId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        if self.panel_to_view.contains_key(&panel_id) {
            let cur = self.panel_to_view[&panel_id];
            return Err(PanelError::PanelAlreadyMounted {
                panel_id,
                current_view: cur,
            });
        }
        if self.view_to_panel.contains_key(&view_id) {
            let existing = self.view_to_panel[&view_id];
            let content = ViewContent::Panel(existing);
            return Err(PanelError::AlreadyMounted {
                view_id,
                existing: content,
            });
        }
        let state = self.panel_state_at_controller_fence(panel_id, generation)?;
        if !matches!(state, UiPanelState::Created | UiPanelState::Suspended) {
            return Err(PanelError::InvalidState {
                current: state,
                expected: "Created or Suspended",
            });
        }
        // Commit mount. The fence above resolved the record and accepted
        // `generation` as its binding key, which a bound controller generation
        // is while the allocation epoch is not; the commit is therefore keyed
        // by the panel id instead of re-checking the allocation epoch.
        let Some(rec_mut) = self.panels.get_mut(&panel_id.0) else {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            });
        };
        rec_mut.state = UiPanelState::Mounted;
        rec_mut.view = Some(view_id);
        // A bind clears the placeholder: the panel has a controller and content
        // again, so the unbound representation no longer describes it (OQ-084
        // E: the placeholder "disappears on rebind").
        rec_mut.placeholder = None;
        self.view_to_panel.insert(view_id, panel_id);
        self.panel_to_view.insert(panel_id, view_id);
        Ok(())
    }

    /// Unmounts a panel, preserving its identity and generation, transitioning
    /// `Mounted`/`Focused`/`Suspended` -> `Suspended` (retain) or `Created`.
    pub fn unmount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<ViewId, PanelError> {
        self.ensure_not_disposed()?;
        let workspace = {
            let rec = self.get_panel(panel_id, generation)?;
            if !matches!(
                rec.state,
                UiPanelState::Mounted | UiPanelState::Focused | UiPanelState::Suspended
            ) {
                return Err(PanelError::InvalidState {
                    current: rec.state,
                    expected: "Mounted/Focused/Suspended",
                });
            }
            rec.workspace
        };
        let view_id = self
            .panel_to_view
            .remove(&panel_id)
            .ok_or(PanelError::NotFound {
                kind: "panel view",
                id_raw: panel_id.0,
            })?;
        self.view_to_panel.remove(&view_id);
        // Focus MRU update if focused
        if let Some(ws) = workspace {
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        rec_mut.view = None;
        Ok(view_id)
    }

    /// Focuses a mounted panel within its workspace, moving to MRU front and
    /// transitioning `Mounted -> Focused`. Hidden panels cannot be focused.
    pub fn focus_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        workspace: WorkspaceId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        // CTX-0535 (#922): a panel with no view attachment can never be
        // focused, even if a corrupted/legacy record claims `Mounted`.
        if rec.view.is_none()
            || (rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused)
        {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused with an attached view",
            });
        }
        if rec.workspace != Some(workspace) && rec.workspace.is_some() {
            // Allow focus only within its workspace; if panel has workspace binding, enforce it
            return Err(PanelError::NotFound {
                kind: "workspace",
                id_raw: workspace.0,
            });
        }
        // Update MRU
        let focus = self.focus_per_workspace.entry(workspace).or_default();
        focus.set(panel_id);
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Focused;
        Ok(())
    }

    /// Suspends a focused or mounted panel (invisible without destroying attachment).
    pub fn suspend_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused",
            });
        }
        let ws = rec.workspace;
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        if let Some(w) = ws {
            if let Some(focus) = self.focus_per_workspace.get_mut(&w) {
                focus.on_panel_hidden(panel_id);
            }
        }
        Ok(())
    }

    /// Resumes a suspended panel back to the appropriate state.
    ///
    /// A panel suspended with its view attachment retained (`suspend_panel`)
    /// returns to `Mounted`. A viewless record (`unmount_panel` destroyed the
    /// attachment) returns to `Created` and must pass through `mount_panel`
    /// to re-attach a view, so `Mounted` never coexists with `view == None`
    /// (CTX-0535, issue #922).
    pub fn resume_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Suspended {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Suspended",
            });
        }
        let has_view = rec.view.is_some();
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = if has_view {
            UiPanelState::Mounted
        } else {
            UiPanelState::Created
        };
        Ok(())
    }

    /// Disposes a panel, clearing its view attachment, MRU, capabilities,
    /// event queues, and retiring `(PanelId, Generation)`.
    ///
    /// The `StaleHandle` fence is on the allocation epoch the handle carries,
    /// so the public error contract does not move when a controller is bound:
    /// a stale handle is refused exactly as before. The generations released
    /// are the ones the record's binding names (OQ-084 D.2 phase 1), which is
    /// the allocation epoch while no controller is bound.
    ///
    /// The release is fail-closed across every key the fences can mint: the
    /// binding generation is released first, and once that succeeded whatever
    /// the writers left under the allocation epoch is swept before the record
    /// goes (bitty#2476 residual 1). A refused release returns with every
    /// family — including the epoch's rows and attributed overlays — intact.
    pub fn dispose_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let (stored_gen, binding_key, workspace, was_controller) = {
            let rec = self.panels.get(&panel_id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            })?;
            // A generation that mounted, or that held panel-scoped grants, was
            // a controller; a bare `Created` record rolled back from a failed
            // bind was not. The grant row and every other generation-owned
            // family are keyed by the binding generation (D.2 phase 1/2 step
            // 1), which is the allocation epoch while no controller is bound.
            let binding_key = rec.grant_key_generation();
            let held_grants = self
                .capabilities
                .get(&(panel_id, binding_key))
                .is_some_and(|grants| !grants.is_empty());
            (
                rec.generation,
                binding_key,
                rec.workspace,
                rec.state != UiPanelState::Created || held_grants,
            )
        };
        if stored_gen != generation {
            self.bump_error("StaleHandle");
            return Err(PanelError::StaleHandle {
                expected_generation: stored_gen,
                found_generation: generation,
                id_raw: panel_id.0,
            });
        }
        // Run the generation release before any other teardown: a tombstone
        // write that fails closed must leave the panel, its view, its queues,
        // and its live grant row intact.
        if was_controller {
            // OQ-084 B-4: the edge release copies the departing generation's
            // grant set into the retained ceiling and then removes the live
            // row, so a bind for the successor reads a real ceiling instead of
            // a drained table. The release acts on the binding generation, so
            // it is handed that key while the fence above stays on the
            // allocation epoch.
            self.release_generation_scope(panel_id, binding_key)?;
        } else {
            // A panel that never mounted and held nothing is not a
            // predecessor: a rolled-back create leaves no ceiling behind. Its
            // generation-owned rows and surfaces are still removed under the
            // binding key, so a bound-but-never-mounted panel leaves no grant
            // row behind.
            self.capabilities.remove(&(panel_id, binding_key));
            self.release_attributed_overlays(panel_id, binding_key);
        }
        // OQ-084 D.2 phase 1, bitty#2476 residual 1: the release above names the
        // record's binding generation, but the writers in the tree key by the
        // generation the handle carries, which the fence restricts to the
        // allocation epoch. Sweep what was left under the epoch before the
        // record goes, so no family keyed by a dead `PanelId` outlives its
        // container. Reached only after a successful release, so a fail-closed
        // release leaves every family in place.
        self.sweep_allocation_epoch_families(panel_id, stored_gen);
        // Remove view attachment if any
        if let Some(view_id) = self.panel_to_view.remove(&panel_id) {
            self.view_to_panel.remove(&view_id);
        }
        // Remove from workspace list and MRU
        if let Some(ws) = workspace {
            let mut emptied = false;
            if let Some(list) = self.workspace_panels.get_mut(&ws) {
                list.retain(|&id| id != panel_id);
                emptied = list.is_empty();
            }
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
            // OQ-084 A.7: a workspace id is minted once and never reused, so an
            // empty bucket could never be reclaimed. Dropping it with the last
            // panel keeps the table bounded by the live workspaces rather than
            // by every workspace ever opened (the workspace-close sweep of C.9
            // makes this the ordinary path).
            if emptied {
                self.workspace_panels.remove(&ws);
                self.focus_per_workspace.remove(&ws);
            }
        }
        // Clear commands and bus queues
        self.command_registry.unregister_panel(panel_id);
        self.event_bus.clear_panel(panel_id);
        // OQ-084 A.2/PGO-23: the tombstone is dropped with the container. A
        // disposed panel is gone, so a later panel is a first bind rather than
        // a reload of this identity; keeping the ceiling here would leave a
        // record that no bind can name and no bound can reclaim. The entry is
        // keyed by `(owner, panel)`, so the key is resolved first.
        if let Some(key) = self.retained_ceiling_key(panel_id) {
            self.retained_ceilings.remove(&key);
        }
        self.release_records.remove(&panel_id);
        // The recovery counter is scoped to a container, so it retires with it:
        // a new panel is a first bind, not a continuation of this one's budget.
        self.recovery.retire(panel_id);
        // Retire panel with generation bump
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        }
        self.panels.remove(&panel_id.0);
        Ok(())
    }

    /// Presents the placeholder for a panel whose controller is gone, or
    /// disposes it when the reason is a user close (OQ-084 Decision E).
    ///
    /// `presented` is the controller generation the transition names.
    /// `reason_token` is a token of Decision E rule 2's closed vocabulary;
    /// `closed`, `dismissed`, and `quit` select the `Drop` outcome, `denied`
    /// selects `Unavailable`, and the rest select `Placeholder`.
    ///
    /// The FS-4 record's owner is **not** an argument: it is read from the
    /// panel record's own binding (`PanelRecord::owner`) before the unbind
    /// edge clears it, so a transition can never be attributed to a plugin the
    /// panel did not bind, and it is `None` exactly while no controller is
    /// bound.
    ///
    /// The three rules this enforces:
    ///
    /// 1. **The container is retained, the binding is not.** A placeholder
    ///    transition leaves state, workspace membership, title, and view
    ///    attachment exactly as they were; it clears the controller binding
    ///    through the unbind edge ([`Self::release_generation_scope`], which
    ///    writes the retired-ceiling tombstone before it drops the generation's
    ///    grant row) and clears the content. Focus is not moved: the panel keeps
    ///    its focus history (C.3).
    /// 2. **Recovery is bounded.** One counter keyed
    ///    `(PanelId, presented, reason)`, capped at eight. The ninth attempt
    ///    changes no state and emits the single `saturated` note for the
    ///    binding; later attempts emit nothing. The per-panel key table is
    ///    bounded too, and makes room by evicting its oldest key rather than
    ///    dropping the transition: a panel past its key budget is still counted
    ///    and still recorded.
    /// 3. **The reason is closed.** A token outside the vocabulary is refused
    ///    with [`PanelError::RecoveryReasonRejected`] before anything is read or
    ///    written.
    ///
    /// Nothing here reaches a terminal, a PTY, a renderer handle, or a plugin
    /// callback: the presentation is a Core-owned [`bitty_rich::scene::RichBlock`]
    /// built from a constant table (see [`crate::panel_placeholder`]).
    ///
    /// **Callers.** The terminal generation edge: `PluginRuntime::dispose_inner`
    /// asks the installed `PanelGenerationPort::release_generation` for the
    /// containers the ended generation still drives, and the composition root's
    /// implementation reaches this method through
    /// `PanelRuntime::present_placeholder`. It runs with no successor, after the
    /// generation's own content release, which is exactly the edge A.2
    /// describes — and because this method performs the unbind release itself,
    /// the release always completes before the placeholder becomes visible.
    /// A generation that never bound a container presents nothing, because
    /// nothing in the tree names its panels (OQ-084 follow-up (e)), and a
    /// generation whose edge already ran is refused by the controller fence
    /// rather than released twice.
    ///
    /// # Errors
    /// `NotFound`, `RegistryDisposed`, `RecoveryReasonRejected`,
    /// `ControllerConflict`, `ControllerReleased`, `StaleHandle` (from the
    /// controller fence), `ResourceExhausted` (from the unbind edge), and the
    /// propagation of any `dispose_panel` refusal on the `Drop` path.
    pub fn present_placeholder(
        &mut self,
        panel_id: PanelId,
        presented: Generation,
        reason_token: &str,
    ) -> Result<PlaceholderPresentation, PanelError> {
        self.ensure_not_disposed()?;
        let outcome = match RecoveryOutcome::classify(reason_token) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.bump_error("RecoveryReasonRejected");
                return Err(PanelError::RecoveryReasonRejected {
                    panel_id,
                    spelling: error.spelling().to_owned(),
                });
            }
        };
        if outcome.disposes_container() {
            // `Drop`: a user close is Core-initiated, not a plugin verb, so the
            // container is retired by its allocation epoch rather than through
            // the controller fence. The record names that epoch — the
            // generation the disposal actually used — and the owner the record
            // held at that moment, read before the record is gone.
            let (epoch, owner) = match self.panels.get(&panel_id.0) {
                Some(record) => (
                    record.generation,
                    record.owner().map(|plugin| plugin.as_str().to_owned()),
                ),
                None => {
                    return Err(PanelError::NotFound {
                        kind: "panel",
                        id_raw: panel_id.0,
                    });
                }
            };
            self.dispose_panel(panel_id, epoch)?;
            let step =
                self.recovery
                    .note(panel_id, owner.as_deref(), epoch.get(), outcome.reason());
            let record = match step {
                RecoveryStep::Recorded(record) => Some(record),
                RecoveryStep::Saturated { .. } | RecoveryStep::Unchanged => None,
            };
            self.recovery.retire(panel_id);
            return Ok(PlaceholderPresentation {
                outcome,
                placeholder: None,
                record,
                saturated: false,
            });
        }
        // Placeholder/Unavailable: revalidate the ending generation at use
        // before any state moves (C.5: a transition issued by a generation the
        // record no longer binds fails closed).
        let state = self.panel_state_at_controller_fence(panel_id, presented)?;
        let (bound, last, owner) = match self.panels.get(&panel_id.0) {
            Some(record) => (
                record.controller_generation(),
                record.last_controller_generation,
                record.owner().map(|plugin| plugin.as_str().to_owned()),
            ),
            None => {
                return Err(PanelError::NotFound {
                    kind: "panel",
                    id_raw: panel_id.0,
                });
            }
        };
        let placeholder_reason = match outcome.placeholder_reason() {
            Some(reason) => reason,
            // Unreachable: `Drop` returned above.
            None => {
                return Err(PanelError::InvalidState {
                    current: state,
                    expected: "a placeholder outcome",
                });
            }
        };
        if bound == Some(presented) {
            // The unbind edge: tombstone, then the generation's grant row,
            // bus queues, commands, and attributed overlays. A refused release
            // leaves every family and the binding in place, and this call
            // propagates without presenting anything.
            self.release_generation_scope(panel_id, presented)?;
        }
        let step = self.recovery.note(
            panel_id,
            owner.as_deref(),
            presented.get(),
            outcome.reason(),
        );
        let (record, saturated) = match step {
            RecoveryStep::Recorded(record) => (Some(record), false),
            RecoveryStep::Saturated { .. } => (None, true),
            RecoveryStep::Unchanged => (None, false),
        };
        // The placeholder is written for every panel that reaches this path,
        // whatever the counter answered. A saturated or unchanged counter emits
        // no new record, but the generation's families have already been
        // released: returning without a placeholder would leave the container
        // unbound with nothing to present, the silent orphan Decision E and
        // PGO-23 forbid.
        if let Some(record_mut) = self.panels.get_mut(&panel_id.0) {
            record_mut.placeholder = Some(PanelPlaceholder {
                reason: placeholder_reason,
                unbound_generation: bound.or(last),
            });
        }
        Ok(PlaceholderPresentation {
            outcome,
            placeholder: self.placeholder(panel_id),
            record,
            saturated,
        })
    }

    /// The placeholder on a panel, if it is unbound (OQ-084 E).
    #[must_use]
    pub fn placeholder(&self, panel_id: PanelId) -> Option<PanelPlaceholder> {
        self.panels
            .get(&panel_id.0)
            .and_then(|record| record.placeholder)
    }

    /// The single reason a panel currently presents, if it presents one
    /// (`PGO-14`: exactly one findable reason per panel).
    #[must_use]
    pub fn placeholder_reason(&self, panel_id: PanelId) -> Option<PlaceholderReason> {
        self.placeholder(panel_id).map(PanelPlaceholder::reason)
    }

    /// The counted recovery attempts for one `(panel, generation, reason)` key.
    #[must_use]
    pub fn recovery_attempts(
        &self,
        panel_id: PanelId,
        generation: Generation,
        reason: PlaceholderReason,
    ) -> u8 {
        self.recovery
            .count(panel_id, generation.get(), RecoveryReason::from(reason))
    }

    /// The binding a panel's single `saturated` note already went out for.
    #[must_use]
    pub fn recovery_saturated_for(&self, panel_id: PanelId) -> Option<Generation> {
        self.recovery.saturated_note(panel_id).map(Generation)
    }

    /// Distinct `(controller generation, reason)` counter keys tracked for a
    /// panel.
    ///
    /// Always `<= MAX_RECOVERY_KEYS_PER_PANEL`: the table makes room by evicting
    /// its oldest key rather than growing, so a transition that arrives at the
    /// cap is still counted and still recorded.
    #[must_use]
    pub fn recovery_keys(&self, panel_id: PanelId) -> usize {
        self.recovery.tracked_keys(panel_id)
    }

    /// The retained recovery records for a panel, oldest first (at most
    /// `RECOVERY_ATTEMPT_CAP`).
    #[must_use]
    pub fn recovery_records(&self, panel_id: PanelId) -> Vec<RecoveryRecord> {
        self.recovery.records(panel_id)
    }

    /// The Core-owned placeholder block a panel currently presents, rebuilt
    /// from its reason and admitted against `SCN-1..3`.
    ///
    /// `Ok(None)` when the panel is bound (it has no placeholder). The block is
    /// never stored, so no caller and no plugin can hold or mutate it.
    ///
    /// # Errors
    /// [`PanelError::PlaceholderSceneRejected`] if the fixed table ever stops
    /// fitting the accepted scene limits (a Core defect), and `NotFound` when
    /// the panel is gone or the registry is disposed.
    pub fn placeholder_block(
        &self,
        panel_id: PanelId,
    ) -> Result<Option<bitty_rich::scene::RichBlock>, PanelError> {
        self.ensure_not_disposed()?;
        let record = match self.panels.get(&panel_id.0) {
            Some(record) => record,
            None => {
                return Err(PanelError::NotFound {
                    kind: "panel",
                    id_raw: panel_id.0,
                });
            }
        };
        let Some(placeholder) = record.placeholder else {
            return Ok(None);
        };
        match crate::panel_placeholder::core_placeholder_block(
            panel_id,
            placeholder.reason(),
            record.generation.get(),
        ) {
            Ok(block) => Ok(Some(block)),
            Err(error) => Err(PanelError::PlaceholderSceneRejected {
                panel_id,
                detail: error.to_string(),
            }),
        }
    }

    /// Returns focused panel for a workspace.
    #[must_use]
    pub fn focused_panel(&self, workspace: WorkspaceId) -> Option<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .and_then(|f| f.focused())
    }

    #[must_use]
    pub fn mru_order(&self, workspace: WorkspaceId) -> Vec<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .map(|f| f.mru_order())
            .unwrap_or_default()
    }

    /// Panels attributed to `workspace`, in the order OQ-084 C.9 disposes them:
    /// the focused panel first, then the rest of the workspace MRU, then any
    /// panel the workspace bucket still holds that was never focused (creation
    /// order).
    ///
    /// The bucket is authoritative for membership and the focus tracker only
    /// for order, because [`PanelFocus`] holds the panels that were *focused*,
    /// not the panels that exist: a `Created` panel that never took focus is in
    /// the bucket and in no MRU list. Reading MRU alone would lose it and a
    /// workspace close would silently leave its container behind (A.7).
    ///
    /// The workspace here is the runtime's minted [`WorkspaceId`] — the same
    /// value [`PanelRegistry::create_panel`] was handed — never an index or a
    /// terminal-registry sequence (A.7).
    #[must_use]
    pub fn panels_for_workspace(&self, workspace: WorkspaceId) -> Vec<PanelId> {
        let Some(bucket) = self.workspace_panels.get(&workspace) else {
            return Vec::new();
        };
        let mut ordered: Vec<PanelId> = Vec::with_capacity(bucket.len());
        let focus = self.focus_per_workspace.get(&workspace);
        if let Some(focused) = focus.and_then(|focus| focus.focused()) {
            if bucket.contains(&focused) {
                ordered.push(focused);
            }
        }
        if let Some(focus) = focus {
            for panel in focus.mru_order() {
                if !ordered.contains(&panel) && bucket.contains(&panel) {
                    ordered.push(panel);
                }
            }
        }
        for &panel in bucket {
            if !ordered.contains(&panel) {
                ordered.push(panel);
            }
        }
        ordered
    }

    /// Number of workspace buckets the registry tracks.
    ///
    /// A bucket is created by [`Self::create_panel`] with a `WorkspaceId` and
    /// dropped by [`Self::dispose_panel`] when its last panel goes, so a
    /// disposed workspace leaves no empty bucket behind. `WorkspaceId`s are
    /// minted once and never reused (A.7), so an empty bucket would never be
    /// reclaimed.
    #[must_use]
    pub fn attributed_workspace_count(&self) -> usize {
        self.workspace_panels.len()
    }

    /// The facts a workspace sweep stamps on a panel's disposal record before
    /// the container goes (OQ-084 C.9), or `None` when the panel is not live.
    ///
    /// `controller_generation` is the generation the disposal releases—the
    /// bound controller generation when one exists, otherwise the allocation
    /// epoch (D.2 phase 2 step 1)—and `owner` is the plugin that held the
    /// content binding. Only a bound controller writes an owner (A.2), so a
    /// container that never bound one has none.
    #[must_use]
    pub fn panel_disposal_facts(&self, panel_id: PanelId) -> Option<PanelDisposalFacts> {
        let record = self.panels.get(&panel_id.0)?;
        Some(PanelDisposalFacts {
            allocation_generation: record.generation,
            controller_generation: record.grant_key_generation(),
            owner: record.owner().cloned(),
            state: record.state,
        })
    }

    /// Disposes every panel attributed to `workspace`, focused panel first
    /// (OQ-084 C.9: "a workspace close is a disposal path, not a category").
    ///
    /// Each panel goes through [`Self::dispose_panel`], so every panel is
    /// all-or-nothing and consults the six-state model rather than a second
    /// transition table, and the `Focused -> Disposed` / `Created -> Disposed`
    /// release obligations of C.8 are the ones already implemented there.
    /// Ordering matters for the focused panel: disposing it releases input
    /// routing and moves focus to the next MRU entry, so the workspace never
    /// holds a focus pointer to a retired container.
    ///
    /// One [`PanelDisposalRecord`] is written per *disposed* panel with reason
    /// [`PanelDisposalReason::Quit`] and action
    /// [`PanelDisposalAction::Disposed`]; a refused panel emits none, because
    /// nothing about it was disposed.
    ///
    /// **Failure semantics: fail-fast, resumable.** The first refusal stops the
    /// sweep. Panels already disposed stay disposed (each disposal is atomic
    /// and irreversible), the refused panel keeps its previous valid state
    /// untouched, and every panel after it is reported in
    /// [`WorkspaceDisposeReport::remaining`] rather than silently skipped — a
    /// caller that only read `outcomes` would mistake a partial close for a
    /// complete one. The sweep is idempotent: the refused panel and the
    /// remainder are still attributed to the workspace, so re-running it after
    /// the cause is fixed disposes exactly those.
    ///
    /// A refusal caused by the registry itself ([`PanelError::RegistryDisposed`])
    /// therefore reports, it does not panic: the workspace close that called it
    /// already succeeded in the layout domain and cannot be undone here.
    #[must_use]
    pub fn dispose_workspace_panels(&mut self, workspace: WorkspaceId) -> WorkspaceDisposeReport {
        let ordered = self.panels_for_workspace(workspace);
        let mut outcomes: Vec<PanelDisposalOutcome> = Vec::with_capacity(ordered.len());
        let mut records: Vec<PanelDisposalRecord> = Vec::with_capacity(ordered.len());
        let mut remaining: Vec<PanelId> = Vec::new();
        for (index, panel) in ordered.iter().copied().enumerate() {
            let Some(facts) = self.panel_disposal_facts(panel) else {
                // The bucket and the record are written and dropped together,
                // so this is a stale bucket entry rather than a live panel.
                // Report it instead of skipping it: a silent skip would leave a
                // container nothing could ever close.
                outcomes.push(PanelDisposalOutcome {
                    panel,
                    result: Err(PanelError::NotFound {
                        kind: "panel",
                        id_raw: panel.0,
                    }),
                });
                remaining.extend(ordered[index + 1..].iter().copied());
                break;
            };
            match self.dispose_panel(panel, facts.allocation_generation) {
                Ok(()) => {
                    outcomes.push(PanelDisposalOutcome {
                        panel,
                        result: Ok(()),
                    });
                    records.push(PanelDisposalRecord {
                        panel,
                        workspace,
                        owner: facts.owner,
                        controller_generation: facts.controller_generation,
                        state: facts.state,
                        reason: PanelDisposalReason::Quit,
                        action: PanelDisposalAction::Disposed,
                    });
                }
                Err(error) => {
                    outcomes.push(PanelDisposalOutcome {
                        panel,
                        result: Err(error),
                    });
                    remaining.extend(ordered[index + 1..].iter().copied());
                    break;
                }
            }
        }
        WorkspaceDisposeReport {
            workspace,
            outcomes,
            remaining,
            records,
        }
    }

    // ------------------------------------------------------------------
    // Command registry (owner.name:command)
    // ------------------------------------------------------------------

    pub fn register_command(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        raw: &str,
    ) -> Result<QualifiedCommand, PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.command_registry
            .register(panel_id, raw)
            .map_err(|e| match e {
                bitty_ui::panel::CommandError::Duplicate { command, owner } => {
                    PanelError::DuplicateCommand { command, owner }
                }
                bitty_ui::panel::CommandError::TooManyCommands { max, current } => {
                    PanelError::TooManyCommands { max, current }
                }
                bitty_ui::panel::CommandError::Invalid(msg) => {
                    PanelError::InvalidCommand { reason: msg }
                }
            })
    }

    #[must_use]
    pub fn command_owner(&self, command: &str) -> Option<PanelId> {
        self.command_registry.owner_of(command)
    }

    // ------------------------------------------------------------------
    // Overlay (4+1)
    // ------------------------------------------------------------------

    pub fn create_overlay(
        &mut self,
        kind: UiOverlayKind,
        bounds: UiRect,
        text: impl Into<String>,
        tooltip: Option<String>,
    ) -> Result<u64, PanelError> {
        self.ensure_not_disposed()?;
        self.overlay_manager
            .create_overlay(kind, bounds, text, tooltip, self.registry_generation.get())
            .map_err(|e| match e {
                OverlayError::OverlayBusy => PanelError::OverlayBusy,
                OverlayError::TooManyOverlays { max, current } => {
                    PanelError::TooManyOverlays { max, current }
                }
            })
    }

    /// Creates an overlay attributed to `(panel_id, generation)` (OQ-084
    /// A.2/D.4).
    ///
    /// The handle is validated before allocation, so a stale or unknown
    /// generation cannot claim a surface. The unbind edge dismisses exactly
    /// the overlays attributed to the ended generation; an overlay created
    /// through [`Self::create_overlay`] carries no attribution and is not
    /// touched by that edge.
    ///
    /// # Errors
    /// The [`Self::create_overlay`] errors plus `NotFound`/`StaleHandle`.
    pub fn create_panel_overlay(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        kind: UiOverlayKind,
        bounds: UiRect,
        text: impl Into<String>,
        tooltip: Option<String>,
    ) -> Result<u64, PanelError> {
        self.get_panel(panel_id, generation)?;
        let id = self.create_overlay(kind, bounds, text, tooltip)?;
        self.overlay_attribution.insert(id, (panel_id, generation));
        Ok(id)
    }

    /// The `(PanelId, generation)` attribution of a live overlay, if any.
    #[must_use]
    pub fn overlay_attribution_of(&self, id: u64) -> Option<(PanelId, Generation)> {
        self.overlay_attribution.get(&id).copied()
    }

    /// Number of live overlays carrying panel attribution.
    #[must_use]
    pub fn attributed_overlay_count(&self) -> usize {
        self.overlay_attribution.len()
    }

    pub fn dismiss_overlay(&mut self, id: u64) -> Option<Overlay> {
        self.overlay_attribution.remove(&id);
        self.overlay_manager.dismiss(id)
    }

    /// Dismisses every overlay attributed to `(panel_id, generation)` and
    /// returns how many were released (OQ-084 A.2/D.4).
    fn release_attributed_overlays(&mut self, panel_id: PanelId, generation: Generation) -> usize {
        self.dismiss_attributed_overlays_matching(|owner| *owner == (panel_id, generation))
    }

    /// Removes the generation-owned rows a panel can still own under its
    /// allocation epoch and dismisses every overlay it still has attributed
    /// (OQ-084 D.2 phase 1, bitty#2476 residual 1).
    ///
    /// [`Self::dispose_panel`] releases the record's binding generation, but
    /// every writer in the tree keys its row by the generation the caller
    /// presents and `get_panel` admits only the allocation epoch:
    /// `grant_panel_capability`, `create_panel_overlay` and
    /// `install_rebound_grants` all write under the epoch, while a bound
    /// panel's release runs under the binding key. Those are the two keys the
    /// fences can mint, and the release above already removed the binding one,
    /// so the epoch is what is left to sweep. Without this a disposed bound
    /// panel would leave a grant row — and attributed overlays — keyed by a
    /// `PanelId` no live record names, which no later path can reclaim.
    ///
    /// The overlay pass is by panel id rather than by key: an attribution is a
    /// live surface, so anything still attributed to this container is
    /// dismissed whatever generation wrote it. Callers run this only after the
    /// release succeeded, so a fail-closed release keeps every family intact.
    fn sweep_allocation_epoch_families(&mut self, panel_id: PanelId, epoch: Generation) {
        self.capabilities.remove(&(panel_id, epoch));
        self.dismiss_attributed_overlays_matching(|owner| owner.0 == panel_id);
    }

    /// Dismisses every overlay whose attribution satisfies `matches` and
    /// returns how many were released (OQ-084 A.2/D.4).
    fn dismiss_attributed_overlays_matching(
        &mut self,
        matches: impl Fn(&(PanelId, Generation)) -> bool,
    ) -> usize {
        let ids: Vec<u64> = self
            .overlay_attribution
            .iter()
            .filter(|(_, owner)| matches(owner))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.overlay_attribution.remove(id);
            self.overlay_manager.dismiss(*id);
        }
        ids.len()
    }

    #[must_use]
    pub fn overlay_len(&self) -> usize {
        self.overlay_manager.len()
    }

    #[must_use]
    pub fn overlay_modal_active(&self) -> bool {
        self.overlay_manager.modal_active()
    }

    // ------------------------------------------------------------------
    // EventBus
    // ------------------------------------------------------------------

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.declare_topic(raw)
    }

    pub fn subscribe(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        topic: &EventTopic,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.event_bus.subscribe(panel_id, topic)
    }

    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.publish(topic, payload)
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        self.event_bus
            .drain_batch(panel_id, topic, max_events, max_bytes)
    }

    #[must_use]
    pub fn bus_total_events(&self) -> usize {
        self.event_bus.total_queued_events()
    }

    #[must_use]
    pub fn bus_total_bytes(&self) -> usize {
        self.event_bus.total_queued_bytes()
    }

    #[must_use]
    pub fn bus_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.event_bus.queued_events_for_panel(panel_id)
    }

    #[must_use]
    pub fn bus_total_dropped(&self) -> u64 {
        self.event_bus.total_dropped()
    }

    // ------------------------------------------------------------------
    // Capability isolation per (PanelId, generation) — panel.*
    // ------------------------------------------------------------------

    /// Grants a `panel.*` capability to a panel handle. Validates closed set.
    pub fn grant_panel_capability(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        Self::check_panel_capability(panel_id, capability)?;
        self.capabilities
            .entry((panel_id, generation))
            .or_default()
            .insert(capability.to_string());
        Ok(())
    }

    /// Validates one `panel.*` capability against the plugin-host grammar.
    ///
    /// # Errors
    /// `CapabilityDenied` for a malformed id or a non-`panel` family.
    fn check_panel_capability(panel_id: PanelId, capability: &str) -> Result<(), PanelError> {
        // Validate capability via plugin-host grammar (reuse)
        let cid = bitty_plugin_host::CapabilityId::parse(capability).map_err(|e| {
            PanelError::CapabilityDenied {
                panel_id,
                capability: format!("{capability}: {e}"),
            }
        })?;
        if cid.family() != bitty_plugin_host::CapabilityFamily::Panel {
            return Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            });
        }
        Ok(())
    }

    /// Runs the unbind release for one controller generation (OQ-084 A.2/B-4).
    ///
    /// The release covers every generation-owned family, in one call:
    ///
    /// - the panel-scoped grant row (copied into the retired-ceiling
    ///   tombstone first);
    /// - the panel's queued bus events, drained;
    /// - the commands this generation registered, unregistered;
    /// - the overlays attributed to `(panel_id, generation)`, dismissed.
    ///
    /// It writes one [`GenerationReleaseRecord`] naming the ended generation
    /// and counting each family. The record is created even when every family
    /// releases nothing, so "the predecessor released `∅`" is distinguishable
    /// from "no edge ran" through [`Self::unbind_release_count`].
    ///
    /// Copies the generation's panel-scoped grant set into the retired-ceiling
    /// tombstone and only then removes the live row, so the ceiling exists for
    /// the whole window in which the row does not. The copy happens even when
    /// the generation held nothing, because "the predecessor existed and held
    /// `∅`" is a ceiling of `∅` and must not be confused with "there is no
    /// predecessor" (which grants the successor's whole resolution).
    ///
    /// When the released generation is the bound controller generation, the
    /// allocation-epoch row goes with it: writers fence on the epoch rather
    /// than on the binding, so such a row can exist while a controller holds
    /// the panel, and once the unbind exposes the epoch again it would key the
    /// panel's grants and outrank the tombstone in
    /// [`Self::derive_rebind_grants`]. The epoch row is not copied into the
    /// tombstone — it never was a controller's grant set.
    ///
    /// A generation that already reached its edge is not released twice: the
    /// record the first release wrote is left untouched and `false` is
    /// returned. `A.2` has a bind repeat these releases in phase 1 as defence in
    /// depth, so the repetition is a normal call, and it must be idempotent —
    /// "already released" is not a predecessor that held `∅`. Note that an
    /// unbound generation holds no live row at all, which is why the duplicate
    /// is detected from the retained `ended_generation` rather than from the
    /// presence of a row: absence alone cannot tell "held nothing" from
    /// "already released". An edge that kept a controller predecessor has no
    /// tombstone of its own to be named by, so its repeat is recognised from the
    /// [`GenerationReleaseRecord`] the first pass wrote.
    ///
    /// An edge that ended the allocation epoch ended no controller binding, so
    /// it also leaves the record's controller predecessor pair — and the
    /// tombstone stamped with that pair — exactly as the last controller edge
    /// wrote them (bitty#2476 residual 7(f)). The epoch is never a controller
    /// generation (B.1/B.2), so it is not a predecessor: overwriting
    /// `(last_controller_owner, last_controller_generation)` with
    /// `(None, epoch)` would destroy the only pair [`Self::panels_ended_at`] can
    /// still yield the container by, and an epoch tombstone written over the
    /// controller's would leave that pair unreadable at phase 0, so the
    /// generation edge's successor could neither find the container nor rebind
    /// it. Every other effect is the same for either edge: the live row keyed by
    /// `generation` is removed, the queued events, commands and overlays are
    /// drained, and the [`GenerationReleaseRecord`] is written. A panel with no
    /// controller predecessor at all is the exception: the epoch edge is then
    /// its first, ownerless predecessor, and that is the ceiling a never-bound
    /// panel's first bind reads.
    ///
    /// The write fails closed: if the tombstone cannot be stored, the live row
    /// is kept, no other family is released, no previous-controller generation
    /// is recorded, and the error is returned. The registry never evicts
    /// another panel's ceiling to make room.
    ///
    /// Callers that are not a generation edge — a rolled-back create — must not
    /// call this.
    ///
    /// `generation` must be the panel's binding generation: the controller
    /// generation the record currently names when one is bound, otherwise the
    /// allocation epoch. Any other value is refused as `StaleHandle` before a
    /// family is touched, so the release cannot copy or delete a row the
    /// binding does not own. The exception is a generation whose edge already
    /// ran: it is recognised from the retained tombstone before that check, so
    /// the repeat is answered `false` instead of being measured against the
    /// epoch the unbind exposed.
    ///
    /// Returns `true` when a live grant row was removed.
    ///
    /// # Errors
    /// `RegistryDisposed`, `NotFound`, `StaleHandle`, or `ResourceExhausted`
    /// (fail-closed tombstone write; the live row and every other family are
    /// untouched).
    pub fn release_generation_scope(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<bool, PanelError> {
        self.ensure_not_disposed()?;
        // OQ-084 D.2 phase 1: the release acts on the row keyed by the panel's
        // binding generation — the controller generation when one is bound,
        // otherwise the allocation epoch. The re-check is against that same
        // key, so a caller presenting the allocation epoch while a controller
        // is bound, or a generation the panel has already left behind, is
        // refused before any family is touched rather than releasing the wrong
        // row.
        // `record.last_controller_owner` is deliberately not read here. The
        // owner this edge retains is the owner of the binding *this* edge ends,
        // so it comes from the live binding alone (`bound_owner`); an edge that
        // ends the allocation epoch ended no binding and owns nothing. Reading
        // the record's retained owner would copy an earlier edge's owner
        // forward (bitty#2476 residual 7(a)); the repeat branch below needs
        // that retained value but reads it from the record itself.
        let (released_generation, bound, last_ended, allocation_epoch, bound_owner) =
            match self.panels.get(&panel_id.0) {
                Some(record) => (
                    record.grant_key_generation(),
                    record.controller_generation(),
                    record.last_controller_generation,
                    record.generation,
                    record.owner().cloned(),
                ),
                None => {
                    return Err(PanelError::NotFound {
                        kind: "panel",
                        id_raw: panel_id.0,
                    });
                }
            };
        // `A.2`'s phase-1 repeat has to be decided from the *presented*
        // generation, before the binding-key check: the first release clears the
        // binding, so the repeat would otherwise be measured against the
        // allocation epoch the unbind exposed and refused as `StaleHandle`
        // instead of recognised as an edge that already ran. While a controller
        // is still bound the key check keeps its place ahead of the tombstone,
        // so a foreign generation is still refused before any family is
        // touched.
        let tombstone_names_generation = self
            .retained_grant_ceiling(panel_id)
            .is_some_and(|ceiling| ceiling.ended_generation == generation);
        let already_ended = if bound.is_some() {
            released_generation == generation && tombstone_names_generation
        } else {
            tombstone_names_generation
                || last_ended == Some(generation)
                // A kept controller predecessor leaves no tombstone of the
                // epoch edge's own and the retained pair names the controller,
                // so neither test above recognises the epoch edge's repeat. The
                // release record is the "an edge ran" marker the release always
                // writes, whatever family it released (bitty#2476 7(f)).
                || self
                    .release_records
                    .get(&panel_id)
                    .is_some_and(|record| record.generation == generation)
        };
        if already_ended {
            // `A.2`'s phase-1 repeat: this edge ran, so there is nothing to
            // retain, nothing to overwrite, and no family to release again.
            if let Some(record) = self.panels.get_mut(&panel_id.0) {
                // The predecessor owner the first release stamped is not
                // rewritten with the `None` the cleared binding now yields.
                // The rule is the first pass's (bitty#2476 residual 7(f)): an
                // edge that ends no binding writes neither half, so a repeat
                // epoch edge leaves a kept controller predecessor pair alone
                // instead of mixing its generation with that edge's owner.
                if bound.is_some() || record.last_controller_generation.is_none() {
                    let ended_owner = bound_owner.or_else(|| record.last_controller_owner.clone());
                    record.last_controller_generation = Some(generation);
                    record.last_controller_owner = ended_owner;
                }
                record.release_controller_binding(generation);
            }
            return Ok(self.capabilities.remove(&(panel_id, generation)).is_some());
        }
        if released_generation != generation {
            self.bump_error("StaleHandle");
            return Err(PanelError::StaleHandle {
                expected_generation: released_generation,
                found_generation: generation,
                id_raw: panel_id.0,
            });
        }
        let held = self.capabilities.get(&(panel_id, generation)).cloned();
        let had_row = held.is_some();
        // OQ-084 A.2: the tombstone is stamped with the owner of the binding
        // this edge ends, read from the record **before** the binding is
        // released — after the release the record can no longer name it. An
        // edge that ends the allocation epoch ends no binding, so it has no
        // owner to stamp and retains `None`, which is the field's contract
        // ([`PanelRecord::last_controller_owner`]). Carrying an earlier edge's
        // owner forward would key the record and the `(owner, PanelId, N)`
        // tombstone under a plugin this edge never had, and the next legitimate
        // bind from another plugin would then be refused as `CeilingLost`
        // against that phantom predecessor (bitty#2476 residual 7(a)).
        let ended_owner = bound_owner;
        // bitty#2476 residual 7(f): whether this edge ends a bound controller.
        // The binding-key check above proved `released_generation == generation`
        // and that key is the binding's own generation when one is bound, so
        // this is true exactly for the edge that ended a controller binding.
        let ends_bound_controller = bound == Some(generation);
        // An edge that ends the allocation epoch ends no binding, and the epoch
        // is never a controller generation (B.1/B.2), so such an edge has no
        // predecessor fact of its own to record. When the panel already carries
        // one it must leave it alone: the pair
        // `(last_controller_generation, last_controller_owner)` and the
        // tombstone stamped with it are one fact (`N-13`: the owner is "written
        // and retained with" the generation, never on its own), and once the
        // release cleared the binding they are the only thing that still names
        // the ended generation — [`Self::panels_ended_at`] is what the
        // generation edge's successor bind enumerates. Overwriting them with
        // `(None, epoch)` would hand the container to nobody, and a tombstone
        // for the epoch would contradict the pair it left behind, so the
        // successor's own bind would then be refused `CeilingLost` against a
        // lost ceiling. The pair and its ceiling survive together or neither
        // can be read. A panel that never bound a controller has no such
        // predecessor: the epoch edge is its first, ownerless predecessor, and
        // this edge stamps one.
        let keeps_controller_predecessor = !ends_bound_controller && last_ended.is_some();
        // A controller generation that held no grant has no row; that is a
        // ceiling of `∅`, not an absent predecessor. Store the tombstone before
        // deleting the row so a failed write leaves the live row (and every
        // other family) in place.
        if !keeps_controller_predecessor {
            if let Err(error) = self.retain_grant_ceiling(
                panel_id,
                ended_owner.as_ref(),
                generation,
                held.clone().unwrap_or_default(),
            ) {
                self.bump_error("RetiredCeilingTableFull");
                return Err(error);
            }
        }
        if let Some(record) = self.panels.get_mut(&panel_id.0) {
            // Same edge, same reason: an epoch edge that keeps a controller
            // predecessor writes neither half of the pair.
            if !keeps_controller_predecessor {
                record.last_controller_generation = Some(generation);
                record.last_controller_owner = ended_owner;
            }
            record.release_controller_binding(generation);
        }
        let grants = held.map_or(0, |set| set.len());
        self.capabilities.remove(&(panel_id, generation));
        if bound == Some(generation) {
            // OQ-084 D.2 phase 2 step 1: the row the allocation epoch keys was
            // written by a writer that fences on the epoch (`get_panel`), not on
            // the binding, so it is not the ending controller's grant set. It
            // must not survive as the panel's binding key once the unbind
            // exposes the epoch again — `derive_rebind_grants` prefers a live
            // row over the tombstone, so a leftover epoch row would widen the
            // successor's ceiling with no consent diff. Sweep it with the
            // binding it outlived; a panel that never bound keeps its epoch row.
            self.capabilities.remove(&(panel_id, allocation_epoch));
        }
        let events = self.event_bus.clear_panel_counted(panel_id);
        let commands = self.command_registry.commands_for(panel_id).len();
        self.command_registry.unregister_panel(panel_id);
        let overlays = self.release_attributed_overlays(panel_id, generation);
        self.release_records.insert(
            panel_id,
            GenerationReleaseRecord {
                generation,
                grants,
                events,
                commands,
                overlays,
            },
        );
        Ok(had_row)
    }

    /// The ceiling phase 0 reads at bind entry (OQ-084 D.2).
    ///
    /// `None` means no predecessor generation reached an edge for this panel
    /// and no tombstone exists; `Some` with an empty `grants` set means the
    /// predecessor existed and held nothing. Reads the retained tombstone, not
    /// the live grant table.
    ///
    /// The table is keyed `(owner, PanelId)` and holds at most one entry per
    /// panel, so this reads the panel's entry without the caller having to name
    /// the owner it was stamped with. [`RetainedGrantCeiling::owner`] reports
    /// that owner.
    #[must_use]
    pub fn retained_grant_ceiling(&self, panel_id: PanelId) -> Option<&RetainedGrantCeiling> {
        self.retained_ceilings
            .iter()
            .find(|((_, keyed_panel), _)| *keyed_panel == panel_id)
            .map(|(_, ceiling)| ceiling)
    }

    /// The key this registry holds `panel_id`'s tombstone under, if any.
    fn retained_ceiling_key(
        &self,
        panel_id: PanelId,
    ) -> Option<(Option<bitty_plugin_host::PluginId>, PanelId)> {
        self.retained_ceilings
            .keys()
            .find(|(_, keyed_panel)| *keyed_panel == panel_id)
            .cloned()
    }

    /// Drops the panel's retired-ceiling tombstone: D.2 phase 2 step 3, the
    /// tombstone half (OQ-084; §D.2 "Two-phase bind" of
    /// `bitty-terminal-docs/specifications/panel-generation-ownership-decision.md`).
    ///
    /// D.2 puts this drop last in phase 2 and after the lease reset: "Reset the
    /// panel's lease entry to `Idle`, drop the panel's retired-ceiling tombstone
    /// for the predecessor owner and generation, then and only then permit
    /// successor verbs." The lease table is
    /// [`PanelRuntime`](super::host::PanelRuntime)'s and the tombstone table is
    /// this registry's, so the two writes cannot happen in one call.
    /// [`Self::rebind_panel`] therefore commits phase 2 steps 1 and 2 and
    /// stops; the facade resets the lease, calls this method, and returns only
    /// after both ran, which is D.2's order.
    ///
    /// The tombstone table is keyed by `(owner, panel)`, so this resolves the
    /// panel's key and drops that entry: a later bind must not consume a
    /// ceiling that has already been used. A panel holds at most one entry, so
    /// the owner is read rather than presented by the caller.
    ///
    /// A no-op unless the record currently binds a controller. A caller that
    /// has not committed phase 2 step 2 must not be able to retire a ceiling a
    /// later bind still needs, so the drop is gated on the binding rather than
    /// on the caller's word. Returns whether a tombstone was removed.
    pub fn retire_rebound_ceiling(&mut self, panel_id: PanelId) -> bool {
        let bound = self
            .panels
            .get(&panel_id.0)
            .is_some_and(|record| record.controller_generation().is_some());
        if !bound {
            return false;
        }
        let Some(key) = self.retained_ceiling_key(panel_id) else {
            return false;
        };
        self.retained_ceilings.remove(&key).is_some()
    }

    /// The most recently ended controller generation retained on the panel
    /// record (OQ-084 D.2 phase 0), or `None` before the first edge and after
    /// panel disposal.
    #[must_use]
    pub fn last_controller_generation(&self, panel_id: PanelId) -> Option<Generation> {
        self.panels
            .get(&panel_id.0)
            .and_then(|record| record.last_controller_generation)
    }

    /// The plugin that owned the most recently ended controller generation
    /// (OQ-084 A.2, D.2 phase 0), or `None` before the first edge, after panel
    /// disposal, and on a panel whose only ended edge is an allocation-epoch
    /// one. An edge that ended no binding stamps no owner, and an edge that
    /// ended the allocation epoch after a controller edge leaves that
    /// controller's pair in place (bitty#2476 residual 7(f)).
    ///
    /// The tombstone this registry holds for the panel is stamped with the same
    /// owner, which is what makes a tombstone under another owner decidable as
    /// a lost ceiling rather than a usable one.
    #[must_use]
    pub fn last_controller_owner(&self, panel_id: PanelId) -> Option<&bitty_plugin_host::PluginId> {
        self.panels
            .get(&panel_id.0)
            .and_then(|record| record.last_controller_owner.as_ref())
    }

    /// The controller generation the panel record currently binds (OQ-084 A.3,
    /// D.2 phase 2 step 2), or `None` while no controller is bound.
    ///
    /// Read-only: it answers `Some` only for a panel whose generation edge (or a
    /// test) wrote a binding, and `None` for a container that never bound a
    /// controller — including every `bitty.panel.create` container, whose first
    /// bind has no writer yet (OQ-084 follow-up (e)). It names the binding in
    /// force, not the allocation epoch (`generation` on the record, which never
    /// moves) and not the predecessor ([`Self::last_controller_generation`]).
    #[must_use]
    pub fn controller_generation(&self, panel_id: PanelId) -> Option<Generation> {
        self.panels
            .get(&panel_id.0)
            .and_then(PanelRecord::controller_generation)
    }

    /// The panels whose binding names `owner` at `generation` (OQ-084 A.2/A.3,
    /// bitty#2476 residual 2).
    ///
    /// The plugin generation edge asks this to learn which containers a
    /// generation currently drives. The predicate is the *binding* pair written
    /// by D.2 phase 2 step 2 — never the allocation epoch and never the
    /// retained predecessor pair — so a panel that never bound a controller, a
    /// panel whose edge already ended the generation (its binding is cleared
    /// and only [`Self::last_controller_generation`] remains), and a panel bound
    /// to another owner or another generation are all absent from the result.
    ///
    /// Results are in `PanelId` order and structurally bounded: every record
    /// this walks is already counted against `max_panels_per_window`, so an
    /// edge that binds every container of one generation cannot exceed the
    /// registry's own live-panel bound. Read-only; it touches no family and
    /// moves no counter.
    #[must_use]
    pub fn panels_bound_to(
        &self,
        owner: &bitty_plugin_host::PluginId,
        generation: Generation,
    ) -> Vec<PanelId> {
        let mut bound: Vec<PanelId> = self
            .panels
            .values()
            .filter(|record| record.state != UiPanelState::Disposed)
            .filter(|record| {
                record.binding.as_ref().is_some_and(|binding| {
                    &binding.plugin_id == owner && binding.controller_generation == generation
                })
            })
            .map(|record| record.id)
            .collect();
        bound.sort_unstable();
        bound
    }

    /// The panels whose controller binding ended at `owner` and `generation`
    /// (OQ-084 A.2/D.2 phase 0, bitty#2476 residual 2).
    ///
    /// The complement of [`Self::panels_bound_to`] for one generation: after the
    /// edge release cleared the binding, the record retains
    /// [`Self::last_controller_owner`] and [`Self::last_controller_generation`],
    /// and this is the only pair that still names the generation. The generation
    /// edge's successor bind enumerates these, because on the reload path the
    /// release ran before the successor activated (A.2, C.5) — a query over live
    /// bindings would find nothing and the containers would never be rebound.
    ///
    /// A panel still bound to the generation is absent (it is
    /// [`Self::panels_bound_to`]'s answer), as is a panel bound to a successor
    /// and a panel whose retained pair names another owner. Same ordering and
    /// same structural bound as [`Self::panels_bound_to`]; read-only.
    #[must_use]
    pub fn panels_ended_at(
        &self,
        owner: &bitty_plugin_host::PluginId,
        generation: Generation,
    ) -> Vec<PanelId> {
        let mut ended: Vec<PanelId> = self
            .panels
            .values()
            .filter(|record| record.state != UiPanelState::Disposed)
            .filter(|record| record.controller_generation().is_none())
            .filter(|record| {
                record.last_controller_generation == Some(generation)
                    && record.last_controller_owner.as_ref() == Some(owner)
            })
            .map(|record| record.id)
            .collect();
        ended.sort_unstable();
        ended
    }

    /// Number of retired-ceiling tombstones.
    ///
    /// One entry per live panel at most, so the count is structurally bounded
    /// by the configured `max_panels_per_window`
    /// (see [`MAX_RETAINED_GRANT_CEILINGS`]).
    #[must_use]
    pub fn retained_grant_ceiling_count(&self) -> usize {
        self.retained_ceilings.len()
    }

    /// The per-family release record written by the panel's last unbind edge
    /// (OQ-084 A.2), or `None` when no edge has run for this panel.
    ///
    /// A record whose every family is zero means the edge ran and released
    /// nothing; that is a different fact from `None`.
    #[must_use]
    pub fn unbind_release_record(&self, panel_id: PanelId) -> Option<&GenerationReleaseRecord> {
        self.release_records.get(&panel_id)
    }

    /// Total items released at the panel's last unbind edge (OQ-084 A.2).
    ///
    /// `None` means no edge ran; `Some(0)` means an edge ran and every family
    /// released nothing.
    #[must_use]
    pub fn unbind_release_count(&self, panel_id: PanelId) -> Option<usize> {
        self.release_records
            .get(&panel_id)
            .map(GenerationReleaseRecord::total)
    }

    /// Number of live per-panel release records.
    ///
    /// One entry per live panel at most, so the table is structurally bounded
    /// by the configured `max_panels_per_window`.
    #[must_use]
    pub fn unbind_release_record_count(&self) -> usize {
        self.release_records.len()
    }

    /// Stores one retired-ceiling tombstone, keyed `(owner, panel)` and
    /// replacing any previous entry for the same panel.
    ///
    /// OQ-084 A.2 keys the tombstone `(owner, PanelId, N)` because `PanelId` is
    /// registry-scoped but the owner is not implied: two plugins can carry the
    /// same controller generation number, so a key without the owner could hand
    /// a successor a wider ceiling than its own predecessor held (`N-13`). The
    /// table holds at most one entry per `(owner, panel)`, and the panel's next
    /// edge replaces its previous entry, so it holds at most one entry per live
    /// panel: "never one row per reload". An edge that ended the allocation
    /// epoch instead of a bound controller has no owner and stamps `None`.
    ///
    /// The registry admits at most `max_panels_per_window` live records, so the
    /// capacity check is a fail-closed invariant guard, not an eviction path:
    /// inserting a new key cannot reach the bound while the records that own the
    /// existing entries are alive. If the guard does trip, the caller keeps the
    /// live grant row.
    fn retain_grant_ceiling(
        &mut self,
        panel_id: PanelId,
        owner: Option<&bitty_plugin_host::PluginId>,
        generation: Generation,
        grants: BTreeSet<String>,
    ) -> Result<(), PanelError> {
        let capacity = self
            .config
            .max_panels_per_window
            .min(MAX_RETAINED_GRANT_CEILINGS);
        // A.2: the panel's next edge replaces its own tombstone, whatever owner
        // stamped the previous one. Without this the table could hold one entry
        // per (owner, panel) for the same panel across owners and the capacity
        // guard would refuse a live panel's ceiling.
        self.retained_ceilings
            .retain(|(_, keyed_panel), _| *keyed_panel != panel_id);
        if self.retained_ceilings.len() >= capacity {
            return Err(PanelError::ResourceExhausted {
                reason: format!(
                    "retired-ceiling table is full ({capacity}); refusing to evict a live panel's ceiling"
                ),
            });
        }
        self.retained_ceilings.insert(
            (owner.cloned(), panel_id),
            RetainedGrantCeiling {
                owner: owner.cloned(),
                ended_generation: generation,
                grants,
            },
        );
        Ok(())
    }

    /// Captures the predecessor ceiling at bind entry (OQ-084 D.2 phase 0).
    ///
    /// Reads and mutates nothing: the snapshot is a copy, the live grant row
    /// stays where it is, and the caller's phase 1 removes it afterwards. The
    /// source is selected once, in this order, and nowhere else:
    ///
    /// 1. **A still-bound predecessor** — the record names a controller — reads
    ///    the live row keyed by that binding. The live row wins even when a
    ///    tombstone for an older generation is present (PGO-22(a)). A bound
    ///    predecessor holding no row is a ceiling of `∅`, not an absent
    ///    ceiling: it existed, so nothing of it may widen the successor.
    /// 2. **A live row with no binding, after a predecessor ended** is refused.
    ///    That is the signature of an edge release whose tombstone write did
    ///    not complete (A.2): the predecessor ended, the release did not, and
    ///    the row is diagnostic evidence rather than a ceiling any successor
    ///    may consume (N-15). Reported as [`PanelError::CeilingLost`], the same
    ///    class D.2 gives the lost-record refusal, and the row is left
    ///    untouched.
    /// 3. **A tombstone that names the record's last controller generation**
    ///    is the reload ceiling — the ordinary case, because generation `N` is
    ///    disposed before `N+1` binds. A tombstone naming any other generation
    ///    is a lost record and is refused the same way.
    /// 4. **No predecessor at all** — a panel that has never been bound —
    ///    yields an absent ceiling (`None`), not an empty one: the first bind
    ///    commits the whole resolution because `panel.create` is the
    ///    authorization. A never-bound panel's allocation-epoch row is not a
    ///    predecessor either; the successor's own resolution is `R`, and the
    ///    epoch row becomes unreadable once the binding names a controller
    ///    generation (it is swept by the next edge release).
    ///
    /// Phase 0 stamps the snapshot with `attempt` so a caller can prove which
    /// bind a ceiling belongs to. An empty tombstone is a real empty ceiling
    /// (PGO-22(c)); only a missing source is refused.
    ///
    /// # Errors
    /// `RegistryDisposed`, `NotFound` (unknown or disposed panel), or
    /// `CeilingLost` when the predecessor's ceiling record is unusable.
    pub fn capture_rebind_source(
        &self,
        panel_id: PanelId,
        attempt: Generation,
    ) -> Result<RebindCapture, PanelError> {
        self.ensure_not_disposed()?;
        let record = self.panels.get(&panel_id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: panel_id.0,
        })?;
        if record.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            });
        }
        let bound = record.controller_generation();
        let last = record.last_controller_generation;
        let last_owner = record.last_controller_owner.clone();
        let live_row = self
            .capabilities
            .get(&(panel_id, record.grant_key_generation()));

        let (predecessor_generation, predecessor_owner, ceiling) = if let Some(predecessor) = bound
        {
            (
                Some(predecessor),
                record.owner().cloned(),
                Some(live_row.cloned().unwrap_or_default()),
            )
        } else if live_row.is_some() && last.is_some() {
            return Err(PanelError::CeilingLost {
                panel_id,
                bind_generation: attempt,
                last_controller_generation: last,
                tombstone_generation: self
                    .retained_grant_ceiling(panel_id)
                    .map(|ceiling| ceiling.ended_generation),
            });
        } else if let Some(ended) = last {
            let tombstone = self.retained_grant_ceiling(panel_id);
            let Some(tombstone) = tombstone else {
                return Err(PanelError::CeilingLost {
                    panel_id,
                    bind_generation: attempt,
                    last_controller_generation: Some(ended),
                    tombstone_generation: None,
                });
            };
            // OQ-084 A.2/`N-13`: the tombstone is this predecessor's ceiling
            // only when the pair it was written for matches the pair the record
            // names. A generation match under another owner is a foreign
            // plugin's ceiling — two plugins can carry the same controller
            // generation number — so it is refused exactly like a missing one
            // (PGO-22(d)) rather than consumed.
            if tombstone.ended_generation != ended || tombstone.owner != last_owner {
                return Err(PanelError::CeilingLost {
                    panel_id,
                    bind_generation: attempt,
                    last_controller_generation: Some(ended),
                    tombstone_generation: Some(tombstone.ended_generation),
                });
            }
            (
                Some(ended),
                last_owner.clone(),
                Some(tombstone.grants.clone()),
            )
        } else {
            (None, None, None)
        };

        Ok(RebindCapture {
            panel_id,
            attempt,
            predecessor_generation,
            predecessor_owner,
            ceiling,
        })
    }

    /// The one derivation rule (OQ-084 D.1) as a pure function of a captured
    /// ceiling: `(R ∩ P) ∪ C`.
    ///
    /// `P` can only subtract. An element of `P \ R` is dropped with no prompt,
    /// and an element of `R \ P` needs an explicit consent for that capability
    /// in `consented` — the predecessor's absence is never permission. With no
    /// predecessor (`None`) the whole resolution is granted.
    fn derive_grant_set(
        ceiling: Option<&BTreeSet<String>>,
        requested: &BTreeSet<String>,
        consented: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let Some(ceiling) = ceiling else {
            return requested.clone();
        };
        let mut grants: BTreeSet<String> = requested.intersection(ceiling).cloned().collect();
        for capability in consented {
            if requested.contains(capability) && !ceiling.contains(capability) {
                grants.insert(capability.clone());
            }
        }
        grants
    }

    /// Derives a successor's panel-scoped grant set (OQ-084 D.1, D.2 phase 0).
    ///
    /// `grant(n+1) = (R ∩ P) ∪ C`, where `R` is `requested`, `P` is the
    /// predecessor ceiling, and `C` is the subset of `consented` that lies in
    /// `R \ P`. The ceiling comes from [`Self::capture_rebind_source`], which
    /// prefers the live row when the predecessor is still bound, then the
    /// retired-ceiling tombstone written at the predecessor's edge. With no
    /// predecessor the whole resolution is granted with no permission-diff
    /// consent: an absent predecessor is not an empty ceiling.
    ///
    /// `generation` is the handle's allocation epoch, the same fence every
    /// other public panel entry takes. This is the pre-controller-generation
    /// seam: it derives and returns the set without installing it, and a
    /// production bind uses [`Self::rebind_panel`] instead, because a bind must
    /// derive from its phase 0 snapshot rather than re-entering the capture
    /// (D.1) and must not be able to observe a half-committed phase 2.
    ///
    /// # Errors
    /// `RegistryDisposed`, `NotFound`, `StaleHandle`, or `CeilingLost` when the
    /// previous controller generation has no usable ceiling record.
    pub fn derive_rebind_grants(
        &self,
        panel_id: PanelId,
        generation: Generation,
        requested: &BTreeSet<String>,
        consented: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, PanelError> {
        self.get_panel(panel_id, generation)?;
        let capture = self.capture_rebind_source(panel_id, generation)?;
        Ok(Self::derive_grant_set(
            capture.ceiling(),
            requested,
            consented,
        ))
    }

    /// Commits a rebind's derived grant set (OQ-084 D.2 phase 2 step 1).
    ///
    /// The set is written once, all-or-nothing: every element is validated
    /// before any is installed. This is step 1 only — the retired-ceiling
    /// tombstone is retired by phase 2 step 3
    /// ([`Self::retire_rebound_ceiling`]), which runs after the binding is
    /// written, so a bind that commits grants and then fails cannot lose the
    /// ceiling its own retry needs. A caller that installs without binding a
    /// controller keeps the tombstone.
    ///
    /// # Errors
    /// `NotFound`, `StaleHandle`, `CapabilityDenied` (nothing installed),
    /// `CeilingLost` (nothing installed and no binding written), or
    /// `RegistryDisposed`.
    pub fn install_rebound_grants(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        requested: &BTreeSet<String>,
        consented: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        let grants = match self.derive_rebind_grants(panel_id, generation, requested, consented) {
            Ok(grants) => grants,
            Err(error) => {
                if matches!(error, PanelError::CeilingLost { .. }) {
                    self.bump_error("CeilingLost");
                }
                return Err(error);
            }
        };
        for capability in &grants {
            Self::check_panel_capability(panel_id, capability)?;
        }
        self.capabilities
            .insert((panel_id, generation), grants.clone());
        Ok(grants)
    }

    /// Moves a mounted panel's controller binding from its current generation
    /// to `successor` (OQ-084 D.2, bitty#2476 residual 2).
    ///
    /// This is the production bind. It is a pure function of one capture: phase
    /// 0 reads the predecessor ceiling and stamps a snapshot, the derivation
    /// runs against that snapshot, and phase 2 writes exactly that result once.
    /// Nothing in this call re-reads the live grant table after phase 0.
    ///
    /// **Entry fences** run before the capture and before any mutation, so a
    /// stale or conflicting attempt changes nothing:
    ///
    /// - the allocation epoch is never a controller generation (B.1/B.2), so a
    ///   bind that presents it is refused as [`PanelError::ControllerConflict`];
    /// - the generation the record already binds is not a move, and is refused
    ///   as [`PanelError::ControllerConflict`] as well (PGO-8: at most one
    ///   controller is ever recorded);
    /// - a generation whose own binding already ended — the record's retained
    ///   `last_controller_generation`, whether or not a successor is bound — is
    ///   refused as [`PanelError::ControllerReleased`], so a caller cannot
    ///   resurrect a retired generation (`N -> N+1 -> N`);
    /// - a bind presenting a different owner than the predecessor's is refused
    ///   as [`PanelError::CeilingLost`] (A.2, `N-13`): the tombstone phase 0
    ///   would read was written for another plugin, and cross-plugin adoption is
    ///   undecided (B.6).
    ///
    /// **Phase 0** is [`Self::capture_rebind_source`] plus the derivation and
    /// the capability validation of the derived set. Validating here, before
    /// the predecessor is invalidated, is what makes a refused bind leave the
    /// prior valid state intact: phase 2 step 1 then has nothing left that can
    /// fail, so no caller can observe a panel whose predecessor was released
    /// and whose successor holds an empty or partial set.
    ///
    /// **Phase 1** invalidates the predecessor in D.2's five-step order
    /// (§D.2 "Two-phase bind" of
    /// `bitty-terminal-docs/specifications/panel-generation-ownership-decision.md`,
    /// the draft decision record; bitty#2476 Decision D.2 is the same text).
    /// **Three of the five steps fuse at this seam.** The other two are owned
    /// by the plugin runtime's generation edge, which this registry cannot
    /// reach; they are named below rather than faked with a no-op:
    ///
    /// 1. *Here.* The retired-ceiling tombstone (written before the live row is
    ///    removed, so a failed write leaves the row in place), then the live
    ///    row keyed by the displaced generation.
    /// 2. *Here.* The panel's queued events are cleared.
    /// 3. *Not here.* The predecessor's capture handles and its live overlay
    ///    capture session are released by `PluginServices::clear_ui_blocks`
    ///    (`plugin_runtime/services.rs`: the generation's UI blocks, spec
    ///    overlay handles, and terminal-slot scenes, the last through
    ///    `SceneHost::release_owner`) and by `OverlayCapture::revoke_plugin`
    ///    (`plugin_runtime/overlay.rs`: the input capture session).
    ///    `PluginRuntime::suspend_inner`, `dispose_inner`, and `rollback` are
    ///    those callers, and `reload_inner` runs `dispose_inner` before it
    ///    activates a successor, so the reload path already discharged this
    ///    step before any bind can run. D.4 asks the bind to replay none of it;
    ///    the bind also cannot perform it.
    /// 4. *Here.* Attributed overlays are dismissed — the generation-scoped
    ///    pass inside the release, then a container-wide pass, because a writer
    ///    that ran while no controller was bound attributed its surface under
    ///    the allocation epoch, which the generation pass never matches. This
    ///    is the only phase-1 overlay family this registry owns.
    /// 5. *Not here.* In-flight panel verbs are cancelled. This registry holds
    ///    no verb queue: panel verbs are synchronous and
    ///    `panel_state_at_controller_fence` revalidates at use, so a verb that
    ///    still reaches the registry after the swap presents the predecessor
    ///    generation and is refused [`PanelError::ControllerReleased`]. The
    ///    predecessor's parked command is cancelled by
    ///    `PluginRuntime::cancel_parked_command`, again at the plugin runtime's
    ///    generation edge.
    ///
    /// Steps 1, 2, 4 and the panel-command release are one fail-closed call to
    /// [`Self::release_generation_scope`], which is also idempotent — on the
    /// reload path the generation edge already ran and this repeats it as
    /// defence in depth, answering `false`.
    ///
    /// **Callers.** The plugin runtime cannot reach this registry (the panel
    /// host is app-owned), so the caller is the app-owned port it installs:
    /// `PanelGenerationPort::bind_successor`
    /// (`crates/bitty-runtime/src/plugin_runtime/panel_generation.rs`),
    /// invoked from the plugin generation edge in `PluginRuntime::reload_inner`
    /// once the predecessor's `dispose_inner` returned and the successor
    /// activated. The composition root's implementation
    /// (`crates/bitty-terminal/src/plugin_runtime.rs`) reaches this bind through
    /// `PanelRuntime::rebind_panel`, so phase 2 step 3's two writes run in
    /// D.2's order.
    ///
    /// What the wiring can name is the container that already carries a
    /// binding: on the reload path the edge release ran first, so the port
    /// enumerates the panels whose record retained the predecessor pair
    /// ([`Self::panels_ended_at`]) or still binds it
    /// ([`Self::panels_bound_to`]). A container that has never bound a
    /// controller has no owner anywhere in the tree — `bitty.panel.create`
    /// hands the host a type string and nothing else — so the first bind of
    /// such a container is not part of this wiring (OQ-084 follow-up (e)).
    ///
    /// **Obligations on the wiring that installs this bind.** Steps 3 and 5
    /// are the reason residual 2 leaves work behind, so the activation-time
    /// swap must, in this order:
    ///
    /// 1. run the predecessor's `PluginRuntime::dispose_inner` (the reload
    ///    path) or `suspend_inner` first, which is what revokes the capture
    ///    session and quiesces its parked command (phase-1 steps 3 and 5);
    /// 2. call this bind only after that edge returned, through
    ///    `PanelRuntime::rebind_panel`, so phase 2 step 3's lease reset runs
    ///    before the tombstone drop;
    /// 3. pass the successor's own resolution as `requested` and only the
    ///    permission-diff consents granted for *this* binding as `consented`;
    /// 4. never bind a predecessor whose edge has not run yet: nothing later in
    ///    this call revokes a capture session, and the at-use fence is a
    ///    backstop for verbs, not a substitute for the revoke.
    ///
    /// **Phase 2** commits in D.2's order. Steps 1 and 2 are here: the derived
    /// set keyed by the *controller* generation (not the allocation epoch),
    /// then the binding as one `(owner, controller generation)` pair with the
    /// placeholder cleared. Step 3 — "reset the panel's lease entry to `Idle`,
    /// drop the panel's retired-ceiling tombstone ..., then and only then
    /// permit successor verbs" — straddles two owners: the lease table is
    /// `PanelRuntime`'s and the tombstone table is this registry's. So the
    /// commit stops after step 2; the facade resets the lease, then calls
    /// [`Self::retire_rebound_ceiling`], and returns only after both ran. That
    /// keeps D.2's order (lease, then tombstone) without writing across the
    /// ownership boundary. A caller that drives this registry directly must
    /// make the same two calls in the same order; until it does, the
    /// predecessor's tombstone is still retained.
    ///
    /// # Errors
    /// `RegistryDisposed`, `NotFound`, `ControllerConflict`,
    /// `ControllerReleased`, `CeilingLost`, `CapabilityDenied`, and the
    /// fail-closed `ResourceExhausted` of the tombstone write. Every one leaves
    /// the previous valid state intact: a still-bound predecessor stays bound
    /// on a phase 0 refusal, and a phase 1 refusal leaves the binding, the live
    /// row, and every released family in place.
    pub fn rebind_panel(
        &mut self,
        panel_id: PanelId,
        successor: Generation,
        owner: &bitty_plugin_host::PluginId,
        requested: &BTreeSet<String>,
        consented: &BTreeSet<String>,
    ) -> Result<PanelRebind, PanelError> {
        self.ensure_not_disposed()?;

        // ------------------------------------------------------------------
        // Entry fences — before the capture and before any mutation.
        // ------------------------------------------------------------------
        let (bound, last, epoch, predecessor_owner) = {
            let record = self.panels.get(&panel_id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            })?;
            if record.state == UiPanelState::Disposed {
                return Err(PanelError::NotFound {
                    kind: "panel",
                    id_raw: panel_id.0,
                });
            }
            let bound = record.controller_generation();
            // The predecessor's owner: the live binding while one is in force,
            // otherwise the owner retained at the edge. `None` only for a panel
            // that never had a controller (or whose edge ended the allocation
            // epoch), where there is no owner to compare.
            let predecessor_owner = if bound.is_some() {
                record.owner().cloned()
            } else {
                record.last_controller_owner.clone()
            };
            (
                bound,
                record.last_controller_generation,
                record.generation,
                predecessor_owner,
            )
        };
        if successor == epoch {
            // B.1/B.2: the allocation epoch names the container and never
            // moves; a controller generation is a different value by
            // construction, so presenting the epoch would collapse the two.
            self.bump_error("ControllerConflict");
            return Err(PanelError::ControllerConflict {
                panel_id,
                bound_generation: bound,
                presented_generation: successor,
            });
        }
        if bound == Some(successor) {
            // PGO-8: a bind never installs a second controller, and a rebind
            // to the binding already in force is not a move.
            self.bump_error("ControllerConflict");
            return Err(PanelError::ControllerConflict {
                panel_id,
                bound_generation: bound,
                presented_generation: successor,
            });
        }
        if last == Some(successor) {
            // A generation whose own binding already ended may not be
            // resurrected: its families were released and its ceiling belongs
            // to the successor, not back to it. The fence is not scoped to
            // `bound.is_none()`: `N -> N+1 -> N` presents `N` while `N+1` is
            // bound, and rebinding it would restore a generation whose scene,
            // grants, capture handles, and queued events phase 1 already
            // released.
            self.bump_error("ControllerReleased");
            return Err(PanelError::ControllerReleased {
                panel_id,
                released_generation: successor,
            });
        }
        if let Some(predecessor_owner) = predecessor_owner.as_ref() {
            if predecessor_owner != owner {
                // OQ-084 A.2: phase 0 accepts a tombstone only when the pair it
                // was written for matches the predecessor the record names, and
                // cross-plugin adoption is undecided (B.6). A bind presenting a
                // different owner than the predecessor's is therefore refused
                // as a lost record, before phase 1 invalidates anything, rather
                // than handed a ceiling another plugin's edge wrote.
                self.bump_error("CeilingLost");
                return Err(PanelError::CeilingLost {
                    panel_id,
                    bind_generation: successor,
                    last_controller_generation: last,
                    tombstone_generation: self
                        .retained_grant_ceiling(panel_id)
                        .map(|ceiling| ceiling.ended_generation),
                });
            }
        }

        // ------------------------------------------------------------------
        // Phase 0 — capture, then validate. Reads and mutates nothing.
        // ------------------------------------------------------------------
        let capture = match self.capture_rebind_source(panel_id, successor) {
            Ok(capture) => capture,
            Err(error) => {
                if matches!(error, PanelError::CeilingLost { .. }) {
                    self.bump_error("CeilingLost");
                }
                return Err(error);
            }
        };
        let grants = Self::derive_grant_set(capture.ceiling(), requested, consented);
        // Validate before invalidate: every element of the set phase 2 will
        // install is checked here, so the commit below cannot fail halfway.
        for capability in &grants {
            Self::check_panel_capability(panel_id, capability)?;
        }

        // ------------------------------------------------------------------
        // Phase 1 — invalidate the predecessor, in D.2's order.
        // ------------------------------------------------------------------
        if let Some(predecessor) = capture.predecessor_generation() {
            // Phase 1 steps 1, 2 and 4, plus the panel-command release, in one
            // fail-closed call. Steps 3 (capture handles, overlay capture
            // session) and 5 (in-flight verbs) are owned by the plugin
            // runtime's generation edge and have no registry table to act on;
            // the rustdoc above names them. A refusal here leaves the binding,
            // the live row, and every other family exactly as they were.
            self.release_generation_scope(panel_id, predecessor)?;
        }
        // Step 4, second pass: whatever attribution is still on the container.
        // The release above dismisses the predecessor *generation*'s overlays,
        // but the writers in the tree key the allocation epoch while no
        // controller generation is bound (`create_panel_overlay` fences on the
        // epoch), so a predecessor's surface can sit under a key the
        // generation pass never matches. At a bind the container changes hands:
        // D.3 says overlay attribution does not survive, and a queued modal
        // from the predecessor is never presented by the successor. This is
        // also what makes the first bind of a container that already carries an
        // attributed surface hand the successor an empty one.
        self.dismiss_attributed_overlays_matching(|owner| owner.0 == panel_id);

        // ------------------------------------------------------------------
        // Phase 2 — commit, in D.2's order. Nothing below can fail.
        // ------------------------------------------------------------------
        // Step 1: the derived set, keyed by the controller generation.
        self.capabilities
            .insert((panel_id, successor), grants.clone());
        // Step 2: the binding as one pair. The placeholder a predecessor's end
        // presented is cleared with it: a controller owns the content again.
        if let Some(record) = self.panels.get_mut(&panel_id.0) {
            record.binding = Some(PanelBinding {
                plugin_id: owner.clone(),
                controller_generation: successor,
            });
            record.placeholder = None;
        }
        // Step 3 is split by ownership: the lease entry is the facade's, this
        // registry owns the tombstone. The facade resets the lease first and
        // then calls `retire_rebound_ceiling`; this commit stops after step 2
        // so the two writes happen in D.2's order (see the rustdoc above).
        Ok(PanelRebind {
            panel_id,
            predecessor_generation: capture.predecessor_generation(),
            controller_generation: successor,
            grants,
        })
    }

    #[must_use]
    pub fn is_panel_capability_granted(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> bool {
        let Some(key) = self.resolved_grant_generation(panel_id, generation) else {
            return false;
        };
        self.capabilities
            .get(&(panel_id, key))
            .is_some_and(|set| set.contains(capability))
    }

    /// Checks capability and returns error if not granted (deny-by-default).
    pub fn require_panel_capability(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        if self.is_panel_capability_granted(panel_id, generation, capability) {
            Ok(())
        } else {
            Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            })
        }
    }

    // ------------------------------------------------------------------
    // Disposal of whole registry
    // ------------------------------------------------------------------

    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.panels.clear();
        self.panel_to_view.clear();
        self.view_to_panel.clear();
        self.workspace_panels.clear();
        self.focus_per_workspace.clear();
        self.command_registry = UiCommandRegistry::new();
        self.overlay_manager.clear();
        // Bus cleared via dropping? Recreate
        self.event_bus = PanelEventBus::new(BusDropPolicy::DropOldest);
        self.capabilities.clear();
        self.retained_ceilings.clear();
        self.release_records.clear();
        self.overlay_attribution.clear();
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        } else {
            self.registry_generation = Generation(u64::MAX);
        }
        self.disposed = true;
    }

    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    #[must_use]
    pub fn error_count(&self, variant: &str) -> u64 {
        self.errors.get(variant).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub fn set_generation_for_test(&mut self, generation: Generation) {
        self.registry_generation = generation;
    }

    /// Binds a controller generation on the panel record for tests (OQ-084 D.2
    /// phase 2 step 2).
    ///
    /// Phase 2 step 2 writes the binding as ONE pair, so the binder installs
    /// the owner and the controller generation in a single assignment: there is
    /// no way to reach a record holding one half of a binding.
    ///
    /// The phase 2 step 2 writer is `rebind_panel`, which the generation edge
    /// reaches through the panel-generation port for a container that already
    /// carries a binding. A container's first binding still has no live writer
    /// (`bitty.panel.create` carries no `PluginId` and no generation, OQ-084
    /// follow-up (e)), so this helper mints one here.
    #[cfg(test)]
    fn bind_controller_generation_for_test(
        &mut self,
        panel_id: PanelId,
        plugin: &str,
        controller: Generation,
    ) {
        if let Some(record) = self.panels.get_mut(&panel_id.0) {
            record.binding = Some(PanelBinding {
                plugin_id: bitty_plugin_host::PluginId::new(plugin).expect("valid plugin id"),
                controller_generation: controller,
            });
        }
    }

    #[must_use]
    pub fn total_created(&self) -> u64 {
        self.total_created
    }
}

/// OQ-084 C.9: the workspace sweep's failure semantics.
///
/// C.9 requires a refused disposal to leave the previous valid state intact and
/// not to skip the remaining panels silently. The sweep is fail-fast and
/// resumable: the refusal is reported with the panels it did not reach, and
/// re-running the sweep after the cause clears disposes exactly those.
///
/// A module test because the refusal has to be reached through the dispose
/// guard: `PanelRegistry::dispose` is the only public way to set it and it
/// empties the very bucket the sweep walks, which would leave nothing to refuse.
#[cfg(test)]
mod workspace_dispose_tests {
    use super::*;

    /// The first refusal stops the sweep, names the rest, and changes nothing.
    #[test]
    fn a_refused_panel_stops_the_sweep_and_names_the_rest() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let ws = WorkspaceId::new(42);
        let first = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("first panel")
            .id;
        let second = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel")
            .id;
        let third = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("third panel")
            .id;

        // The guard `ensure_not_disposed` reads, set without the map clear a
        // public `dispose()` performs: the bucket still holds the three panels
        // the sweep has to refuse.
        reg.disposed = true;

        let report = reg.dispose_workspace_panels(ws);
        assert!(!report.is_complete(), "a refusal is never a complete close");
        let (refused, error) = report.refused().expect("the refusal is reported");
        assert_eq!(refused, first);
        assert!(matches!(error, PanelError::RegistryDisposed { .. }));
        assert_eq!(
            report.outcomes.len(),
            1,
            "the sweep stops at the refused panel instead of walking past it"
        );
        assert!(report.records.is_empty(), "nothing was disposed");
        assert_eq!(
            report.remaining,
            vec![second, third],
            "the panels the sweep never attempted are named, not silently skipped"
        );
        // Previous valid state intact.
        assert_eq!(reg.panel_count(), 3);
        assert_eq!(reg.panels_for_workspace(ws), vec![first, second, third]);

        // Resumable: with the registry usable again, exactly the refused panel
        // and the remainder go.
        reg.disposed = false;
        let retry = reg.dispose_workspace_panels(ws);
        assert!(retry.is_complete());
        assert!(retry.remaining.is_empty());
        assert_eq!(retry.disposed(), vec![first, second, third]);
        assert_eq!(retry.records.len(), 3);
        assert_eq!(reg.panel_count(), 0);
    }

    /// A sweep over a workspace with no panels is complete and writes nothing.
    #[test]
    fn an_empty_workspace_sweeps_to_nothing() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let report = reg.dispose_workspace_panels(WorkspaceId::new(77));
        assert!(report.is_complete());
        assert!(report.outcomes.is_empty());
        assert!(report.records.is_empty());
        assert!(report.remaining.is_empty());
    }
}

#[cfg(test)]
mod grant_ceiling_tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn ended_panel(grants: &[&str], view: u64) -> (PanelRegistry, PanelId, Generation) {
        let mut registry =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds");
        let handle = registry
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        registry
            .mount_panel(handle.id, handle.generation, ViewId::new(view))
            .expect("mount panel");
        for capability in grants {
            registry
                .grant_panel_capability(handle.id, handle.generation, capability)
                .expect("grant panel capability");
        }
        registry
            .release_generation_scope(handle.id, handle.generation)
            .expect("edge release");
        (registry, handle.id, handle.generation)
    }

    #[test]
    fn a_missing_tombstone_refuses_the_bind_and_keeps_the_record_unchanged() {
        let (mut registry, panel, generation) = ended_panel(&["panel.create"], 1);
        // Model the invariant violation the refusal guards: the record still
        // names the ended controller but the tombstone is gone.
        registry
            .retained_ceilings
            .retain(|(_, keyed_panel), _| *keyed_panel != panel);
        let registry_generation = registry.generation();
        let state = registry
            .panel_state(panel, generation)
            .expect("panel record survives the edge");

        let error = registry
            .install_rebound_grants(
                panel,
                generation,
                &set(&["panel.create", "panel.focus"]),
                &BTreeSet::new(),
            )
            .expect_err("a lost ceiling must refuse the bind");
        assert_eq!(
            error,
            PanelError::CeilingLost {
                panel_id: panel,
                bind_generation: generation,
                last_controller_generation: Some(generation),
                tombstone_generation: None,
            }
        );
        assert_eq!(
            registry.generation(),
            registry_generation,
            "a refused bind does not advance the registry generation"
        );
        assert_eq!(
            registry.panel_state(panel, generation).expect("record"),
            state,
            "a refused bind leaves the panel state unchanged"
        );
        assert!(
            !registry.is_panel_capability_granted(panel, generation, "panel.create"),
            "a refused bind installs nothing"
        );
        assert!(
            !registry.is_panel_capability_granted(panel, generation, "panel.focus"),
            "a refused bind does not grant the requested widening"
        );
        assert_eq!(
            registry.last_controller_generation(panel),
            Some(generation),
            "a refused bind does not rewrite the retained predecessor"
        );
        assert_eq!(registry.retained_grant_ceiling_count(), 0);
        assert_eq!(registry.error_count("CeilingLost"), 1);
    }

    #[test]
    fn a_tombstone_for_another_generation_is_refused_as_missing() {
        let (mut registry, panel, generation) = ended_panel(&["panel.create"], 2);
        let other = Generation(generation.get() + 1);
        let key = registry
            .retained_ceiling_key(panel)
            .expect("the edge retained a tombstone");
        if let Some(ceiling) = registry.retained_ceilings.get_mut(&key) {
            ceiling.ended_generation = other;
        }

        let error = registry
            .install_rebound_grants(panel, generation, &set(&["panel.create"]), &BTreeSet::new())
            .expect_err("a tombstone for another generation is not this panel's ceiling");
        assert_eq!(
            error,
            PanelError::CeilingLost {
                panel_id: panel,
                bind_generation: generation,
                last_controller_generation: Some(generation),
                tombstone_generation: Some(other),
            }
        );
        assert!(!registry.is_panel_capability_granted(panel, generation, "panel.create"));
        assert_eq!(registry.error_count("CeilingLost"), 1);
        assert!(
            registry.retained_grant_ceiling(panel).is_some(),
            "a mismatched tombstone is not consumed by the refused bind"
        );
    }

    /// PGO-22(d), owner half: the tombstone the record can read is the one
    /// stamped for the predecessor's owner. A tombstone under a foreign owner
    /// with the same generation number is refused exactly like a missing one —
    /// two plugins can carry the same controller generation (`N-13`).
    ///
    /// The public path cannot build this state (the table is keyed by the pair
    /// the edge stamps, and a cross-owner bind is refused before phase 0), so
    /// the invariant violation is constructed here; the reachable half is
    /// `tests/panel_generation_ownership.rs`.
    #[test]
    fn a_tombstone_stamped_for_another_owner_is_refused_as_missing() {
        let mut registry =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds");
        let handle = registry
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        registry
            .mount_panel(handle.id, handle.generation, ViewId::new(21))
            .expect("mount panel");
        let controller = handle.generation.next().expect("controller generation");
        registry
            .rebind_panel(
                handle.id,
                controller,
                &bitty_plugin_host::PluginId::new("acme.notes").expect("valid plugin id"),
                &set(&["panel.create"]),
                &BTreeSet::new(),
            )
            .expect("first bind");
        registry
            .release_generation_scope(handle.id, controller)
            .expect("generation edge");

        let key = registry
            .retained_ceiling_key(handle.id)
            .expect("the edge stamped a tombstone");
        let foreign = bitty_plugin_host::PluginId::new("evil.other").expect("valid plugin id");
        let ceiling = registry
            .retained_ceilings
            .remove(&key)
            .expect("the entry the edge wrote");
        registry.retained_ceilings.insert(
            (Some(foreign.clone()), handle.id),
            RetainedGrantCeiling {
                owner: Some(foreign),
                ..ceiling
            },
        );

        let error = registry
            .install_rebound_grants(
                handle.id,
                handle.generation,
                &set(&["panel.create"]),
                &BTreeSet::new(),
            )
            .expect_err("a tombstone for another owner is not this panel's ceiling");
        assert_eq!(
            error,
            PanelError::CeilingLost {
                panel_id: handle.id,
                bind_generation: handle.generation,
                last_controller_generation: Some(controller),
                tombstone_generation: Some(controller),
            },
            "the generation matches and the owner does not; only the owner half can refuse it"
        );
        assert!(
            !registry.is_panel_capability_granted(handle.id, handle.generation, "panel.create"),
            "a refused bind installs nothing"
        );
        assert!(
            registry.retained_grant_ceiling(handle.id).is_some(),
            "the foreign tombstone is not consumed either"
        );
    }

    #[test]
    fn a_full_tombstone_table_fails_closed_without_eviction() {
        let config = PanelRegistryConfig {
            max_panels_per_window: 2,
            ..PanelRegistryConfig::default()
        };
        let mut registry = PanelRegistry::new(config).expect("panel registry builds");
        let first = registry
            .create_panel(PanelType::Helper, None)
            .expect("create first panel");
        let second = registry
            .create_panel(PanelType::Helper, None)
            .expect("create second panel");
        registry
            .grant_panel_capability(second.id, second.generation, "panel.create")
            .expect("grant second panel");
        registry
            .release_generation_scope(second.id, second.generation)
            .expect("second edge release");
        // Fill the table with an entry whose record is gone. The public path
        // cannot produce this state (one entry per live panel and live panels
        // are bounded by the cap), so the guard is defense in depth.
        registry.retained_ceilings.insert(
            (None, PanelId::new(99)),
            RetainedGrantCeiling {
                owner: None,
                ended_generation: Generation(9),
                grants: set(&["panel.create"]),
            },
        );
        registry
            .mount_panel(first.id, first.generation, ViewId::new(100))
            .expect("mount first panel");
        registry
            .grant_panel_capability(first.id, first.generation, "panel.create")
            .expect("grant first panel");

        let error = registry
            .dispose_panel(first.id, first.generation)
            .expect_err("a full table must refuse the release");
        assert!(matches!(error, PanelError::ResourceExhausted { .. }));
        assert!(
            registry.is_panel_capability_granted(first.id, first.generation, "panel.create"),
            "the live grant row is kept when the tombstone cannot be written"
        );
        assert_eq!(
            registry
                .panel_state(first.id, first.generation)
                .expect("record"),
            PanelState::Mounted,
            "the failed release runs before any other teardown"
        );
        assert_eq!(
            registry
                .panel_view(first.id, first.generation)
                .expect("record"),
            Some(ViewId::new(100)),
            "the view attachment survives the fail-closed release"
        );
        assert_eq!(
            registry.last_controller_generation(first.id),
            None,
            "a failed release does not claim the controller ended"
        );
        assert_eq!(registry.error_count("RetiredCeilingTableFull"), 1);
    }

    #[test]
    fn a_new_tombstone_replaces_the_previous_entry_for_the_same_panel() {
        let (mut registry, panel, generation) = ended_panel(&["panel.create"], 3);
        assert_eq!(registry.retained_grant_ceiling_count(), 1);

        let newer = Generation(generation.get() + 1);
        registry
            .retain_grant_ceiling(panel, None, newer, set(&["panel.focus"]))
            .expect("replace the tombstone");
        assert_eq!(
            registry.retained_grant_ceiling_count(),
            1,
            "one entry per panel, never one per reload"
        );
        let ceiling = registry
            .retained_grant_ceiling(panel)
            .expect("the newer tombstone is stored");
        assert_eq!(ceiling.ended_generation, newer);
        assert_eq!(ceiling.grants, set(&["panel.focus"]));
    }
}

/// Phase 0 and phase 1 refusal tests for the production bind (OQ-084 D.2,
/// bitty#2476 residual 2).
///
/// A module test, because two of the states these rules guard cannot be
/// produced through the public surface: a stale tombstone beside a live binding,
/// and a tombstone table that is full for a panel the registry still admits.
/// Both are invariant violations the refusal exists to survive, so they are
/// built here deliberately rather than through a path that would have to be
/// wrong to reach them.
#[cfg(test)]
mod rebind_panel_tests {
    use super::*;

    const CREATE: &str = "panel.create";
    const FOCUS: &str = "panel.focus";
    const OVERLAY: &str = "panel.overlay";

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn owner(name: &str) -> bitty_plugin_host::PluginId {
        bitty_plugin_host::PluginId::new(name).expect("valid plugin id")
    }

    fn empty() -> BTreeSet<String> {
        BTreeSet::new()
    }

    /// Creates and mounts a panel, then performs its first bind (the
    /// `panel.create`-authorized one) onto a controller generation.
    fn mounted_and_bound(
        reg: &mut PanelRegistry,
        grants: &[&str],
    ) -> (PanelId, Generation, Generation) {
        let handle = reg
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        reg.mount_panel(handle.id, handle.generation, ViewId::new(1))
            .expect("mount panel");
        let controller = handle.generation.next().expect("controller generation");
        reg.rebind_panel(
            handle.id,
            controller,
            &owner("acme.notes"),
            &set(grants),
            &empty(),
        )
        .expect("first bind");
        (handle.id, handle.generation, controller)
    }

    /// `PGO-22(a)`: a still-bound predecessor is read from its live row even
    /// when an older tombstone is present, and the stale ceiling never widens
    /// the successor.
    #[test]
    fn the_live_row_wins_over_a_stale_tombstone() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let (panel, _epoch, controller) = mounted_and_bound(&mut reg, &[CREATE]);
        // The stale entry an earlier edge would have left. Only the private
        // table can hold one beside a live binding.
        reg.retain_grant_ceiling(
            panel,
            Some(&owner("acme.notes")),
            Generation(900),
            set(&[OVERLAY]),
        )
        .expect("stale tombstone");

        let successor = controller.next().expect("successor generation");
        let committed = reg
            .rebind_panel(
                panel,
                successor,
                &owner("acme.notes"),
                &set(&[CREATE, OVERLAY]),
                &empty(),
            )
            .expect("rebind");

        assert_eq!(
            committed.grants(),
            &set(&[CREATE]),
            "the live row is P; the stale tombstone is never consulted"
        );
        assert_eq!(committed.predecessor_generation(), Some(controller));
        assert_eq!(committed.controller_generation(), successor);
        assert!(
            !reg.is_panel_capability_granted(panel, successor, OVERLAY),
            "the stale ceiling does not widen the successor"
        );
        assert!(
            reg.retained_grant_ceiling(panel).is_some(),
            "the commit stops after step 2, so the tombstone survives it"
        );
        assert!(
            reg.retire_rebound_ceiling(panel),
            "step 3 retires the tombstone the bind found"
        );
        assert!(
            reg.retained_grant_ceiling(panel).is_none(),
            "a committed bind retires the tombstone it found"
        );
    }

    /// D.2 phase 2 step 3 is split from the registry commit so the facade can
    /// reset the lease first. The tombstone half is gated on a committed
    /// binding: a caller that has not written phase 2 step 2 cannot retire a
    /// ceiling a later bind still needs.
    #[test]
    fn the_ceiling_retirement_is_gated_on_the_committed_binding() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let handle = reg
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        let epoch = handle.generation;
        reg.retain_grant_ceiling(handle.id, None, epoch, set(&[CREATE]))
            .expect("tombstone");

        // No controller bound: step 2 has not run, so step 3 must not run.
        assert!(
            !reg.retire_rebound_ceiling(handle.id),
            "an unbound record cannot retire its ceiling"
        );
        assert!(
            reg.retained_grant_ceiling(handle.id).is_some(),
            "the ceiling a later bind needs is still there"
        );

        // The same call after the commit is the step-3 write.
        let controller = epoch.next().expect("controller generation");
        reg.rebind_panel(
            handle.id,
            controller,
            &owner("acme.notes"),
            &set(&[CREATE]),
            &empty(),
        )
        .expect("first bind");
        assert!(
            reg.retire_rebound_ceiling(handle.id),
            "a committed binding unlocks step 3"
        );
        assert!(reg.retained_grant_ceiling(handle.id).is_none());
    }

    /// `PGO-22(d)`: a panel that names a previous controller with no matching
    /// ceiling record refuses the bind before any phase-1 step.
    #[test]
    fn a_missing_ceiling_is_refused_before_any_invalidation() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let (panel, _epoch, controller) = mounted_and_bound(&mut reg, &[CREATE]);
        reg.release_generation_scope(panel, controller)
            .expect("generation edge");
        // The lost record: the edge ran, the tombstone is gone.
        reg.retained_ceilings
            .retain(|(_, keyed_panel), _| *keyed_panel != panel);

        let successor = controller.next().expect("successor generation");
        let error = reg
            .rebind_panel(
                panel,
                successor,
                &owner("acme.notes"),
                &set(&[CREATE, FOCUS]),
                &empty(),
            )
            .expect_err("a lost ceiling refuses the bind");

        assert_eq!(
            error,
            PanelError::CeilingLost {
                panel_id: panel,
                bind_generation: successor,
                last_controller_generation: Some(controller),
                tombstone_generation: None,
            }
        );
        assert_eq!(
            reg.controller_generation(panel),
            None,
            "a refused capture binds nothing"
        );
        assert!(
            !reg.is_panel_capability_granted(panel, successor, CREATE),
            "a refused bind installs nothing"
        );
        assert_eq!(
            reg.error_count("CeilingLost"),
            1,
            "the refusal is counted once"
        );
    }

    /// `N-15`/D.2 phase 0: a live row with no binding on the record — what a
    /// failed edge release leaves behind — is diagnostic evidence, never a
    /// ceiling a successor may consume.
    #[test]
    fn a_live_row_with_no_binding_is_refused_as_a_ceiling() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let (panel, epoch, controller) = mounted_and_bound(&mut reg, &[CREATE]);
        reg.release_generation_scope(panel, controller)
            .expect("generation edge");
        // The failed-release signature: a row the ended generation's writers
        // left under the allocation epoch. Reading it would hand the successor
        // a capability the predecessor never held.
        reg.capabilities.insert((panel, epoch), set(&[OVERLAY]));

        let successor = controller.next().expect("successor generation");
        let error = reg
            .rebind_panel(
                panel,
                successor,
                &owner("acme.notes"),
                &set(&[OVERLAY]),
                &empty(),
            )
            .expect_err("the orphan row is not a source");

        assert!(
            matches!(error, PanelError::CeilingLost { .. }),
            "the row is refused as a lost ceiling, got {error:?}"
        );
        assert_eq!(
            reg.capabilities.get(&(panel, epoch)),
            Some(&set(&[OVERLAY])),
            "the refused bind leaves the diagnostic row untouched"
        );
        assert!(
            !reg.is_panel_capability_granted(panel, successor, OVERLAY),
            "no successor row is installed"
        );
    }

    /// `PGO-16`, at the step that can fail today: the phase-1 tombstone write.
    /// The bind is refused whole, the predecessor stays bound, and no phase-2
    /// step runs.
    #[test]
    fn a_failed_invalidation_refuses_the_bind_and_keeps_the_predecessor_bound() {
        let config = PanelRegistryConfig {
            max_panels_per_window: 2,
            ..PanelRegistryConfig::default()
        };
        let mut reg = PanelRegistry::new(config).expect("registry");
        let (panel, _epoch, controller) = mounted_and_bound(&mut reg, &[CREATE]);
        // Fill the retired-ceiling table to its capacity with entries no live
        // record owns, so the release's fail-closed write cannot complete.
        reg.retain_grant_ceiling(PanelId::new(9_001), None, Generation(1), empty())
            .expect("padding entry");
        reg.retain_grant_ceiling(PanelId::new(9_002), None, Generation(2), empty())
            .expect("padding entry");

        let successor = controller.next().expect("successor generation");
        let error = reg
            .rebind_panel(
                panel,
                successor,
                &owner("acme.notes"),
                &set(&[CREATE, FOCUS]),
                &empty(),
            )
            .expect_err("the invalidation cannot complete");

        assert!(
            matches!(error, PanelError::ResourceExhausted { .. }),
            "the failed step is reported, got {error:?}"
        );
        assert_eq!(
            reg.controller_generation(panel),
            Some(controller),
            "a refused invalidation leaves the predecessor bound"
        );
        assert!(
            reg.is_panel_capability_granted(panel, controller, CREATE),
            "the predecessor keeps its live row"
        );
        assert!(
            !reg.is_panel_capability_granted(panel, successor, CREATE),
            "no successor row is installed"
        );
        assert_eq!(
            reg.unbind_release_record(panel),
            None,
            "the release did not complete, so it wrote no record"
        );
        assert_eq!(
            reg.error_count("RetiredCeilingTableFull"),
            1,
            "the fail-closed tombstone write is counted"
        );
    }

    /// D.2 phase 2: the installed row is keyed by the controller generation,
    /// never by the allocation epoch the handle carries.
    #[test]
    fn the_commit_is_keyed_by_the_controller_generation() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
        let (panel, epoch, controller) = mounted_and_bound(&mut reg, &[CREATE, FOCUS]);

        assert!(
            reg.is_panel_capability_granted(panel, controller, CREATE),
            "the successor resolves its own row"
        );
        assert!(
            !reg.is_panel_capability_granted(panel, epoch, CREATE),
            "the allocation epoch is stale once a controller is bound"
        );
        assert_eq!(reg.last_controller_generation(panel), None);

        let successor = controller.next().expect("successor generation");
        reg.rebind_panel(
            panel,
            successor,
            &owner("acme.notes"),
            &set(&[CREATE]),
            &empty(),
        )
        .expect("rebind");

        assert_eq!(
            reg.last_controller_generation(panel),
            Some(controller),
            "the displaced generation is retained as the next predecessor"
        );
        assert_eq!(reg.controller_generation(panel), Some(successor));
        assert!(
            !reg.is_panel_capability_granted(panel, controller, CREATE),
            "the predecessor's row is gone"
        );
    }
}

/// Shape tests for the controller binding (OQ-084 D.2 phase 2, bitty#2476
/// residual 1).
///
/// The record carries the shape; a container's first binding still has no live
/// writer (`bitty.panel.create` carries no `PluginId` and no generation, OQ-084
/// follow-up (e)), so these tests mint the binding directly and prove what the
/// grant table does once one exists.
#[cfg(test)]
mod controller_binding_shape_tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn a_panel() -> (PanelRegistry, PanelHandle) {
        let mut registry =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry builds");
        let handle = registry
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        (registry, handle)
    }

    #[test]
    fn the_allocation_epoch_and_the_controller_generation_are_separate_values() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        assert_eq!(
            registry.controller_generation(handle.id),
            None,
            "create mints a container, never a binding"
        );

        let controller = epoch.next().expect("controller generation");
        assert_ne!(controller, epoch, "the two generations can differ");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);

        assert_eq!(
            registry.controller_generation(handle.id),
            Some(controller),
            "the record names the bound controller"
        );
        assert!(
            registry.panel_state(handle.id, epoch).is_ok(),
            "a bind does not move the allocation epoch the handles carry"
        );
        assert_eq!(
            registry.last_controller_generation(handle.id),
            None,
            "a live binding is not a predecessor"
        );
    }

    #[test]
    fn a_grant_under_the_allocation_epoch_resolves_while_no_controller_is_bound() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant panel capability");

        assert_eq!(registry.controller_generation(handle.id), None);
        assert!(
            registry.is_panel_capability_granted(handle.id, epoch, "panel.create"),
            "with no controller bound the allocation epoch keys the row"
        );
        assert!(
            !registry.is_panel_capability_granted(
                handle.id,
                epoch.next().expect("other generation"),
                "panel.create"
            ),
            "a generation that is not the record's epoch reads no row"
        );
    }

    #[test]
    fn a_bound_controller_generation_keys_the_row_and_a_stale_one_reads_nothing() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant panel capability");

        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);
        // D.2 phase 2 step 1 installs the successor's set keyed by the
        // controller generation; that writer is bitty#2476 residual 2, so the
        // row is placed directly here.
        registry
            .capabilities
            .insert((handle.id, controller), set(&["panel.focus"]));

        assert!(
            registry.is_panel_capability_granted(handle.id, controller, "panel.focus"),
            "the controller generation resolves its own row"
        );
        assert!(
            !registry.is_panel_capability_granted(handle.id, controller, "panel.create"),
            "the controller does not inherit the allocation epoch's row"
        );
        assert!(
            !registry.is_panel_capability_granted(handle.id, epoch, "panel.create"),
            "the allocation epoch is stale once a controller is bound"
        );
        assert!(
            !registry.is_panel_capability_granted(handle.id, epoch, "panel.focus"),
            "a stale generation cannot read the controller's row"
        );
    }

    #[test]
    fn the_release_rechecks_the_binding_before_touching_a_row() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant panel capability");
        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);
        registry
            .capabilities
            .insert((handle.id, controller), set(&["panel.focus"]));

        let refused = registry
            .release_generation_scope(handle.id, epoch)
            .expect_err("the allocation epoch is not the binding generation");
        assert_eq!(
            refused,
            PanelError::StaleHandle {
                expected_generation: controller,
                found_generation: epoch,
                id_raw: handle.id.0,
            }
        );
        assert_eq!(registry.error_count("StaleHandle"), 1);
        assert!(
            registry.is_panel_capability_granted(handle.id, controller, "panel.focus"),
            "a refused release keeps the controller's row"
        );
        assert_eq!(registry.retained_grant_ceiling_count(), 0);
        assert_eq!(registry.unbind_release_count(handle.id), None);

        assert!(
            registry
                .release_generation_scope(handle.id, controller)
                .expect("the binding generation is released"),
            "the live row was removed"
        );
        assert_eq!(
            registry.last_controller_generation(handle.id),
            Some(controller)
        );
        let ceiling = registry
            .retained_grant_ceiling(handle.id)
            .expect("the controller's ceiling is retained");
        assert_eq!(ceiling.ended_generation, controller);
        assert_eq!(ceiling.grants, set(&["panel.focus"]));
        assert!(!registry.is_panel_capability_granted(handle.id, controller, "panel.focus"));
        assert!(
            !registry.is_panel_capability_granted(handle.id, epoch, "panel.create"),
            "the allocation-epoch row must not become the binding key again"
        );
        assert_eq!(
            registry
                .derive_rebind_grants(
                    handle.id,
                    epoch,
                    &set(&["panel.create", "panel.focus"]),
                    &BTreeSet::new(),
                )
                .expect("the retained tombstone is the predecessor"),
            set(&["panel.focus"]),
            "the successor's ceiling is the controller's tombstone, not the epoch row"
        );
        assert_eq!(registry.unbind_release_count(handle.id), Some(1));
    }

    /// OQ-084 A.2: `A.2`'s bind repeats the release in phase 1 as defence in
    /// depth, so a second release of a generation whose edge already ran is a
    /// normal call and must answer `false`, not `StaleHandle`. The first release
    /// clears the binding, which exposes the allocation epoch as the record's
    /// binding key — the repeat must be recognised from the tombstone before
    /// that key is compared with the presented generation.
    #[test]
    fn a_repeated_release_of_an_ended_controller_generation_is_a_no_op() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);
        registry
            .capabilities
            .insert((handle.id, controller), set(&["panel.focus"]));

        assert!(
            registry
                .release_generation_scope(handle.id, controller)
                .expect("the binding generation is released"),
            "the first release removed the live row"
        );
        let ceiling = registry
            .retained_grant_ceiling(handle.id)
            .expect("the controller's ceiling is retained")
            .grants
            .clone();
        assert_eq!(ceiling, set(&["panel.focus"]));

        assert!(
            !registry
                .release_generation_scope(handle.id, controller)
                .expect("the repeat of an ended edge is not a stale handle"),
            "the second release removed nothing"
        );
        assert_eq!(
            registry.error_count("StaleHandle"),
            0,
            "the repeat is a no-op, not a refusal"
        );
        assert_eq!(
            registry.last_controller_generation(handle.id),
            Some(controller),
            "the repeat leaves the ended generation on the record"
        );
        assert_eq!(
            registry
                .retained_grant_ceiling(handle.id)
                .expect("the tombstone survives the repeat")
                .grants
                .clone(),
            ceiling,
            "the repeat does not overwrite the retained ceiling"
        );
        assert_eq!(
            registry.unbind_release_count(handle.id),
            Some(1),
            "the repeat does not rewrite the release record"
        );
    }

    /// bitty#2476 residual 3 (OQ-084 D.2, "Two typed refusals, neither
    /// borrowed from `StaleHandle`"): a mount that names a generation the
    /// record's binding does not permit is refused with the typed class, not
    /// with the generic stale-handle error, and commits nothing.
    #[test]
    fn a_mount_from_a_generation_the_record_does_not_bind_is_a_controller_conflict() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        let bound = epoch.next().expect("controller generation");
        let foreign = bound.next().expect("foreign generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", bound);

        let refusal = registry
            .mount_panel(handle.id, foreign, ViewId::new(1))
            .expect_err("a generation the record does not bind cannot mount");
        assert_eq!(
            refusal,
            PanelError::ControllerConflict {
                panel_id: handle.id,
                bound_generation: Some(bound),
                presented_generation: foreign,
            },
            "D.2 asks for the typed conflict class here"
        );
        assert_eq!(
            refusal.to_string(),
            format!(
                "panel {} controller conflict: bound {bound}, presented {foreign}",
                handle.id
            )
        );
        assert_eq!(registry.error_count("ControllerConflict"), 1);
        assert_eq!(
            registry.error_count("StaleHandle"),
            0,
            "the refusal is not borrowed from StaleHandle"
        );
        // C.5: the mount is transactional, so the refused attempt leaves the
        // panel in its pre-mount state and binds no view.
        assert_eq!(
            registry.panel_state(handle.id, epoch).expect("record"),
            PanelState::Created
        );
        assert_eq!(registry.panel_view(handle.id, epoch).expect("record"), None);
        assert!(!registry.panel_to_view.contains_key(&handle.id));
        assert!(registry.view_to_panel.is_empty());
    }

    /// bitty#2476 residual 3 (OQ-084 D.2 phase 1 step 5, C.5): a verb in flight
    /// when its own generation was unbound is refused `ControllerReleased`, and
    /// the record no longer claims the controller the unbind edge released.
    #[test]
    fn a_mount_from_a_generation_whose_binding_was_released_is_controller_released() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        let bound = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", bound);
        registry
            .release_generation_scope(handle.id, bound)
            .expect("the binding generation is released");
        assert_eq!(
            registry.controller_generation(handle.id),
            None,
            "the unbind edge drops the binding, so the record does not claim a released controller"
        );
        assert_eq!(
            registry.last_controller_generation(handle.id),
            Some(bound),
            "the ended generation is what survives the edge"
        );

        let refusal = registry
            .mount_panel(handle.id, bound, ViewId::new(1))
            .expect_err("the released generation cannot mount");
        assert_eq!(
            refusal,
            PanelError::ControllerReleased {
                panel_id: handle.id,
                released_generation: bound,
            }
        );
        assert_eq!(
            refusal.to_string(),
            format!(
                "panel {} controller released: generation {bound} is unbound",
                handle.id
            )
        );
        assert_eq!(registry.error_count("ControllerReleased"), 1);
        assert_eq!(
            registry.error_count("ControllerConflict"),
            0,
            "an ended generation is released, not a conflict"
        );
        assert_eq!(registry.error_count("StaleHandle"), 0);
        assert_eq!(
            registry.panel_state(handle.id, epoch).expect("record"),
            PanelState::Created
        );
        assert!(registry.view_to_panel.is_empty());
    }

    /// The allocation epoch names the container and never moves (B.1/B.2), so an
    /// epoch that once ended is not a *released controller generation*: a verb
    /// presenting it against a foreign binding is a conflict, not a release.
    /// Without the epoch guard in the fence, the `ControllerReleased` arm would
    /// claim it, because the record's retained ended generation *is* the epoch.
    #[test]
    fn the_allocation_epoch_is_never_refused_as_a_released_controller() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .release_generation_scope(handle.id, epoch)
            .expect("the epoch's own edge release");
        let bound = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", bound);

        let refusal = registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect_err("the epoch is not the bound generation");
        assert_eq!(
            refusal,
            PanelError::ControllerConflict {
                panel_id: handle.id,
                bound_generation: Some(bound),
                presented_generation: epoch,
            },
            "the epoch is a container identity, never a released controller"
        );
        assert_eq!(registry.error_count("ControllerReleased"), 0);
    }

    /// The accepted side of the fence: while nothing binds, the allocation
    /// epoch mounts exactly as before, including after an edge release of that
    /// epoch — which retains `last_controller_generation` on every live path and
    /// must not turn into a release refusal (fail-open by construction,
    /// bitty#2476 residual 3).
    #[test]
    fn an_unbound_panel_mounts_through_its_allocation_epoch_exactly_as_before() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("an unbound panel mounts through its allocation epoch");
        assert_eq!(
            registry.panel_view(handle.id, epoch).expect("record"),
            Some(ViewId::new(1))
        );
        registry
            .unmount_panel(handle.id, epoch)
            .expect("unmount, back to Suspended");
        registry
            .mount_panel(handle.id, epoch, ViewId::new(2))
            .expect("a remount through the same epoch");
        registry
            .unmount_panel(handle.id, epoch)
            .expect("unmount before the edge release");

        registry
            .release_generation_scope(handle.id, epoch)
            .expect("the epoch's own edge release");
        assert_eq!(
            registry.last_controller_generation(handle.id),
            Some(epoch),
            "the live edge retains the epoch as the ended generation"
        );

        registry
            .mount_panel(handle.id, epoch, ViewId::new(3))
            .expect("the epoch is still the accepted key after its own release");
        assert_eq!(
            registry.panel_view(handle.id, epoch).expect("record"),
            Some(ViewId::new(3))
        );
        assert_eq!(
            registry.error_count("ControllerReleased") + registry.error_count("ControllerConflict"),
            0,
            "no live path is refused by a binding class"
        );
    }

    /// The bound controller generation is the key the fence accepts, and the
    /// allocation epoch stops being one once a controller is bound (A.3) —
    /// the same rule `resolved_grant_generation` already applies to grants.
    #[test]
    fn a_bound_controller_generation_mounts_and_the_allocation_epoch_no_longer_does() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        let bound = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", bound);

        registry
            .mount_panel(handle.id, bound, ViewId::new(1))
            .expect("the bound generation issues the mount");
        assert_eq!(
            registry.panel_view(handle.id, epoch).expect("record"),
            Some(ViewId::new(1)),
            "the mount is recorded on the container's own record"
        );
        assert_eq!(
            registry.panel_to_view.get(&handle.id),
            Some(&ViewId::new(1))
        );
        assert_eq!(
            registry.view_to_panel.get(&ViewId::new(1)),
            Some(&handle.id)
        );

        registry
            .unmount_panel(handle.id, epoch)
            .expect("unmount through the epoch");
        let refusal = registry
            .mount_panel(handle.id, epoch, ViewId::new(2))
            .expect_err("the allocation epoch is stale once a controller is bound");
        assert_eq!(
            refusal,
            PanelError::ControllerConflict {
                panel_id: handle.id,
                bound_generation: Some(bound),
                presented_generation: epoch,
            }
        );
        assert_eq!(registry.error_count("ControllerConflict"), 1);
    }

    #[test]
    fn no_live_path_writes_a_binding() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        let record = registry.panels.get(&handle.id.0).expect("panel record");
        assert!(record.owner().is_none(), "create writes no owner");
        assert!(
            record.controller_generation().is_none(),
            "create writes no controller binding"
        );

        registry
            .release_generation_scope(handle.id, epoch)
            .expect("edge release");
        let record = registry.panels.get(&handle.id.0).expect("panel record");
        assert!(
            record.owner().is_none(),
            "the edge release writes no owner either"
        );
        assert!(
            record.controller_generation().is_none(),
            "the edge release binds no controller either"
        );
    }

    #[test]
    fn owner_and_controller_generation_are_both_present_or_both_absent_on_every_live_path() {
        // One binding, so the two accessors can never disagree: half a binding
        // is unrepresentable, not merely unconstructed (D.2 phase 2 step 2).
        fn assert_pair(registry: &PanelRegistry, panel: PanelId, expected_owner: Option<&str>) {
            let record = registry.panels.get(&panel.0).expect("panel record");
            assert_eq!(
                record.owner().map(bitty_plugin_host::PluginId::as_str),
                expected_owner,
                "the owner half of the binding"
            );
            assert_eq!(
                record.owner().is_some(),
                record.controller_generation().is_some(),
                "an owner never exists without its controller generation, and vice versa"
            );
        }

        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        assert_pair(&registry, handle.id, None); // create_panel

        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        assert_pair(&registry, handle.id, None); // mount_panel

        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant panel capability");
        assert_pair(&registry, handle.id, None); // grant_panel_capability

        registry
            .release_generation_scope(handle.id, epoch)
            .expect("edge release");
        assert_pair(&registry, handle.id, None); // D.2 phase 1 unbind edge

        registry
            .install_rebound_grants(handle.id, epoch, &set(&["panel.create"]), &BTreeSet::new())
            .expect("rebind install");
        assert_pair(&registry, handle.id, None); // D.2 phase 2 step 1

        registry.bind_controller_generation_for_test(
            handle.id,
            "acme.notes",
            epoch.next().expect("controller generation"),
        );
        assert_pair(&registry, handle.id, Some("acme.notes")); // D.2 phase 2 step 2

        let controller = registry
            .controller_generation(handle.id)
            .expect("the binder names a controller");
        assert_eq!(
            registry.panels[&handle.id.0]
                .binding
                .as_ref()
                .map(|binding| binding.controller_generation),
            Some(controller),
            "the accessor and the value it derives from name one fact"
        );

        registry
            .dispose_panel(handle.id, epoch)
            .expect("the bound panel disposes through its allocation epoch");
        assert!(
            !registry.panels.contains_key(&handle.id.0),
            "a disposed record carries no half binding"
        );
    }

    #[test]
    fn an_owner_key_is_the_plugin_and_its_generation_together() {
        let markdown = PanelBinding {
            plugin_id: bitty_plugin_host::PluginId::new("xuepoo.markdown").expect("plugin id"),
            controller_generation: Generation(7),
        };
        let notes = PanelBinding {
            plugin_id: bitty_plugin_host::PluginId::new("acme.notes").expect("plugin id"),
            controller_generation: Generation(7),
        };
        assert_ne!(
            markdown, notes,
            "the same controller generation number under two plugins is two owners (A.2)"
        );
        assert_eq!(markdown.controller_generation, Generation(7));
        assert_eq!(markdown.plugin_id.as_str(), "xuepoo.markdown");
    }

    /// D-1: a bound panel disposes through the allocation epoch its handle
    /// carries, and the release runs under the binding generation.
    #[test]
    fn a_bound_controller_generation_disposes_under_its_binding_key() {
        // A capacity-1 tombstone table with one foreign entry makes the
        // release's fail-closed tombstone write the first thing a disposal of
        // the bound panel runs, which is what proves the release was reached
        // under the binding key rather than refused as `StaleHandle`.
        let config = PanelRegistryConfig {
            max_panels_per_window: 1,
            ..PanelRegistryConfig::default()
        };
        let mut registry = PanelRegistry::new(config).expect("panel registry builds");
        let handle = registry
            .create_panel(PanelType::Helper, None)
            .expect("create panel");
        let epoch = handle.generation;
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);
        // D.2 phase 2 step 1 installs the successor's set keyed by the
        // controller generation; that writer is bitty#2476 residual 2, so the
        // row is placed directly here.
        registry
            .capabilities
            .insert((handle.id, controller), set(&["panel.focus"]));
        // The writers in the tree key by the generation the handle carries,
        // which the fence restricts to the allocation epoch, so one panel can
        // hold a row and a surface under its epoch while the binding names a
        // controller generation (bitty#2476 residual 1).
        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant under the allocation epoch");
        let epoch_overlay = registry
            .create_panel_overlay(
                handle.id,
                epoch,
                UiOverlayKind::NonModal,
                UiRect::new(0, 0, 20, 10),
                "epoch surface",
                None,
            )
            .expect("overlay under the allocation epoch");
        registry.retained_ceilings.insert(
            (None, PanelId::new(99)),
            RetainedGrantCeiling {
                owner: None,
                ended_generation: Generation(9),
                grants: set(&["panel.create"]),
            },
        );

        let refused = registry
            .dispose_panel(handle.id, epoch)
            .expect_err("the bound release must reach the fail-closed tombstone write");
        assert!(matches!(refused, PanelError::ResourceExhausted { .. }));
        assert!(
            registry.is_panel_capability_granted(handle.id, controller, "panel.focus"),
            "a fail-closed release keeps the bound row"
        );
        assert!(
            registry.capabilities.contains_key(&(handle.id, epoch)),
            "a fail-closed release keeps the epoch's row too"
        );
        assert_eq!(
            registry.overlay_attribution_of(epoch_overlay),
            Some((handle.id, epoch)),
            "a fail-closed release dismisses no attributed overlay"
        );
        assert!(
            registry.panels.contains_key(&handle.id.0),
            "a failed release retires no record"
        );

        registry.retained_ceilings.remove(&(None, PanelId::new(99)));
        registry
            .dispose_panel(handle.id, epoch)
            .expect("a bound panel disposes through the allocation epoch it was created with");
        assert!(
            !registry.panels.contains_key(&handle.id.0),
            "the record is retired"
        );
        assert!(
            matches!(
                registry.panel_state(handle.id, epoch),
                Err(PanelError::NotFound { .. })
            ),
            "the disposed identity resolves no record"
        );
        assert!(
            registry.capabilities.is_empty(),
            "the binding generation's grant row is released and the epoch's row with it"
        );
        assert_eq!(
            registry.attributed_overlay_count(),
            0,
            "no overlay stays attributed to a disposed panel"
        );
        assert_eq!(
            registry.overlay_len(),
            0,
            "the epoch's surface is dismissed with the container"
        );
        assert_eq!(
            registry.retained_grant_ceiling_count(),
            0,
            "the ceiling written at the edge is dropped with the container"
        );
        assert_eq!(
            registry.unbind_release_count(handle.id),
            None,
            "the release record is dropped with the container"
        );
        assert_eq!(
            registry.error_count("StaleHandle"),
            0,
            "no caller presented a stale generation"
        );
    }

    /// D-1: the rolled-back-create branch must clear the row under the binding
    /// key, not the literal presented generation.
    #[test]
    fn a_bound_panel_that_never_mounted_disposes_without_leaking_a_grant_row() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);
        // A `Created` record holding an empty row under the bound key is not a
        // controller (it mounted nothing and granted nothing), so disposal
        // takes the rolled-back-create branch; the row still has to be removed
        // under the binding key rather than the allocation epoch.
        registry
            .capabilities
            .insert((handle.id, controller), BTreeSet::new());
        // The writers in the tree key by the generation the handle carries,
        // which the fence restricts to the allocation epoch, so the same
        // container can hold a row under that key too (bitty#2476 residual 1).
        registry
            .capabilities
            .insert((handle.id, epoch), BTreeSet::new());

        registry
            .dispose_panel(handle.id, epoch)
            .expect("a bound panel that never mounted disposes");
        assert!(
            registry.capabilities.is_empty(),
            "no grant row survives the container, whichever key it was written under"
        );
        assert!(
            !registry.panels.contains_key(&handle.id.0),
            "the record is retired"
        );
        assert_eq!(registry.retained_grant_ceiling_count(), 0);
    }

    /// D-3: disposal is fail-closed across every key the fences can mint. The
    /// release names the binding generation, but every writer in the tree keys
    /// by the generation the handle carries — the allocation epoch — so a
    /// disposed bound panel must not leave a grant row or an attributed
    /// overlay behind under that key (bitty#2476 residual 1).
    #[test]
    fn a_bound_panel_disposes_the_rows_and_overlays_its_allocation_epoch_owns() {
        let (mut registry, handle) = a_panel();
        let epoch = handle.generation;
        // The live path: grant and overlay through the handle's allocation
        // epoch, then mount, then bind a controller generation.
        registry
            .grant_panel_capability(handle.id, epoch, "panel.create")
            .expect("grant under the allocation epoch");
        let overlay = registry
            .create_panel_overlay(
                handle.id,
                epoch,
                UiOverlayKind::NonModal,
                UiRect::new(0, 0, 20, 10),
                "epoch surface",
                None,
            )
            .expect("overlay under the allocation epoch");
        registry
            .mount_panel(handle.id, epoch, ViewId::new(1))
            .expect("mount panel");
        let controller = epoch.next().expect("controller generation");
        registry.bind_controller_generation_for_test(handle.id, "acme.notes", controller);

        assert!(
            registry.capabilities.contains_key(&(handle.id, epoch)),
            "the fixture holds a row under the epoch the release does not name"
        );
        assert_eq!(
            registry.overlay_attribution_of(overlay),
            Some((handle.id, epoch)),
            "the fixture holds a surface under the epoch the release does not name"
        );

        registry
            .dispose_panel(handle.id, epoch)
            .expect("a bound panel disposes through its allocation epoch");

        assert!(
            !registry.panels.contains_key(&handle.id.0),
            "the record is retired"
        );
        assert!(
            registry.capabilities.is_empty(),
            "no grant row outlives the container, under the epoch or the binding key"
        );
        assert_eq!(
            registry.attributed_overlay_count(),
            0,
            "no overlay stays attributed to a disposed panel"
        );
        assert_eq!(
            registry.overlay_len(),
            0,
            "an overlay attributed to the epoch is dismissed with the container"
        );
        assert_eq!(
            registry.error_count("StaleHandle"),
            0,
            "no caller presented a stale generation"
        );
    }
}

// ---------------------------------------------------------------------------
// Unbound / placeholder panel state (OQ-084 Decision E, bitty#2476 residual 4)
// ---------------------------------------------------------------------------
//
// Each test names the wrong implementation it is written to fail, per the
// record's Decision F practice. All of these are unreachable from a production
// caller today (the generation boundary is residual 5), so they drive the seam
// directly.

#[cfg(test)]
mod placeholder_state_tests {
    use super::*;

    const OWNER: &str = "acme.notes";

    fn registry() -> PanelRegistry {
        PanelRegistry::new(PanelRegistryConfig::default()).expect("default panel registry")
    }

    /// A mounted panel in workspace 1 at view 10.
    fn mounted_panel(reg: &mut PanelRegistry) -> (PanelHandle, WorkspaceId) {
        let workspace = WorkspaceId::new(1);
        let handle = reg
            .create_panel(PanelType::Rich, Some(workspace))
            .expect("create panel");
        reg.mount_panel(handle.id, handle.generation, ViewId::new(10))
            .expect("mount panel");
        (handle, workspace)
    }

    /// C.3/E: a crash keeps the container, its state, its attachment, and its
    /// focus history; it clears the binding through the unbind edge and presents
    /// the placeholder. Fails for an implementation that disposes the container
    /// on crash (the automatic deletion E forbids), that drops the attachment,
    /// that moves focus, or that leaves the binding claiming the crashed
    /// generation.
    #[test]
    fn a_crash_presents_the_placeholder_and_keeps_the_container() {
        let mut reg = registry();
        let (handle, workspace) = mounted_panel(&mut reg);
        reg.focus_panel(handle.id, handle.generation, workspace)
            .expect("focus");
        let controller = handle.generation;
        reg.bind_controller_generation_for_test(handle.id, OWNER, controller);

        let presented = reg
            .present_placeholder(handle.id, controller, "crashed")
            .expect("placeholder");

        assert_eq!(
            presented.outcome,
            RecoveryOutcome::Placeholder(PlaceholderReason::Crashed)
        );
        assert!(!presented.saturated);
        let placeholder = presented.placeholder.expect("placeholder on the record");
        assert_eq!(placeholder.reason(), PlaceholderReason::Crashed);
        assert_eq!(placeholder.unbound_generation(), Some(controller));

        // Container retained: state, workspace membership, view, focus.
        assert_eq!(
            reg.panel_state(handle.id, handle.generation)
                .expect("state readable"),
            PanelState::Focused
        );
        assert_eq!(
            reg.panel_view(handle.id, handle.generation)
                .expect("view readable"),
            Some(ViewId::new(10))
        );
        assert_eq!(reg.focused_panel(workspace), Some(handle.id));

        // Controller unbound, and the ended generation is what the fence keeps.
        assert_eq!(reg.controller_generation(handle.id), None);
        assert_eq!(reg.last_controller_generation(handle.id), Some(controller));
        // The unbind edge wrote the tombstone before dropping the row.
        assert_eq!(
            reg.retained_grant_ceiling(handle.id)
                .map(|ceiling| ceiling.ended_generation),
            Some(controller)
        );

        // One FS-4-style record: owner, generation, reason, action.
        let record = presented.record.expect("one record per transition");
        assert_eq!(record.panel, handle.id);
        assert_eq!(
            record.owner.as_deref(),
            Some(OWNER),
            "the owner is the binding's plugin id, not a caller string"
        );
        assert_eq!(record.controller_generation, controller.get());
        assert_eq!(record.reason, RecoveryReason::Crashed);
        assert_eq!(record.action, RecoveryAction::Placeholder);
        assert_eq!(record.attempt, 1);
        assert_eq!(
            reg.recovery_attempts(handle.id, controller, PlaceholderReason::Crashed),
            1
        );
        assert_eq!(reg.recovery_records(handle.id).len(), 1);
    }

    /// Decision E / PGO-23 through the *cap* path: a panel whose counter for the
    /// key is already at the cap, whose placeholder a rebind cleared, and which
    /// is then presented the same key again. The counter answers `Unchanged`
    /// (no record), so a seam that wrote the placeholder only when a record came
    /// back would leave the container unbound with nothing to present — the same
    /// silent orphan, reached without the key table.
    #[test]
    fn a_saturated_counter_still_presents_the_placeholder_after_a_rebind() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        let epoch = handle.generation;
        for _ in 0..=RECOVERY_ATTEMPT_CAP {
            reg.present_placeholder(handle.id, epoch, "disabled")
                .expect("placeholder");
        }
        assert_eq!(reg.recovery_saturated_for(handle.id), Some(epoch));

        // A rebind clears the placeholder but not the counter.
        reg.unmount_panel(handle.id, epoch).expect("unmount");
        reg.mount_panel(handle.id, epoch, ViewId::new(13))
            .expect("remount");
        assert!(
            reg.placeholder(handle.id).is_none(),
            "the rebind cleared it"
        );

        let presented = reg
            .present_placeholder(handle.id, epoch, "disabled")
            .expect("the transition still presents");
        assert!(
            presented.record.is_none(),
            "the counter is at its cap: nothing new is counted"
        );
        assert!(
            presented.placeholder.is_some(),
            "the placeholder is written even when the counter emits no record"
        );
        assert_eq!(
            reg.placeholder_reason(handle.id),
            Some(PlaceholderReason::Disabled)
        );
        assert!(
            reg.placeholder_block(handle.id)
                .expect("the fixed table is admitted")
                .is_some(),
            "the Core surface is presentable"
        );
    }

    /// FS-4: the record's owner is the panel record's own binding owner, never a
    /// caller-asserted string. `None` is the honest value while no controller is
    /// bound; `Some` is the plugin the binding named, read before the unbind edge
    /// cleared it. Fails for an implementation that stamps a caller's argument
    /// onto the record (which let any caller attribute a transition to a plugin
    /// the panel never bound, and reported an owner for a container that had
    /// none).
    #[test]
    fn the_record_owner_is_read_from_the_panel_binding() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        let epoch = handle.generation;

        // No binding: no owner to name.
        let unloaded = reg
            .present_placeholder(handle.id, epoch, "unloaded")
            .expect("placeholder");
        assert_eq!(
            unloaded.record.expect("one record").owner,
            None,
            "a container that never bound a controller names no owner"
        );

        // Bound: the owner is the binding's plugin id, and it survives the
        // unbind edge that clears the binding before the caller reads it.
        let controller = Generation(epoch.get() + 5);
        reg.bind_controller_generation_for_test(handle.id, OWNER, controller);
        let crashed = reg
            .present_placeholder(handle.id, controller, "crashed")
            .expect("placeholder");
        assert_eq!(
            reg.controller_generation(handle.id),
            None,
            "the edge cleared the binding before the record was read"
        );
        assert_eq!(
            crashed.record.expect("one record").owner.as_deref(),
            Some(OWNER),
            "the owner comes from the binding the transition released"
        );
        // The retained history carries the same owner.
        let retained = reg.recovery_records(handle.id);
        assert_eq!(
            retained
                .last()
                .and_then(|record| record.owner.clone())
                .as_deref(),
            Some(OWNER)
        );
    }

    /// E.4: a missing plugin is `unloaded`, not deleted. The container stays,
    /// nothing is released (there is no predecessor generation), and a later
    /// reinstall can still rebind. Fails for an implementation that invents an
    /// unbind edge (writing a ceiling for a controller that never existed) or
    /// that disposes the panel.
    #[test]
    fn a_missing_plugin_names_unloaded_without_an_unbind_edge() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);

        let presented = reg
            .present_placeholder(handle.id, handle.generation, "unloaded")
            .expect("placeholder");

        let placeholder = presented.placeholder.expect("placeholder on the record");
        assert_eq!(placeholder.reason(), PlaceholderReason::Unloaded);
        assert_eq!(
            placeholder.unbound_generation(),
            None,
            "a container that never had a controller names none"
        );
        assert!(
            reg.retained_grant_ceiling(handle.id).is_none(),
            "no predecessor, so no retained ceiling"
        );
        assert_eq!(
            reg.unbind_release_count(handle.id),
            None,
            "the release edge must not run for a never-bound panel"
        );
        assert_eq!(
            reg.panel_view(handle.id, handle.generation)
                .expect("view readable"),
            Some(ViewId::new(10)),
            "the placeholder keeps the attachment"
        );
    }

    /// E's table: a denied capability selects `Unavailable`, which keeps the
    /// container, names the denial, and is still a bounded recovery attempt.
    #[test]
    fn a_denied_capability_presents_unavailable() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);

        let presented = reg
            .present_placeholder(handle.id, handle.generation, "denied")
            .expect("unavailable");

        assert_eq!(presented.outcome, RecoveryOutcome::Unavailable);
        let placeholder = presented.placeholder.expect("placeholder on the record");
        assert_eq!(placeholder.reason(), PlaceholderReason::Denied);
        let record = presented.record.expect("one record");
        assert_eq!(record.reason, RecoveryReason::Denied);
        assert_eq!(record.action, RecoveryAction::Unavailable);
        assert_eq!(
            reg.recovery_attempts(handle.id, handle.generation, PlaceholderReason::Denied),
            1
        );
    }

    /// E.1: eight attempts are counted, the ninth changes nothing and emits the
    /// one `saturated` note for the binding, and every later attempt emits
    /// nothing at all. Fails for an unbounded counter, for a per-attempt
    /// diagnostic, for a per-attempt record list, and for an implementation that
    /// lets the ninth attempt replace the placeholder reason.
    #[test]
    fn the_counter_stops_at_eight_and_emits_one_saturated_note() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        let epoch = handle.generation;
        let mut counted = 0u8;

        for attempt in 1..=RECOVERY_ATTEMPT_CAP {
            let presented = reg
                .present_placeholder(handle.id, epoch, "disabled")
                .expect("placeholder");
            let record = presented.record.expect("counted transition");
            assert_eq!(record.attempt, attempt);
            assert!(!presented.saturated);
            counted = attempt;
        }
        assert_eq!(counted, RECOVERY_ATTEMPT_CAP);
        assert_eq!(
            reg.panel_state(handle.id, epoch).expect("state readable"),
            PanelState::Mounted
        );

        let ninth = reg
            .present_placeholder(handle.id, epoch, "disabled")
            .expect("saturated note");
        assert!(ninth.saturated, "the ninth attempt emits the note");
        assert!(
            ninth.record.is_none(),
            "the ninth attempt is not counted and is not recorded"
        );
        assert_eq!(
            ninth.placeholder.expect("placeholder kept").reason(),
            PlaceholderReason::Disabled
        );
        assert_eq!(
            reg.recovery_attempts(handle.id, epoch, PlaceholderReason::Disabled),
            RECOVERY_ATTEMPT_CAP
        );
        assert_eq!(reg.recovery_saturated_for(handle.id), Some(epoch));

        let tenth = reg
            .present_placeholder(handle.id, epoch, "disabled")
            .expect("no-op attempt");
        assert!(!tenth.saturated && tenth.record.is_none());
        assert_eq!(reg.recovery_records(handle.id).len(), 8);
    }

    /// Decision E / PGO-23: a panel that reaches the placeholder path always
    /// leaves a placeholder, even when its per-panel key table is already full.
    /// Fails for an implementation that answers a full table with `Saturated`
    /// and returns `Ok` without writing the placeholder: the generation is
    /// released, the binding is cleared, and the retained container is left
    /// with nothing to present — the silent orphan this record forbids.
    #[test]
    fn a_full_key_table_still_presents_the_placeholder() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        let epoch = handle.generation;

        // Eight distinct `(generation, reason)` keys, each presented by a bound
        // controller. The per-panel key table is full afterwards.
        for step in 1..=MAX_RECOVERY_KEYS_PER_PANEL as u64 {
            let controller = Generation(epoch.get() + step);
            reg.bind_controller_generation_for_test(handle.id, OWNER, controller);
            reg.present_placeholder(handle.id, controller, "crashed")
                .expect("placeholder");
        }
        assert_eq!(
            reg.recovery_keys(handle.id),
            MAX_RECOVERY_KEYS_PER_PANEL,
            "the fixture fills the key table"
        );

        // A rebind clears the placeholder but not the key table.
        reg.unmount_panel(handle.id, epoch).expect("unmount");
        reg.mount_panel(handle.id, epoch, ViewId::new(12))
            .expect("remount");
        assert!(
            reg.placeholder(handle.id).is_none(),
            "the rebind cleared it"
        );

        // The ninth distinct key: a new controller generation crashes.
        let ninth = Generation(epoch.get() + 100);
        reg.bind_controller_generation_for_test(handle.id, OWNER, ninth);
        let presented = reg
            .present_placeholder(handle.id, ninth, "crashed")
            .expect("the transition still presents");

        assert!(
            presented.placeholder.is_some(),
            "a released generation always leaves a placeholder, even at the key cap"
        );
        assert_eq!(
            reg.placeholder_reason(handle.id),
            Some(PlaceholderReason::Crashed),
            "exactly one findable reason"
        );
        assert_eq!(
            reg.controller_generation(handle.id),
            None,
            "the unbind edge ran: the binding is gone"
        );
        assert!(
            presented.record.is_some(),
            "the transition is counted and recorded rather than answered saturated"
        );
        assert_eq!(
            reg.recovery_attempts(handle.id, ninth, PlaceholderReason::Crashed),
            1,
            "the new key counts from one"
        );
        assert!(
            reg.placeholder_block(handle.id)
                .expect("the fixed table is admitted")
                .is_some(),
            "the Core surface is presentable"
        );
        assert_eq!(
            reg.recovery_keys(handle.id),
            MAX_RECOVERY_KEYS_PER_PANEL,
            "the table made room instead of growing"
        );
    }

    /// E.2: a reason outside the vocabulary is a typed refusal. Nothing moves:
    /// no placeholder, no unbind, no counter, no record, and the binding still
    /// names the generation. Fails for an implementation that defaults to a
    /// reason, truncates the token into submission, or accepts a user-supplied
    /// string.
    #[test]
    fn a_reason_outside_the_vocabulary_is_refused_before_anything_moves() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        let controller = handle.generation;
        reg.bind_controller_generation_for_test(handle.id, OWNER, controller);

        let error = reg
            .present_placeholder(handle.id, controller, "user pressed crash")
            .expect_err("a user-supplied reason is refused");

        match error {
            PanelError::RecoveryReasonRejected { panel_id, spelling } => {
                assert_eq!(panel_id, handle.id);
                assert_eq!(spelling, "user pressed crash");
            }
            other => panic!("expected RecoveryReasonRejected, got {other:?}"),
        }
        assert_eq!(reg.error_count("RecoveryReasonRejected"), 1);
        assert!(reg.placeholder(handle.id).is_none());
        assert_eq!(
            reg.controller_generation(handle.id),
            Some(controller),
            "the binding is untouched"
        );
        assert_eq!(
            reg.recovery_attempts(handle.id, controller, PlaceholderReason::Crashed),
            0
        );
        assert!(reg.recovery_records(handle.id).is_empty());
    }

    /// E.2: the three close-side tokens are user-initiated ends of the content,
    /// so they select `Drop` — one disposal record, no placeholder, no orphan.
    /// Fails for an implementation that presents a placeholder on `closed`
    /// (which would leave a container no generation can ever fill again).
    #[test]
    fn a_close_token_disposes_the_container_and_reports_one_drop_record() {
        for token in ["closed", "dismissed", "quit"] {
            let mut reg = registry();
            let (handle, _workspace) = mounted_panel(&mut reg);

            let presented = reg
                .present_placeholder(handle.id, handle.generation, token)
                .expect("drop");

            assert_eq!(presented.outcome, RecoveryOutcome::Drop);
            assert!(
                presented.placeholder.is_none(),
                "{token} must leave no placeholder"
            );
            assert!(
                reg.panel_state(handle.id, handle.generation).is_err(),
                "{token} retires the container"
            );
            let record = presented.record.expect("one record");
            assert_eq!(record.reason, RecoveryReason::Closed, "{token} normalises");
            assert_eq!(record.action, RecoveryAction::Disposed);
            assert_eq!(record.attempt, 0, "a disposal is not a recovery attempt");
            assert_eq!(
                record.owner, None,
                "{token}: the container never bound a controller, so the record names no owner"
            );
            assert_eq!(
                record.controller_generation,
                handle.generation.get(),
                "{token}: the record names the epoch the disposal used, not a caller's generation"
            );
        }
    }

    /// E.6 / PGO-3: after the placeholder, no verb reaches content. The ended
    /// generation is refused at the controller fence and holds no capability.
    #[test]
    fn no_verb_reaches_content_after_the_placeholder() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        // A controller generation distinct from the allocation epoch, so the
        // released generation is still recognisable after the unbind.
        let controller = Generation(handle.generation.get() + 100);
        reg.bind_controller_generation_for_test(handle.id, OWNER, controller);
        reg.grant_panel_capability(handle.id, handle.generation, "panel.create")
            .expect("grant");
        reg.unmount_panel(handle.id, handle.generation)
            .expect("unmount");

        reg.present_placeholder(handle.id, controller, "crashed")
            .expect("placeholder");

        let error = reg
            .mount_panel(handle.id, controller, ViewId::new(11))
            .expect_err("a released generation mounts nothing");
        assert!(
            matches!(error, PanelError::ControllerReleased { .. }),
            "expected ControllerReleased, got {error:?}"
        );
        assert_eq!(
            reg.require_panel_capability(handle.id, controller, "panel.create"),
            Err(PanelError::CapabilityDenied {
                panel_id: handle.id,
                capability: "panel.create".to_owned(),
            })
        );
        assert_eq!(reg.error_count("ControllerReleased"), 1);
    }

    /// E: the placeholder clears on a rebind, so the panel is no longer
    /// presented as unbound once a controller owns it again.
    #[test]
    fn a_rebind_clears_the_placeholder() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        reg.present_placeholder(handle.id, handle.generation, "reloaded")
            .expect("placeholder");
        assert_eq!(
            reg.placeholder_reason(handle.id),
            Some(PlaceholderReason::Reloaded)
        );

        // The successor re-issues the mount; the view is released first.
        reg.unmount_panel(handle.id, handle.generation)
            .expect("unmount");
        reg.mount_panel(handle.id, handle.generation, ViewId::new(11))
            .expect("remount");

        assert!(reg.placeholder(handle.id).is_none());
        assert!(
            reg.placeholder_block(handle.id)
                .expect("no defect")
                .is_none()
        );
        assert_eq!(
            reg.panel_state(handle.id, handle.generation)
                .expect("state readable"),
            PanelState::Mounted
        );
    }

    /// E.5: the surface is a Core-owned block built from the constant table,
    /// admitted by the accepted scene gate, and rebuilt on demand — no plugin
    /// text, no stored scene to replace, identical bytes on every read. Fails for
    /// an implementation that stores a plugin-supplied scene, that carries
    /// plugin text, or that lets a style through.
    #[test]
    fn the_placeholder_block_is_core_owned_and_bounded() {
        let mut reg = registry();
        let (handle, _workspace) = mounted_panel(&mut reg);
        assert!(
            reg.placeholder_block(handle.id)
                .expect("no defect")
                .is_none(),
            "a bound panel presents no placeholder"
        );

        reg.present_placeholder(handle.id, handle.generation, "incompatible")
            .expect("placeholder");
        let block = reg
            .placeholder_block(handle.id)
            .expect("no defect")
            .expect("block");
        let surface = PlaceholderScene::for_reason(PlaceholderReason::Incompatible);

        assert_eq!(
            block.owner(),
            crate::panel_placeholder::PLACEHOLDER_BLOCK_OWNER
        );
        assert_eq!(
            block.created_at(),
            crate::panel_placeholder::PLACEHOLDER_BLOCK_CREATED_AT
        );
        assert_eq!(block.content().count_nodes(), surface.node_count());
        assert_eq!(block.content().depth(), surface.depth());
        assert_eq!(block.content().text_bytes(), surface.text_bytes());
        assert!(surface.within_scn_limits(), "the fixed table fits SCN-1..3");

        // The whole text payload is the Core table, in order.
        let mut painted = Vec::new();
        collect_text(block.content(), &mut painted);
        assert_eq!(
            painted,
            vec![
                surface.heading().to_owned(),
                surface.detail().to_owned(),
                surface.actions()[0].label().to_owned(),
                surface.actions()[1].label().to_owned(),
            ]
        );

        // Rebuilt on demand, byte-identical: nothing was stored to mutate.
        assert_eq!(
            reg.placeholder_block(handle.id)
                .expect("no defect")
                .expect("block"),
            block
        );
    }

    fn collect_text(node: &bitty_rich::scene::SceneNode, out: &mut Vec<String>) {
        use bitty_rich::scene::SceneNode;
        match node {
            SceneNode::Text(span) => out.push(span.text.clone()),
            SceneNode::Row(children) | SceneNode::Column(children) => {
                for child in children {
                    collect_text(child, out);
                }
            }
            SceneNode::Block { child, .. } | SceneNode::Styled { child, .. } => {
                collect_text(child, out);
            }
            other => panic!("unexpected placeholder node: {other:?}"),
        }
    }

    /// C.8: the accepted six-state table is unchanged by this residual. The
    /// placeholder is a fact about the binding, not a seventh state, and both
    /// disposal edges stay legal. Fails for an implementation that widens
    /// `PanelState` or narrows the table.
    #[test]
    fn the_six_state_transition_table_is_unchanged() {
        use PanelState as S;
        const STATES: [PanelState; 6] = [
            S::Declared,
            S::Created,
            S::Mounted,
            S::Focused,
            S::Suspended,
            S::Disposed,
        ];
        let mut allowed = Vec::new();
        for from in STATES {
            for to in STATES {
                if S::can_transition(from, to) {
                    allowed.push((from, to));
                }
            }
        }
        assert_eq!(
            allowed,
            vec![
                (S::Declared, S::Created),
                (S::Created, S::Mounted),
                (S::Created, S::Disposed),
                (S::Mounted, S::Focused),
                (S::Mounted, S::Suspended),
                (S::Mounted, S::Disposed),
                (S::Focused, S::Mounted),
                (S::Focused, S::Suspended),
                (S::Focused, S::Disposed),
                (S::Suspended, S::Mounted),
                (S::Suspended, S::Focused),
                (S::Suspended, S::Disposed),
            ]
        );
        assert!(S::can_transition(S::Created, S::Disposed));
        assert!(S::can_transition(S::Focused, S::Disposed));
    }
}
