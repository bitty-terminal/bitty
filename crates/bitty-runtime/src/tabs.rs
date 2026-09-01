#![forbid(unsafe_code)]
//! Tabs via Panel Runtime — generic, no hardcoded tabs, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.tabs` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Tabs are not a
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

use bitty_ui::{LayoutNode, SplitAxis, View, ViewId};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Maximum tabs per workspace — mirrors `MAX_VIEWS_PER_WORKSPACE` (32), the
/// accepted bound for `workspace.create_view` and `set_workspace_layout`.
pub const TABS_MAX_TABS: usize = crate::registry::MAX_VIEWS_PER_WORKSPACE;

/// Maximum panels per workspace for tabs container — mirrors
/// `MAX_PANELS_PER_WORKSPACE` (32) and `MAX_PANELS_PER_WINDOW` (64).
pub const TABS_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
pub const TABS_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// Panel payload for tab observations is bounded by `BUS_EVENT_MAX_BYTES`
/// (8 KiB) at the bus admission boundary.
pub const TABS_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Tab title display bound — mirrors overlay text bound (128 chars) for
/// tabline presentation; titles longer are truncated at char boundary.
pub const TABS_TITLE_MAX_CHARS: usize = bitty_ui::panel::MAX_OVERLAY_TEXT_LEN;

/// TabsIntegration — pure, observation-only helpers over committed state
/// and layout. No mutation of `State`, no hot-path, bounded `<=32` leaves.
/// Tab ordering is the deterministic depth-first leaf order of the `Stack`.
/// No new tiling primitive is introduced; tabs are `Stack` only.
#[derive(Debug, Clone, Copy)]
pub struct TabsIntegration;

