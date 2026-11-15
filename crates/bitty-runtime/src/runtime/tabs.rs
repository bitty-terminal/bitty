//! Per-window tab projection host (CTX-1836, bitty#3169).
//!
//! `crate::registry::panel` registers the Core tab verbs
//! (`core.tab:open` / `core.tab:switch` / `core.tab:close`) on the per-window
//! command registry, and `bitty_ui::tabs` owns their whole transition algebra:
//! [`TabSet`] holds the projection, each verb returns a [`TabCommit`], and
//! [`TabCommit::commit`] refuses to hand out committed state while focus still
//! names the outgoing tab (WS-INV-19, [`bitty_ui::TabError::FocusNotRehomed`]).
//!
//! Until this module existed nothing invoked those verbs: the registry knew the
//! names and no host path could reach them (issue #3169). This is that host
//! path, and it is deliberately the *only* one:
//!
//! - every request becomes a [`TabCommit`] through [`TabSet::open`] /
//!   [`TabSet::switch`] / [`TabSet::close`], so the pre-commit gate
//!   (`validate_structure` + `validate_proposal` + the projection rules) runs
//!   exactly once per request;
//! - focus is re-homed through `Runtime::set_focus` *before* the commit, and
//!   the commit is presented with the focus read back from the runtime, so a
//!   re-home that did not land refuses with `FocusNotRehomed` instead of
//!   persisting a dangling focus id;
//! - the strip is installed only from [`bitty_ui::CommittedTabs`], so there is
//!   no second mutation path and no direct tab or layout edit anywhere in this
//!   file. Nothing here touches the layout tree: hosting and painting a strip
//!   is a separate slice (the `tabline` slot still fails closed as
//!   `UiSlotPlacement::Unsupported`).
//!
//! # What a tab is here
//!
//! A tab is 1:1 with a panel and the tab key is the [`PanelId`] (decision 1 in
//! `bitty_ui::tabs`), and the runtime keeps one panel per view id (`panel list`
//! reports `p:N` for the leaf `v:N`). The strip is therefore the live panel set
//! of this window: tiled leaves in layout order, then detached floats, then
//! pinned panels, all with their current grid geometry. It is projected lazily
//! on the first tab request, so a runtime that never uses tabs carries one
//! `None`.
//!
//! `open` is consequently *re-attaching* a projection: it opens a tab for a
//! live panel the strip does not project (the panel a previous `close`
//! detached), never a new shell. Minting a panel is the split/spawn verb, and a
//! tab verb that minted one would be a second creation path. When every live
//! panel is already projected the refusal is `AlreadyOpen`, and a panel that no
//! longer exists is `GenerationGone` — both typed, never a silent no-op.

use bitty_ui::{PanelId, PanelLiveness, Tab, TabCommit, TabError, TabSet, TabVerb, View, ViewId};

/// The live panel set as a [`PanelLiveness`] oracle.
///
/// A concrete wrapper because `&[PanelId]` cannot coerce to `&dyn
/// PanelLiveness` (the slice type is unsized); this keeps the verbs' oracle
/// argument a plain borrowed set with no allocation per request.
struct LivePanels<'a>(&'a [PanelId]);

impl PanelLiveness for LivePanels<'_> {
    fn is_live(&self, panel: PanelId) -> bool {
        self.0.contains(&panel)
    }
}

/// One host-level tab request.
///
/// The first three map 1:1 onto the Core verbs and are what the ctl surface
/// names. [`Self::Next`] and [`Self::OpenUnprojected`] are the argument-free
/// shapes a keymap chord can express (no chord carries a `p:N`), and they
/// resolve to the *same* verbs: `Next` is a `switch` to the next projection in
/// strip order and `OpenUnprojected` is an `open` of the first live panel the
/// strip does not project. No variant has its own mutation path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabRequest {
    /// `core.tab:open` for one panel (`bitty ctl tab open p:N`).
    Open(PanelId),
    /// `core.tab:switch` to one projected panel (`bitty ctl tab switch p:N`).
    Switch(PanelId),
    /// `core.tab:close` of one projected panel (`bitty ctl tab close p:N`).
    Close(PanelId),
    /// Switch to the next projection in strip order, wrapping at the end.
    Next,
    /// Open the first live panel the strip does not project.
    OpenUnprojected,
    /// Close the active projection (what a chord can express: no chord carries
    /// a `p:N`).
    CloseActive,
}

