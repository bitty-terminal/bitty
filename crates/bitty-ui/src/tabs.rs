//! Tab interaction verbs over panel projections (CTX-1833, issue #3166).
//!
//! A tab is a **projection of a panel**: it carries the [`PanelId`] that owns
//! identity, ownership, and lifetime plus the [`View`] leaf that paints that
//! panel. This module mints no anonymous slot: every tab names a panel, and a
//! tab verb can only ever re-shape an existing projection set.
//!
//! The three Core verbs are [`TAB_CMD_OPEN`], [`TAB_CMD_SWITCH`], and
//! [`TAB_CMD_CLOSE`]. They are ordinary `owner.name:command` qualified names
//! (see [`QualifiedCommand::parse`]) registered on the per-window
//! [`CommandRegistry`] through [`CommandRegistry::register_core`], so they obey
//! exactly the grammar, duplicate-rejection, and per-owner cap discipline the
//! panel command lane already enforces. [`register_tab_verbs`] performs that
//! registration in one call.
//!
//! # Decision record (issue #3166 open questions)
//!
//! 1. **A tab is 1:1 with a panel; the tab key is the [`PanelId`].** No
//!    `TabId` newtype exists. A distinct id would be new persisted identity
//!    with no acceptance criterion behind it, and it would have to answer "is
//!    close destroying the panel or detaching a view?" twice. With 1:1 the
//!    answer is single-valued: [`TabSet::close`] detaches one projection; panel
//!    lifecycle (`PanelState`, disposal) stays entirely with the panel registry.
//! 2. **`LayoutNode::Stack` is neither the tab container nor a projection
//!    cache.** The committed layout of a tab stack is a single
//!    [`LayoutNode::Leaf`] naming the active projection —
//!    [`TabSet::layout`]. That is forced by the existing Core validator:
//!    [`validate_proposal`] rejects any `Stack` interior outright
//!    (`LayoutError::UnsupportedNode { kind: "Stack" }`), so a multi-child
//!    "tab" `Stack` could never have been committed through the validated
//!    layout path. The container therefore stays a pure paint-order node with
//!    no identity and no active child, and the tab model lives here.
//! 3. **The strip order is insertion order and no verb reorders it.** The
//!    active tab is a separate index ([`TabSet::active_index`]), not a
//!    position in the strip: [`TabSet::switch`] moves the index and leaves the
//!    vector alone, so activating a tab never moves it and the strip never
//!    reorders behind the user's back (issue #3170, residual 1). `open`
//!    appends, `close` removes in place, and `switch` may change the active
//!    index without touching order. Closing the *active* tab activates the
//!    neighbour that takes its slot — the successor, or the previous tab when
//!    the closed one was last — because the strip order is the user's page
//!    order and a close must not jump the selection to the far end of it.
//! 4. **Every projection is kept at the strip slot geometry.** The tabs of one
//!    stack project one slot `Rect`, so the slot has one size, not one size per
//!    tab. Because the committed tree holds only the active leaf, the layout
//!    solver never visits an inactive tab — which is how a background tab used
//!    to return at its last, stale grid size (issue #3170, residual 2).
//!    [`TabSet::fit_slot`] stamps the slot onto *every* projection, inactive
//!    ones included, and records it so a later [`TabSet::open`] fits the new
//!    projection as well. Activation therefore needs no resize step: geometry
//!    is already current when [`TabSet::switch`] makes a tab active, and the
//!    effect still asks for a full redraw because the visible surface changes
//!    wholesale.
//! 5. **A multi-tab strip is not persisted, deliberately.** The layout format
//!    carries the committed tree, and a tab stack's committed tree is a single
//!    leaf ([`TabSet::layout`]). Persisting `n` tabs would be a layout-format
//!    change with its own migration story, and nothing in the acceptance
//!    criteria asks for one, so restore rebuilds a *single-tab* set from that
//!    leaf: the `n - 1` inactive tabs are dropped on restart, by contract, and
//!    no persisted field records them (issue #3170, residual 3). The contract
//!    is asserted by `multi_tab_strips_are_not_persisted`; a future slice that
//!    wants a strip back changes the format and that test, and does not add a
//!    field here.
//! 6. **`tab.switch` re-attributes input-mode caches and forces a full
//!    redraw.** A switch moves focus to the activated tab exactly like the
//!    WS-INV-19 re-home (crash-safe ordering: re-home before persist), so the
//!    host must run `sync_mode_caches_to_focus` for the new focus, and the
//!    visible surface changes wholesale, so nothing partial is repaintable.
//!    [`TabEffect`] carries both facts; [`TabCommit::commit`] refuses to hand
//!    out committed state while focus still names the outgoing tab.
//! 7. **The `workspaceline` exclusive claim stays exactly as it is.** It gates
//!    *plugin mounts into the `tabline` UI slot*, which is a different
//!    authority from Core-owned verbs: tab verbs mount no UI block, never
//!    consult plugin claims, and no plugin can claim a `core.*` name because
//!    [`CommandRegistry::register`] rejects the whole
//!    [`crate::panel::CORE_OWNER_FAMILY`] owner prefix, not just the verbs Core
//!    happens to have registered. The slot itself keeps failing closed
//!    (`UiSlotPlacement::Unsupported`) because hosting a tab strip is a
//!    separate slice.
//!
//! # Atomicity
//!
//! Every verb is a function from one [`TabSet`] to a [`TabCommit`]; none of
//! them mutates in place. A transition validates before it exists:
//! [`LayoutNode::validate_structure`] over the projected tree, then
//! [`validate_proposal`] over that tree and the projection's own view set, then
//! the projection-level rules a single-leaf tree cannot express (unique panels,
//! unique views, non-empty, capped, active in range). Any failure returns a
//! [`TabError`] and the caller's [`TabSet`] and focus are untouched, so there is
//! no partial mutation and no orphaned leaf.
//!
//! [`TabSet::fit_slot`] is deliberately *not* a verb and not a second commit
//! path: it changes no identity, no order, and no active index, mints no
//! [`TabCommit`], and cannot invalidate one — it is the projection half of the
//! step the runtime already performs when it stamps a solved allocation onto a
//! leaf, extended to the tabs that are not in the tree. Nothing else in this
//! module writes a view.
//!
//! Deterministic and headless; this module adds no crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::Rect;
use crate::layout::{LayoutNode, TreeViolation};
use crate::panel::{CommandError, CommandRegistry, PanelId, QualifiedCommand};
use crate::provider::{LayoutError, validate_proposal};
use crate::view::{View, ViewId};

// ---------------------------------------------------------------------------
// Core command surface
// ---------------------------------------------------------------------------

/// Core verb that opens a tab: appends a panel projection and activates it.
pub const TAB_CMD_OPEN: &str = "core.tab:open";

/// Core verb that switches the active tab to an existing panel projection.
pub const TAB_CMD_SWITCH: &str = "core.tab:switch";

/// Core verb that closes one tab projection (never the last one).
pub const TAB_CMD_CLOSE: &str = "core.tab:close";

/// The three Core tab verbs, in canonical order.
pub const TAB_CMDS: [&str; 3] = [TAB_CMD_OPEN, TAB_CMD_SWITCH, TAB_CMD_CLOSE];

/// Hard cap on tabs in one stack.
///
/// Mirrors the runtime's `MAX_VIEWS_PER_WORKSPACE` bound so a tab stack can
/// never outgrow the workspace that hosts it. Exceeding the cap is a typed
/// [`TabError::TooManyTabs`], never a silent drop.
pub const MAX_TABS_PER_STACK: usize = 32;