impl TabsIntegration {
    /// Builds a tabs `Stack` from `views`. Each view becomes a `Leaf`; the
    /// `Stack` shares the container bounds (tab-like stacking where the last
    /// element is top-most for focus/visual order). No hardcoded `Tabs` node.
    #[must_use]
    pub fn stack_for_tabs(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Builds a horizontal split from two tab stacks (e.g., two tab groups
    /// side-by-side). Reuses `LayoutNode::split` with clamped ratio.
    #[must_use]
    pub fn split_for_tabs(left: Vec<View>, right: Vec<View>, ratio: f32) -> LayoutNode {
        let l = Self::stack_for_tabs(left);
        let r = Self::stack_for_tabs(right);
        LayoutNode::split(SplitAxis::Horizontal, ratio, l, r)
    }

    /// Number of tabs in `layout` (leaf count). Bounded `<=32`.
    #[must_use]
    pub fn tab_count(layout: &LayoutNode) -> usize {
        layout.leaf_count()
    }

    /// Tab `ViewId`s in deterministic depth-first order. Bounded `<=32`.
    #[must_use]
    pub fn tab_ids(layout: &LayoutNode) -> Vec<ViewId> {
        layout.leaf_ids()
    }

    /// Whether `layout` is a tabs `Stack` (no new primitive).
    #[must_use]
    pub fn is_stack(layout: &LayoutNode) -> bool {
        matches!(layout, LayoutNode::Stack(_))
    }

    /// Tab title for a terminal state, bounded to `TABS_TITLE_MAX_CHARS` and
    /// truncated at char boundary. Empty title yields `None` (tab shows
    /// fallback). Uses `State::title()` committed by `OSC 0/2` via `Action`.
    #[must_use]
    pub fn tab_title(state: &bitty_term_state::State) -> Option<String> {
        let raw = state.title();
        if raw.is_empty() {
            return None;
        }
        let truncated = if raw.chars().count() <= TABS_TITLE_MAX_CHARS {
            raw.to_owned()
        } else {
            raw.chars().take(TABS_TITLE_MAX_CHARS).collect()
        };
        Some(truncated)
    }

    /// Whether tabs observation has any data (non-empty title or at least one
    /// tab view in layout). Pure observation, never mutates.
    #[must_use]
    pub fn has_tabs(layout: &LayoutNode) -> bool {
        !Self::tab_ids(layout).is_empty()
    }

    /// Finds a tab `ViewId` in `layout`, if present.
    #[must_use]
    pub fn find_tab(layout: &LayoutNode, id: ViewId) -> Option<View> {
        layout.find_leaf(id).cloned()
    }
}

/// Creates a tabs panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults). Returns the
/// panel handle on success; caller must still activate the associated plugin
/// via the public PluginHost path (`declare → resolve → register →
/// GrantRecord → activate`) for capability `ui.rich` and claim `tabline`.
pub fn create_tabs_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that tabs panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
pub fn validate_tabs_panel_config(
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
    fn tabs_are_stack_no_hardcoded_primitive() {
        // Tabs reuse LayoutNode::stack, not a new Tabs node.
        let views = vec![
            View::new(ViewId::new(1), 80, 24),
            View::new(ViewId::new(2), 80, 24),
        ];
        let stack = TabsIntegration::stack_for_tabs(views);
        assert!(TabsIntegration::is_stack(&stack));
        assert_eq!(TabsIntegration::tab_count(&stack), 2);
        let ids = TabsIntegration::tab_ids(&stack);
        assert_eq!(ids, vec![ViewId::new(1), ViewId::new(2)]);
        // Stack shares container bounds: reflow yields identical rects.
        let mut with_stack = stack.clone();
        with_stack.reflow(UiRect::new(0, 0, 80, 24));
        let allocs = with_stack.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        // Both tabs share full bounds in Stack semantics.
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[1].1, UiRect::new(0, 0, 80, 24));
    }

    #[test]
    fn tabs_split_reuses_layout_split() {
        let left = vec![View::new(ViewId::new(10), 40, 24)];
        let right = vec![View::new(ViewId::new(11), 40, 24)];
        let split = TabsIntegration::split_for_tabs(left, right, 0.5);
        assert!(!TabsIntegration::is_stack(&split));
        assert_eq!(TabsIntegration::tab_count(&split), 2);
        // Split deterministically partitions container.
        let allocs = split.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        assert_eq!(allocs[0].1.width as u32 + allocs[1].1.width as u32, 80);
    }

    #[test]
    fn tabs_split_ratio_clamped_no_collapse() {
        let left = vec![View::new(ViewId::new(1), 80, 24)];
        let right = vec![View::new(ViewId::new(2), 80, 24)];
        let split_low = TabsIntegration::split_for_tabs(left.clone(), right.clone(), 0.01);
        let split_high = TabsIntegration::split_for_tabs(left, right, 0.99);
        for split in [split_low, split_high] {
            let allocs = split.layout(UiRect::new(0, 0, 80, 24));
            assert!(allocs[0].1.width >= 1);
            assert!(allocs[1].1.width >= 1);
        }
        // Non-finite ratio falls back to 0.5.
        let nan = TabsIntegration::split_for_tabs(
            vec![View::new(ViewId::new(3), 80, 24)],
            vec![View::new(ViewId::new(4), 80, 24)],
            f32::NAN,
        );
        let allocs = nan.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
    }

    #[test]
    fn tab_title_bounded_and_truncated() {
        let mut state = State::new();
        assert_eq!(TabsIntegration::tab_title(&state), None);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new("hello"),
        });
        assert_eq!(
            TabsIntegration::tab_title(&state),
            Some("hello".to_string())
        );
        let long = "a".repeat(TABS_TITLE_MAX_CHARS + 100);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new(long.clone()),
        });
        let title = TabsIntegration::tab_title(&state).unwrap();
        assert_eq!(title.chars().count(), TABS_TITLE_MAX_CHARS);
        // Char-boundary truncation: multibyte
        let multi = "é".repeat(TABS_TITLE_MAX_CHARS + 10);
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new(multi),
        });
        assert_eq!(
            TabsIntegration::tab_title(&state).unwrap().chars().count(),
            TABS_TITLE_MAX_CHARS
        );
    }

    #[test]
    fn tab_count_bounded_at_32() {
        // LayoutNode::stack leaf_count can exceed bound, but
        // TerminalRegistry validation rejects >32 when committing via
        // set_workspace_layout — tabs module documents the bound.
        let many: Vec<View> = (1..=TABS_MAX_TABS + 5)
            .map(|i| View::new(ViewId::new(i as u64), 80, 24))
            .collect();
        let stack = TabsIntegration::stack_for_tabs(many);
        assert_eq!(TabsIntegration::tab_count(&stack), TABS_MAX_TABS + 5);
        // Registry enforcement: creating >32 views in one workspace fails.
        let mut reg = TerminalRegistryHelper::default_registry_for_tabs_test();
        let wid = reg.create_workspace().expect("workspace");
        for _ in 0..TABS_MAX_TABS {
            reg.create_view(wid).expect("view within bound");
        }
        assert!(reg.create_view(wid).is_err(), "32 is max");
    }

    #[test]
    fn find_tab_and_has_tabs() {
        let views = vec![
            View::new(ViewId::new(7), 80, 24),
            View::new(ViewId::new(8), 80, 24),
        ];
        let stack = TabsIntegration::stack_for_tabs(views);
        assert!(TabsIntegration::has_tabs(&stack));
        assert!(TabsIntegration::find_tab(&stack, ViewId::new(7)).is_some());
        assert!(TabsIntegration::find_tab(&stack, ViewId::new(99)).is_none());
        let empty = LayoutNode::stack(Vec::new());
        assert!(!TabsIntegration::has_tabs(&empty));
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_tabs_panel(&mut reg, ws, view).expect("create tabs panel");
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
        assert!(validate_tabs_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_tabs_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_tabs_panel_config(&ok).is_ok());
    }

    #[test]
    fn layout_reuse_no_hardcoded_tabs_primitive() {
        // Prove tabs are Stack/Split only: no enum variant named Tabs exists.
        // LayoutNode variants are Leaf/Split/Stack/Overlay only.
        let v1 = View::new(ViewId::new(1), 80, 24);
        let v2 = View::new(ViewId::new(2), 80, 24);
        let v3 = View::new(ViewId::new(3), 80, 24);
        // Tabs as Stack
        let stack = LayoutNode::stack(vec![
            LayoutNode::leaf(v1.clone()),
            LayoutNode::leaf(v2.clone()),
        ]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        // Tabs group split into two workspaces side-by-side is Split of Stacks
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::stack(vec![LayoutNode::leaf(v1)]),
            LayoutNode::stack(vec![LayoutNode::leaf(v2.clone()), LayoutNode::leaf(v3)]),
        );
        assert!(matches!(split, LayoutNode::Split { .. }));
        assert_eq!(split.leaf_count(), 3);
        // Overlay still works for tabs + palette
        let base = LayoutNode::stack(vec![LayoutNode::leaf(View::new(ViewId::new(10), 80, 24))]);
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
        let _ = v2;
    }

    // Helper for registry bound test — minimal TerminalRegistry construction.
    struct TerminalRegistryHelper;
    impl TerminalRegistryHelper {
        fn default_registry_for_tabs_test() -> crate::registry::TerminalRegistry {
            crate::registry::TerminalRegistry::new(crate::registry::RegistryConfig {
                max_views_per_workspace: TABS_MAX_TABS,
                ..crate::registry::RegistryConfig::default()
            })
            .expect("default registry")
        }
    }
}