impl TabRequest {
    /// The Core verb this request runs, for receipts and diagnostics.
    #[must_use]
    pub fn verb(self) -> TabVerb {
        match self {
            Self::Open(_) | Self::OpenUnprojected => TabVerb::Open,
            Self::Switch(_) | Self::Next => TabVerb::Switch,
            Self::Close(_) | Self::CloseActive => TabVerb::Close,
        }
    }
}

/// What one committed tab request produced.
///
/// Built only from [`bitty_ui::CommittedTabs`], so a receipt can never describe
/// a transition that did not pass [`TabCommit::commit`]. The two effect flags
/// are [`bitty_ui::TabEffect::FOCUS_AND_REDRAW`] for every verb (decision 6):
/// the host re-attributes input-mode caches to the reported focus and asks for
/// one full redraw, because the visible surface changes wholesale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabOutcome {
    verb: TabVerb,
    focus: PanelId,
    active_index: usize,
    tabs: Vec<PanelId>,
    removed: Option<PanelId>,
    reattribute_input_mode: bool,
    pending_full_redraw: bool,
}

impl TabOutcome {
    /// Receipt for one committed transition.
    ///
    /// The effect flags are [`bitty_ui::TabEffect::FOCUS_AND_REDRAW`], the one
    /// effect every tab transition has (decision 6 in `bitty_ui::tabs`).
    /// `CommittedTabs` carries no effect accessor, so the constant is the
    /// contract this receipt publishes.
    fn from_committed(committed: &bitty_ui::CommittedTabs) -> Self {
        let set = committed.set();
        let effect = bitty_ui::TabEffect::FOCUS_AND_REDRAW;
        Self {
            verb: committed.verb(),
            focus: committed.focus(),
            active_index: set.active_index(),
            tabs: set.tabs().iter().map(Tab::panel).collect(),
            removed: committed.removed(),
            reattribute_input_mode: effect.reattribute_input_mode,
            pending_full_redraw: effect.pending_full_redraw,
        }
    }

    /// The verb that produced this state.
    #[must_use]
    pub fn verb(&self) -> TabVerb {
        self.verb
    }

    /// The panel focus names after the transition.
    #[must_use]
    pub fn focus(&self) -> PanelId {
        self.focus
    }

    /// Index of the active tab in strip order.
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// The strip in insertion order.
    #[must_use]
    pub fn tabs(&self) -> &[PanelId] {
        &self.tabs
    }

    /// The panel a close detached, if any.
    #[must_use]
    pub fn removed(&self) -> Option<PanelId> {
        self.removed
    }

    /// Whether the host must re-attribute input-mode caches to [`Self::focus`].
    #[must_use]
    pub fn reattribute_input_mode(&self) -> bool {
        self.reattribute_input_mode
    }

    /// Whether the host must request one full redraw.
    #[must_use]
    pub fn pending_full_redraw(&self) -> bool {
        self.pending_full_redraw
    }

    /// Receipt as the ctl `result_json` object.
    ///
    /// Field set is stable and additive-only, like every other ctl receipt:
    /// `verb`, `active` (`p:N`), `active_index`, `tabs` (`p:N` array),
    /// `removed` (`p:N` or `null`), `reattribute_input_mode`,
    /// `pending_full_redraw`.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut tabs = String::new();
        for (index, panel) in self.tabs.iter().enumerate() {
            if index > 0 {
                tabs.push(',');
            }
            tabs.push('"');
            tabs.push_str(&panel_ref(*panel));
            tabs.push('"');
        }
        let removed = match self.removed {
            Some(panel) => format!("\"{}\"", panel_ref(panel)),
            None => String::from("null"),
        };
        format!(
            "{{\"verb\":\"{}\",\"active\":\"{}\",\"active_index\":{},\"tabs\":[{}],\
             \"removed\":{},\"reattribute_input_mode\":{},\"pending_full_redraw\":{}}}",
            verb_name(self.verb),
            panel_ref(self.focus),
            self.active_index,
            tabs,
            removed,
            self.reattribute_input_mode,
            self.pending_full_redraw,
        )
    }
}