/// Registers the three Core tab verbs on `registry`.
///
/// Runs through [`CommandRegistry::register_core`], so the same qualified-name
/// grammar, cross-lane duplicate rejection, and per-owner cap apply as for
/// panel commands.
///
/// # Errors
///
/// [`CommandError`] when a verb is malformed, already registered, or the Core
/// bucket is full.
pub fn register_tab_verbs(
    registry: &mut CommandRegistry,
) -> Result<[QualifiedCommand; 3], CommandError> {
    let open = registry.register_core(TAB_CMD_OPEN)?;
    let switch = registry.register_core(TAB_CMD_SWITCH)?;
    let close = registry.register_core(TAB_CMD_CLOSE)?;
    Ok([open, switch, close])
}

// ---------------------------------------------------------------------------
// Liveness oracle
// ---------------------------------------------------------------------------

/// Whether a panel identity is still live.
///
/// A tab projects a panel instance; the panel registry owns generations and is
/// the only authority that can say whether the instance behind a [`PanelId`]
/// still exists. The verbs take the oracle as a parameter instead of caching
/// liveness, so a stale projection cannot be activated by mistake.
pub trait PanelLiveness {
    /// Whether `panel` still names a live panel instance.
    fn is_live(&self, panel: PanelId) -> bool;
}

impl PanelLiveness for [PanelId] {
    fn is_live(&self, panel: PanelId) -> bool {
        self.contains(&panel)
    }
}

impl<F> PanelLiveness for F
where
    F: Fn(PanelId) -> bool,
{
    fn is_live(&self, panel: PanelId) -> bool {
        self(panel)
    }
}

// ---------------------------------------------------------------------------
// Tab and projection set
// ---------------------------------------------------------------------------

/// One tab: a panel identity plus the leaf that projects it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    panel: PanelId,
    view: View,
}

impl Tab {
    /// Creates a tab projecting `panel` through `view`.
    #[must_use]
    pub fn new(panel: PanelId, view: View) -> Self {
        Self { panel, view }
    }

    /// The panel this tab projects.
    #[must_use]
    pub fn panel(&self) -> PanelId {
        self.panel
    }

    /// The leaf that paints the panel.
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The leaf id that paints the panel.
    #[must_use]
    pub fn view_id(&self) -> ViewId {
        self.view.id()
    }
}

/// Which verb produced a [`TabCommit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabVerb {
    /// [`TabSet::open`].
    Open,
    /// [`TabSet::switch`].
    Switch,
    /// [`TabSet::close`].
    Close,
}

/// Host-side consequences of a committed tab transition.
///
/// Both flags are set for every verb, which is deliberately a safe
/// over-approximation rather than an exact prediction:
///
/// - `open` and `switch` always change the active tab;
/// - `close` changes it only when the closed tab was the active one. Closing a
///   background tab keeps the active panel (and therefore the input-mode
///   cache) where it was, so the re-attribution is redundant but harmless, and
///   the strip still changed, so the full redraw is still required.
///
/// The host must run `sync_mode_caches_to_focus` for the new focus on the
/// WS-INV-19 path and set `pending_full_redraw`; no verb produces a partial
/// repaint.
///
/// There is deliberately no resize flag (decision 4): background projections
/// are kept at the strip slot geometry by [`TabSet::fit_slot`], so activating a
/// background tab never needs a resize step, and the full redraw already
/// repaints the activated leaf at its current size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabEffect {
    /// The host must re-attribute input-mode caches to the new focus.
    pub reattribute_input_mode: bool,
    /// The host must set `pending_full_redraw`.
    pub pending_full_redraw: bool,
}

impl TabEffect {
    /// The effect every tab transition has (decision 6).
    pub const FOCUS_AND_REDRAW: Self = Self {
        reattribute_input_mode: true,
        pending_full_redraw: true,
    };
}

/// Typed failure for every tab verb.
///
/// Every variant leaves the caller's [`TabSet`] and focus untouched: verbs do
/// not mutate, they return a [`TabCommit`], and a failure never produces one.
#[derive(Clone, Debug, PartialEq)]
pub enum TabError {
    /// A tab set must project at least one panel; an empty set is a tombstone.
    EmptyStack,
    /// The active index does not name a tab in the strip.
    ///
    /// The active tab is an explicit index (decision 3), so a strip whose index
    /// falls outside `0..len` has no active projection and no layout to commit.
    ActiveOutOfRange {
        /// The out-of-range index.
        index: usize,
        /// The number of tabs in the strip.
        len: usize,
    },
    /// Two tabs named the same panel; a panel projects at most once.
    DuplicatePanel {
        /// The repeated panel.
        panel: PanelId,
    },
    /// Two tabs projected the same leaf; a view paints at most one tab.
    DuplicateView {
        /// The repeated leaf.
        view: ViewId,
    },
    /// The stack already holds [`MAX_TABS_PER_STACK`] tabs.
    TooManyTabs {
        /// The cap that was exceeded.
        cap: usize,
        /// The size the transition asked for.
        requested: usize,
    },
    /// No tab projects this panel.
    UnknownTab {
        /// The panel nobody projects.
        panel: PanelId,
    },
    /// The tab exists but its panel generation is gone.
    GenerationGone {
        /// The dead panel.
        panel: PanelId,
    },
    /// The panel is already projected; a stack holds one tab per panel.
    AlreadyOpen {
        /// The already projected panel.
        panel: PanelId,
    },
    /// The stack holds a single tab, so there is nothing to switch to.
    SingleTab {
        /// The only tab's panel.
        panel: PanelId,
    },
    /// The named tab is already active.
    AlreadyActive {
        /// The already active panel.
        panel: PanelId,
    },
    /// Closing this tab would leave an empty stack (a layout tombstone).
    ///
    /// A tab stack always keeps at least one tab. Closing the pane itself is a
    /// layout/workspace operation, not a tab verb.
    LastTab {
        /// The last tab's panel.
        panel: PanelId,
    },
    /// Focus still named another tab when the transition was committed.
    ///
    /// WS-INV-19 ordering: focus must already be off the outgoing id (and on
    /// the activated tab) before the mutation is persisted, so no persisted
    /// focus id can ever be read back as dead.
    FocusNotRehomed {
        /// The tab the transition activates.
        expected: PanelId,
        /// The focus the caller presented.
        actual: Option<PanelId>,
    },
    /// [`LayoutNode::validate_structure`] rejected the projected tree.
    Structure(TreeViolation),
    /// [`validate_proposal`] rejected the projected tree.
    Proposal(LayoutError),
}

impl std::fmt::Display for TabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyStack => f.write_str("a tab stack must project at least one panel"),
            Self::ActiveOutOfRange { index, len } => {
                write!(f, "active tab index {index} is out of range for {len} tabs")
            }
            Self::DuplicatePanel { panel } => write!(f, "panel projected by two tabs: {panel}"),
            Self::DuplicateView { view } => write!(f, "view projecting two tabs: {view}"),
            Self::TooManyTabs { cap, requested } => write!(
                f,
                "too many tabs: cap {cap}, transition requested {requested}"
            ),
            Self::UnknownTab { panel } => write!(f, "unknown tab: {panel}"),
            Self::GenerationGone { panel } => {
                write!(f, "tab panel generation is gone: {panel}")
            }
            Self::AlreadyOpen { panel } => write!(f, "panel already has a tab: {panel}"),
            Self::SingleTab { panel } => {
                write!(f, "single-tab stack: nothing to switch to for {panel}")
            }
            Self::AlreadyActive { panel } => write!(f, "tab already active: {panel}"),
            Self::LastTab { panel } => write!(
                f,
                "closing {panel} would leave an empty tab stack; close the pane instead"
            ),
            Self::FocusNotRehomed { expected, actual } => match actual {
                Some(actual) => write!(
                    f,
                    "focus must be re-homed to {expected} before commit, found {actual}"
                ),
                None => write!(f, "focus must be re-homed to {expected} before commit"),
            },
            Self::Structure(violation) => {
                write!(f, "projected layout is structurally invalid: {violation:?}")
            }
            Self::Proposal(error) => write!(f, "projected layout is not a valid proposal: {error}"),
        }
    }
}

