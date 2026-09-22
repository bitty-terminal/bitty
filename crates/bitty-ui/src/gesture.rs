//! U-5 gesture transaction and command ontological equivalence (UX-21,
//! UX-22; issues #1027, #1028).
//!
//! Builds on the PW drag family (`crate::drag`: Mod+drag move, edge/corner
//! resize, cross-workspace drops) and the CW-08 command-registry path: every
//! mutating step validates a registered workspace command before touching
//! the tree and fails with the tree untouched otherwise. All operations are
//! deterministic and headless; this module adds no new crate dependency.
//!
//! # UX-21: gesture transaction
//!
//! [`GestureTransaction`] is the lift/preview/commit/rollback lifecycle for
//! one pointer gesture:
//!
//! - **Lift** ([`GestureTransaction::lift`]) gates on Mod held and on
//!   `owner.name:command` grammar. The bound command is stored, never
//!   executed here.
//! - **Preview** ([`GestureTransaction::update_preview`]) hit-tests the
//!   hovered drop target. Advisory only, recorded for the caller, never
//!   mutates the tree.
//! - **Interactive drop targets** ([`GestureTransaction::drop_targets`])
//!   enumerate every valid anchor (all live leaves except the source) in
//!   deterministic depth-first order for the drop-target UI.
//! - **Rollback** ([`GestureTransaction::cancel`]) is the Esc path: the
//!   transaction settles as [`GestureOutcome::Cancelled`] with the tree and
//!   the [`DragHistory`](crate::drag::DragHistory) untouched. Preview
//!   records hover state only, so there is nothing to rewind beyond
//!   discarding the transaction.
//! - **Atomic commit** ([`GestureTransaction::commit`]) validates the bound
//!   command against the workspace [`CommandRegistry`](crate::panel::CommandRegistry),
//!   records a [`DragHistory`](crate::drag::DragHistory) snapshot, and
//!   re-parents the source leaf. Validation order (tree untouched on every
//!   failure): registry ownership, self-drop, source lookup, target lookup.
//!
//! # UX-22: command ontological equivalence
//!
//! [`CommandOrigin`] names the six invocation surfaces (gesture, keyboard,
//! palette, CLI, IPC, agent). [`resolve_invocation`] maps any
//! [`CommandInvocation`] to the owning
//! [`PanelId`](crate::panel::PanelId) through the single workspace
//! [`CommandRegistry`](crate::panel::CommandRegistry): the origin selects
//! *how* the command string arrives, never *what* it resolves to. That is
//! the 1:1 equivalence claim, and [`verify_origin_equivalence`] is the
//! conformance check for it: it resolves one command from every origin and
//! requires a single identical owner. This module executes nothing; the
//! router owns execution after resolution returns.

#![forbid(unsafe_code)]

use crate::drag::{DragHistory, DropSpec};
use crate::geometry::{Gaps, Point, Rect};
use crate::layout::LayoutNode;
use crate::panel::{CommandRegistry, PanelId, QualifiedCommand};
use crate::view::ViewId;

// ---------------------------------------------------------------------------
// UX-21: gesture transaction
// ---------------------------------------------------------------------------

/// Lifecycle phase of a [`GestureTransaction`] before settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GesturePhase {
    /// Lifted with Mod held; no hover recorded yet.
    Lifted,
    /// At least one preview hover was recorded; the last hovered target (if
    /// any) is advisory for the commit call.
    Previewing,
}

/// Settlement report for a [`GestureTransaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GestureOutcome {
    /// The move committed atomically through the command registry.
    Committed,
    /// The gesture was cancelled (Esc); the tree is untouched.
    Cancelled,
}

/// Failure to lift, preview, cancel, or commit a [`GestureTransaction`].
/// Every variant leaves the tree and the history untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GestureError {
    /// The gesture started without Mod held; nothing was recorded.
    ModNotHeld,
    /// The bound command is not `owner.name:command` grammar; nothing was
    /// recorded.
    MalformedCommand(String),
    /// The bound command is not registered in the workspace
    /// [`CommandRegistry`]; the tree is untouched.
    Unregistered(String),
    /// No leaf with this id exists in the tree; the tree is untouched.
    SourceNotFound(ViewId),
    /// No leaf with this id exists in the tree; the tree is untouched.
    TargetNotFound(ViewId),
    /// Source and target are the same leaf (self-drop no-op); untouched.
    SelfDrop,
}