/// Canonical `p:N` reference for a panel (the identity `panel list` reports).
#[must_use]
pub fn panel_ref(panel: PanelId) -> String {
    format!("p:{}", panel.0)
}

/// Stable verb spelling used in receipts and diagnostics.
#[must_use]
pub const fn verb_name(verb: TabVerb) -> &'static str {
    match verb {
        TabVerb::Open => "open",
        TabVerb::Switch => "switch",
        TabVerb::Close => "close",
    }
}

/// Host-owned tab strip for one window.
///
/// One `Option<TabSet>` and nothing else: the strip is the projection, so there
/// is no parallel tab list to fall out of sync with it. `None` means no tab
/// request has run yet (the strip is projected on demand from the live panel
/// set, see the module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabHost {
    set: Option<TabSet>,
}

impl TabHost {
    /// The installed projection, if a tab request has run.
    #[must_use]
    pub fn snapshot(&self) -> Option<&TabSet> {
        self.set.as_ref()
    }

    /// Projects the strip from the live panel set on first use.
    ///
    /// `active` is the panel the window currently focuses; when it is not part
    /// of `panels` (a focus the projection cannot represent) the first
    /// projection becomes active, so the strip is always a valid [`TabSet`].
    fn project(
        &mut self,
        panels: &[(PanelId, View)],
        active: Option<PanelId>,
    ) -> Result<(), TabError> {
        if self.set.is_some() {
            return Ok(());
        }
        let tabs: Vec<Tab> = panels
            .iter()
            .map(|(panel, view)| Tab::new(*panel, view.clone()))
            .collect();
        let index = active
            .and_then(|focus| tabs.iter().position(|tab| tab.panel() == focus))
            .unwrap_or(0);
        self.set = Some(TabSet::new(tabs, index)?);
        Ok(())
    }

    /// Installs the state a committed transition handed out.
    fn install(&mut self, committed: &bitty_ui::CommittedTabs) {
        self.set = Some(committed.set().clone());
    }
}

/// Panel identity of a runtime view (the runtime keeps one panel per leaf).
#[must_use]
pub const fn panel_of_view(view: ViewId) -> PanelId {
    PanelId::new(view.0)
}

/// View identity of a panel.
#[must_use]
pub const fn view_of_panel(panel: PanelId) -> ViewId {
    ViewId::new(panel.0)
}

/// Candidate projection for one request.
///
/// Runs the verb against the installed strip and returns the staged commit.
/// Split out of [`TabHost`] so it stays a pure function of the projection and
/// the request: nothing here mutates the host, and the commit it returns is the
/// only way the caller reaches new state.
fn stage(
    set: &TabSet,
    panels: &[(PanelId, View)],
    live: &[PanelId],
    request: TabRequest,
) -> Result<TabCommit, TabError> {
    let liveness = LivePanels(live);
    match request {
        TabRequest::Open(panel) => set.open(tab_for(panel, panels)?, &liveness),
        TabRequest::Switch(panel) => set.switch(panel, &liveness),
        TabRequest::Close(panel) => set.close(panel),
        TabRequest::CloseActive => set.close(set.active_panel()),
        TabRequest::Next => {
            if set.len() < 2 {
                return Err(TabError::SingleTab {
                    panel: set.active_panel(),
                });
            }
            let next = (set.active_index() + 1) % set.len();
            set.switch(set.tabs()[next].panel(), &liveness)
        }
        TabRequest::OpenUnprojected => {
            let target = panels
                .iter()
                .map(|(panel, _)| *panel)
                .find(|panel| set.find(*panel).is_none())
                .ok_or_else(|| TabError::AlreadyOpen {
                    panel: set.active_panel(),
                })?;
            set.open(tab_for(target, panels)?, &liveness)
        }
    }
}

/// The projection a tab for `panel` carries, or the typed "gone" refusal.
fn tab_for(panel: PanelId, panels: &[(PanelId, View)]) -> Result<Tab, TabError> {
    panels
        .iter()
        .find(|(candidate, _)| *candidate == panel)
        .map(|(_, view)| Tab::new(panel, view.clone()))
        .ok_or(TabError::GenerationGone { panel })
}

// ---------------------------------------------------------------------------
// Runtime entry point
// ---------------------------------------------------------------------------

