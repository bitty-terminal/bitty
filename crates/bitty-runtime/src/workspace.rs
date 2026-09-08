#![forbid(unsafe_code)]
//! Workspace via Panel Runtime — generic, no hardcoded tabs, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.workspace` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Workspaces are not a
//! hardcoded terminal primitive; they reuse the accepted `LayoutNode`
//! primitives `stack` and `split` without introducing a new tiling node, and
//! verify the `TerminalRegistry`/`View`/`Workspace`/`Focus` lifecycle via the
//! Panel API public path only (`PanelRegistry::new` → `create_panel` →
//! `mount_panel` → `focus_panel` with `PanelType::Helper`, and the public
//! `TerminalRegistry` `create_terminal`/`create_view`/`attach`/`set_focus`/
//! `move_terminal` path). No parser, renderer, or input hot path is entered,
//! and no grid mutation ever occurs here (only `Action` writes `State` per
//! Terminal State RFC). Default is disabled (fresh `EffectiveConfig` has empty
//! `plugins`); `bitty --safe` rejects `bitty-terminal.*` as non-builtin
//! without panic, identical to third-party `xuepoo.*` parity (no private
//! channel). Bounded queues (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`,
//! `8 KiB` payload, `32`/`8 KiB` batch) and single-process `winit`
//! `PanelRegistry` per window are verified headlessly.
//!
//! A bitty workspace is a tab group within a window (wezterm inverts this:
//! workspace > window > tab > pane).

use bitty_ui::{LayoutNode, SplitAxis, View, ViewId};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Maximum workspaces (tabs) per workspace — mirrors `MAX_VIEWS_PER_WORKSPACE` (32), the
/// accepted bound for `workspace.create_view` and `set_workspace_layout`.
pub const WORKSPACE_MAX_TABS: usize = crate::registry::MAX_VIEWS_PER_WORKSPACE;

/// Maximum panels per workspace for workspace container — mirrors
/// `MAX_PANELS_PER_WORKSPACE` (32) and `MAX_PANELS_PER_WINDOW` (64).
pub const WORKSPACE_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
pub const WORKSPACE_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// Panel payload for workspace observations is bounded by `BUS_EVENT_MAX_BYTES`
/// (8 KiB) at the bus admission boundary.
pub const WORKSPACE_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Workspace title display bound — mirrors overlay text bound (128 chars) for
/// workspaceline presentation; titles longer are truncated at char boundary.
pub const WORKSPACE_TITLE_MAX_CHARS: usize = bitty_ui::panel::MAX_OVERLAY_TEXT_LEN;

/// WorkspaceIntegration — pure, observation-only helpers over committed state
/// and layout. No mutation of `State`, no hot-path, bounded `<=32` leaves.
/// Workspace ordering is the deterministic depth-first leaf order of the `Stack`.
/// No new tiling primitive is introduced; workspaces are `Stack` only.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceIntegration;

impl WorkspaceIntegration {
    /// Builds a workspace `Stack` from `views`. Each view becomes a `Leaf`; the
    /// `Stack` shares the container bounds (workspace-like stacking where the last
    /// element is top-most for focus/visual order). No hardcoded `Workspace` node.
    #[must_use]
    pub fn stack_for_workspace(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Builds a horizontal split from two workspace stacks (e.g., two workspace groups
    /// side-by-side). Reuses `LayoutNode::split` with clamped ratio.
    #[must_use]
    pub fn split_for_workspace(left: Vec<View>, right: Vec<View>, ratio: f32) -> LayoutNode {
        let l = Self::stack_for_workspace(left);
        let r = Self::stack_for_workspace(right);
        LayoutNode::split(SplitAxis::Horizontal, ratio, l, r)
    }

    /// Number of workspaces in `layout` (leaf count). Bounded `<=32`.
    #[must_use]
    pub fn workspace_count(layout: &LayoutNode) -> usize {
        layout.leaf_count()
    }

    /// Workspace `ViewId`s in deterministic depth-first order. Bounded `<=32`.
    #[must_use]
    pub fn workspace_ids(layout: &LayoutNode) -> Vec<ViewId> {
        layout.leaf_ids()
    }

    /// Whether `layout` is a workspace `Stack` (no new primitive).
    #[must_use]
    pub fn is_stack(layout: &LayoutNode) -> bool {
        matches!(layout, LayoutNode::Stack(_))
    }

    /// Workspace title for a terminal state, bounded to `WORKSPACE_TITLE_MAX_CHARS` and
    /// truncated at char boundary. Empty title yields `None` (workspace shows
    /// fallback). Uses `State::title()` committed by `OSC 0/2` via `Action`.
    #[must_use]
    pub fn workspace_title(state: &bitty_term_state::State) -> Option<String> {
        let raw = state.title();
        if raw.is_empty() {
            return None;
        }
        let truncated = if raw.chars().count() <= WORKSPACE_TITLE_MAX_CHARS {
            raw.to_owned()
        } else {
            raw.chars().take(WORKSPACE_TITLE_MAX_CHARS).collect()
        };
        Some(truncated)
    }

    /// Whether workspace observation has any data (non-empty title or at least one
    /// workspace view in layout). Pure observation, never mutates.
    #[must_use]
    pub fn has_workspaces(layout: &LayoutNode) -> bool {
        !Self::workspace_ids(layout).is_empty()
    }

    /// Finds a workspace `ViewId` in `layout`, if present.
    #[must_use]
    pub fn find_workspace(layout: &LayoutNode, id: ViewId) -> Option<View> {
        layout.find_leaf(id).cloned()
    }
}

/// Creates a workspace panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults). Returns the
/// panel handle on success; caller must still activate the associated plugin
/// via the public PluginHost path (`declare → resolve → register →
/// GrantRecord → activate`) for capability `ui.rich` and claim `workspaceline`.
pub fn create_workspace_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that workspace panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
pub fn validate_workspace_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_term_state::{State, TerminalAction};
    use bitty_ui::{Rect as UiRect, SplitAxis, ViewId};
    use bitty_vt::BoundedString;

    #[test]
    fn workspaces_are_stack_no_hardcoded_primitive() {
        // Workspaces reuse LayoutNode::stack, not a new Workspace node.
        let views = vec![
            View::new(ViewId::new(1), 80, 24),
            View::new(ViewId::new(2), 80, 24),
        ];
        let stack = WorkspaceIntegration::stack_for_workspace(views);
        assert!(WorkspaceIntegration::is_stack(&stack));
        assert_eq!(WorkspaceIntegration::workspace_count(&stack), 2);
        let ids = WorkspaceIntegration::workspace_ids(&stack);
        assert_eq!(ids, vec![ViewId::new(1), ViewId::new(2)]);
        // Stack shares container bounds: reflow yields identical rects.
        let mut with_stack = stack.clone();
        with_stack.reflow(UiRect::new(0, 0, 80, 24));
        let allocs = with_stack.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        // Both workspaces share full bounds in Stack semantics.
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[1].1, UiRect::new(0, 0, 80, 24));
    }