impl std::error::Error for TabError {}

/// Core-owned tab projection over one panel stack.
///
/// Invariants, enforced on construction and on every transition:
///
/// - at least one tab (an empty stack is a layout tombstone),
/// - at most [`MAX_TABS_PER_STACK`] tabs,
/// - exactly one tab per [`PanelId`] and one tab per [`ViewId`],
/// - [`TabSet::active_index`] is inside `0..len`.
///
/// The strip order is the vector order and it is **insertion order**: no verb
/// reorders it. The active tab is a separate index, so [`TabSet::switch`] moves
/// only that index (decision 3) and the committed leaf is whatever
/// [`TabSet::layout`] reads from it. Every projection, background tabs
/// included, is kept at the strip slot geometry by [`TabSet::fit_slot`]
/// (decision 4).
///
/// Nothing here reaches the layout format except the single leaf
/// [`TabSet::layout`] emits, so a multi-tab strip is deliberately not persisted
/// (decision 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSet {
    tabs: Vec<Tab>,
    /// Index of the active tab; always inside `tabs` (`build` validates it).
    active: usize,
    /// The strip slot every projection was last fitted to.
    slot: Option<Rect>,
}

impl TabSet {
    /// Builds a tab set from `tabs` in strip order with `active` selected.
    ///
    /// # Errors
    ///
    /// [`TabError::EmptyStack`], [`TabError::TooManyTabs`],
    /// [`TabError::ActiveOutOfRange`], [`TabError::DuplicatePanel`], or
    /// [`TabError::DuplicateView`].
    pub fn new(tabs: Vec<Tab>, active: usize) -> Result<Self, TabError> {
        Self::build(tabs, active, None)
    }

    /// The one validated constructor; [`TabSet::new`] and every verb land here.
    fn build(tabs: Vec<Tab>, active: usize, slot: Option<Rect>) -> Result<Self, TabError> {
        let set = Self { tabs, active, slot };
        set.validate()?;
        Ok(set)
    }

    /// Builds a single-tab set.
    #[must_use]
    pub fn single(tab: Tab) -> Self {
        Self {
            tabs: vec![tab],
            active: 0,
            slot: None,
        }
    }

    /// Tabs in strip order: insertion order, never reordered by a verb.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Number of tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Always `false`: [`TabSet::new`] rejects an empty stack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Index of the active tab in strip order.
    ///
    /// The strip order and the active position are separate state on purpose:
    /// activating a tab changes this index and nothing else, so the strip the
    /// user sees keeps its order (decision 3).
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab.
    #[must_use]
    pub fn active(&self) -> &Tab {
        // `build` validates `active < tabs.len()`; `single` sets both.
        &self.tabs[self.active]
    }

    /// The active tab's panel.
    #[must_use]
    pub fn active_panel(&self) -> PanelId {
        self.active().panel
    }