impl crate::Runtime {
    /// Live panels of this window, in strip order, with their grid geometry.
    ///
    /// Tiled leaves come first in layout order, then the workspace's detached
    /// floats, then pinned panels — the same occupied set `panel list` reports
    /// as `p:N` (one panel per view id). A panel's projection carries the
    /// solved allocation when it is tiled and the primary grid size otherwise,
    /// so a tab is never minted at a made-up geometry.
    #[must_use]
    pub fn tab_panels(&self) -> Vec<(PanelId, View)> {
        let allocations = self.layout_allocations();
        let (cols, rows) = (self.state().width(), self.state().height());
        let mut ids = self.layout().leaf_ids();
        ids.extend(self.floated_views());
        ids.extend(self.pinned_views());
        let mut out: Vec<(PanelId, View)> = Vec::with_capacity(ids.len());
        for id in ids {
            let panel = panel_of_view(id);
            if out.iter().any(|(seen, _)| *seen == panel) {
                continue;
            }
            let geometry = allocations
                .iter()
                .find(|(leaf, _)| *leaf == id)
                .map_or((cols, rows), |(_, rect)| {
                    (usize::from(rect.width), usize::from(rect.height))
                });
            out.push((panel, View::new(id, geometry.0, geometry.1)));
        }
        out
    }

    /// The installed tab strip, if a tab request has run in this window.
    #[must_use]
    pub fn tab_snapshot(&self) -> Option<&TabSet> {
        self.tabs.snapshot()
    }