impl std::fmt::Display for GestureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModNotHeld => f.write_str("gesture requires Mod held"),
            Self::MalformedCommand(cmd) => write!(f, "malformed gesture command: {cmd}"),
            Self::Unregistered(cmd) => {
                write!(f, "gesture command not registered: {cmd}")
            }
            Self::SourceNotFound(id) => write!(f, "gesture source not found: {id}"),
            Self::TargetNotFound(id) => write!(f, "gesture target not found: {id}"),
            Self::SelfDrop => f.write_str("gesture source and target are the same leaf"),
        }
    }
}

impl std::error::Error for GestureError {}

/// One interactive drop target: a live anchor leaf the dragged source may
/// dock beside (UX-21).
///
/// The list from [`GestureTransaction::drop_targets`] drives the
/// drop-target UI; docking geometry (axis, side, ratio) stays with the
/// commit call via [`DropTarget::docking`], which builds the
/// [`DropSpec`] the registry-routed commit consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DropTarget {
    /// Anchor leaf the dragged source docks beside.
    pub anchor: ViewId,
}

impl DropTarget {
    /// Creates a drop target over `anchor`.
    #[must_use]
    pub const fn new(anchor: ViewId) -> Self {
        Self { anchor }
    }

    /// Builds the docking plan for [`GestureTransaction::commit`] with the
    /// caller-chosen axis, side (`after` puts the dragged leaf second), and
    /// split ratio.
    #[must_use]
    pub fn docking(self, axis: crate::geometry::SplitAxis, ratio: f32, after: bool) -> DropSpec {
        DropSpec::new(self.anchor, axis, ratio, after)
    }
}

/// In-progress pointer gesture with lift/preview/commit/rollback semantics
/// (UX-21).
///
/// The transaction owns hover state only until commit: preview never
/// mutates the tree, so Esc rollback ([`Self::cancel`]) discards state with
/// nothing to rewind. Both settlement paths consume the transaction, so a
/// gesture settles exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GestureTransaction {
    source: ViewId,
    command: String,
    preview: Option<ViewId>,
    phase: GesturePhase,
}

impl GestureTransaction {
    /// Lifts a gesture for `source` carrying workspace `command`.
    ///
    /// Fails with [`GestureError::ModNotHeld`] when Mod is not held, or
    /// [`GestureError::MalformedCommand`] when `command` is not
    /// `owner.name:command` grammar. Registry ownership is checked at
    /// commit, not here: lift binds the intent, commit is the atomic
    /// point.
    pub fn lift(source: ViewId, mod_held: bool, command: &str) -> Result<Self, GestureError> {
        if !mod_held {
            return Err(GestureError::ModNotHeld);
        }
        if QualifiedCommand::parse(command).is_err() {
            return Err(GestureError::MalformedCommand(command.to_owned()));
        }
        Ok(Self {
            source,
            command: command.to_owned(),
            preview: None,
            phase: GesturePhase::Lifted,
        })
    }

    /// The dragged leaf.
    #[must_use]
    pub fn source(&self) -> ViewId {
        self.source
    }