    /// The tab projecting `panel`, if any.
    #[must_use]
    pub fn find(&self, panel: PanelId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.panel == panel)
    }

    /// Strip index of `panel`, if projected.
    fn index_of(&self, panel: PanelId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.panel == panel)
    }

    /// The strip slot the projections were last fitted to, if any.
    ///
    /// `None` until the host reports the slot through [`TabSet::fit_slot`];
    /// every projection then keeps whatever geometry it was constructed with.
    #[must_use]
    pub fn slot_rect(&self) -> Option<Rect> {
        self.slot
    }

    /// Fits every projection — active *and* background — to the strip slot.
    ///
    /// Every tab of a stack projects the same slot, so one `Rect` is the whole
    /// geometry of the strip. A tab that is not the active leaf is not in the
    /// committed tree, so the layout solver never visits it: without this call
    /// it keeps the size it had when it was last active and returns at stale
    /// geometry (decision 4). The slot is remembered so [`TabSet::open`] can
    /// fit a projection that arrives later.
    ///
    /// Not a verb and not a second commit path: it changes no identity, no
    /// order, and no active index, mints no [`TabCommit`], and cannot
    /// invalidate one (see the module atomicity note). Returns whether any
    /// projection's grid size changed, so a host can skip work when nothing
    /// moved.
    pub fn fit_slot(&mut self, rect: Rect) -> bool {
        self.slot = Some(rect);
        let mut resized = false;
        for tab in &mut self.tabs {
            resized |= tab.view.reflow_to_rect(rect);
        }
        resized
    }

    /// Every projected leaf, in strip order.
    #[must_use]
    pub fn view_ids(&self) -> Vec<ViewId> {
        self.tabs.iter().map(Tab::view_id).collect()
    }

    /// The leaf set the committed tree must contain: the active projection.
    ///
    /// Inactive tabs stay in the projection set and are not laid out, so the
    /// committed tree is Stack-free and [`validate_proposal`] — which rejects
    /// `Stack` interiors — applies to every tab commit.
    #[must_use]
    pub fn projected_views(&self) -> Vec<ViewId> {
        vec![self.active().view_id()]
    }

    /// The layout a committed tab stack contributes: the active projection.
    #[must_use]
    pub fn layout(&self) -> LayoutNode {
        LayoutNode::leaf(self.active().view().clone())
    }

    /// Re-runs the full commit gate over the current projection.
    ///
    /// Checks, in order: the projection-level invariants a single-leaf tree
    /// cannot express (non-empty, cap, unique panels, unique views),
    /// [`LayoutNode::validate_structure`] over the projected tree, then
    /// [`validate_proposal`] over that tree and [`TabSet::projected_views`].
    ///
    /// # Why the two tree validators are defence-in-depth here
    ///
    /// [`TabSet::layout`] always emits exactly one [`LayoutNode::Leaf`], so
    /// neither tree validator can fail for a projection this type emits today:
    /// a leaf is trivially structurally valid, and `validate_proposal(leaf,
    /// [leaf])` is a one-element set equality. The invariants above the calls
    /// are the *effective* gate; deleting the two lines would not change any
    /// observable behaviour.
    ///
    /// They are kept deliberately, not because they are exercised: they are
    /// the contract a future multi-leaf projection (a strip that lays out more
    /// than the active tab) must satisfy, and the private
    /// `validate_candidate` gate pins them against trees this type does not yet
    /// emit. Replacing them with an assertion that the tree is a single leaf
    /// would be the alternative; keeping the real validators means the
    /// contract cannot be silently weakened when `layout()` grows.
    ///
    /// # Errors
    ///
    /// The first [`TabError`] a check reports.
    pub fn validate(&self) -> Result<(), TabError> {
        if self.tabs.is_empty() {
            return Err(TabError::EmptyStack);
        }
        if self.tabs.len() > MAX_TABS_PER_STACK {
            return Err(TabError::TooManyTabs {
                cap: MAX_TABS_PER_STACK,
                requested: self.tabs.len(),
            });
        }
        if self.active >= self.tabs.len() {
            return Err(TabError::ActiveOutOfRange {
                index: self.active,
                len: self.tabs.len(),
            });
        }
        let mut panels: Vec<PanelId> = Vec::with_capacity(self.tabs.len());
        let mut views: Vec<ViewId> = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            if panels.contains(&tab.panel) {
                return Err(TabError::DuplicatePanel { panel: tab.panel });
            }
            panels.push(tab.panel);
            let view = tab.view_id();
            if views.contains(&view) {
                return Err(TabError::DuplicateView { view });
            }
            views.push(view);
        }
        Self::validate_candidate(&self.layout(), &self.projected_views())
    }

    /// The tree half of the commit gate: structure, then proposal.
    ///
    /// Split out from [`TabSet::validate`] so the two validators stay
    /// testable against candidate trees [`TabSet::layout`] does not emit
    /// (see the note on `validate`). Production callers only ever pass
    /// `layout()`'s output, so this is not a second gate — it is the same two
    /// lines, in a scope a test can drive.
    fn validate_candidate(layout: &LayoutNode, projected: &[ViewId]) -> Result<(), TabError> {
        layout.validate_structure().map_err(TabError::Structure)?;
        validate_proposal(layout, projected).map_err(TabError::Proposal)?;
        Ok(())
    }

    /// Opens `tab` and activates it.
    ///
    /// The tab is appended to the strip (insertion order, decision 3) and
    /// becomes active. When the strip has been fitted to a slot, the incoming
    /// projection is fitted to it first, so a new tab never starts at a
    /// geometry the slot does not have (decision 4).
    ///
    /// # Errors
    ///
    /// [`TabError::GenerationGone`] when the panel is not live,
    /// [`TabError::AlreadyOpen`] when the panel already has a tab, or any
    /// [`TabError`] from [`TabSet::validate`] on the candidate projection.
    pub fn open(&self, mut tab: Tab, live: &dyn PanelLiveness) -> Result<TabCommit, TabError> {
        let panel = tab.panel;
        if !live.is_live(panel) {
            return Err(TabError::GenerationGone { panel });
        }
        if self.index_of(panel).is_some() {
            return Err(TabError::AlreadyOpen { panel });
        }
        if let Some(slot) = self.slot {
            tab.view.reflow_to_rect(slot);
        }
        let mut next = self.tabs.clone();
        next.push(tab);
        let active = next.len() - 1;
        Self::transition(TabVerb::Open, next, active, self.slot, None)
    }

    /// Switches the active tab to `panel`.
    ///
    /// Only the active index moves; the strip keeps its insertion order and the
    /// target stays where it is in it (decision 3). The activated projection is
    /// already at the strip slot geometry, so a switch needs no resize step
    /// (decision 4).
    ///
    /// # Errors
    ///
    /// [`TabError::UnknownTab`], [`TabError::SingleTab`] when the stack holds
    /// one tab, [`TabError::GenerationGone`] when the panel is not live,
    /// [`TabError::AlreadyActive`], or any [`TabError`] from
    /// [`TabSet::validate`] on the candidate projection.
    pub fn switch(&self, panel: PanelId, live: &dyn PanelLiveness) -> Result<TabCommit, TabError> {
        let index = self.index_of(panel).ok_or(TabError::UnknownTab { panel })?;
        if self.tabs.len() == 1 {
            return Err(TabError::SingleTab { panel });
        }
        if !live.is_live(panel) {
            return Err(TabError::GenerationGone { panel });
        }
        if index == self.active {
            return Err(TabError::AlreadyActive { panel });
        }
        Self::transition(TabVerb::Switch, self.tabs.clone(), index, self.slot, None)
    }

    /// Closes the tab projecting `panel`.
    ///
    /// Closing detaches one projection; it never destroys the panel, whose
    /// lifecycle belongs to the panel registry.
    ///
    /// The strip keeps its insertion order and the remaining tabs do not move
    /// in it. Closing a background tab leaves the active tab active. Closing
    /// the *active* tab activates the neighbour that takes its slot — the
    /// successor, or the previous tab when the closed one was last — which is
    /// the focus target [`TabCommit::focus_target`] reports (decision 3).
    ///
    /// A tab whose panel generation is gone is still closable: that is the
    /// repair path for a stale projection (activation is what
    /// [`TabSet::switch`] refuses). [`TabSet::close`] is the one verb that does
    /// not consult [`PanelLiveness`].
    ///
    /// # Errors
    ///
    /// [`TabError::UnknownTab`], [`TabError::LastTab`] when closing would leave
    /// an empty stack, or any [`TabError`] from [`TabSet::validate`] on the
    /// candidate projection.
    pub fn close(&self, panel: PanelId) -> Result<TabCommit, TabError> {
        let index = self.index_of(panel).ok_or(TabError::UnknownTab { panel })?;
        if self.tabs.len() == 1 {
            return Err(TabError::LastTab { panel });
        }
        let mut next = self.tabs.clone();
        next.remove(index);
        let active = if index < self.active {
            // A tab before the active one shifted left; the same tab stays active.
            self.active - 1
        } else if index == self.active {
            // The successor slid into the closed slot; the last tab has none.
            index.min(next.len() - 1)
        } else {
            self.active
        };
        Self::transition(TabVerb::Close, next, active, self.slot, Some(panel))
    }

    /// Validates a candidate projection and stages it as a [`TabCommit`].
    ///
    /// [`TabSet::build`] runs the whole commit gate (projection invariants,
    /// then [`LayoutNode::validate_structure`], then [`validate_proposal`]), so
    /// a commit only exists for a candidate that passed it.
    fn transition(
        verb: TabVerb,
        tabs: Vec<Tab>,
        active: usize,
        slot: Option<Rect>,
        removed: Option<PanelId>,
    ) -> Result<TabCommit, TabError> {
        let next = Self::build(tabs, active, slot)?;
        let focus_target = next.active_panel();
        let layout = next.layout();
        Ok(TabCommit {
            verb,
            set: next,
            layout,
            focus_target,
            removed,
        })
    }
}

/// A validated tab transition that has not been persisted yet.
///
/// Holds the candidate [`TabSet`] and its projected tree. The only way to reach
/// the committed state is [`TabCommit::commit`], which refuses while focus
/// still names anything other than the activated tab (WS-INV-19).
#[derive(Clone, Debug, PartialEq)]
pub struct TabCommit {
    verb: TabVerb,
    set: TabSet,
    layout: LayoutNode,
    focus_target: PanelId,
    removed: Option<PanelId>,
}

impl TabCommit {
    /// The verb that produced this transition.
    #[must_use]
    pub fn verb(&self) -> TabVerb {
        self.verb
    }

    /// The candidate projection (not yet applied by the caller).
    #[must_use]
    pub fn next(&self) -> &TabSet {
        &self.set
    }

    /// The validated tree the candidate projection commits.
    ///
    /// Always one [`LayoutNode::Leaf`] — the activated projection — so this is
    /// the whole of what the layout format ever sees of a tab stack (decision 5).
    #[must_use]
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }

    /// The panel focus must name before this transition can be committed.
    #[must_use]
    pub fn focus_target(&self) -> PanelId {
        self.focus_target
    }

    /// The panel this transition detaches, for a close.
    #[must_use]
    pub fn removed(&self) -> Option<PanelId> {
        self.removed
    }

    /// Host-side consequences of applying this transition.
    #[must_use]
    pub fn effect(&self) -> TabEffect {
        TabEffect::FOCUS_AND_REDRAW
    }

    /// Hands out the committed state once focus has been re-homed.
    ///
    /// `focus` is the focus the caller has *already* applied, in the WS-INV-19
    /// order: re-home first, persist second. Passing the outgoing id (or
    /// `None`) fails with [`TabError::FocusNotRehomed`] and consumes nothing,
    /// so a dangling focus id can never reach a saved layout.
    ///
    /// # Errors
    ///
    /// [`TabError::FocusNotRehomed`] when `focus` is not
    /// [`TabCommit::focus_target`].
    pub fn commit(self, focus: Option<PanelId>) -> Result<CommittedTabs, TabError> {
        if focus != Some(self.focus_target) {
            return Err(TabError::FocusNotRehomed {
                expected: self.focus_target,
                actual: focus,
            });
        }
        Ok(CommittedTabs {
            verb: self.verb,
            set: self.set,
            layout: self.layout,
            focus: self.focus_target,
            removed: self.removed,
        })
    }
}

