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
    PlaceholderReason, PlaceholderScene, RECOVERY_ATTEMPT_CAP, RecoveryAction, RecoveryBudget,
    RecoveryOutcome, RecoveryReason, RecoveryRecord, RecoveryStep,
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
    /// Unreachable on a live path today: no production surface binds a
    /// controller (bitty#2476 residual 2), and while `PanelRecord::binding` is
    /// `None` the allocation epoch is the accepted key, so every existing call
    /// site is unaffected. Tests mint a binding to reach it.
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
/// value, which is why one generation field carries both; the tombstone key
/// stays `(owner, PanelId, N)` (bitty#2476 residual 7).
#[allow(dead_code)] // bitty#2476 residual 5: shape only, no bind constructs one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelBinding {
    /// Plugin that created the panel content and holds this binding.
    plugin_id: bitty_plugin_host::PluginId,
    /// Controller generation this binding names, and (while B.6 is undecided)
    /// the owner generation the tombstone pairs with `plugin_id`.
    controller_generation: Generation,
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
    /// `None` on every live path today — the phase 2 step 2 writer is not
    /// installed, so no panel carries a binding until a bind lands one.
    binding: Option<PanelBinding>,
    /// Most recently ended controller generation, retained across the unbind
    /// edge (OQ-084 D.2 phase 0).
    ///
    /// `None` until the panel's first controller generation ends. Phase 0 uses
    /// it to distinguish "never bound" (the first bind commits `R`) from "a
    /// previous controller ended but its retired-ceiling tombstone is missing"
    /// This is the predecessor read, not the live binding:
    /// `controller_generation` names the binding in force now.
    last_controller_generation: Option<Generation>,
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
    #[allow(dead_code)] // bitty#2476 residual 7: the tombstone key is the only
    // consumer; nothing binds yet, so it has no live caller.
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
/// The table holds at most one entry per panel. A new tombstone for the same
/// panel replaces the previous one; the entry is dropped when a successor
/// successfully binds the panel or when the panel is disposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedGrantCeiling {
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
    /// Retired-ceiling tombstones (OQ-084 B-4, D.2 phase 0): at most one per
    /// panel, written by [`PanelRegistry::release_generation_scope`] before the
    /// live row is removed, read by [`PanelRegistry::derive_rebind_grants`] at
    /// bind entry, and dropped by a successful successor bind or panel
    /// disposal. Never evicted while the panel record is alive.
    retained_ceilings: BTreeMap<PanelId, RetainedGrantCeiling>,
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
    /// Fail-open by construction while nothing binds: no production surface
    /// installs a binding (D.2 phase 2 step 2 is bitty#2476 residual 2), so
    /// every live call site presents the allocation epoch, hits the first arm,
    /// and behaves exactly as before — while `binding == None` and
    /// `last_controller_generation == None`. Both new refusals are therefore
    /// unreachable on today's live paths and reachable from tests, which mint a
    /// binding.
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
            if let Some(list) = self.workspace_panels.get_mut(&ws) {
                list.retain(|&id| id != panel_id);
            }
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        // Clear commands and bus queues
        self.command_registry.unregister_panel(panel_id);
        self.event_bus.clear_panel(panel_id);
        // OQ-084 A.2/PGO-23: the tombstone is dropped with the container. A
        // disposed panel is gone, so a later panel is a first bind rather than
        // a reload of this identity; keeping the ceiling here would leave a
        // record that no bind can name and no bound can reclaim.
        self.retained_ceilings.remove(&panel_id);
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
    /// `presented` is the controller generation the transition names, and
    /// `owner` is the plugin that owned the content, both stamped on the FS-4
    /// record. `reason_token` is a token of Decision E rule 2's closed
    /// vocabulary; `closed`, `dismissed`, and `quit` select the `Drop` outcome,
    /// `denied` selects `Unavailable`, and the rest select `Placeholder`.
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
    ///    binding; later attempts emit nothing.
    /// 3. **The reason is closed.** A token outside the vocabulary is refused
    ///    with [`PanelError::RecoveryReasonRejected`] before anything is read or
    ///    written.
    ///
    /// Nothing here reaches a terminal, a PTY, a renderer handle, or a plugin
    /// callback: the presentation is a Core-owned [`bitty_rich::scene::RichBlock`]
    /// built from a constant table (see [`crate::panel_placeholder`]).
    ///
    /// No production caller today: the plugin generation boundary that would
    /// invoke this is bitty#2476 residual 5, so the seam is implemented and
    /// tested but unwired.
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
        owner: &str,
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
            // the controller fence.
            let epoch = match self.panels.get(&panel_id.0) {
                Some(record) => record.generation,
                None => {
                    return Err(PanelError::NotFound {
                        kind: "panel",
                        id_raw: panel_id.0,
                    });
                }
            };
            self.dispose_panel(panel_id, epoch)?;
            let step = self.recovery.note(
                panel_id,
                owner,
                presented.get(),
                outcome.reason(),
                RecoveryAction::Disposed,
            );
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
        let (bound, last) = match self.panels.get(&panel_id.0) {
            Some(record) => (
                record.controller_generation(),
                record.last_controller_generation,
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
            owner,
            presented.get(),
            outcome.reason(),
            outcome.action(),
        );
        let (record, saturated) = match step {
            RecoveryStep::Recorded(record) => (Some(record), false),
            RecoveryStep::Saturated { .. } => (None, true),
            RecoveryStep::Unchanged => (None, false),
        };
        if record.is_some() {
            if let Some(record_mut) = self.panels.get_mut(&panel_id.0) {
                record_mut.placeholder = Some(PanelPlaceholder {
                    reason: placeholder_reason,
                    unbound_generation: bound.or(last),
                });
            }
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
    /// "already released".
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
        let (released_generation, bound, last_ended, allocation_epoch) =
            match self.panels.get(&panel_id.0) {
                Some(record) => (
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
        // `A.2`'s phase-1 repeat has to be decided from the *presented*
        // generation, before the binding-key check: the first release clears the
        // binding, so the repeat would otherwise be measured against the
        // allocation epoch the unbind exposed and refused as `StaleHandle`
        // instead of recognised as an edge that already ran. While a controller
        // is still bound the key check keeps its place ahead of the tombstone,
        // so a foreign generation is still refused before any family is
        // touched.
        let tombstone_names_generation = self
            .retained_ceilings
            .get(&panel_id)
            .is_some_and(|ceiling| ceiling.ended_generation == generation);
        let already_ended = if bound.is_some() {
            released_generation == generation && tombstone_names_generation
        } else {
            tombstone_names_generation || last_ended == Some(generation)
        };
        if already_ended {
            // `A.2`'s phase-1 repeat: this edge ran, so there is nothing to
            // retain, nothing to overwrite, and no family to release again.
            if let Some(record) = self.panels.get_mut(&panel_id.0) {
                record.last_controller_generation = Some(generation);
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
        // A controller generation that held no grant has no row; that is a
        // ceiling of `∅`, not an absent predecessor. Store the tombstone before
        // deleting the row so a failed write leaves the live row (and every
        // other family) in place.
        if let Err(error) =
            self.retain_grant_ceiling(panel_id, generation, held.clone().unwrap_or_default())
        {
            self.bump_error("RetiredCeilingTableFull");
            return Err(error);
        }
        if let Some(record) = self.panels.get_mut(&panel_id.0) {
            record.last_controller_generation = Some(generation);
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
    #[must_use]
    pub fn retained_grant_ceiling(&self, panel_id: PanelId) -> Option<&RetainedGrantCeiling> {
        self.retained_ceilings.get(&panel_id)
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

    /// The controller generation the panel record currently binds (OQ-084 A.3,
    /// D.2 phase 2 step 2), or `None` while no controller is bound.
    ///
    /// Read-only: the phase 2 step 2 writer is not installed, so this answers
    /// `None` for every panel a live path creates. It names the binding in
    /// force, not the allocation epoch (`generation` on the record, which never
    /// moves) and not the predecessor ([`Self::last_controller_generation`]).
    #[must_use]
    pub fn controller_generation(&self, panel_id: PanelId) -> Option<Generation> {
        self.panels
            .get(&panel_id.0)
            .and_then(PanelRecord::controller_generation)
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

    /// Stores one retired-ceiling tombstone, replacing any previous entry for
    /// the same panel.
    ///
    /// The table holds at most one entry per live panel and the registry admits
    /// at most `max_panels_per_window` live records, so the capacity check is a
    /// fail-closed invariant guard, not an eviction path: inserting a new key
    /// cannot reach the bound while the records that own the existing entries
    /// are alive. If the guard does trip, the caller keeps the live grant row.
    fn retain_grant_ceiling(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        grants: BTreeSet<String>,
    ) -> Result<(), PanelError> {
        let capacity = self
            .config
            .max_panels_per_window
            .min(MAX_RETAINED_GRANT_CEILINGS);
        if !self.retained_ceilings.contains_key(&panel_id)
            && self.retained_ceilings.len() >= capacity
        {
            return Err(PanelError::ResourceExhausted {
                reason: format!(
                    "retired-ceiling table is full ({capacity}); refusing to evict a live panel's ceiling"
                ),
            });
        }
        self.retained_ceilings.insert(
            panel_id,
            RetainedGrantCeiling {
                ended_generation: generation,
                grants,
            },
        );
        Ok(())
    }

    /// Derives a successor's panel-scoped grant set (OQ-084 D.1, D.2 phase 0).
    ///
    /// `grant(n+1) = (R ∩ P) ∪ C`, where `R` is `requested`, `P` is the
    /// predecessor ceiling, and `C` is the subset of `consented` that lies in
    /// `R \ P`. Phase 0 prefers the live row when the predecessor is still
    /// bound, then the retired-ceiling tombstone written at the predecessor's
    /// edge. With no predecessor the whole resolution is granted with no
    /// permission-diff consent: an absent predecessor is not an empty ceiling.
    ///
    /// A panel whose record names a previous controller generation must have a
    /// tombstone for exactly that generation. A missing tombstone, or one that
    /// names a different generation, means the ceiling record was lost: the
    /// bind is refused rather than falling back to `R`.
    ///
    /// # Errors
    /// `RegistryDisposed`, `NotFound`, `StaleHandle`, or `CeilingLost` when the
    /// previous controller generation has no matching tombstone.
    pub fn derive_rebind_grants(
        &self,
        panel_id: PanelId,
        generation: Generation,
        requested: &BTreeSet<String>,
        consented: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, PanelError> {
        let record = self.get_panel(panel_id, generation)?;
        let live_row = self
            .capabilities
            .get(&(panel_id, record.grant_key_generation()));
        let predecessor = if let Some(live) = live_row {
            // `D.2` phase 0: the live row wins over a tombstone. The row is
            // keyed by the bound generation — the controller generation when
            // one is bound, else the allocation epoch — not by the successor
            // the caller presents, so a displaced still-bound predecessor is
            // read here.
            Some(live.clone())
        } else if let Some(tombstone) = self.retained_ceilings.get(&panel_id) {
            if record.last_controller_generation == Some(tombstone.ended_generation) {
                Some(tombstone.grants.clone())
            } else {
                return Err(PanelError::CeilingLost {
                    panel_id,
                    bind_generation: generation,
                    last_controller_generation: record.last_controller_generation,
                    tombstone_generation: Some(tombstone.ended_generation),
                });
            }
        } else if let Some(last) = record.last_controller_generation {
            return Err(PanelError::CeilingLost {
                panel_id,
                bind_generation: generation,
                last_controller_generation: Some(last),
                tombstone_generation: None,
            });
        } else {
            None
        };
        let Some(ceiling) = predecessor else {
            // No predecessor: `panel.create` is the authorization and the first
            // bind commits `R` with no permission-diff consent.
            return Ok(requested.clone());
        };
        let mut grants: BTreeSet<String> = requested.intersection(&ceiling).cloned().collect();
        for capability in consented {
            if requested.contains(capability) && !ceiling.contains(capability) {
                grants.insert(capability.clone());
            }
        }
        Ok(grants)
    }

    /// Commits a rebind's derived grant set and retires the retired-ceiling
    /// tombstone (OQ-084 D.2 phase 2 step 1).
    ///
    /// The set is written once, all-or-nothing: every element is validated
    /// before any is installed. A successful install ends the tombstone's
    /// lifetime — the retention rule is "until the panel's next successful
    /// bind".
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
        self.retained_ceilings.remove(&panel_id);
        Ok(grants)
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
    /// The phase 2 step 2 writer is bitty#2476 residual 2; until it lands no
    /// live path binds a controller, so only tests mint one here.
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
        registry.retained_ceilings.remove(&panel);
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
        if let Some(ceiling) = registry.retained_ceilings.get_mut(&panel) {
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
            PanelId::new(99),
            RetainedGrantCeiling {
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
            .retain_grant_ceiling(panel, newer, set(&["panel.focus"]))
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

/// Shape tests for the controller binding (OQ-084 D.2 phase 2, bitty#2476
/// residual 1).
///
/// The record carries the shape; nothing binds a controller on a live path yet,
/// so these tests mint the binding directly and prove what the grant table does
/// once one exists.
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
            PanelId::new(99),
            RetainedGrantCeiling {
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

        registry.retained_ceilings.remove(&PanelId::new(99));
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
            .present_placeholder(handle.id, controller, OWNER, "crashed")
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
        assert_eq!(record.owner, OWNER);
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
            .present_placeholder(handle.id, handle.generation, OWNER, "unloaded")
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
            .present_placeholder(handle.id, handle.generation, OWNER, "denied")
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
                .present_placeholder(handle.id, epoch, OWNER, "disabled")
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
            .present_placeholder(handle.id, epoch, OWNER, "disabled")
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
            .present_placeholder(handle.id, epoch, OWNER, "disabled")
            .expect("no-op attempt");
        assert!(!tenth.saturated && tenth.record.is_none());
        assert_eq!(reg.recovery_records(handle.id).len(), 8);
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
            .present_placeholder(handle.id, controller, OWNER, "user pressed crash")
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
                .present_placeholder(handle.id, handle.generation, OWNER, token)
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
            assert_eq!(record.owner, OWNER);
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

        reg.present_placeholder(handle.id, controller, OWNER, "crashed")
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
        reg.present_placeholder(handle.id, handle.generation, OWNER, "reloaded")
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

        reg.present_placeholder(handle.id, handle.generation, OWNER, "incompatible")
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