    /// Runs one tab request through the validated commit path.
    ///
    /// The order is the WS-INV-19 one and it is the whole contract of this
    /// method:
    ///
    /// 1. stage the transition ([`TabSet::open`] / [`TabSet::switch`] /
    ///    [`TabSet::close`]), which runs the module's full pre-commit gate;
    /// 2. re-home focus to [`TabCommit::focus_target`] through
    ///    [`crate::Runtime::set_focus`];
    /// 3. commit with the focus **read back** from the runtime, so the
    ///    `FocusNotRehomed` gate is exercised rather than assumed;
    /// 4. install the committed projection and arm the full redraw.
    ///
    /// No step touches the layout tree: the committed layout is reported by
    /// [`TabOutcome`] only, and hosting a strip is a separate slice.
    ///
    /// # Errors
    ///
    /// Every [`TabError`] the staged verb produces — including
    /// [`TabError::FocusNotRehomed`] when the re-home did not land — with the
    /// strip left exactly as it was before the request (the projection is
    /// installed only from a committed transition).
    pub fn tab_apply(&mut self, request: TabRequest) -> Result<TabOutcome, TabError> {
        let panels = self.tab_panels();
        let live: Vec<PanelId> = panels.iter().map(|(panel, _)| *panel).collect();
        let focus = self.focused_view().map(panel_of_view);

        let commit = {
            let host = &mut self.tabs;
            host.project(&panels, focus)?;
            let set = host.snapshot().ok_or(TabError::EmptyStack)?;
            stage(set, &panels, &live, request)?
        };

        // WS-INV-19: focus is re-homed first, and the commit is presented with
        // the focus the runtime now reports — never with the target we asked
        // for. A re-home that did not land is the `FocusNotRehomed` refusal,
        // and the strip stays as it was because nothing is installed.
        self.set_focus(view_of_panel(commit.focus_target()));
        let applied = commit.commit(self.focused_view().map(panel_of_view))?;
        self.tabs.install(&applied);
        self.pending_full_redraw = true;
        Ok(TabOutcome::from_committed(&applied))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ui::{TAB_CMDS, TabEffect};

    const GEOMETRY: (usize, usize) = (80, 24);

    fn view_of(panel: u64) -> View {
        View::new(view_of_panel(PanelId::new(panel)), GEOMETRY.0, GEOMETRY.1)
    }

    fn panels_of(ids: &[u64]) -> Vec<(PanelId, View)> {
        ids.iter()
            .map(|id| (PanelId::new(*id), view_of(*id)))
            .collect()
    }

    fn live_of(panels: &[(PanelId, View)]) -> Vec<PanelId> {
        panels.iter().map(|(panel, _)| *panel).collect()
    }

    /// Projects a strip over `ids` with `active` selected, exactly as the first
    /// request does, then runs `request` against it.
    fn run(ids: &[u64], active: Option<u64>, request: TabRequest) -> Result<TabCommit, TabError> {
        let panels = panels_of(ids);
        let mut host = TabHost::default();
        host.project(&panels, active.map(PanelId::new))
            .expect("the projection is valid");
        let set = host.snapshot().expect("projected");
        stage(set, &panels, &live_of(&panels), request)
    }

    #[test]
    fn the_projection_is_the_live_panel_set_with_focus_active() {
        let panels = panels_of(&[1, 2, 3]);
        let mut host = TabHost::default();
        assert!(host.snapshot().is_none());
        host.project(&panels, Some(PanelId::new(2)))
            .expect("projection");
        let set = host.snapshot().expect("projected");
        assert_eq!(
            set.tabs().iter().map(Tab::panel).collect::<Vec<_>>(),
            vec![PanelId::new(1), PanelId::new(2), PanelId::new(3)]
        );
        assert_eq!(set.active_panel(), PanelId::new(2));
        assert_eq!(set.active_index(), 1);
        // A second projection is a no-op: the strip is installed once.
        host.project(&panels, Some(PanelId::new(3)))
            .expect("idempotent");
        assert_eq!(
            host.snapshot().expect("projected").active_panel(),
            PanelId::new(2)
        );
    }

    #[test]
    fn the_projection_falls_back_to_the_first_tab_for_an_unknown_focus() {
        let panels = panels_of(&[4, 5]);
        let mut host = TabHost::default();
        host.project(&panels, Some(PanelId::new(99)))
            .expect("projection");
        assert_eq!(
            host.snapshot().expect("projected").active_panel(),
            PanelId::new(4)
        );
        // No panels at all is the typed empty-stack refusal, never a panic.
        assert_eq!(
            TabHost::default()
                .project(&[], None)
                .expect_err("an empty window has no tab"),
            TabError::EmptyStack
        );
    }

    #[test]
    fn open_requires_a_live_panel_the_strip_does_not_project() {
        // Already projected: the typed duplicate refusal.
        assert_eq!(
            run(&[1, 2], Some(1), TabRequest::Open(PanelId::new(2)))
                .expect_err("already projected"),
            TabError::AlreadyOpen {
                panel: PanelId::new(2)
            }
        );
        // Not live at all: the generation refusal, not a silent skip.
        assert_eq!(
            run(&[1, 2], Some(1), TabRequest::Open(PanelId::new(9))).expect_err("dead panel"),
            TabError::GenerationGone {
                panel: PanelId::new(9)
            }
        );
    }

    #[test]
    fn close_then_open_re_attaches_the_projection() {
        // The reopen path: close detaches the projection (the panel stays
        // live), so the strip no longer projects it and `open` stages it again
        // through the same commit gate.
        let closed = run(&[1, 2, 3], Some(2), TabRequest::Close(PanelId::new(3)))
            .expect("close a background tab");
        assert_eq!(closed.removed(), Some(PanelId::new(3)));
        let target = closed.focus_target();
        let reinstalled = closed.commit(Some(target)).expect("focus was re-homed");
        assert!(reinstalled.set().find(PanelId::new(3)).is_none());

        // Now that the projection is gone, `open` stages it back.
        let set = reinstalled.set().clone();
        let panels = panels_of(&[1, 2, 3]);
        let reopened = stage(
            &set,
            &panels,
            &live_of(&panels),
            TabRequest::Open(PanelId::new(3)),
        )
        .expect("reopen the detached projection");
        assert_eq!(reopened.verb(), TabVerb::Open);
        assert_eq!(reopened.focus_target(), PanelId::new(3));
        assert_eq!(reopened.next().len(), 3);
    }

    #[test]
    fn switch_and_next_move_only_the_active_index() {
        let switched =
            run(&[1, 2, 3], Some(1), TabRequest::Switch(PanelId::new(2))).expect("switch");
        assert_eq!(switched.verb(), TabVerb::Switch);
        assert_eq!(switched.focus_target(), PanelId::new(2));
        assert_eq!(
            switched
                .next()
                .tabs()
                .iter()
                .map(Tab::panel)
                .collect::<Vec<_>>(),
            vec![PanelId::new(1), PanelId::new(2), PanelId::new(3)],
            "a switch never reorders the strip"
        );

        // `Next` wraps from the last projection to the first.
        let wrapped = run(&[1, 2, 3], Some(3), TabRequest::Next).expect("cycle");
        assert_eq!(wrapped.focus_target(), PanelId::new(1));
        assert_eq!(wrapped.next().active_index(), 0);

        // The middle projection cycles to the one after it.
        let forward = run(&[1, 2, 3], Some(1), TabRequest::Next).expect("cycle");
        assert_eq!(forward.focus_target(), PanelId::new(2));
    }

    #[test]
    fn next_refuses_a_single_tab_stack() {
        assert_eq!(
            run(&[1], Some(1), TabRequest::Next).expect_err("nothing to cycle to"),
            TabError::SingleTab {
                panel: PanelId::new(1)
            }
        );
    }

    #[test]
    fn open_unprojected_picks_the_first_unprojected_panel() {
        let closed = run(&[1, 2], Some(2), TabRequest::Close(PanelId::new(1))).expect("close");
        let target = closed.focus_target();
        let reinstalled = closed.commit(Some(target)).expect("focus was re-homed");
        let set = reinstalled.set().clone();
        let panels = panels_of(&[1, 2]);
        let opened = stage(
            &set,
            &panels,
            &live_of(&panels),
            TabRequest::OpenUnprojected,
        )
        .expect("reopen the detached projection");
        assert_eq!(opened.focus_target(), PanelId::new(1));

        // With every live panel projected there is nothing to open, and the
        // refusal names the active panel instead of silently doing nothing.
        assert_eq!(
            run(&[1, 2], Some(2), TabRequest::OpenUnprojected).expect_err("nothing to open"),
            TabError::AlreadyOpen {
                panel: PanelId::new(2)
            }
        );
    }

    #[test]
    fn every_request_maps_onto_a_registered_core_verb() {
        // The host never invents a verb: each request names one of the three
        // names the registry knows.
        let verbs = [
            TabRequest::Open(PanelId::new(1)),
            TabRequest::Switch(PanelId::new(1)),
            TabRequest::Close(PanelId::new(1)),
            TabRequest::CloseActive,
            TabRequest::Next,
            TabRequest::OpenUnprojected,
        ];
        for request in verbs {
            let expected = match request {
                TabRequest::Close(_) | TabRequest::CloseActive => "core.tab:close",
                TabRequest::Open(_) | TabRequest::OpenUnprojected => "core.tab:open",
                TabRequest::Switch(_) | TabRequest::Next => "core.tab:switch",
            };
            assert!(
                TAB_CMDS.contains(&expected),
                "{expected} must be a Core verb"
            );
            assert_eq!(format!("core.tab:{}", verb_name(request.verb())), expected);
        }
    }

    #[test]
    fn a_receipt_carries_the_committed_state_and_both_effects() {
        let commit = run(&[1, 2], Some(1), TabRequest::Switch(PanelId::new(2))).expect("switch");
        let committed = commit
            .commit(Some(PanelId::new(2)))
            .expect("focus was re-homed");
        let outcome = TabOutcome::from_committed(&committed);
        assert_eq!(outcome.verb(), TabVerb::Switch);
        assert_eq!(outcome.focus(), PanelId::new(2));
        assert_eq!(outcome.active_index(), 1);
        assert_eq!(outcome.tabs(), &[PanelId::new(1), PanelId::new(2)]);
        assert_eq!(outcome.removed(), None);
        assert!(outcome.reattribute_input_mode());
        assert!(outcome.pending_full_redraw());
        assert_eq!(
            (
                outcome.reattribute_input_mode(),
                outcome.pending_full_redraw()
            ),
            (
                TabEffect::FOCUS_AND_REDRAW.reattribute_input_mode,
                TabEffect::FOCUS_AND_REDRAW.pending_full_redraw
            ),
            "the receipt mirrors the module-level effect contract"
        );
        assert_eq!(
            outcome.to_json(),
            "{\"verb\":\"switch\",\"active\":\"p:2\",\"active_index\":1,\"tabs\":[\"p:1\",\"p:2\"],\
             \"removed\":null,\"reattribute_input_mode\":true,\"pending_full_redraw\":true}"
        );
    }

    #[test]
    fn panel_and_view_identity_round_trip() {
        let panel = PanelId::new(7);
        assert_eq!(panel_of_view(view_of_panel(panel)), panel);
        assert_eq!(panel_ref(panel), "p:7");
    }

    // -- runtime entry point (real Runtime, real panel set) ----------------

    use bitty_ui::{LayoutNode, SplitAxis};

    /// Headless runtime with two tiled panels (`v:1`, `v:2`), focus on `v:1`.
    fn runtime_with_two_panels() -> crate::Runtime {
        let mut runtime = crate::Runtime::with_defaults().expect("headless runtime");
        let first = View::new(ViewId::new(1), GEOMETRY.0, GEOMETRY.1);
        let second = View::new(ViewId::new(2), GEOMETRY.0, GEOMETRY.1);
        runtime.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(first),
            LayoutNode::leaf(second),
        ));
        assert!(runtime.set_focus(ViewId::new(1)));
        runtime
    }

    fn strip(runtime: &crate::Runtime) -> Vec<PanelId> {
        runtime
            .tab_snapshot()
            .expect("a request installed a strip")
            .tabs()
            .iter()
            .map(Tab::panel)
            .collect()
    }

    #[test]
    fn the_strip_is_projected_lazily_from_the_live_panels() {
        let mut runtime = runtime_with_two_panels();
        assert!(runtime.tab_snapshot().is_none());
        assert_eq!(
            runtime
                .tab_panels()
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>(),
            vec![PanelId::new(1), PanelId::new(2)]
        );
        let outcome = runtime
            .tab_apply(TabRequest::Next)
            .expect("cycle to the second tab");
        assert_eq!(outcome.focus(), PanelId::new(2));
        assert_eq!(strip(&runtime), vec![PanelId::new(1), PanelId::new(2)]);
        assert_eq!(
            runtime.focused_view(),
            Some(ViewId::new(2)),
            "the host re-homes focus through `set_focus`"
        );
        assert!(outcome.pending_full_redraw());
        // No layout mutation: the committed tree is reported, never installed.
        assert_eq!(runtime.layout().leaf_count(), 2);
    }

    #[test]
    fn close_then_open_round_trips_through_the_commit_gate() {
        let mut runtime = runtime_with_two_panels();
        let closed = runtime
            .tab_apply(TabRequest::Close(PanelId::new(2)))
            .expect("close the background tab");
        assert_eq!(closed.removed(), Some(PanelId::new(2)));
        assert_eq!(strip(&runtime), vec![PanelId::new(1)]);
        assert_eq!(
            runtime.focused_view(),
            Some(ViewId::new(1)),
            "closing the background tab keeps the activation"
        );

        let reopened = runtime
            .tab_apply(TabRequest::Open(PanelId::new(2)))
            .expect("reopen the detached projection");
        assert_eq!(reopened.focus(), PanelId::new(2));
        assert_eq!(strip(&runtime), vec![PanelId::new(1), PanelId::new(2)]);
        assert_eq!(runtime.focused_view(), Some(ViewId::new(2)));
    }

    #[test]
    fn a_refused_request_leaves_the_strip_and_focus_untouched() {
        let mut runtime = runtime_with_two_panels();
        runtime
            .tab_apply(TabRequest::Switch(PanelId::new(2)))
            .expect("switch");
        let before = strip(&runtime);
        let leafs = runtime.layout().leaf_count();

        // Unknown panel.
        assert_eq!(
            runtime
                .tab_apply(TabRequest::Switch(PanelId::new(9)))
                .expect_err("no such panel"),
            TabError::UnknownTab {
                panel: PanelId::new(9)
            }
        );
        // Already active.
        assert_eq!(
            runtime
                .tab_apply(TabRequest::Switch(PanelId::new(2)))
                .expect_err("already active"),
            TabError::AlreadyActive {
                panel: PanelId::new(2)
            }
        );
        // Already projected.
        assert_eq!(
            runtime
                .tab_apply(TabRequest::Open(PanelId::new(1)))
                .expect_err("already projected"),
            TabError::AlreadyOpen {
                panel: PanelId::new(1)
            }
        );
        assert_eq!(strip(&runtime), before);
        assert_eq!(runtime.focused_view(), Some(ViewId::new(2)));
        assert_eq!(runtime.layout().leaf_count(), leafs);
    }

    #[test]
    fn the_last_tab_cannot_be_closed() {
        let mut runtime = crate::Runtime::with_defaults().expect("headless runtime");
        assert_eq!(
            runtime
                .tab_apply(TabRequest::Close(PanelId::new(1)))
                .expect_err("closing the only tab is not a tab verb"),
            TabError::LastTab {
                panel: PanelId::new(1)
            }
        );
        // The refusal is not a projection: the strip is exactly one tab.
        assert_eq!(strip(&runtime), vec![PanelId::new(1)]);
    }

    #[test]
    fn a_stale_projection_is_refused_without_touching_the_strip() {
        // The strip can still project a panel the window has since dropped
        // (a workspace switch, a closed pane). Activation then refuses with
        // the typed `GenerationGone` — never a silent switch to a dead
        // projection — and the strip keeps the state the last commit
        // installed.
        let mut runtime = runtime_with_two_panels();
        runtime
            .tab_apply(TabRequest::Switch(PanelId::new(2)))
            .expect("switch");
        // Drop `v:2` from the live layout: the strip still projects `p:2`.
        runtime.set_layout(LayoutNode::leaf(View::new(
            ViewId::new(1),
            GEOMETRY.0,
            GEOMETRY.1,
        )));
        assert!(runtime.set_focus(ViewId::new(1)));
        let before = strip(&runtime);
        assert_eq!(
            runtime
                .tab_apply(TabRequest::Switch(PanelId::new(2)))
                .expect_err("a dead projection cannot be activated"),
            TabError::GenerationGone {
                panel: PanelId::new(2)
            }
        );
        assert_eq!(strip(&runtime), before);
        assert_eq!(
            runtime.tab_snapshot().expect("installed").active_panel(),
            PanelId::new(2),
            "the strip keeps the activation the last commit installed"
        );
        assert_eq!(runtime.focused_view(), Some(ViewId::new(1)));
        // `close` is the repair path: it never consults liveness, so the dead
        // projection can still be detached.
        let repaired = runtime
            .tab_apply(TabRequest::Close(PanelId::new(2)))
            .expect("close the stale projection");
        assert_eq!(repaired.removed(), Some(PanelId::new(2)));
        assert_eq!(strip(&runtime), vec![PanelId::new(1)]);
    }

    #[test]
    fn the_commit_gate_refuses_the_pre_re_home_focus() {
        // The WS-INV-19 gate is load-bearing at this seam: the runtime must
        // present `TabCommit::commit` with the focus it read back *after*
        // re-homing. Presenting the outgoing focus (what a host that skipped
        // the re-home would pass) is the `FocusNotRehomed` refusal, so the
        // ordering in `tab_apply` cannot be dropped silently.
        let runtime = runtime_with_two_panels();
        let panels = runtime.tab_panels();
        let live: Vec<PanelId> = panels.iter().map(|(panel, _)| *panel).collect();
        let tabs: Vec<Tab> = panels
            .iter()
            .map(|(panel, view)| Tab::new(*panel, view.clone()))
            .collect();
        let set = TabSet::new(tabs, 0).expect("projection");
        let staged = stage(&set, &panels, &live, TabRequest::Switch(PanelId::new(2)))
            .expect("switch stages");
        assert_eq!(staged.focus_target(), PanelId::new(2));

        // The outgoing focus: refused.
        assert_eq!(
            staged
                .clone()
                .commit(Some(PanelId::new(1)))
                .expect_err("outgoing focus must not commit"),
            TabError::FocusNotRehomed {
                expected: PanelId::new(2),
                actual: Some(PanelId::new(1))
            }
        );
        // No focus at all: refused too, so an absent focus never persists.
        assert_eq!(
            staged
                .clone()
                .commit(None)
                .expect_err("absent focus must not commit"),
            TabError::FocusNotRehomed {
                expected: PanelId::new(2),
                actual: None
            }
        );
        // The focus the runtime reads back after re-homing: committed.
        assert!(staged.commit(Some(PanelId::new(2))).is_ok());
    }
}