    /// The workspace command the commit routes through.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> GesturePhase {
        self.phase
    }

    /// The last hovered drop target from [`Self::update_preview`], if any.
    #[must_use]
    pub fn preview_target(&self) -> Option<ViewId> {
        self.preview
    }

    /// Advisory live preview: hit-tests `point` against `tree` allocations
    /// and records the hovered leaf. Returns the hovered leaf, or `None`
    /// when the point lands on background (which clears the recorded
    /// hover). Never mutates the tree.
    pub fn update_preview(
        &mut self,
        tree: &LayoutNode,
        bounds: Rect,
        gaps: Gaps,
        point: Point,
    ) -> Option<ViewId> {
        let hit = tree.hit_test_leaf(bounds, gaps, point);
        self.preview = hit;
        self.phase = GesturePhase::Previewing;
        hit
    }

    /// Interactive drop targets for the drop-target UI: every live leaf
    /// except the source, in deterministic depth-first
    /// ([`LayoutNode::leaf_ids`]) order. Empty when the tree holds only the
    /// source. Never mutates the tree.
    #[must_use]
    pub fn drop_targets(&self, tree: &LayoutNode) -> Vec<DropTarget> {
        tree.leaf_ids()
            .into_iter()
            .filter(|id| *id != self.source)
            .map(DropTarget::new)
            .collect()
    }

    /// Esc rollback: settles the transaction as
    /// [`GestureOutcome::Cancelled`] with the tree and the history
    /// untouched.
    #[must_use]
    pub fn cancel(self) -> GestureOutcome {
        GestureOutcome::Cancelled
    }

    /// Atomically commits the move: validates the bound command against the
    /// workspace [`CommandRegistry`], records a `history` snapshot, and
    /// re-parents the source leaf beside `drop.target`.
    ///
    /// Validation order (tree and history untouched on every failure):
    /// registry ownership, self-drop, source lookup, target lookup. The
    /// recorded preview (if any) is advisory only and not re-checked: the
    /// caller passes the final `drop` plan explicitly.
    pub fn commit(
        self,
        tree: &mut LayoutNode,
        history: &mut DragHistory,
        registry: &CommandRegistry,
        drop: DropSpec,
    ) -> Result<GestureOutcome, GestureError> {
        if registry.owner_of(&self.command).is_none() {
            return Err(GestureError::Unregistered(self.command));
        }
        if self.source == drop.target {
            return Err(GestureError::SelfDrop);
        }
        if tree.find_leaf(self.source).is_none() {
            return Err(GestureError::SourceNotFound(self.source));
        }
        if tree.find_leaf(drop.target).is_none() {
            return Err(GestureError::TargetNotFound(drop.target));
        }
        history.push(tree);
        if !tree.reparent_leaf(self.source, drop.target, drop.axis, drop.ratio, drop.after) {
            // Unreachable after the lookups above, but never lose the
            // snapshot accounting: roll the snapshot back.
            history.undo(tree);
            return Err(GestureError::TargetNotFound(drop.target));
        }
        Ok(GestureOutcome::Committed)
    }
}

// ---------------------------------------------------------------------------
// UX-22: command ontological equivalence
// ---------------------------------------------------------------------------

/// Invocation surface for a workspace command (UX-22).
///
/// Six origins, one registry: the origin records how the command string
/// arrived (pointer gesture, keybinding, palette entry, CLI call, IPC
/// message, agent tool call) and never changes what it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandOrigin {
    /// Pointer gesture (drag, drop, resize handle).
    Gesture,
    /// Keybinding dispatch.
    Keyboard,
    /// Command-palette entry.
    Palette,
    /// CLI invocation.
    Cli,
    /// IPC message from an external client.
    Ipc,
    /// Agent tool call.
    Agent,
}

impl CommandOrigin {
    /// Every origin, for conformance iteration.
    pub const ALL: [Self; 6] = [
        Self::Gesture,
        Self::Keyboard,
        Self::Palette,
        Self::Cli,
        Self::Ipc,
        Self::Agent,
    ];

    /// Stable surface name for diagnostics and conformance reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gesture => "gesture",
            Self::Keyboard => "keyboard",
            Self::Palette => "palette",
            Self::Cli => "cli",
            Self::Ipc => "ipc",
            Self::Agent => "agent",
        }
    }
}

impl std::fmt::Display for CommandOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One command invocation from one origin (UX-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandInvocation<'a> {
    /// The `owner.name:command` string, byte-identical across origins.
    pub command: &'a str,
    /// How the string arrived. Recorded, never resolved through: every
    /// origin takes the same registry path.
    pub origin: CommandOrigin,
}

