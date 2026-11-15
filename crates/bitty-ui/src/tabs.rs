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
//! 3. **The active tab lives in this projection, and the layout names it.**
//!    [`TabSet`] is the Core-owned per-window projection; the committed tree
//!    holds the active leaf and nothing else, so a save/restore cycle that
//!    persists the layout still recovers *which* tab was active without any new
//!    persisted field. The ordered set of inactive tabs is projection state,
//!    not layout state; persisting a multi-tab strip would change the layout
//!    format and is a separate slice.
//! 4. **`tab.switch` re-attributes input-mode caches and forces a full
//!    redraw.** A switch moves focus to the activated tab exactly like the
//!    WS-INV-19 re-home (crash-safe ordering: re-home before persist), so the
//!    host must run `sync_mode_caches_to_focus` for the new focus, and the
//!    visible surface changes wholesale, so nothing partial is repaintable.
//!    [`TabEffect`] carries both facts; [`TabCommit::commit`] refuses to hand
//!    out committed state while focus still names the outgoing tab.
//! 5. **The `workspaceline` exclusive claim stays exactly as it is.** It gates
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
//! unique views, non-empty, capped). Any failure returns a [`TabError`] and the
//! caller's [`TabSet`] and focus are untouched, so there is no partial
//! mutation and no orphaned leaf.
//!
//! Deterministic and headless; this module adds no crate dependency.

#![forbid(unsafe_code)]

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabEffect {
    /// The host must re-attribute input-mode caches to the new focus.
    pub reattribute_input_mode: bool,
    /// The host must set `pending_full_redraw`.
    pub pending_full_redraw: bool,
}

impl TabEffect {
    /// The effect every tab transition has (decision 4).
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
/// - the **last** tab is the active one, so [`TabSet::layout`] can project the
///   active leaf directly and `Stack`'s paint-order convention needs no active
///   index.
///
/// The strip order is the vector order; switching moves the target to the end,
/// which is the active position. Nothing here is persisted in the layout
/// format (see the module decision record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSet {
    tabs: Vec<Tab>,
}

impl TabSet {
    /// Builds a tab set from `tabs` in strip order; the last entry is active.
    ///
    /// # Errors
    ///
    /// [`TabError::EmptyStack`], [`TabError::TooManyTabs`],
    /// [`TabError::DuplicatePanel`], or [`TabError::DuplicateView`].
    pub fn new(tabs: Vec<Tab>) -> Result<Self, TabError> {
        let set = Self { tabs };
        set.validate()?;
        Ok(set)
    }

    /// Builds a single-tab set.
    #[must_use]
    pub fn single(tab: Tab) -> Self {
        Self { tabs: vec![tab] }
    }

    /// Tabs in strip order; the last entry is the active tab.
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

    /// The active tab (the last entry).
    #[must_use]
    pub fn active(&self) -> &Tab {
        // `new` guarantees non-empty; `single` constructs one element.
        &self.tabs[self.tabs.len().saturating_sub(1)]
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
    /// # Errors
    ///
    /// [`TabError::GenerationGone`] when the panel is not live,
    /// [`TabError::AlreadyOpen`] when the panel already has a tab, or any
    /// [`TabError`] from [`TabSet::validate`] on the candidate projection.
    pub fn open(&self, tab: Tab, live: &dyn PanelLiveness) -> Result<TabCommit, TabError> {
        let panel = tab.panel;
        if !live.is_live(panel) {
            return Err(TabError::GenerationGone { panel });
        }
        if self.index_of(panel).is_some() {
            return Err(TabError::AlreadyOpen { panel });
        }
        let mut next = self.tabs.clone();
        next.push(tab);
        Self::transition(TabVerb::Open, next, None)
    }

    /// Switches the active tab to `panel`.
    ///
    /// The target moves to the end of the strip, which is the active position;
    /// every other tab keeps its relative order.
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
        if index + 1 == self.tabs.len() {
            return Err(TabError::AlreadyActive { panel });
        }
        let mut next = self.tabs.clone();
        let tab = next.remove(index);
        next.push(tab);
        Self::transition(TabVerb::Switch, next, None)
    }