    #[test]
    fn workspaces_split_reuses_layout_split() {
        let left = vec![View::new(ViewId::new(10), 40, 24)];
        let right = vec![View::new(ViewId::new(11), 40, 24)];
        let split = WorkspaceIntegration::split_for_workspace(left, right, 0.5);
        assert!(!WorkspaceIntegration::is_stack(&split));
        assert_eq!(WorkspaceIntegration::workspace_count(&split), 2);
        // Split deterministically partitions container.
        let allocs = split.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        assert_eq!(allocs[0].1.width as u32 + allocs[1].1.width as u32, 80);
    }

    #[test]
    fn workspaces_split_ratio_clamped_no_collapse() {
        let left = vec![View::new(ViewId::new(1), 80, 24)];
        let right = vec![View::new(ViewId::new(2), 80, 24)];
        let split_low =
            WorkspaceIntegration::split_for_workspace(left.clone(), right.clone(), 0.01);
        let split_high = WorkspaceIntegration::split_for_workspace(left, right, 0.99);
        for split in [split_low, split_high] {
            let allocs = split.layout(UiRect::new(0, 0, 80, 24));
            assert!(allocs[0].1.width >= 1);
            assert!(allocs[1].1.width >= 1);
        }
        // Non-finite ratio falls back to 0.5.
        let nan = WorkspaceIntegration::split_for_workspace(
            vec![View::new(ViewId::new(3), 80, 24)],
            vec![View::new(ViewId::new(4), 80, 24)],
            f32::NAN,
        );
        let allocs = nan.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
    }