impl<'a> CommandInvocation<'a> {
    /// Creates an invocation of `command` from `origin`.
    #[must_use]
    pub const fn new(command: &'a str, origin: CommandOrigin) -> Self {
        Self { command, origin }
    }
}

/// Failure to resolve an invocation through the single registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EquivalenceError {
    /// `command` is not `owner.name:command` grammar.
    Malformed(String),
    /// `command` is not registered in the workspace [`CommandRegistry`].
    Unregistered(String),
    /// Two origins resolved one command to different owners. Unreachable
    /// through [`resolve_invocation`] (one registry, one map), but the
    /// conformance check reports it instead of assuming it.
    Diverged {
        /// First disagreeing origin.
        origin_a: CommandOrigin,
        /// Owner seen from `origin_a`.
        owner_a: PanelId,
        /// Second disagreeing origin.
        origin_b: CommandOrigin,
        /// Owner seen from `origin_b`.
        owner_b: PanelId,
    },
}

impl std::fmt::Display for EquivalenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(cmd) => write!(f, "malformed command: {cmd}"),
            Self::Unregistered(cmd) => write!(f, "command not registered: {cmd}"),
            Self::Diverged {
                origin_a,
                owner_a,
                origin_b,
                owner_b,
            } => write!(
                f,
                "command diverged: {origin_a} owned by {owner_a}, {origin_b} owned by {owner_b}"
            ),
        }
    }
}

impl std::error::Error for EquivalenceError {}

/// Resolves one invocation to the owning
/// [`PanelId`](crate::panel::PanelId) through the single workspace
/// [`CommandRegistry`].
///
/// Grammar first, then registry ownership; the origin takes no branch in
/// either step. Executes nothing: the router owns execution after
/// resolution returns.
pub fn resolve_invocation(
    registry: &CommandRegistry,
    invocation: &CommandInvocation<'_>,
) -> Result<PanelId, EquivalenceError> {
    if QualifiedCommand::parse(invocation.command).is_err() {
        return Err(EquivalenceError::Malformed(invocation.command.to_owned()));
    }
    registry
        .owner_of(invocation.command)
        .ok_or_else(|| EquivalenceError::Unregistered(invocation.command.to_owned()))
}