/// State a committed tab transition installs.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedTabs {
    verb: TabVerb,
    set: TabSet,
    layout: LayoutNode,
    focus: PanelId,
    removed: Option<PanelId>,
}

impl CommittedTabs {
    /// The verb that produced this state.
    #[must_use]
    pub fn verb(&self) -> TabVerb {
        self.verb
    }

    /// The projection to install.
    #[must_use]
    pub fn set(&self) -> &TabSet {
        &self.set
    }

    /// The tree to install for the tab slot.
    ///
    /// Always one [`LayoutNode::Leaf`] naming the active projection, so a
    /// save/restore cycle recovers *which* tab was active and nothing else: the
    /// inactive tabs are not persisted and restore rebuilds a single-tab set
    /// from this leaf (decision 5).
    #[must_use]
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }

    /// The focus this state was committed with.
    #[must_use]
    pub fn focus(&self) -> PanelId {
        self.focus
    }

    /// The panel this transition detached, for a close.
    #[must_use]
    pub fn removed(&self) -> Option<PanelId> {
        self.removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Size, SplitAxis};
    use crate::panel::CORE_OWNER_FAMILY;

    const PID: [u64; 4] = [11, 12, 13, 14];
    const VID: [u64; 4] = [101, 102, 103, 104];

    fn tab(index: usize) -> Tab {
        Tab::new(
            PanelId::new(PID[index]),
            View::new(ViewId::new(VID[index]), 80, 24),
        )
    }

    fn live(index: usize) -> impl PanelLiveness {
        let panel = PanelId::new(PID[index]);
        move |candidate: PanelId| candidate == panel
    }

    fn all_live() -> impl PanelLiveness {
        |candidate: PanelId| PID.contains(&candidate.get())
    }

    /// Builds a strip in insertion order with the last tab active, which is
    /// what opening the tabs one by one produces.
    fn set_of(indices: &[usize]) -> TabSet {
        let tabs = indices.iter().map(|index| tab(*index)).collect();
        TabSet::new(tabs, indices.len() - 1).expect("valid tab set")
    }

    fn panels(set: &TabSet) -> Vec<PanelId> {
        set.tabs().iter().map(Tab::panel).collect()
    }

    fn pid(index: usize) -> PanelId {
        PanelId::new(PID[index])
    }

    /// Grid extent of a slot rectangle.
    fn dims(rect: Rect) -> Size {
        Size::new(rect.width, rect.height)
    }

    // -- command surface ---------------------------------------------------

    #[test]
    fn tab_verbs_parse_under_the_existing_grammar() {
        for raw in TAB_CMDS {
            let qc = QualifiedCommand::parse(raw).expect("core tab verb must parse");
            assert_eq!(qc.as_str(), raw);
        }
        // The issue's shorthand is not a qualified name: no parallel namespace.
        assert!(QualifiedCommand::parse("tab.open").is_err());
        assert!(QualifiedCommand::parse("tab:open").is_err());
    }

    #[test]
    fn tab_verbs_register_as_core_commands() {
        let mut registry = CommandRegistry::new();
        let registered = register_tab_verbs(&mut registry).expect("fresh registry accepts them");
        assert_eq!(registered.len(), 3);
        for raw in TAB_CMDS {
            assert!(registry.is_core(raw), "{raw} must be a Core verb");
            // Core verbs are not panel-owned, so the panel lane sees no owner.
            assert!(registry.owner_of(raw).is_none());
        }
        assert_eq!(registry.core_commands().len(), 3);
    }

    #[test]
    fn core_registration_rejects_duplicates_from_both_lanes() {
        let mut registry = CommandRegistry::new();
        register_tab_verbs(&mut registry).expect("first registration");
        let err = registry
            .register_core(TAB_CMD_OPEN)
            .expect_err("duplicate Core verb");
        assert_eq!(
            err,
            CommandError::CoreDuplicate {
                command: TAB_CMD_OPEN.to_string()
            }
        );
        // A panel cannot take a name Core owns.
        let err = registry
            .register(PanelId::new(7), TAB_CMD_CLOSE)
            .expect_err("panel may not steal a Core verb");
        assert_eq!(
            err,
            CommandError::CoreDuplicate {
                command: TAB_CMD_CLOSE.to_string()
            }
        );
    }

    #[test]
    fn core_registration_keeps_the_per_owner_cap() {
        let mut registry = CommandRegistry::new();
        for index in 0..(crate::panel::MAX_COMMANDS_PER_PANEL_TYPE - TAB_CMDS.len()) {
            registry
                .register_core(&format!("core.tab:extra{index}"))
                .expect("under the cap");
        }
        register_tab_verbs(&mut registry).expect("the three verbs still fit");
        let err = registry
            .register_core("core.tab:overflow")
            .expect_err("cap exceeded");
        assert_eq!(
            err,
            CommandError::TooManyCommands {
                max: crate::panel::MAX_COMMANDS_PER_PANEL_TYPE,
                current: crate::panel::MAX_COMMANDS_PER_PANEL_TYPE,
            }
        );
    }

    #[test]
    fn panel_lane_cannot_register_a_core_namespaced_name() {
        // The boundary is the namespace, not the list of verbs Core happens to
        // have registered: a panel can mint neither a future `core.tab:*` verb
        // nor `core.view:split` (a real ctl request id, so a panel command with
        // that spelling would be indistinguishable from a Core surface id).
        let mut registry = CommandRegistry::new();
        register_tab_verbs(&mut registry).expect("Core verbs register");
        let panel = PanelId::new(7);
        for raw in ["core.tab:reorder", "core.view:split"] {
            let err = registry
                .register(panel, raw)
                .expect_err("the core. owner prefix is Core's alone");
            assert_eq!(
                err,
                CommandError::CoreDuplicate {
                    command: raw.to_string()
                },
                "{raw} must be refused as Core-reserved, not as a panel duplicate"
            );
        }
        // A refusal registers nothing and leaves Core untouched.
        assert_eq!(registry.owner_of("core.tab:reorder"), None);
        assert_eq!(registry.owner_of("core.view:split"), None);
        assert!(!registry.is_core("core.tab:reorder"));
        assert!(!registry.is_core("core.view:split"));
        assert!(registry.commands_for(panel).is_empty());
        assert_eq!(registry.len(), TAB_CMDS.len());
    }

    #[test]
    fn core_tab_verbs_still_register_after_the_namespace_reservation() {
        // The reservation constrains the panel lane only: Core keeps its three
        // verbs on the same registry, through the same `register_core` path.
        let mut registry = CommandRegistry::new();
        let registered = register_tab_verbs(&mut registry).expect("Core keeps its own names");
        assert_eq!(registered.len(), TAB_CMDS.len());
        for (qc, raw) in registered.iter().zip(TAB_CMDS) {
            assert_eq!(qc.as_str(), raw);
            assert_eq!(qc.owner_family(), CORE_OWNER_FAMILY);
            assert!(qc.is_core_namespace());
            assert!(
                registry.is_core(raw),
                "{raw} must be a registered Core verb"
            );
            assert!(registry.owner_of(raw).is_none(), "{raw} is not panel-owned");
        }
        assert_eq!(registry.core_commands().len(), TAB_CMDS.len());
        assert_eq!(registry.len(), TAB_CMDS.len());
    }

    #[test]
    fn panel_lane_still_registers_non_core_names() {
        // An owner-family check, not a substring match: only the exact `core`
        // family is reserved, and duplicate semantics are unchanged.
        let mut registry = CommandRegistry::new();
        register_tab_verbs(&mut registry).expect("Core verbs register");
        let panel = PanelId::new(7);
        for raw in ["xuepoo.git:open", "corex.tab:reorder", "panel.core:open"] {
            let qc = registry
                .register(panel, raw)
                .expect("a non-core name still registers");
            assert_eq!(qc.as_str(), raw);
            assert!(!qc.is_core_namespace());
            assert_eq!(registry.owner_of(raw), Some(panel));
        }
        assert_eq!(registry.commands_for(panel).len(), 3);
        // Cross-panel duplicates are still reported as panel duplicates, not as
        // Core collisions.
        let err = registry
            .register(PanelId::new(8), "xuepoo.git:open")
            .expect_err("duplicate across panels");
        assert_eq!(
            err,
            CommandError::Duplicate {
                command: "xuepoo.git:open".to_string(),
                owner: panel,
            }
        );
    }

    // -- projection invariants --------------------------------------------

    #[test]
    fn the_commit_gate_refuses_a_stack_shaped_candidate() {
        // `layout()` emits a single leaf, so no `TabSet` produces this tree
        // today. The gate must still refuse it: that is what makes
        // "the committed tree is Stack-free" a contract instead of an
        // accident of `layout`.
        let candidate = LayoutNode::stack(vec![
            LayoutNode::leaf(tab(0).view().clone()),
            LayoutNode::leaf(tab(1).view().clone()),
        ]);
        let projected = vec![ViewId::new(VID[0]), ViewId::new(VID[1])];
        let err = TabSet::validate_candidate(&candidate, &projected)
            .expect_err("a Stack interior is not a committable tab layout");
        assert!(
            matches!(
                err,
                TabError::Proposal(LayoutError::UnsupportedNode { kind: "Stack" })
            ),
            "the proposal validator must refuse the Stack, got {err:?}"
        );
    }

    #[test]
    fn the_commit_gate_refuses_a_tree_that_disagrees_with_the_projection() {
        // Both directions of the set equality, over structurally valid trees.
        // Only `validate_proposal` can see either, so this is the failing route
        // the call exists for.
        let leaf_only = LayoutNode::leaf(tab(0).view().clone());
        let err =
            TabSet::validate_candidate(&leaf_only, &[ViewId::new(VID[0]), ViewId::new(VID[1])])
                .expect_err("the tree must carry every projected view");
        assert!(
            matches!(err, TabError::Proposal(LayoutError::MissingView { id }) if id == VID[1]),
            "a tree that omits a projected view must be refused, got {err:?}"
        );

        let two_leaves = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(tab(0).view().clone()),
            LayoutNode::leaf(tab(1).view().clone()),
        );
        let err = TabSet::validate_candidate(&two_leaves, &[ViewId::new(VID[0])])
            .expect_err("the tree must carry no leaf outside the projection");
        assert!(
            matches!(err, TabError::Proposal(LayoutError::UnexpectedView { id }) if id == VID[1]),
            "a tree that invents a leaf must be refused, got {err:?}"
        );

        // The agreeing case still passes, so the test is not asserting that the
        // gate always fails.
        TabSet::validate_candidate(&two_leaves, &[ViewId::new(VID[0]), ViewId::new(VID[1])])
            .expect("a tree that matches the projection passes");
    }

    #[test]
    fn tab_set_rejects_empty_and_duplicate_projections() {
        assert_eq!(
            TabSet::new(Vec::new(), 0).expect_err("empty"),
            TabError::EmptyStack
        );
        let duplicate_panel = TabSet::new(
            vec![
                tab(0),
                Tab::new(PanelId::new(PID[0]), View::new(ViewId::new(VID[1]), 80, 24)),
            ],
            0,
        )
        .expect_err("one panel projects once");
        assert_eq!(
            duplicate_panel,
            TabError::DuplicatePanel {
                panel: PanelId::new(PID[0])
            }
        );
        let duplicate_view = TabSet::new(
            vec![
                tab(0),
                Tab::new(PanelId::new(PID[1]), View::new(ViewId::new(VID[0]), 80, 24)),
            ],
            0,
        )
        .expect_err("one view paints one tab");
        assert_eq!(
            duplicate_view,
            TabError::DuplicateView {
                view: ViewId::new(VID[0])
            }
        );
    }

    #[test]
    fn tab_set_rejects_an_active_index_outside_the_strip() {
        // The active tab is an explicit index, so an out-of-range one has no
        // projection to lay out and must be refused rather than clamped.
        let tabs = vec![tab(0), tab(1)];
        assert_eq!(
            TabSet::new(tabs.clone(), 2).expect_err("past the end"),
            TabError::ActiveOutOfRange { index: 2, len: 2 }
        );
        // The last valid index is accepted, so the test is not rejecting the
        // whole range.
        let set = TabSet::new(tabs, 1).expect("the last index is valid");
        assert_eq!(set.active_index(), 1);
        assert_eq!(set.active_panel(), PanelId::new(PID[1]));
    }

    #[test]
    fn tab_set_rejects_a_stack_over_the_cap() {
        let tabs = (0..=MAX_TABS_PER_STACK)
            .map(|index| {
                Tab::new(
                    PanelId::new(1000 + index as u64),
                    View::new(ViewId::new(10_000 + index as u64), 80, 24),
                )
            })
            .collect();
        assert_eq!(
            TabSet::new(tabs, 0).expect_err("over the cap"),
            TabError::TooManyTabs {
                cap: MAX_TABS_PER_STACK,
                requested: MAX_TABS_PER_STACK + 1,
            }
        );
    }

    #[test]
    fn layout_projects_only_the_active_leaf() {
        // The committed tree is Stack-free because `validate_proposal`
        // rejects Stack interiors; the other tabs stay in the projection.
        let set = set_of(&[0, 1, 2]);
        let layout = set.layout();
        assert_eq!(layout, LayoutNode::leaf(tab(2).view().clone()));
        assert_eq!(set.projected_views(), vec![ViewId::new(VID[2])]);
        assert_eq!(
            set.view_ids(),
            vec![
                ViewId::new(VID[0]),
                ViewId::new(VID[1]),
                ViewId::new(VID[2])
            ]
        );
        set.validate().expect("a well-formed projection validates");
    }

    #[test]
    fn validation_rejects_a_projection_the_tree_cannot_carry() {
        // Two tabs sharing one leaf is invisible to the single-leaf tree, so
        // the projection-level rule has to catch it.
        let mut set = set_of(&[0, 1]);
        set.tabs[1] = Tab::new(PanelId::new(PID[1]), set.tabs[0].view().clone());
        assert_eq!(
            set.validate().expect_err("duplicate leaf"),
            TabError::DuplicateView {
                view: ViewId::new(VID[0])
            }
        );
    }

    // -- open --------------------------------------------------------------

    #[test]
    fn open_appends_and_activates() {
        let set = set_of(&[0]);
        let commit = set.open(tab(1), &all_live()).expect("open is valid");
        assert_eq!(commit.verb(), TabVerb::Open);
        assert_eq!(commit.next().len(), 2);
        assert_eq!(commit.next().active_panel(), PanelId::new(PID[1]));
        assert_eq!(commit.focus_target(), PanelId::new(PID[1]));
        assert_eq!(commit.removed(), None);
        assert_eq!(*commit.layout(), LayoutNode::leaf(tab(1).view().clone()));
        // The source set is untouched: verbs do not mutate.
        assert_eq!(set.len(), 1);
        assert_eq!(set.active_panel(), PanelId::new(PID[0]));
    }

    #[test]
    fn open_rejects_a_dead_generation_and_a_duplicate() {
        let set = set_of(&[0, 1]);
        assert_eq!(
            set.open(tab(2), &live(1)).expect_err("dead panel"),
            TabError::GenerationGone {
                panel: PanelId::new(PID[2])
            }
        );
        assert_eq!(
            set.open(tab(1), &all_live())
                .expect_err("already projected"),
            TabError::AlreadyOpen {
                panel: PanelId::new(PID[1])
            }
        );
    }

    // -- switch ------------------------------------------------------------

    #[test]
    fn switch_keeps_the_strip_in_insertion_order() {
        // Decision 3: activation moves the index, not the tab. Pre-fix,
        // `switch` moved the target to the end of the vector, which read as a
        // user-visible reorder of the strip.
        let set = set_of(&[0, 1, 2]);
        assert_eq!(set.active_index(), 2);
        let commit = set
            .switch(PanelId::new(PID[0]), &all_live())
            .expect("switch");
        assert_eq!(commit.verb(), TabVerb::Switch);
        assert_eq!(commit.focus_target(), PanelId::new(PID[0]));
        assert_eq!(
            panels(commit.next()),
            vec![pid(0), pid(1), pid(2)],
            "switch must not reorder the strip"
        );
        assert_eq!(commit.next().active_index(), 0);
        assert_eq!(commit.next().active_panel(), PanelId::new(PID[0]));
        assert_eq!(*commit.layout(), LayoutNode::leaf(tab(0).view().clone()));

        // Switching again keeps the order too, and the previous active tab
        // stays where it was.
        let back = commit
            .next()
            .switch(PanelId::new(PID[1]), &all_live())
            .expect("switch to the middle tab");
        assert_eq!(panels(back.next()), vec![pid(0), pid(1), pid(2)]);
        assert_eq!(back.next().active_index(), 1);
        assert_eq!(back.focus_target(), PanelId::new(PID[1]));
        // The source set is untouched.
        assert_eq!(set.active_panel(), PanelId::new(PID[2]));
        assert_eq!(panels(&set), vec![pid(0), pid(1), pid(2)]);
    }

    #[test]
    fn returning_to_a_background_tab_uses_the_current_slot_geometry() {
        // Decision 4: background projections are resized with the strip, so a
        // tab that was inactive for the whole resize comes back at the current
        // grid size instead of the geometry it had when it was last active.
        let mut set = set_of(&[0, 1, 2]);
        let slot = Rect::new(0, 0, 120, 40);
        assert!(
            set.fit_slot(slot),
            "a 120x40 slot resizes the 80x24 projections"
        );
        assert_eq!(set.slot_rect(), Some(slot));
        for tab in set.tabs() {
            assert_eq!(tab.view().size(), dims(slot), "every projection fits");
        }
        // Refitting the same slot changes nothing.
        assert!(!set.fit_slot(slot));

        // Tab 0 has been the background tab for the whole resize.
        let commit = set
            .switch(PanelId::new(PID[0]), &all_live())
            .expect("switch back to the first tab");
        let reactivated = commit
            .next()
            .find(PanelId::new(PID[0]))
            .expect("tab 0 is still projected");
        assert_eq!(reactivated.view().size(), dims(slot));
        match commit.layout() {
            LayoutNode::Leaf(leaf) => assert_eq!(
                leaf.size(),
                dims(slot),
                "the committed leaf must carry the current geometry, not the stale one"
            ),
            other => panic!("a tab layout is a single leaf, got {other:?}"),
        }

        // The resize reaches the tab that is inactive *now* as well: fitting
        // while tab 0 is active must reach tab 2.
        let smaller = Rect::new(0, 0, 100, 30);
        assert!(set.fit_slot(smaller));
        assert_eq!(
            set.find(PanelId::new(PID[2]))
                .expect("tab 2 is projected")
                .view()
                .size(),
            dims(smaller)
        );
        assert_eq!(
            set.find(PanelId::new(PID[0]))
                .expect("tab 0 is projected")
                .view()
                .size(),
            dims(smaller)
        );
    }

    #[test]
    fn a_tab_opened_after_a_slot_fit_arrives_at_the_slot_geometry() {
        // The strip remembers its slot, so a projection that arrives later is
        // fitted too — otherwise `open` would reintroduce the stale-geometry
        // path through a different verb.
        let mut set = set_of(&[0, 1]);
        let slot = Rect::new(0, 0, 132, 44);
        set.fit_slot(slot);
        let commit = set.open(tab(2), &all_live()).expect("open");
        let opened = commit
            .next()
            .find(PanelId::new(PID[2]))
            .expect("the new tab is projected");
        assert_eq!(opened.view().size(), dims(slot));
        assert_eq!(commit.next().slot_rect(), Some(slot));
        match commit.layout() {
            LayoutNode::Leaf(leaf) => assert_eq!(leaf.size(), dims(slot)),
            other => panic!("a tab layout is a single leaf, got {other:?}"),
        }
    }

    #[test]
    fn fitting_the_slot_changes_no_identity_order_or_active_tab() {
        // `fit_slot` is a geometry stamp, not a verb: it must not be able to
        // reorder the strip, move the active tab, or mint a commit.
        let mut set = set_of(&[0, 1, 2]);
        let before = panels(&set);
        let active_before = set.active_panel();
        assert!(set.fit_slot(Rect::new(3, 1, 90, 20)));
        assert_eq!(panels(&set), before);
        assert_eq!(set.active_index(), 2);
        assert_eq!(set.active_panel(), active_before);
        set.validate().expect("a fitted strip is still valid");
        assert_eq!(
            set.layout().leaf_ids(),
            vec![ViewId::new(VID[2])],
            "the layout still names only the active projection"
        );
        assert_eq!(set.len(), 3, "a fit detaches nothing");
    }

    #[test]
    fn multi_tab_strips_are_not_persisted() {
        // Decision 5, the explicit contract: the layout format carries the
        // committed tree, and a tab stack's committed tree is one leaf. A strip
        // of n tabs therefore persists as the active tab alone; restore drops
        // the other n - 1 tabs by contract and no persisted field records them.
        let set = set_of(&[0, 1, 2]);
        let committed = set
            .switch(PanelId::new(PID[0]), &all_live())
            .expect("switch")
            .commit(Some(PanelId::new(PID[0])))
            .expect("focus was re-homed first");
        let layout = committed.layout();
        assert_eq!(
            layout.leaf_count(),
            1,
            "the persisted tree carries exactly one projection"
        );
        assert_eq!(layout.leaf_ids(), vec![ViewId::new(VID[0])]);
        for inactive in [VID[1], VID[2]] {
            assert!(
                !layout.leaf_ids().contains(&ViewId::new(inactive)),
                "inactive view {} must not reach the layout format",
                ViewId::new(inactive)
            );
        }

        // Restore sees only that leaf, so it rebuilds a single-tab set.
        let LayoutNode::Leaf(restored_leaf) = layout.clone() else {
            panic!("a tab layout is a single leaf");
        };
        let restored = TabSet::single(Tab::new(PanelId::new(PID[0]), restored_leaf));
        assert_eq!(
            restored.len(),
            1,
            "restore deliberately rebuilds one tab, not the strip"
        );
        assert_eq!(restored.active_panel(), PanelId::new(PID[0]));
        assert_eq!(
            restored.layout(),
            *layout,
            "the restored leaf is the committed leaf"
        );
        // ... and the strip is not recoverable from it: the dropped tabs have
        // no representation anywhere in the committed tree.
        assert!(restored.find(PanelId::new(PID[1])).is_none());
        assert!(restored.find(PanelId::new(PID[2])).is_none());
    }

    #[test]
    fn switch_reports_every_typed_refusal() {
        let single = set_of(&[0]);
        assert_eq!(
            single
                .switch(PanelId::new(PID[0]), &all_live())
                .expect_err("single tab"),
            TabError::SingleTab {
                panel: PanelId::new(PID[0])
            }
        );
        let set = set_of(&[0, 1]);
        assert_eq!(
            set.switch(PanelId::new(PID[3]), &all_live())
                .expect_err("unknown"),
            TabError::UnknownTab {
                panel: PanelId::new(PID[3])
            }
        );
        assert_eq!(
            set.switch(PanelId::new(PID[1]), &all_live())
                .expect_err("already active"),
            TabError::AlreadyActive {
                panel: PanelId::new(PID[1])
            }
        );
        assert_eq!(
            set.switch(PanelId::new(PID[0]), &live(1))
                .expect_err("dead generation"),
            TabError::GenerationGone {
                panel: PanelId::new(PID[0])
            }
        );
    }

    // -- close -------------------------------------------------------------

    #[test]
    fn close_activates_the_neighbour_that_takes_the_closed_slot() {
        // Decision 3: the strip is insertion order, so closing the active tab
        // must not jump the selection to the far end of it.
        let set = set_of(&[0, 1, 2]);
        let middle = set
            .switch(PanelId::new(PID[1]), &all_live())
            .expect("activate the middle tab")
            .next()
            .clone();
        assert_eq!(middle.active_index(), 1);
        let closed = middle.close(PanelId::new(PID[1])).expect("close it");
        // The successor slides into the closed slot and keeps the selection.
        assert_eq!(panels(closed.next()), vec![pid(0), pid(2)]);
        assert_eq!(closed.next().active_index(), 1);
        assert_eq!(closed.focus_target(), PanelId::new(PID[2]));

        // Closing the last tab has no successor, so the previous tab takes over.
        let last = middle
            .close(PanelId::new(PID[2]))
            .expect("close the last tab");
        assert_eq!(panels(last.next()), vec![pid(0), pid(1)]);
        assert_eq!(last.next().active_index(), 1);
        assert_eq!(last.focus_target(), PanelId::new(PID[1]));

        // Closing before the active tab keeps the same tab active, shifted down.
        let before = middle
            .close(PanelId::new(PID[0]))
            .expect("close a tab before it");
        assert_eq!(panels(before.next()), vec![pid(1), pid(2)]);
        assert_eq!(before.next().active_index(), 0);
        assert_eq!(before.focus_target(), PanelId::new(PID[1]));
    }

    #[test]
    fn close_detaches_and_re_homes_focus_to_the_new_active_tab() {
        let set = set_of(&[0, 1, 2]);
        let commit = set.close(PanelId::new(PID[2])).expect("close active");
        assert_eq!(commit.verb(), TabVerb::Close);
        assert_eq!(commit.removed(), Some(PanelId::new(PID[2])));
        assert_eq!(commit.focus_target(), PanelId::new(PID[1]));
        assert_eq!(commit.next().len(), 2);
        assert_eq!(panels(commit.next()), vec![pid(0), pid(1)]);
        assert!(
            commit.next().find(PanelId::new(PID[2])).is_none(),
            "the closed projection is gone"
        );
        let committed = commit
            .commit(Some(PanelId::new(PID[1])))
            .expect("focus was re-homed first");
        assert_eq!(committed.focus(), PanelId::new(PID[1]));
        assert_eq!(committed.removed(), Some(PanelId::new(PID[2])));

        // Closing a background tab keeps the active tab active and the order.
        let background = set.close(PanelId::new(PID[0])).expect("close background");
        assert_eq!(background.focus_target(), PanelId::new(PID[2]));
        assert_eq!(background.removed(), Some(PanelId::new(PID[0])));
        assert_eq!(panels(background.next()), vec![pid(1), pid(2)]);
    }

    #[test]
    fn close_of_a_dead_generation_is_the_repair_path() {
        let set = set_of(&[0, 1]);
        // `switch` refuses the dead panel; `close` can still detach it.
        assert!(set.switch(PanelId::new(PID[0]), &live(1)).is_err());
        let commit = set.close(PanelId::new(PID[0])).expect("stale tab closes");
        assert_eq!(commit.removed(), Some(PanelId::new(PID[0])));
    }

    #[test]
    fn close_refuses_the_last_tab_and_unknown_tabs() {
        let single = set_of(&[0]);
        assert_eq!(
            single.close(PanelId::new(PID[0])).expect_err("last tab"),
            TabError::LastTab {
                panel: PanelId::new(PID[0])
            }
        );
        let set = set_of(&[0, 1]);
        assert_eq!(
            set.close(PanelId::new(PID[3])).expect_err("unknown"),
            TabError::UnknownTab {
                panel: PanelId::new(PID[3])
            }
        );
    }

    // -- atomicity and ordering -------------------------------------------

    #[test]
    fn a_failed_verb_leaves_the_layout_and_focus_untouched() {
        let set = set_of(&[0, 1]);
        let layout_before = set.layout();
        let focus_before = set.active_panel();
        assert!(set.switch(PanelId::new(PID[3]), &all_live()).is_err());
        assert!(set.close(PanelId::new(PID[3])).is_err());
        assert!(set.open(tab(0), &all_live()).is_err());
        assert_eq!(set.layout(), layout_before);
        assert_eq!(set.active_panel(), focus_before);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn commit_refuses_while_focus_still_names_the_outgoing_tab() {
        let set = set_of(&[0, 1]);
        let commit = set.close(PanelId::new(PID[1])).expect("close");
        let err = commit
            .clone()
            .commit(Some(PanelId::new(PID[1])))
            .expect_err("dangling focus must not commit");
        assert_eq!(
            err,
            TabError::FocusNotRehomed {
                expected: PanelId::new(PID[0]),
                actual: Some(PanelId::new(PID[1]))
            }
        );
        let err = commit.clone().commit(None).expect_err("no focus");
        assert_eq!(
            err,
            TabError::FocusNotRehomed {
                expected: PanelId::new(PID[0]),
                actual: None
            }
        );
        assert!(commit.commit(Some(PanelId::new(PID[0]))).is_ok());
    }

    #[test]
    fn every_verb_re_attributes_input_modes_and_requests_a_full_redraw() {
        let set = set_of(&[0, 1]);
        let open = set.open(tab(2), &all_live()).expect("open");
        let switch = set
            .switch(PanelId::new(PID[0]), &all_live())
            .expect("switch");
        let close = set.close(PanelId::new(PID[0])).expect("close");
        for commit in [open, switch, close] {
            assert_eq!(commit.effect(), TabEffect::FOCUS_AND_REDRAW);
            assert!(commit.effect().reattribute_input_mode);
            assert!(commit.effect().pending_full_redraw);
        }
    }

    #[test]
    fn liveness_oracle_covers_slices_and_closures() {
        let slice: &[PanelId] = &[PanelId::new(PID[0])];
        assert!(PanelLiveness::is_live(slice, PanelId::new(PID[0])));
        assert!(!PanelLiveness::is_live(slice, PanelId::new(PID[1])));
        let predicate = all_live();
        assert!(predicate.is_live(PanelId::new(PID[3])));
        assert!(!predicate.is_live(PanelId::new(999)));
    }
}