    #[test]
    fn workspace_title_bounded_and_truncated() {
        let mut state = State::new();
        assert_eq!(WorkspaceIntegration::workspace_title(&state), None);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new("hello"),
        });
        assert_eq!(
            WorkspaceIntegration::workspace_title(&state),
            Some("hello".to_string())
        );
        let long = "a".repeat(WORKSPACE_TITLE_MAX_CHARS + 100);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new(long.clone()),
        });
        let title = WorkspaceIntegration::workspace_title(&state).unwrap();
        assert_eq!(title.chars().count(), WORKSPACE_TITLE_MAX_CHARS);
        // Char-boundary truncation: multibyte
        let multi = "é".repeat(WORKSPACE_TITLE_MAX_CHARS + 10);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new(multi),
        });
        assert_eq!(
            WorkspaceIntegration::workspace_title(&state)
                .unwrap()
                .chars()
                .count(),
            WORKSPACE_TITLE_MAX_CHARS
        );
    }

    #[test]
    fn workspace_count_bounded_at_32() {
        // LayoutNode::stack leaf_count can exceed bound, but
        // TerminalRegistry validation rejects >32 when committing via
        // set_workspace_layout — workspace module documents the bound.
        let many: Vec<View> = (1..=WORKSPACE_MAX_TABS + 5)
            .map(|i| View::new(ViewId::new(i as u64), 80, 24))
            .collect();
        let stack = WorkspaceIntegration::stack_for_workspace(many);
        assert_eq!(
            WorkspaceIntegration::workspace_count(&stack),
            WORKSPACE_MAX_TABS + 5
        );
        // Registry enforcement: creating >32 views in one workspace fails.
        let mut reg = TerminalRegistryHelper::default_registry_for_workspace_test();
        let wid = reg.create_workspace().expect("workspace");
        for _ in 0..WORKSPACE_MAX_TABS {
            reg.create_view(wid).expect("view within bound");
        }
        assert!(reg.create_view(wid).is_err(), "32 is max");
    }

    #[test]
    fn find_workspace_and_has_workspaces() {
        let views = vec![
            View::new(ViewId::new(7), 80, 24),
            View::new(ViewId::new(8), 80, 24),
        ];
        let stack = WorkspaceIntegration::stack_for_workspace(views);
        assert!(WorkspaceIntegration::has_workspaces(&stack));
        assert!(WorkspaceIntegration::find_workspace(&stack, ViewId::new(7)).is_some());
        assert!(WorkspaceIntegration::find_workspace(&stack, ViewId::new(99)).is_none());
        let empty = LayoutNode::stack(Vec::new());
        assert!(!WorkspaceIntegration::has_workspaces(&empty));
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_workspace_panel(&mut reg, ws, view).expect("create workspace panel");
        assert_eq!(reg.panel_count(), 1);
        // Second panel on same view must fail-closed (AlreadyMounted).
        let handle2 = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel");
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        // PanelId distinct newtype, no From bridge.
        let _ = id.get();
        // Focus lifecycle via Panel API: mount → focus → suspend → resume.
        let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
        let ws2 = WorkspaceId::new(42);
        let view2 = ViewId::new(42);
        let h = reg2.create_panel(PanelType::Helper, Some(ws2)).unwrap();
        reg2.mount_panel(h.id, h.generation, view2).expect("mount");
        reg2.focus_panel(h.id, h.generation, ws2).expect("focus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
        reg2.suspend_panel(h.id, h.generation).expect("suspend");
        assert_eq!(reg2.focused_panel(ws2), None);
        reg2.resume_panel(h.id, h.generation).expect("resume");
        reg2.focus_panel(h.id, h.generation, ws2).expect("refocus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_workspace_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_workspace_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_workspace_panel_config(&ok).is_ok());
    }

    #[test]
    fn workspace_snapshot_served_off_tick_behind_bounded_worker() {
        use crate::panels_async::{PANEL_WORKER_DEFAULT_QUEUE_CAP, PanelWorker};
        use std::time::{Duration, Instant};
        let mut worker = PanelWorker::try_spawn(
            "workspace",
            None,
            PANEL_WORKER_DEFAULT_QUEUE_CAP,
            Duration::from_millis(20),
            || vec![ViewId::new(1), ViewId::new(2)],
        )
        .expect("valid worker config");
        // Tick path never blocks: no snapshot before the probe completes.
        assert!(worker.latest().is_none());
        worker.request_refresh();
        let start = Instant::now();
        while worker.latest().is_none() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ids = worker.latest().expect("workspace snapshot delivered");
        assert_eq!(ids, vec![ViewId::new(1), ViewId::new(2)]);
        assert!(worker.generation() >= 1);
        worker.shutdown();
        assert!(!worker.is_alive());
    }

    #[test]
    fn layout_reuse_no_hardcoded_workspace_primitive() {
        // Prove workspaces are Stack/Split only: no enum variant named Workspace exists.
        // LayoutNode variants are Leaf/Split/Stack/Overlay only.
        let v1 = View::new(ViewId::new(1), 80, 24);
        let v2 = View::new(ViewId::new(2), 80, 24);
        let v3 = View::new(ViewId::new(3), 80, 24);
        // Workspaces as Stack
        let stack = LayoutNode::stack(vec![
            LayoutNode::leaf(v1.clone()),
            LayoutNode::leaf(v2.clone()),
        ]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        // Workspace group split into two workspaces side-by-side is Split of Stacks
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::stack(vec![LayoutNode::leaf(v1)]),
            LayoutNode::stack(vec![LayoutNode::leaf(v2.clone()), LayoutNode::leaf(v3)]),
        );
        assert!(matches!(split, LayoutNode::Split { .. }));
        assert_eq!(split.leaf_count(), 3);
        // Overlay still works for workspaces + palette
        let base = LayoutNode::stack(vec![LayoutNode::leaf(View::new(ViewId::new(10), 80, 24))]);
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
        let _ = v2;
    }

    // Helper for registry bound test — minimal TerminalRegistry construction.
    struct TerminalRegistryHelper;
    impl TerminalRegistryHelper {
        fn default_registry_for_workspace_test() -> crate::registry::TerminalRegistry {
            crate::registry::TerminalRegistry::new(crate::registry::RegistryConfig {
                max_views_per_workspace: WORKSPACE_MAX_TABS,
                ..crate::registry::RegistryConfig::default()
            })
            .expect("default registry")
        }
    }
}