/// Conformance check for ontological equivalence: resolves `command` from
/// every [`CommandOrigin::ALL`] origin through the single registry and
/// requires one identical owner.
///
/// Returns that owner. This is the executable form of the UX-22
/// conformance proposal: a suite passes a command when this returns `Ok`
/// with the expected owner from every surface (gesture, keyboard, palette,
/// CLI, IPC, agent) with byte-identical command strings.
pub fn verify_origin_equivalence(
    registry: &CommandRegistry,
    command: &str,
) -> Result<PanelId, EquivalenceError> {
    let mut expected: Option<PanelId> = None;
    let mut expected_origin = CommandOrigin::Gesture;
    for origin in CommandOrigin::ALL {
        let owner = resolve_invocation(registry, &CommandInvocation::new(command, origin))?;
        match expected {
            None => {
                expected = Some(owner);
                expected_origin = origin;
            }
            Some(first) => {
                if first != owner {
                    return Err(EquivalenceError::Diverged {
                        origin_a: expected_origin,
                        owner_a: first,
                        origin_b: origin,
                        owner_b: owner,
                    });
                }
            }
        }
    }
    // The loop always sets `expected` on its first iteration; the fallback
    // below is unreachable but keeps the function total without panicking.
    expected.ok_or_else(|| EquivalenceError::Malformed(command.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drag::DRAG_MOVE_CMD;
    use crate::geometry::SplitAxis;
    use crate::presentation::FLOATING_CMD_TOGGLE;
    use crate::view::{View, ViewId};

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    fn registry_with(cmds: &[&str]) -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        let owner = PanelId::new(1);
        for cmd in cmds {
            registry.register(owner, cmd).expect("command registers");
        }
        registry
    }

    // -- UX-21 lift ---------------------------------------------------------

    #[test]
    fn lift_requires_mod_held() {
        assert_eq!(
            GestureTransaction::lift(ViewId::new(1), false, DRAG_MOVE_CMD),
            Err(GestureError::ModNotHeld)
        );
        let tx =
            GestureTransaction::lift(ViewId::new(1), true, DRAG_MOVE_CMD).expect("mod held lifts");
        assert_eq!(tx.source(), ViewId::new(1));
        assert_eq!(tx.command(), DRAG_MOVE_CMD);
        assert_eq!(tx.phase(), GesturePhase::Lifted);
        assert_eq!(tx.preview_target(), None);
    }

    #[test]
    fn lift_rejects_malformed_command() {
        for bad in ["", "plaintext", "bitty.workspace:", "has space:cmd"] {
            assert_eq!(
                GestureTransaction::lift(ViewId::new(1), true, bad),
                Err(GestureError::MalformedCommand(bad.to_owned())),
                "lift must reject {bad:?}"
            );
        }
    }

    // -- UX-21 preview ------------------------------------------------------

    #[test]
    fn preview_hit_tests_drop_target_without_mutating() {
        let tree = pair();
        let bounds = Rect::new(0, 0, 80, 24);
        let mut tx = GestureTransaction::lift(ViewId::new(1), true, DRAG_MOVE_CMD).expect("lift");
        // Left half holds leaf 1, right half leaf 2.
        assert_eq!(
            tx.update_preview(&tree, bounds, Gaps::ZERO, Point::new(60, 12)),
            Some(ViewId::new(2))
        );
        assert_eq!(tx.phase(), GesturePhase::Previewing);
        assert_eq!(tx.preview_target(), Some(ViewId::new(2)));
        // Background hover clears the advisory preview.
        assert_eq!(
            tx.update_preview(&tree, bounds, Gaps::ZERO, Point::new(200, 200)),
            None
        );
        assert_eq!(tx.preview_target(), None);
    }

    // -- UX-21 interactive drop targets -------------------------------------

    #[test]
    fn drop_targets_list_every_other_leaf_deterministically() {
        let tree = pair();
        let tx = GestureTransaction::lift(ViewId::new(1), true, DRAG_MOVE_CMD).expect("lift");
        assert_eq!(
            tx.drop_targets(&tree),
            vec![DropTarget::new(ViewId::new(2))]
        );
        // Docking builds the registry-routed commit plan.
        let spec = tx.drop_targets(&tree)[0].docking(SplitAxis::Vertical, 0.5, true);
        assert_eq!(
            spec,
            DropSpec::new(ViewId::new(2), SplitAxis::Vertical, 0.5, true)
        );
        // A lone source offers no target.
        let solo = leaf(1);
        assert!(tx.drop_targets(&solo).is_empty());
    }

    // -- UX-21 Esc rollback -------------------------------------------------

    #[test]
    fn esc_cancel_leaves_tree_and_history_untouched() {
        let tree = pair();
        let before = tree.clone();
        let bounds = Rect::new(0, 0, 80, 24);
        let mut tx = GestureTransaction::lift(ViewId::new(2), true, DRAG_MOVE_CMD).expect("lift");
        tx.update_preview(&tree, bounds, Gaps::ZERO, Point::new(10, 12));
        assert_eq!(tx.cancel(), GestureOutcome::Cancelled);
        assert_eq!(tree, before, "cancel touches nothing");
    }

    // -- UX-21 atomic commit ------------------------------------------------

    #[test]
    fn commit_routes_through_registry_with_undo() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        let registry = registry_with(&[DRAG_MOVE_CMD]);
        let outcome = GestureTransaction::lift(ViewId::new(2), true, DRAG_MOVE_CMD)
            .expect("lift")
            .commit(
                &mut tree,
                &mut history,
                &registry,
                DropSpec::new(ViewId::new(1), SplitAxis::Vertical, 0.5, true),
            )
            .expect("commit routes through the registry");
        assert_eq!(outcome, GestureOutcome::Committed);
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert!(history.undo(&mut tree));
        assert_eq!(tree, pair());
    }

    #[test]
    fn commit_failures_leave_tree_and_history_untouched() {
        let no_registry = CommandRegistry::new();
        let registered = registry_with(&[DRAG_MOVE_CMD]);
        // (registry, bound command, source, target, want)
        let cases: [(&CommandRegistry, &str, ViewId, ViewId, GestureError); 4] = [
            (
                &no_registry,
                DRAG_MOVE_CMD,
                ViewId::new(2),
                ViewId::new(1),
                GestureError::Unregistered(DRAG_MOVE_CMD.to_string()),
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(1),
                ViewId::new(1),
                GestureError::SelfDrop,
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(404),
                ViewId::new(1),
                GestureError::SourceNotFound(ViewId::new(404)),
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(2),
                ViewId::new(404),
                GestureError::TargetNotFound(ViewId::new(404)),
            ),
        ];
        for (registry, command, source, target, want) in cases {
            let mut tree = pair();
            let mut history = DragHistory::new();
            let err = GestureTransaction::lift(source, true, command)
                .expect("lift")
                .commit(
                    &mut tree,
                    &mut history,
                    registry,
                    DropSpec::new(target, SplitAxis::Horizontal, 0.5, true),
                )
                .expect_err("commit must fail");
            assert_eq!(err, want);
            assert_eq!(tree, pair(), "tree untouched on {err}");
            assert!(history.is_empty(), "no snapshot on {err}");
        }
    }

    // -- UX-22 origins ------------------------------------------------------

    #[test]
    fn origin_names_are_stable_and_distinct() {
        let names: Vec<&str> = CommandOrigin::ALL
            .iter()
            .map(|origin| origin.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["gesture", "keyboard", "palette", "cli", "ipc", "agent"]
        );
        assert_eq!(CommandOrigin::Gesture.to_string(), "gesture");
        assert_eq!(CommandOrigin::Agent.to_string(), "agent");
    }

    #[test]
    fn every_origin_resolves_to_the_same_owner() {
        let owner = PanelId::new(1);
        let mut registry = CommandRegistry::new();
        registry.register(owner, DRAG_MOVE_CMD).expect("registers");
        registry
            .register(owner, FLOATING_CMD_TOGGLE)
            .expect("registers");
        for command in [DRAG_MOVE_CMD, FLOATING_CMD_TOGGLE] {
            for origin in CommandOrigin::ALL {
                let invocation = CommandInvocation::new(command, origin);
                assert_eq!(
                    resolve_invocation(&registry, &invocation),
                    Ok(owner),
                    "{origin} must resolve {command} to the same owner"
                );
            }
        }
    }

    #[test]
    fn resolve_rejects_malformed_and_unregistered_commands() {
        let registry = registry_with(&[DRAG_MOVE_CMD]);
        let invocation = CommandInvocation::new("plaintext", CommandOrigin::Keyboard);
        assert_eq!(
            resolve_invocation(&registry, &invocation),
            Err(EquivalenceError::Malformed("plaintext".to_string()))
        );
        let invocation = CommandInvocation::new(FLOATING_CMD_TOGGLE, CommandOrigin::Ipc);
        assert_eq!(
            resolve_invocation(&registry, &invocation),
            Err(EquivalenceError::Unregistered(
                FLOATING_CMD_TOGGLE.to_string()
            ))
        );
    }

    #[test]
    fn conformance_check_requires_one_owner_from_every_origin() {
        let owner = PanelId::new(1);
        let registry = registry_with(&[DRAG_MOVE_CMD]);
        // Conformance proposal in executable form: byte-identical command
        // strings from all six surfaces resolve to the registered owner.
        assert_eq!(
            verify_origin_equivalence(&registry, DRAG_MOVE_CMD),
            Ok(owner)
        );
        // Unregistered commands fail the check from the first origin.
        assert_eq!(
            verify_origin_equivalence(&registry, FLOATING_CMD_TOGGLE),
            Err(EquivalenceError::Unregistered(
                FLOATING_CMD_TOGGLE.to_string()
            ))
        );
        // Malformed commands fail before any origin resolves.
        assert_eq!(
            verify_origin_equivalence(&registry, "plaintext"),
            Err(EquivalenceError::Malformed("plaintext".to_string()))
        );
    }
}