    /// Closes the tab projecting `panel`.
    ///
    /// Closing detaches one projection; it never destroys the panel, whose
    /// lifecycle belongs to the panel registry. When the closed tab was active
    /// the new last tab becomes active, which is the focus target
    /// [`TabCommit::focus_target`] reports.
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
        Self::transition(TabVerb::Close, next, Some(panel))
    }

    /// Validates a candidate projection and stages it as a [`TabCommit`].
    ///
    /// `Self::new` runs the whole commit gate (projection invariants, then
    /// [`LayoutNode::validate_structure`], then [`validate_proposal`]), so a
    /// commit only exists for a candidate that passed it.
    fn transition(
        verb: TabVerb,
        tabs: Vec<Tab>,
        removed: Option<PanelId>,
    ) -> Result<TabCommit, TabError> {
        let next = Self::new(tabs)?;
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
    use crate::geometry::SplitAxis;
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

    fn set_of(indices: &[usize]) -> TabSet {
        TabSet::new(indices.iter().map(|index| tab(*index)).collect()).expect("valid tab set")
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
            TabSet::new(Vec::new()).expect_err("empty"),
            TabError::EmptyStack
        );
        let duplicate_panel = TabSet::new(vec![
            tab(0),
            Tab::new(PanelId::new(PID[0]), View::new(ViewId::new(VID[1]), 80, 24)),
        ])
        .expect_err("one panel projects once");
        assert_eq!(
            duplicate_panel,
            TabError::DuplicatePanel {
                panel: PanelId::new(PID[0])
            }
        );
        let duplicate_view = TabSet::new(vec![
            tab(0),
            Tab::new(PanelId::new(PID[1]), View::new(ViewId::new(VID[0]), 80, 24)),
        ])
        .expect_err("one view paints one tab");
        assert_eq!(
            duplicate_view,
            TabError::DuplicateView {
                view: ViewId::new(VID[0])
            }
        );
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
            TabSet::new(tabs).expect_err("over the cap"),
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
    fn switch_moves_the_target_to_the_active_position() {
        let set = set_of(&[0, 1, 2]);
        let commit = set
            .switch(PanelId::new(PID[0]), &all_live())
            .expect("switch");
        assert_eq!(commit.verb(), TabVerb::Switch);
        assert_eq!(commit.focus_target(), PanelId::new(PID[0]));
        let order: Vec<PanelId> = commit.next().tabs().iter().map(Tab::panel).collect();
        assert_eq!(
            order,
            vec![
                PanelId::new(PID[1]),
                PanelId::new(PID[2]),
                PanelId::new(PID[0])
            ]
        );
        assert_eq!(*commit.layout(), LayoutNode::leaf(tab(0).view().clone()));
        assert_eq!(set.active_panel(), PanelId::new(PID[2]));
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
    fn close_detaches_and_re_homes_focus_to_the_new_active_tab() {
        let set = set_of(&[0, 1, 2]);
        let commit = set.close(PanelId::new(PID[2])).expect("close active");
        assert_eq!(commit.verb(), TabVerb::Close);
        assert_eq!(commit.removed(), Some(PanelId::new(PID[2])));
        assert_eq!(commit.focus_target(), PanelId::new(PID[1]));
        assert_eq!(commit.next().len(), 2);
        assert!(
            commit.next().find(PanelId::new(PID[2])).is_none(),
            "the closed projection is gone"
        );
        let committed = commit
            .commit(Some(PanelId::new(PID[1])))
            .expect("focus was re-homed first");
        assert_eq!(committed.focus(), PanelId::new(PID[1]));
        assert_eq!(committed.removed(), Some(PanelId::new(PID[2])));

        // Closing a background tab keeps the active tab active.
        let background = set.close(PanelId::new(PID[0])).expect("close background");
        assert_eq!(background.focus_target(), PanelId::new(PID[2]));
        assert_eq!(background.removed(), Some(PanelId::new(PID[0])));
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
